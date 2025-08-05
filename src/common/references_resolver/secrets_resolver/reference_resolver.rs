use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kube::{Api, Client, Resource, ResourceExt};
use kube_core::ObjectMeta;
pub use multiset::HashMultiSet;
use tokio::time;
use tracing::debug;
use typed_builder::TypedBuilder;

use crate::common::{ReferenceValidateRequest, ResourceKey};

#[derive(Clone, TypedBuilder)]
pub struct ReferencesResolver<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static + std::cmp::PartialEq,
    R: Resource<Scope = kube_core::NamespaceResourceScope>,
{
    client: Client,
    #[builder(default)]
    references: Arc<tokio::sync::Mutex<BTreeMap<ResourceKey, HashMultiSet<ResourceKey>>>>,
    #[builder(default)]
    resolved_references: Arc<tokio::sync::Mutex<BTreeMap<ResourceKey, R>>>,
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
    pub async fn add_references_for_gateway<F>(&self, gateway_key: &ResourceKey, references: F)
    where
        F: Fn() -> BTreeSet<ResourceKey>,
    {
        let reference_keys = references();
        if reference_keys.is_empty() {
            debug!("Can't add no references Gateway {gateway_key}");
            return;
        }

        debug!("Adding new references for Gateway {gateway_key} Reference {reference_keys:?}");
        let mut lock = self.references.lock().await;

        for key in reference_keys {
            lock.entry(key.clone())
                .and_modify(|set| {
                    set.insert(gateway_key.clone());
                })
                .or_insert_with(|| {
                    let mut set = HashMultiSet::new();
                    set.insert(gateway_key.clone());
                    set
                });
        }
    }

    pub async fn delete_references_for_gateway<F>(&self, gateway_key: &ResourceKey, references: F)
    where
        F: Fn() -> BTreeSet<ResourceKey>,
    {
        let reference_keys = references();

        debug!("Deleting references for Gateway {gateway_key} Reference {reference_keys:?}");

        let mut lock = self.references.lock().await;
        for reference_key in &reference_keys {
            if let Some(references) = lock.get_mut(reference_key) {
                if references.remove(gateway_key) && references.is_empty() {
                    lock.remove(reference_key);
                    let mut reference_lock = self.resolved_references.lock().await;
                    reference_lock.remove(reference_key);
                }
                debug!("Removed reference {reference_key} for Gateway {gateway_key}");
            }
        }
    }

    pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<R> {
        let resolved_backend_references = { self.resolved_references.lock().await.get(resource_key).cloned() };
        resolved_backend_references
    }

    pub async fn resolve(&self) {
        let mut interval = time::interval(time::Duration::from_secs(1));

        loop {
            interval.tick().await;
            let references = {
                let references = self.references.lock().await;
                references.clone()
            };

            for key in references.keys() {
                let key = key.clone();
                let myself = (*self).clone();
                tokio::spawn(async move {
                    debug!("Checking reference {key}");
                    let api: Api<R> = Api::namespaced(myself.client.clone(), &key.namespace);
                    let maybe_reference = api.get(&key.name).await;
                    if let Ok(reference) = maybe_reference {
                        let mut update_gateway = false;
                        {
                            let mut resolved_references = myself.resolved_references.lock().await;
                            resolved_references
                                .entry(key.clone())
                                .and_modify(|f: &mut R| {
                                    let mut this = f.clone();
                                    *this.meta_mut() = ObjectMeta::default();

                                    let mut other = reference.clone();
                                    *other.meta_mut() = ObjectMeta::default();

                                    if this != other {
                                        *f = reference.clone();
                                        update_gateway = true;
                                    }
                                })
                                .or_insert_with(|| {
                                    update_gateway = true;
                                    reference
                                });
                        };

                        debug!("Resolved reference {key} {update_gateway}");

                        if update_gateway {
                            myself.update_gateways(&key).await;
                        }
                    } else {
                        debug!("Problem with checking reference {key} {:?}", maybe_reference.err());
                        let resolved_references = {
                            let mut resolved_references = myself.resolved_references.lock().await;
                            resolved_references.remove(&key).is_some()
                        };

                        if resolved_references {
                            myself.update_gateways(&key).await;
                        }
                    }
                });
            }
        }
    }

    async fn update_gateways(&self, key: &ResourceKey) {
        let references = self.references.lock().await;
        let gateways = references.get(key).cloned().map(|set| set.distinct_elements().cloned().collect::<BTreeSet<_>>()).unwrap_or_default();
        debug!("Reference changed... updating gateways {key} {gateways:?}");
        let _res = self
            .reference_validate_channel_sender
            .send(ReferenceValidateRequest::UpdatedGateways { reference: key.clone(), gateways })
            .await;
    }
}
