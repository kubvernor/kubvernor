use std::collections::BTreeSet;

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
    pub async fn add_references(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();

        let adder = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);

            let mut backend_reference_keys = BTreeSet::new();
            for route in linked_routes {
                let route_config = route.config();
                for rule in &route_config.routing_rules {
                    for backend in &rule.backends {
                        if let Backend::Maybe(backend_service_config) = backend {
                            backend_reference_keys.insert(backend_service_config.resource_key.clone());
                        };
                    }
                }
            }
            backend_reference_keys
        };
        self.reference_resolver.add_references(gateway_key, adder).await;
    }

    pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<Service> {
        self.reference_resolver.get_reference(resource_key).await
    }

    pub async fn resolve(&self) {
        self.reference_resolver.resolve().await;
    }
}
