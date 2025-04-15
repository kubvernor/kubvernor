use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use tracing::{debug, info};

use crate::common::{self, ListenerCondition, ProtocolType, ReferenceGrantRef, ReferenceGrantsResolver, ResolvedRefs, SecretsResolver, TlsType};
pub struct ListenerTlsConfigValidator<'a> {
    gateway: common::Gateway,
    secrets_resolver: &'a SecretsResolver,
    reference_grants_resolver: &'a ReferenceGrantsResolver,
}

struct SameSpace<'a>(&'a str);

impl SameSpace<'_> {
    fn is_samespace(&self, namespace: &str) -> bool {
        self.0 == namespace
    }
}

impl<'a> ListenerTlsConfigValidator<'a> {
    pub fn new(gateway: common::Gateway, secrets_resolver: &'a SecretsResolver, reference_grants_resolver: &'a ReferenceGrantsResolver) -> Self {
        Self {
            gateway,
            secrets_resolver,
            reference_grants_resolver,
        }
    }

    pub async fn validate(mut self) -> common::Gateway {
        let gateway_name = self.gateway.name().to_owned();
        let gateway_key = self.gateway.key().clone();

        debug!("Validating TLS certs {gateway_name}");

        for listener in self.gateway.listeners_mut().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
            let listener_data = listener.data_mut();

            let name = listener_data.config.name.clone();
            let conditions = &mut listener_data.conditions;
            let supported_routes = conditions
                .get(&ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![])))
                .map(ListenerCondition::supported_routes)
                .unwrap_or_default();
            debug!("Supported routes {} {name} {supported_routes:?}", gateway_name);
            if let Some(TlsType::Terminate(certificates)) = &mut listener_data.config.tls_type {
                for certificate in certificates {
                    let certificate_key = certificate.resouce_key();

                    let reference_grant_allowed = self.reference_grants_resolver.is_allowed(&gateway_key, certificate_key, &gateway_key).await;

                    let grant_ref = ReferenceGrantRef::builder()
                        .namespace(certificate_key.namespace.clone())
                        .from((&gateway_key).into())
                        .to(certificate_key.into())
                        .gateway_key(gateway_key.clone())
                        .build();

                    let is_samespace = SameSpace(&gateway_key.namespace).is_samespace(&certificate.resouce_key().namespace);
                    info!("Secret ReferenceGrant Allowing because of reference grant {grant_ref:?} {reference_grant_allowed} samespace {is_samespace}");

                    if let Some(secret) = self.secrets_resolver.get_reference(certificate_key).await {
                        if reference_grant_allowed || is_samespace {
                            if secret.type_ == Some("kubernetes.io/tls".to_owned()) {
                                let supported_routes = supported_routes.clone();
                                if let Some(data) = secret.data {
                                    let secret_private_key = data.get("tls.key");
                                    let secret_certificate = data.get("tls.crt");
                                    if let (Some(secret_private_key), Some(secret_certificate)) = (secret_private_key, secret_certificate) {
                                        let valid_cert = CertificateDer::from_pem_slice(&secret_certificate.0);
                                        let valid_key = PrivateKeyDer::from_pem_slice(&secret_private_key.0);
                                        match (valid_cert, valid_key) {
                                            (Ok(_), Ok(_)) => {
                                                if gateway_key.namespace == certificate.resouce_key().namespace {
                                                    *certificate = certificate.resolve();
                                                    debug!("Private key and certificate are valid");
                                                } else {
                                                    *certificate = certificate.resolve_cross_space();
                                                    info!("Cross space certificate: Private key and certificate are valid");
                                                }
                                            }
                                            (Ok(_), Err(e)) => {
                                                *certificate = certificate.invalid();
                                                debug!("Key is invalid {e}");
                                                _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                            }
                                            (Err(e), Ok(_)) => {
                                                *certificate = certificate.invalid();
                                                debug!("Certificate is invalid {e}");
                                                _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                            }
                                            (Err(e_cert), Err(e_key)) => {
                                                *certificate = certificate.invalid();
                                                debug!("Key and cer certificate are invalid {e_cert}{e_key}");
                                                _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                            }
                                        };
                                    } else {
                                        *certificate = certificate.invalid();
                                        _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                    }
                                } else {
                                    *certificate = certificate.not_resolved();
                                    _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes)));
                                }
                            } else {
                                *certificate = certificate.not_resolved();
                                _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes.clone())));
                            }
                        } else if is_samespace {
                            *certificate = certificate.not_resolved();
                            _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes.clone())));
                        } else {
                            *certificate = certificate.not_resolved();
                            _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::RefNotPermitted(supported_routes.clone())));
                        }
                    } else {
                        *certificate = certificate.not_resolved();
                        _ = conditions.replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(supported_routes.clone())));
                    }
                }
            }
        }

        self.gateway
    }
}
