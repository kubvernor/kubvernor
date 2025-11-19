use std::{collections::BTreeSet, sync::Arc};

use futures::{future::BoxFuture, FutureExt};
use gateway_api::referencegrants::{ReferenceGrant, ReferenceGrantFrom, ReferenceGrantTo};
use kube::{api::ListParams, Api, Client, ResourceExt};
use kube_core::ObjectList;
use tokio::time;
use tracing::{info, span, warn, Level};
use typed_builder::TypedBuilder;

use crate::{
    common::{resource_key::DEFAULT_GROUP_NAME, Backend, Gateway, ProtocolType, ReferenceValidateRequest, ResourceKey, TlsType},
    controllers::find_linked_routes,
    state::State,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, TypedBuilder)]
pub struct ReferenceGrantRef {
    from: FromResourceKey,
    to: ToResourceKey,
    gateway_key: ResourceKey,
    namespace: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FromResourceKey {
    pub group: String,
    pub namespace: String,
    pub kind: String,
}

impl From<ResourceKey> for FromResourceKey {
    fn from(rk: ResourceKey) -> Self {
        Self {
            group: rk.group,
            namespace: rk.namespace,
            kind: rk.kind,
        }
    }
}

impl From<&ResourceKey> for FromResourceKey {
    fn from(rk: &ResourceKey) -> Self {
        Self::from(rk.clone())
    }
}

impl From<&ReferenceGrantFrom> for FromResourceKey {
    fn from(rk: &ReferenceGrantFrom) -> Self {
        Self {
            group: rk.group.clone(),
            namespace: rk.namespace.clone(),
            kind: rk.kind.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ToResourceKey {
    pub group: String,
    pub name: Option<String>,
    pub kind: String,
}

impl From<&ResourceKey> for ToResourceKey {
    fn from(rk: &ResourceKey) -> Self {
        Self::from(rk.clone())
    }
}

impl From<ResourceKey> for ToResourceKey {
    fn from(rk: ResourceKey) -> Self {
        Self {
            group: if rk.group.is_empty() { DEFAULT_GROUP_NAME.to_owned() } else { rk.group },
            name: if rk.name.is_empty() { None } else { Some(rk.name) },
            kind: rk.kind,
        }
    }
}

impl From<ReferenceGrantTo> for ToResourceKey {
    fn from(rk: ReferenceGrantTo) -> Self {
        Self {
            group: if rk.group.is_empty() { DEFAULT_GROUP_NAME.to_owned() } else { rk.group },
            name: rk.name,
            kind: rk.kind,
        }
    }
}

impl From<&ReferenceGrantTo> for ToResourceKey {
    fn from(rk: &ReferenceGrantTo) -> Self {
        Self::from(rk.clone())
    }
}

#[derive(Clone, TypedBuilder)]
pub struct ReferenceGrantsResolver {
    client: Client,
    #[builder(default)]
    references: Arc<tokio::sync::Mutex<BTreeSet<ReferenceGrantRef>>>,
    #[builder(default)]
    resolved_reference_grants: Arc<tokio::sync::Mutex<BTreeSet<ReferenceGrantRef>>>,
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

            for backend in &route.backends() {
                if let Backend::Maybe(backend_service_config) = backend {
                    let to = &backend_service_config.resource_key;
                    backend_reference_keys.insert(
                        ReferenceGrantRef::builder()
                            .namespace(to.namespace.clone())
                            .from(from.into())
                            .to(to.into())
                            .gateway_key(gateway_key.clone())
                            .build(),
                    );
                };
            }
        }

        let mut secrets_references = BTreeSet::new();
        for listener in gateway.listeners().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
            let listener_data = listener.data();
            if let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type {
                for certificate in certificates {
                    let certificate_key = certificate.resouce_key();
                    secrets_references.insert(
                        ReferenceGrantRef::builder()
                            .namespace(certificate_key.namespace.clone())
                            .from(gateway_key.into())
                            .to(certificate_key.into())
                            .gateway_key(gateway_key.clone())
                            .build(),
                    );
                }
            }
        }
        backend_reference_keys.append(&mut secrets_references);
        backend_reference_keys
    }

