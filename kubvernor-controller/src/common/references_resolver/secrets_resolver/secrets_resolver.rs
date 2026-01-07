use std::collections::BTreeSet;

use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use typed_builder::TypedBuilder;

use super::reference_resolver::ReferencesResolver;
use crate::common::{Gateway, ProtocolType, ReferenceValidateRequest, ResourceKey, TlsType};

#[derive(Clone, TypedBuilder)]
pub struct SecretsResolver {
    #[builder(setter(transform =
        |client:Client, reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>|
            ReferencesResolver::builder().client(client).reference_validate_channel_sender(reference_validate_channel_sender).build()))]
    reference_resolver: ReferencesResolver<Secret>,
}

impl SecretsResolver {
    pub async fn add_secretes_by_gateway(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();

        let references = || {
            let mut keys = BTreeSet::new();
            for listener in gateway.listeners().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
                let listener_data = listener.data();
                if let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type {
                    for certificate in certificates {
                        let certificate_key = certificate.resouce_key();
                        keys.insert(certificate_key.clone());
                    }
                }
            }
            keys
        };
        self.reference_resolver.add_references_for_gateway(gateway_key, references).await;
    }

    pub async fn delete_secrets_by_gateway(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();
        let references = || {
            let mut keys = BTreeSet::new();
            for listener in gateway.listeners().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
                let listener_data = listener.data();
                if let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type {
                    for certificate in certificates {
                        let certificate_key = certificate.resouce_key();
                        keys.insert(certificate_key.clone());
                    }
                }
            }
            keys
        };
        self.reference_resolver.delete_references_for_gateway(gateway_key, references).await;
    }

    pub async fn get_reference(&self, resource_key: &ResourceKey) -> Option<Secret> {
        self.reference_resolver.get_reference(resource_key).await
    }

    pub async fn resolve(&self) -> crate::Result<()> {
        self.reference_resolver.resolve().await;
        Ok(())
    }
}
