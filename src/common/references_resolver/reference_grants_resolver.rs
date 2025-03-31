use crate::common::gateway_api::referencegrants::ReferenceGrant;
use std::{collections::BTreeSet, fmt};

use kube::Client;
use typed_builder::TypedBuilder;

use super::ReferencesResolver;
use crate::{
    common::{Backend, Gateway, ProtocolType, ReferenceValidateRequest, ResourceKey, TlsType},
    controllers::find_linked_routes,
    state::State,
};

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
    #[builder(default)]
    references: BTreeSet<ReferenceGrantRef>,
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

    pub async fn delete_references_by_gateway(&self, gateway: &Gateway) {
        let mut references = self.search_references(gateway);
        self.references.difference(&mut references);
    }

    // pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<ReferenceGrant> {
    //     self.reference_resolver.get_reference(resource_key).await
    // }

    // pub async fn resolve(&self) -> crate::Result<()> {
    //     self.reference_resolver.resolve().await;
    //     Ok(())
    // }
}