    pub async fn add_references_by_gateway(&self, gateway: &Gateway) {
        let key = gateway.key();
        let mut references = self.search_references(gateway);
        info!("Adding references gateway {key:?} : {references:?} ");
        self.references.lock().await.append(&mut references);
    }

    pub async fn delete_references_by_gateway(&self, gateway: &Gateway) {
        let gateway_references = self.search_references(gateway);
        let mut references = self.references.lock().await;
        let diff = references.difference(&gateway_references).cloned();
        *references = diff.collect();
    }

    pub async fn resolve(self) -> crate::Result<()> {
        let client = self.client.clone();

        let resolver_fn = move |key_namespace: String| {
            let client = client.clone();
            let api: Api<ReferenceGrant> = Api::namespaced(client, &key_namespace);
            let api = api.clone();
            async move {
                let api = api.clone();
                let lp = ListParams::default();
                api.list(&lp).await
            }
            .boxed()
        };

        self.start_resolve_loop(resolver_fn).await;
        Ok(())
    }

    pub async fn start_resolve_loop<U>(self, resolver_fn: U)
    where
        U: Fn(String) -> BoxFuture<'static, Result<ObjectList<ReferenceGrant>, kube::Error>> + Clone,
    {
        let mut interval = time::interval(time::Duration::from_secs(10));
        let span = span!(Level::INFO, "ReferenceGrantsResolver");
        let _entered = span.enter();

        loop {
            let this = self.clone();
            interval.tick().await;
            this.resolve_internal(resolver_fn.clone()).await;
        }
    }

    async fn resolve_internal<U>(self, resolver_fn: U)
    where
        U: Fn(String) -> BoxFuture<'static, Result<ObjectList<ReferenceGrant>, kube::Error>>,
    {
        let mut resolved_reference_grants = self.resolved_reference_grants.lock().await;
        let mut configured_reference_grants = BTreeSet::new();

        let references = self.references.lock().await;

        for resolved_reference in references.iter() {
            let resolved_namespace = &resolved_reference.namespace;
            if let Ok(reference_grants) = resolver_fn(resolved_namespace.clone()).await {
                for grant in reference_grants {
                    for from in &grant.spec.from {
                        for to in &grant.spec.to {
                            configured_reference_grants.insert(
                                ReferenceGrantRef::builder()
                                    .namespace(grant.namespace().unwrap_or(String::new()))
                                    .from(from.into())
                                    .to(to.into())
                                    .gateway_key(resolved_reference.gateway_key.clone())
                                    .build(),
                            );
                        }
                    }
                }
            } else {
                warn!("Unable to list ReferenceGrants for {resolved_namespace}");
            }
        }

        info!("All available grants {configured_reference_grants:?}");
        let mut allowed_reference_grants = BTreeSet::new();

        for reference in references.iter() {
            let valid = configured_reference_grants.iter().any(|rgr| {
                rgr == reference || {
                    let mut temp = reference.clone();
                    temp.to.name = None;
                    rgr == &temp
                }
            });
            info!("Checking reference {reference:?} {valid}");
            if valid {
                allowed_reference_grants.insert(reference.clone());
            }
        }

        let removed_reference_grants = resolved_reference_grants.difference(&allowed_reference_grants);
        info!("Removed grants {removed_reference_grants:?}");
        let added_reference_grants = allowed_reference_grants.difference(&resolved_reference_grants);
        info!("Added grants {added_reference_grants:?}");

        let changed_gateways: BTreeSet<_> = added_reference_grants.chain(removed_reference_grants).collect();

        info!("Changed gateways {changed_gateways:?}");

        if !changed_gateways.is_empty() {
            let _res = self
                .reference_validate_channel_sender
                .send(ReferenceValidateRequest::UpdatedGateways {
                    reference: ResourceKey {
                        group: "ReferenceGrant".to_owned(),
                        kind: "ReferenceGrant".to_owned(),
                        ..Default::default()
                    },
                    gateways: changed_gateways.iter().map(|f| f.gateway_key.clone()).collect(),
                })
                .await;
        }

        *resolved_reference_grants = allowed_reference_grants.into_iter().collect();
    }

