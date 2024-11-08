use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};

use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

use tracing::debug;

use crate::common::{self, ListenerCondition, ProtocolType, ResolvedRefs};
pub struct ListenerTlsConfigValidator<'a> {
    gateway: common::Gateway,
    client: Client,
    log_context: &'a str,
}

impl<'a> ListenerTlsConfigValidator<'a> {
    pub fn new(gateway: common::Gateway, client: Client, log_context: &'a str) -> Self {
        Self { gateway, client, log_context }
    }

    pub async fn validate(mut self) -> common::Gateway {
        let client = self.client.clone();
        let log_context = self.log_context;
        let gateway_name = self.gateway.name().to_owned();
        debug!("{log_context} Validating TLS certs {gateway_name}");

        for listener in self.gateway.listeners_mut().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
            let listener_data = listener.data_mut();

            let name = listener_data.config.name.clone();
            let conditions = &mut listener_data.conditions;
            let supported_routes = conditions
                .get(&ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![])))
                .map(ListenerCondition::supported_routes)
                .unwrap_or_default();
            debug!("{log_context} Supported routes {} {name} {supported_routes:?}", gateway_name);
            for certificate_key in &listener_data.config.certificates {
                let secret_api: Api<Secret> = Api::namespaced(client.clone(), &certificate_key.namespace);
                if let Ok(secret) = secret_api.get(&certificate_key.name).await {
                    if secret.type_ == Some("kubernetes.io/tls".to_owned()) {
                        let supported_routes = supported_routes.clone();
                        if let Some(data) = secret.data {
                            let private_key = data.get("tls.key");
                            let certificate = data.get("tls.crt");
                            match (private_key, certificate) {
                                (Some(private_key), Some(certificate)) => {
                                    let valid_cert = CertificateDer::from_pem_slice(&certificate.0);
                                    let valid_key = PrivateKeyDer::from_pem_slice(&private_key.0);
                                    match (valid_cert, valid_key) {
                                        (Ok(_), Ok(_)) => debug!("{log_context} Private key and certificate are valid"),
                                        (Ok(_), Err(e)) => {
                                            debug!("{log_context} Key is invalid {e}");
                                            _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                        }
                                        (Err(e), Ok(_)) => {
                                            debug!("{log_context} Certificate is invalid {e}");
                                            _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                        }
                                        (Err(e_cert), Err(e_key)) => {
                                            debug!("{log_context} Key and cer certificate are invalid {e_cert}{e_key}");
                                            _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                        }
                                    };
                                }
                                _ => {
                                    _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                }
                            }
                        } else {
                            _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                        }
                    } else {
                        _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes.clone())));
                    }
                } else {
                    _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes.clone())));
                }
            }
        }
        self.gateway
    }
}
