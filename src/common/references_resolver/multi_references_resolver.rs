use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use kube::{Api, Client, Resource, ResourceExt};
use kube_core::ObjectMeta;
use tokio::time;
use tracing::debug;
use typed_builder::TypedBuilder;

use crate::common::{ReferenceValidateRequest, ResourceKey};

pub type RouteKey = ResourceKey;
pub type ReferenceKey = ResourceKey;
pub type GatewayKey = ResourceKey;
pub type RouteToReferenceMapping = BTreeMap<RouteKey, BTreeSet<ReferenceKey>>;
pub type GatewayToRouteReferenceMapping = BTreeMap<GatewayKey, RouteToReferenceMapping>;

#[derive(Clone, Debug)]
struct References<R> {
    parents: BTreeMap<ResourceKey, usize>,
    resolved_references: BTreeMap<ResourceKey, R>,
}
impl<R> Default for References<R> {
    fn default() -> Self {
        Self { parents: BTreeMap::new(), resolved_references: BTreeMap::new() }
    }
}
impl<R> References<R> {
    fn remove(&mut self, k: &ResourceKey) -> Option<R> {
        self.parents.remove(k);
        self.resolved_references.remove(k)
    }

    fn remove_resolved_reference(&mut self, k: &ResourceKey) -> Option<R> {
        self.resolved_references.remove(k)
    }

    fn add_resolved_reference(&mut self, key: &ResourceKey, reference: R) -> bool
    where
        R: Clone + std::cmp::PartialEq,
        R: ResourceExt,
    {
        let mut update_gateway = false;
        self.resolved_references
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
        update_gateway
    }
    fn get(&self, k: &ResourceKey) -> Option<&R> {
        self.resolved_references.get(k)
    }
}

#[derive(Clone, TypedBuilder)]
pub struct MultiReferencesResolver<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static + std::cmp::PartialEq,
    R: Resource<Scope = kube_core::NamespaceResourceScope>,
{
    client: Client,
    #[builder(default)]
    gateway_route_reference_mapping: Arc<tokio::sync::Mutex<GatewayToRouteReferenceMapping>>,

    #[builder(default)]
    references: Arc<tokio::sync::Mutex<References<R>>>,
    reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>,
}