    pub async fn is_allowed(&self, from: &ResourceKey, to: &ResourceKey, gateway_key: &ResourceKey) -> bool {
        let resolved_grants = self.resolved_reference_grants.lock().await;
        info!("Resolved grants {resolved_grants:?}");
        resolved_grants.contains(
            &ReferenceGrantRef::builder()
                .namespace(to.namespace.clone())
                .from(from.into())
                .to(to.into())
                .gateway_key(gateway_key.clone())
                .build(),
        )
    }
}

// fn normalize_from_and_to(mut from: ResourceKey, mut to: ResourceKey) -> (ResourceKey, ResourceKey) {
//     from.name = String::new();
//     to.namespace = String::new();
//     (from, to)
// }

impl From<&ReferenceGrantFrom> for ResourceKey {
    fn from(value: &ReferenceGrantFrom) -> Self {
        if value.group.is_empty() {
            ResourceKey {
                group: DEFAULT_GROUP_NAME.to_owned(),
                namespace: value.namespace.clone(),
                name: String::default(),
                kind: value.kind.clone(),
            }
        } else {
            ResourceKey {
                group: value.group.clone(),
                namespace: value.namespace.clone(),
                name: String::default(),
                kind: value.kind.clone(),
            }
        }
    }
}

impl From<&ReferenceGrantTo> for ResourceKey {
    fn from(value: &ReferenceGrantTo) -> Self {
        if value.group.is_empty() {
            ResourceKey {
                group: DEFAULT_GROUP_NAME.to_owned(),
                namespace: String::default(),
                name: value.name.as_ref().unwrap_or(&String::new()).to_owned(),
                kind: value.kind.clone(),
            }
        } else {
            ResourceKey {
                group: value.group.clone(),
                namespace: String::default(),
                name: value.name.as_ref().unwrap_or(&String::new()).to_owned(),
                kind: value.kind.clone(),
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use gateway_api::referencegrants;
    use http::{Request, Response};
    use kube::client::Body;
    use kube_core::{ApiResource, ListMeta, ObjectMeta, TypeMeta};
    use referencegrants::ReferenceGrantSpec;
    use tokio::sync::{mpsc, Mutex};
    use tower_test::mock;

    use super::*;

    #[tokio::test]
    async fn test_resolver_references() {
        let (mock_service, mut _handle) = mock::pair::<Request<Body>, Response<Body>>();
        let client = Client::new(mock_service, "default");
        let (sender, mut receiver) = mpsc::channel(100);
        let reference_grant_resolver = ReferenceGrantsResolver {
            client,
            references: Arc::new(Mutex::new(BTreeSet::new())),
            resolved_reference_grants: Arc::new(Mutex::new(BTreeSet::new())),
            reference_validate_channel_sender: sender,
            state: State::new(),
        };

        let resolver_fn = move |_: String| {
            async move {
                let tos: Vec<ReferenceGrantTo> = vec![ReferenceGrantTo {
                    group: "to_group_1".to_owned(),
                    kind: "to_kind_1".to_owned(),
                    name: Some("to_name_1".to_owned()),
                }];
                let froms: Vec<ReferenceGrantFrom> = vec![ReferenceGrantFrom {
                    group: "from_group_1".to_owned(),
                    kind: "from_kind_1".to_owned(),
                    namespace: "from_namespace_1".to_owned(),
                }];
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
                            namespace: Some("to_namespace_3".into()),
                            ..ObjectMeta::default()
                        },
                        spec: ReferenceGrantSpec { to: tos, from: froms },
                    }],
                };
                Result::Ok(reference_grant_list)
            }
            .boxed()
        };

