use crate::common::gateway_api::referencegrants::ReferenceGrant;
use std::{collections::BTreeSet, sync::Arc};

use crate::{
    common::{Backend, Gateway, ProtocolType, ReferenceValidateRequest, ResourceKey, TlsType},
    controllers::find_linked_routes,
    state::State,
};
use futures::future::BoxFuture;
use futures::FutureExt;
use gateway_api::apis::standard::referencegrants::{ReferenceGrantFrom, ReferenceGrantTo};
use kube::{api::ListParams, Api, Client};
use kube_core::ObjectList;
use tokio::time;
use tracing::{span, warn, Level};
use typed_builder::TypedBuilder;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct ReferenceGrantRef {
    from: ResourceKey,
    to: ResourceKey,
}

impl ReferenceGrantRef {
    fn new(from: ResourceKey, to: ResourceKey) -> Self {
        Self { from, to }
    }
}

#[derive(Clone, TypedBuilder)]
pub struct ReferenceGrantsResolver {
    client: Client,
    #[builder(default)]
    references: BTreeSet<ReferenceGrantRef>,
    #[builder(default)]
    resolved_grants: Arc<tokio::sync::Mutex<BTreeSet<ReferenceGrantRef>>>,
    reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>,
    state: State,
}

impl ReferenceGrantsResolver {
    fn search_references(&self, gateway: &Gateway) -> BTreeSet<ReferenceGrantRef> {
        let gateway_key = gateway.key();

        let linked_routes = find_linked_routes(&self.state, gateway_key);

        let mut backend_reference_keys = BTreeSet::new();

        for route in linked_routes {
            let from = route.resource_key();
            let route_config = route.config();
            for rule in &route_config.routing_rules {
                for backend in &rule.backends {
                    if let Backend::Maybe(backend_service_config) = backend {
                        backend_reference_keys.insert(ReferenceGrantRef::new(from.clone(), backend_service_config.resource_key.clone()));
                    };
                }
            }
        }

        let mut secrets_references = BTreeSet::new();
        for listener in gateway.listeners().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
            let listener_data = listener.data();
            if let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type {
                for certificate in certificates {
                    let certificate_key = certificate.resouce_key();
                    secrets_references.insert(ReferenceGrantRef::new(gateway_key.clone(), certificate_key.clone()));
                }
            }
        }
        backend_reference_keys.append(&mut secrets_references);
        backend_reference_keys
    }

    pub async fn add_references_by_gateway(&mut self, gateway: &Gateway) {
        let mut references = self.search_references(gateway);
        self.references.append(&mut references);
    }

    pub async fn delete_references_by_gateway(&mut self, gateway: &Gateway) {
        let mut references = self.search_references(gateway);
        let diff = self.references.difference(&mut references).cloned();
        self.references = diff.collect();
    }

    pub async fn resolve(self, key_namespace: String) {
        let client = self.client.clone();
        let api: Api<ReferenceGrant> = Api::namespaced(client, &key_namespace);
        let resolver_fn = move |_: String| {
            let api = api.clone();
            async move {
                let api = api.clone();
                let lp = ListParams::default();
                api.list(&lp).await
            }
            .boxed()
        };

        //let b = resolver_fn("balls".to_owned()).await;
        self.start_resolve_loop(resolver_fn).await
        //self.resolve_internal(resolver_fn);
    }

    pub async fn start_resolve_loop<U>(self, resolver_fn: U)
    where
        U: Fn(String) -> BoxFuture<'static, Result<ObjectList<ReferenceGrant>, kube::Error>> + Clone,
        //U: Fn(String) -> BoxFuture<'static, Result<(), kube::Error>>,
    {
        let mut interval = time::interval(time::Duration::from_secs(10));
        let span = span!(Level::INFO, "ReferenceGrantsResolver");
        let _entered = span.enter();

        loop {
            let this = self.clone();
            interval.tick().await;
            this.resolve_internal(resolver_fn.clone()).await
        }
    }

    async fn resolve_internal<U>(self, resolver_fn: U)
    where
        U: Fn(String) -> BoxFuture<'static, Result<ObjectList<ReferenceGrant>, kube::Error>>,
    {
        let mut allowed_grants = BTreeSet::new();

        let mut configured_grants = BTreeSet::new();

        let resolved_namespace_keys: BTreeSet<&ResourceKey> = self.references.iter().map(|reference| &reference.to).collect();
        for resolved_namespace_key in resolved_namespace_keys {
            if let Ok(reference_grants) = resolver_fn(resolved_namespace_key.namespace.clone()).await {
                for grant in reference_grants {
                    for from in &grant.spec.from {
                        for to in &grant.spec.to {
                            ReferenceGrantRef::new(ResourceKey::from(from), ResourceKey::from(to));
                            configured_grants.insert(ReferenceGrantRef::new(ResourceKey::from(from), ResourceKey::from(to)));
                        }
                    }
                }
            } else {
                warn!("Unable to list ReferenceGrants for {resolved_namespace_key}");
            }
        }

        for reference in &self.references {
            if configured_grants.contains(reference) {
                allowed_grants.insert(reference);
            }
        }
        let mut resolved_grants = self.resolved_grants.lock().await;
        *resolved_grants = allowed_grants.into_iter().cloned().collect();
    }

    pub async fn is_allowed(&self, from: &ResourceKey, to: &ResourceKey) -> bool {
        self.resolved_grants.lock().await.contains(&ReferenceGrantRef::new(from.clone(), to.clone()))
    }
}

impl From<&ReferenceGrantFrom> for ResourceKey {
    fn from(value: &ReferenceGrantFrom) -> Self {
        ResourceKey {
            group: value.group.clone(),
            namespace: value.namespace.clone(),
            name: String::default(),
            kind: value.kind.clone(),
        }
    }
}

impl From<&ReferenceGrantTo> for ResourceKey {
    fn from(value: &ReferenceGrantTo) -> Self {
        ResourceKey {
            group: value.group.clone(),
            namespace: String::default(),
            name: value.name.as_ref().unwrap_or(&String::new()).to_owned(),
            kind: value.kind.clone(),
        }
    }
}
#[cfg(test)]
mod tests {
    use gateway_api::apis::standard::referencegrants::ReferenceGrantSpec;
    use kube_core::{ApiResource, ListMeta, ObjectMeta, TypeMeta};
    use tokio::sync::{mpsc, Mutex};

    use super::*;

    async fn it_works() -> Result<(), kube::Error> {
        let client = Client::try_default().await?;
        let (sender, _receiver) = mpsc::channel(100);
        let reference_grant_resolver = ReferenceGrantsResolver {
            client,
            references: BTreeSet::new(),
            resolved_grants: Arc::new(Mutex::new(BTreeSet::new())),
            reference_validate_channel_sender: sender,
            state: State::new(),
        };
        let resolver_fn = move |_: String| {
            async move {
                let ar = ApiResource::erase::<ReferenceGrant>(&());
                let reference_grant_list: ObjectList<ReferenceGrant> = ObjectList {
                    types: TypeMeta {
                        api_version: ar.api_version,
                        kind: ar.kind + "List",
                    },
                    metadata: ListMeta { ..Default::default() },
                    items: vec![ReferenceGrant {
                        metadata: ObjectMeta {
                            name: Some("test".into()),
                            namespace: Some("dev".into()),
                            ..ObjectMeta::default()
                        },
                        spec: ReferenceGrantSpec::default(),
                    }],
                };
                Result::Ok(reference_grant_list)
            }
            .boxed()
        };
        reference_grant_resolver.resolve_internal(resolver_fn).await;
        Ok(())
    }
}