impl<R> MultiReferencesResolver<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static + std::cmp::PartialEq,
    R: Resource<Scope = kube_core::NamespaceResourceScope>,
{
    pub async fn add_references_for_gateway<F>(&self, gateway_key: &ResourceKey, route_to_reference_mapping: F)
    where
        F: Fn() -> RouteToReferenceMapping,
    {
        let route_to_reference_mapping = route_to_reference_mapping();
        if route_to_reference_mapping.is_empty() {
            debug!("Can't add no references Gateway {gateway_key}");
            return;
        }

        debug!("Adding new references for Gateway {gateway_key} Reference {route_to_reference_mapping:?}");
        let mut lock = self.gateway_route_reference_mapping.lock().await;

        let mut mappings_to_delete = BTreeMap::new();
        lock.entry(gateway_key.clone())
            .and_modify(|f| {
                mappings_to_delete = f.clone();
                *f = route_to_reference_mapping.clone();
            })
            .or_insert(route_to_reference_mapping.clone());

        let references = &mut self.references.lock().await;

        let resolved_references_parents = &mut references.parents;

        for (_, resources) in mappings_to_delete {
            for resource_key in resources {
                resolved_references_parents.entry(resource_key).and_modify(|f| *f -= 1);
            }
        }
        for (_, resources) in route_to_reference_mapping {
            for resource_key in resources {
                resolved_references_parents.entry(resource_key).and_modify(|f| *f += 1).or_insert(1);
            }
        }
        let keys: Vec<_> = resolved_references_parents.iter().filter_map(|(k, v)| (*v < 1).then_some(k)).cloned().collect();
        for key in keys {
            references.remove(&key);
        }
    }

    pub async fn delete_references_for_gateway<F>(&self, gateway_key: &ResourceKey, route_to_reference_mapping: F)
    where
        F: Fn() -> RouteToReferenceMapping,
    {
        let route_to_reference_mapping = route_to_reference_mapping();
        debug!("Deleting references for Gateway {gateway_key} Reference {route_to_reference_mapping:?}");
        let mut lock = self.gateway_route_reference_mapping.lock().await;
        if let Some(old_mappings) = lock.remove(gateway_key) {
            let references = &mut self.references.lock().await;
            let resolved_references_parents = &mut references.parents;

            for (_, resources) in old_mappings {
                for resource_key in resources {
                    resolved_references_parents.entry(resource_key).and_modify(|f| *f -= 1);
                }
            }

            let keys: Vec<_> = resolved_references_parents.iter().filter_map(|(k, v)| (*v < 1).then_some(k)).cloned().collect();

            for key in keys {
                references.remove(&key);
            }
        }
    }

    pub async fn delete_route_references<I>(&self, route_key: &RouteKey, reference_keys: I) -> BTreeSet<ResourceKey>
    where
        I: Iterator<Item = ResourceKey> + fmt::Debug,
    {
        debug!("Deleting route references {route_key} {reference_keys:?}");
        let mut gateway_route_reference_mapping = self.gateway_route_reference_mapping.lock().await;

        let (affected_gateways, resources_to_delete): (BTreeSet<_>, BTreeSet<_>) = gateway_route_reference_mapping
            .iter_mut()
            .filter_map(|(gateway, route_mapping)| {
                route_mapping.remove(route_key).map(|resources_to_delete| (gateway.clone(), resources_to_delete))
            })
            .unzip();

        let resources_to_delete: BTreeSet<_> = resources_to_delete.iter().flatten().cloned().collect();

        let references = &mut self.references.lock().await;
        let resolved_references_parents = &mut references.parents;

        for resource_key in resources_to_delete {
            resolved_references_parents.entry(resource_key).and_modify(|f| *f -= 1);
        }

        let keys: Vec<_> = resolved_references_parents.iter().filter_map(|(k, v)| (*v < 1).then_some(k)).cloned().collect();

        for key in keys {
            references.remove(&key);
        }

        affected_gateways
    }

    pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<R> {
        self.references.lock().await.get(resource_key).cloned()
    }

    pub async fn resolve(&self) {
        let mut interval = time::interval(time::Duration::from_secs(1));

        loop {
            interval.tick().await;
            let references = {
                let references = self.references.lock().await;
                references.parents.keys().cloned().collect::<BTreeSet<_>>()
            };

            for key in references {
                let key = key.clone();
                let myself = (*self).clone();
                tokio::spawn(async move {
                    debug!("Checking reference {key}");
                    let api: Api<R> = Api::namespaced(myself.client.clone(), &key.namespace);
                    let maybe_reference = api.get(&key.name).await;
                    if let Ok(reference) = maybe_reference {
                        let update_gateway = {
                            let mut resolved_references = myself.references.lock().await;
                            resolved_references.add_resolved_reference(&key, reference)
                        };

                        debug!("Resolved reference {key} gateway needs an update {update_gateway}");

                        if update_gateway {
                            myself.update_gateways(&key).await;
                        }
                    } else {
                        debug!("Problem with checking reference {key} {:?}", maybe_reference.err());
                        let resolved_references = {
                            let mut resolved_references = myself.references.lock().await;
                            resolved_references.remove_resolved_reference(&key).is_some()
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
        let gateway_route_reference_mapping = self.gateway_route_reference_mapping.lock().await;

        let (gateways, routes): (BTreeSet<_>, BTreeSet<_>) = gateway_route_reference_mapping
            .iter()
            .filter(|&(_gateway, route_reference)| route_reference.iter().any(|(_, references)| references.contains(key)))
            .map(|(gateway, route_reference)| (gateway.clone(), route_reference.keys().collect::<BTreeSet<_>>()))
            .unzip();
        let changed_routes = routes.into_iter().flatten().collect::<BTreeSet<_>>();

        debug!("Reference changed... updating gateways {key} {gateways:?} changed routes {changed_routes:?}");
        let _res = self
            .reference_validate_channel_sender
            .send(ReferenceValidateRequest::UpdatedGateways { reference: key.clone(), gateways })
            .await;

        let _res = self
            .reference_validate_channel_sender
            .send(ReferenceValidateRequest::UpdatedRoutes {
                reference: key.clone(),
                updated_routes: changed_routes.into_iter().cloned().collect(),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {

    use std::collections::{BTreeMap, BTreeSet};

    use http::{Request, Response};
    use k8s_openapi::api::core::v1::Service;
    use kube::{Client, client::Body};
    use tokio::sync::mpsc;
    use tower_test::mock;

    use crate::common::{
        ResourceKey,
        references_resolver::multi_references_resolver::{MultiReferencesResolver, ReferenceKey, RouteKey},
    };

    #[tokio::test]
    async fn test_resolver_multiple_references() {
        let (mock_service, _handle) = mock::pair::<Request<Body>, Response<Body>>();
        let client = Client::new(mock_service, "dummy");

        let (sender, _receiver) = mpsc::channel(100);
        let multi_refernces_resolver =
            MultiReferencesResolver::<Service>::builder().client(client).reference_validate_channel_sender(sender).build();

        let g1 = ResourceKey::new("g1");
        let g2 = ResourceKey::new("g2");

        let r1 = ResourceKey::new("r1");
        let r2 = ResourceKey::new("r2");
        let r3 = ResourceKey::new("r2");

        // two gateways, two different routes, sharing one service... when the first route is deleted we want keep on resolving it

        let route1_resources = [r1.clone(), r2.clone(), r3.clone()].iter().cloned().collect::<BTreeSet<_>>();
        let route1 = ResourceKey::new("route1");

        let route2_resources = [r1.clone(), r2.clone(), r3.clone()].iter().cloned().collect::<BTreeSet<_>>();
        let route2 = ResourceKey::new("route2");
        let mut g1_mapping: BTreeMap<RouteKey, BTreeSet<ReferenceKey>> = BTreeMap::new();
        g1_mapping.insert(route1, route1_resources);

        let mut g2_mapping: BTreeMap<RouteKey, BTreeSet<ReferenceKey>> = BTreeMap::new();
        g2_mapping.insert(route2, route2_resources);

        {
            let g1_mapping_fn = || g1_mapping.clone();
            let g2_mapping_fn = || g2_mapping.clone();
            multi_refernces_resolver.add_references_for_gateway(&g1, g1_mapping_fn).await;
            multi_refernces_resolver.add_references_for_gateway(&g2, g2_mapping_fn).await;

            let refernces = multi_refernces_resolver.references.lock().await;
            let parents = &refernces.parents;
            assert_eq!(*parents.get(&r1).unwrap(), 2_usize);
            assert_eq!(*parents.get(&r2).unwrap(), 2_usize);
            assert_eq!(*parents.get(&r3).unwrap(), 2_usize);

            let referenes = &refernces.resolved_references;
            assert_eq!(referenes.get(&r1), None);
            assert_eq!(referenes.get(&r2), None);
            assert_eq!(referenes.get(&r3), None);

            let _ = parents;
        }

        {
            let mut resolved_references = multi_refernces_resolver.references.lock().await;
            resolved_references.resolved_references.insert(r1.clone(), Service::default());
            resolved_references.resolved_references.insert(r2.clone(), Service::default());
            resolved_references.resolved_references.insert(r3.clone(), Service::default());
        }

        {
            let g1_mapping_fn = || g1_mapping.clone();
            multi_refernces_resolver.delete_references_for_gateway(&g1, g1_mapping_fn).await;
            let refernces = multi_refernces_resolver.references.lock().await;
            let parents = &refernces.parents;
            assert_eq!(*parents.get(&r1).unwrap(), 1_usize);
            assert_eq!(*parents.get(&r2).unwrap(), 1_usize);
            assert_eq!(*parents.get(&r3).unwrap(), 1_usize);

            let referenes = &refernces.resolved_references;
            assert!(referenes.get(&r1).is_some());
            assert!(referenes.get(&r2).is_some());
            assert!(referenes.get(&r3).is_some());
        }

        {
            let g2_mapping_fn = || g2_mapping.clone();
            multi_refernces_resolver.delete_references_for_gateway(&g2, g2_mapping_fn).await;
            let refernces = multi_refernces_resolver.references.lock().await;
            let parents = &refernces.parents;
            assert_eq!(parents.get(&r1), None);
            assert_eq!(parents.get(&r2), None);
            assert_eq!(parents.get(&r3), None);
            let referenes = &refernces.resolved_references;
            assert!(referenes.get(&r1).is_none());
            assert!(referenes.get(&r2).is_none());
            assert!(referenes.get(&r3).is_none());
            let _ = parents;
        }
    }
}