        let cloned_reference_grant_resolver = reference_grant_resolver.clone();
        reference_grant_resolver.resolve_internal(resolver_fn).await;
        assert!(cloned_reference_grant_resolver.resolved_reference_grants.lock().await.is_empty());

        let to_1 = ResourceKey {
            group: "to_group_1".to_owned(),
            kind: "to_kind_1".to_owned(),
            name: "to_name_1".to_owned(),
            namespace: "to_namespace_3".to_owned(),
        };
        let from_1 = ResourceKey {
            group: "from_group_1".to_owned(),
            kind: "from_kind_1".to_owned(),
            namespace: "from_namespace_1".to_owned(),
            ..Default::default()
        };

        let to_2 = ResourceKey {
            group: "to_group_2".to_owned(),
            kind: "to_kind_2".to_owned(),
            name: "to_name_2".to_owned(),
            namespace: String::new(),
        };
        let from_2 = ResourceKey {
            group: "from_group_2".to_owned(),
            kind: "from_kind_2".to_owned(),
            namespace: "from_namespace_2".to_owned(),
            ..Default::default()
        };

        let gateway_id_1 = ResourceKey {
            group: "gateway".to_owned(),
            kind: "gateway".to_owned(),
            namespace: "namespace_1".to_owned(),
            name: "gateway_1".to_owned(),
        };
        let gateway_id_2 = ResourceKey {
            group: "gateway".to_owned(),
            kind: "gateway".to_owned(),
            namespace: "namespace_1".to_owned(),
            name: "gateway_2".to_owned(),
        };

        let mut gateway_references = BTreeSet::new();
        gateway_references.insert(
            ReferenceGrantRef::builder()
                .namespace(to_1.namespace.clone())
                .from((&from_1).into())
                .to((&to_1).into())
                .gateway_key(gateway_id_1.clone())
                .build(),
        );
        gateway_references.insert(
            ReferenceGrantRef::builder()
                .namespace(to_2.namespace.clone())
                .from((&from_2).into())
                .to((&to_2).into())
                .gateway_key(gateway_id_2.clone())
                .build(),
        );

        cloned_reference_grant_resolver.references.lock().await.append(&mut gateway_references);
        let reference_grant_resolver = cloned_reference_grant_resolver.clone();
        reference_grant_resolver.resolve_internal(resolver_fn).await;
        assert!(cloned_reference_grant_resolver.is_allowed(&from_1, &to_1, &gateway_id_1).await);
        assert!(!cloned_reference_grant_resolver.is_allowed(&from_2, &to_2, &gateway_id_2).await);

        tokio::spawn(async move {
            let event = receiver.recv().await;
            match event {
                Some(ReferenceValidateRequest::UpdatedGateways { reference: _, gateways }) => {
                    assert_eq!(gateways.len(), 1);
                }

                _ => {
                    panic!("Invalid event");
                }
            }
        });
    }

