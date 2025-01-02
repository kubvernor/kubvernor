use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kube::{Api, Client, Resource, ResourceExt};
use tokio::time;
use tracing::{debug, span, Instrument, Level};
use typed_builder::TypedBuilder;

use crate::common::{ReferenceValidateRequest, ResourceKey};
mod backends_resolver;
mod secrets_resolver;

pub use backends_resolver::BackendReferenceResolver;
pub use secrets_resolver::SecretsResolver;

#[derive(Clone, TypedBuilder)]
pub struct ReferencesResolver<R> {
    client: Client,
    #[builder(default)]
    backend_references: Arc<tokio::sync::Mutex<BTreeMap<ResourceKey, BTreeSet<ResourceKey>>>>,
    #[builder(default)]
    resolved_backend_references: Arc<tokio::sync::Mutex<BTreeMap<ResourceKey, R>>>,
    reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>,
}

impl<R> ReferencesResolver<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static + std::cmp::PartialEq,
    R: Resource<Scope = kube_core::NamespaceResourceScope>,
{
    pub async fn add_references<F>(&self, gateway_key: &ResourceKey, references: F)
    where
        F: Fn() -> BTreeSet<ResourceKey>,
    {
        let backend_reference_keys = references();

        debug!("Adding new backend reference for Gateway {gateway_key} BackendRef {backend_reference_keys:?}");
        let mut lock = self.backend_references.lock().await;

        for key in backend_reference_keys {
            lock.entry(key.clone())
                .and_modify(|set| {
                    set.insert(gateway_key.clone());
                })
                .or_insert_with(|| {
                    let mut set = BTreeSet::new();
                    set.insert(gateway_key.clone());
                    set
                });
        }
    }

    pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<R> {
        let resolved_backend_references = self.resolved_backend_references.lock().await;
        resolved_backend_references.get(resource_key).cloned()
    }

    pub async fn resolve(&self) {
        let mut interval = time::interval(time::Duration::from_secs(1));
        let span = span!(Level::INFO, "BackendReferenceResolver");
        let _entered = span.enter();
        loop {
            interval.tick().await;
            let secrets = {
                let secrets = self.backend_references.lock().await;
                secrets.clone()
            };

            for key in secrets.keys() {
                let key = key.clone();
                let myself = (*self).clone();
                let span = span!(Level::INFO, "BackendReferenceResolverTask", secret = %key);
                tokio::spawn(
                    async move {
                        debug!("Checkig backend reference  {key}");
                        let api: Api<R> = Api::namespaced(myself.client.clone(), &key.namespace);
                        if let Ok(service) = api.get(&key.name).await {
                            let mut update_gateway = false;
                            {
                                let mut resolved_backend_references = myself.resolved_backend_references.lock().await;
                                resolved_backend_references
                                    .entry(key.clone())
                                    .and_modify(|f| {
                                        if *f != service {
                                            *f = service.clone();
                                            update_gateway = true;
                                        }
                                    })
                                    .or_insert_with(|| {
                                        update_gateway = true;
                                        service
                                    });
                            };

                            if update_gateway {
                                myself.update_gateways(&key).await;
                            }
                        } else {
                            let resolved_secret = {
                                let mut resolved_secrets = myself.resolved_backend_references.lock().await;
                                resolved_secrets.remove(&key).is_some()
                            };

                            if resolved_secret {
                                myself.update_gateways(&key).await;
                            }
                        }
                    }
                    .instrument(span.clone()),
                );
            }
        }
    }

    async fn update_gateways(&self, key: &ResourceKey) {
        let secrets = self.backend_references.lock().await;
        let gateways = secrets.get(key).cloned().unwrap_or_default();
        debug!("Refernce changed... updating gateways {key} {gateways:#?}");
        let _res = self.reference_validate_channel_sender.send(ReferenceValidateRequest::UpdatedGateways(gateways)).await;
    }
}
