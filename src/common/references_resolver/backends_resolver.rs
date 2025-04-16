use std::{collections::BTreeSet, fmt};

use k8s_openapi::api::core::v1::Service;
use kube::Client;
use typed_builder::TypedBuilder;

use super::ReferencesResolver;
use crate::{
    common::{Backend, Gateway, ReferenceValidateRequest, ResourceKey},
    controllers::find_linked_routes,
    state::State,
};

#[derive(Clone, TypedBuilder)]
pub struct BackendReferenceResolver {
    #[builder(setter(transform = |client:Client, reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>| ReferencesResolver::builder().client(client).reference_validate_channel_sender(reference_validate_channel_sender).build()))]
    reference_resolver: ReferencesResolver<Service>,
    state: State,
}

impl BackendReferenceResolver {
    pub async fn add_references_by_gateway(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();

        let references = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);

            let mut backend_reference_keys = BTreeSet::new();
            for route in linked_routes {
                for backend in &route.backends() {
                    if let Backend::Maybe(backend_service_config) = backend {
                        backend_reference_keys.insert(backend_service_config.resource_key.clone());
                    };
                }
            }
            backend_reference_keys
        };
        self.reference_resolver.add_references_for_gateway(gateway_key, references).await;
    }

    pub async fn delete_all_references<'a, I>(&self, reference_keys: I) -> BTreeSet<ResourceKey>
    where
        I: Iterator<Item = &'a ResourceKey> + fmt::Debug,
    {
        self.reference_resolver.delete_references(reference_keys).await
    }

    pub async fn add_references<'a, I>(&self, reference_keys: I)
    where
        I: Iterator<Item = &'a ResourceKey> + fmt::Debug,
    {
        self.reference_resolver.add_references(reference_keys).await;
    }

    pub async fn delete_references_by_gateway(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();
        let references = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);

            let mut backend_reference_keys = BTreeSet::new();
            for route in linked_routes {
                for backend in &route.backends() {
                    if let Backend::Maybe(backend_service_config) = backend {
                        backend_reference_keys.insert(backend_service_config.resource_key.clone());
                    };
                }
            }
            backend_reference_keys
        };
        self.reference_resolver.delete_references_for_gateway(gateway_key, references).await;
    }

    pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<Service> {
        self.reference_resolver.get_reference(resource_key).await
    }

    pub async fn resolve(&self) -> crate::Result<()> {
        self.reference_resolver.resolve().await;
        Ok(())
    }
}