    #[tokio::test]
    async fn test_resolver_multiple_references() {
        let (mock_service, mut _handle) = mock::pair::<Request<Body>, Response<Body>>();
        let client = Client::new(mock_service, "default");
        let (sender, mut receiver) = mpsc::channel(100);
        let reference_grant_resolver = ReferenceGrantsResolver {
            client,
            references: Arc::new(Mutex::new(BTreeSet::new())),
            resolved_reference_grants: Arc::new(Mutex::new(BTreeSet::new())),
            reference_validate_channel_sender: sender,
            state: State::new(),
        };

        let resolver_fn = move |_: String| {
            async move {
                let tos: Vec<ReferenceGrantTo> = vec![
                    ReferenceGrantTo {
                        group: "to_group_1".to_owned(),
                        kind: "to_kind_1".to_owned(),
                        name: Some("to_name_1".to_owned()),
                    },
                    ReferenceGrantTo {
                        group: "to_group_2".to_owned(),
                        kind: "to_kind_2".to_owned(),
                        name: Some("to_name_2".to_owned()),
                    },
                ];
                let froms: Vec<ReferenceGrantFrom> = vec![
                    ReferenceGrantFrom {
                        group: "from_group_1".to_owned(),
                        kind: "from_kind_1".to_owned(),
                        namespace: "from_namespace_1".to_owned(),
                    },
                    ReferenceGrantFrom {
                        group: "from_group_2".to_owned(),
                        kind: "from_kind_2".to_owned(),
                        namespace: "from_namespace_2".to_owned(),
                    },
                ];
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
                            namespace: Some("to_namespace_3".into()),
                            ..ObjectMeta::default()
                        },
                        spec: ReferenceGrantSpec { to: tos, from: froms },
                    }],
                };
                Result::Ok(reference_grant_list)
            }
            .boxed()
        };

        let cloned_reference_grant_resolver = reference_grant_resolver.clone();
        reference_grant_resolver.resolve_internal(resolver_fn).await;
        assert!(cloned_reference_grant_resolver.resolved_reference_grants.lock().await.is_empty());

        let to_1 = ResourceKey {
            group: "to_group_1".to_owned(),
            kind: "to_kind_1".to_owned(),
            name: "to_name_1".to_owned(),
            namespace: "to_namespace_3".to_owned(),
        };
        let from_1 = ResourceKey {
            group: "from_group_1".to_owned(),
            kind: "from_kind_1".to_owned(),
            namespace: "from_namespace_1".to_owned(),
            ..Default::default()
        };

        let to_2 = ResourceKey {
            group: "to_group_2".to_owned(),
            kind: "to_kind_2".to_owned(),
            name: "to_name_2".to_owned(),
            namespace: "to_namespace_3".to_owned(),
        };
        let from_2 = ResourceKey {
            group: "from_group_2".to_owned(),
            kind: "from_kind_2".to_owned(),
            namespace: "from_namespace_2".to_owned(),
            ..Default::default()
        };

        let gateway_id_1 = ResourceKey {
            group: "gateway".to_owned(),
            kind: "gateway".to_owned(),
            namespace: "namespace_1".to_owned(),
            name: "gateway_1".to_owned(),
        };
        let gateway_id_2 = ResourceKey {
            group: "gateway".to_owned(),
            kind: "gateway".to_owned(),
            namespace: "namespace_1".to_owned(),
            name: "gateway_2".to_owned(),
        };

        let mut gateway_references = BTreeSet::new();
        gateway_references.insert(
            ReferenceGrantRef::builder()
                .namespace(to_1.namespace.clone())
                .from((&from_1).into())
                .to((&to_1).into())
                .gateway_key(gateway_id_1.clone())
                .build(),
        );
        gateway_references.insert(
            ReferenceGrantRef::builder()
                .namespace(to_2.namespace.clone())
                .from((&from_2).into())
                .to((&to_2).into())
                .gateway_key(gateway_id_2.clone())
                .build(),
        );

        cloned_reference_grant_resolver.references.lock().await.append(&mut gateway_references);
        let reference_grant_resolver = cloned_reference_grant_resolver.clone();
        reference_grant_resolver.resolve_internal(resolver_fn).await;
        assert!(cloned_reference_grant_resolver.is_allowed(&from_1, &to_1, &gateway_id_1).await);
        assert!(cloned_reference_grant_resolver.is_allowed(&from_2, &to_2, &gateway_id_2).await);

        tokio::spawn(async move {
            let event = receiver.recv().await;
            match event {
                Some(ReferenceValidateRequest::UpdatedGateways { reference: _, gateways }) => {
                    assert_eq!(gateways.len(), 2);
                }

                _ => {
                    panic!("Invalid event");
                }
            }
        });
    }
}
