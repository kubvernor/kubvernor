use std::sync::Arc;

use gateway_api::apis::standard::gateways::Gateway;
use kube::Client;
use tracing::{info, span, warn, Instrument, Level, Span};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, GatewayDeployRequest, ReferenceValidateRequest, RequestContext},
    controllers::{BackendReferenceResolver, ListenerTlsConfigValidator, RoutesResolver, SecretsResolver},
    state::State,
};

#[derive(TypedBuilder)]
pub struct ReferenceValidatorService {
    client: Client,
    state: State,
    secrets_resolver: SecretsResolver,
    backend_references_resolver: BackendReferenceResolver,
    reference_validate_channel_receiver: tokio::sync::mpsc::Receiver<ReferenceValidateRequest>,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
}

struct ReferenceResolverHandler {
    client: Client,
    state: State,
    secrets_resolver: SecretsResolver,
    backend_references_resolver: BackendReferenceResolver,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
}

impl ReferenceValidatorService {
    pub async fn start(self) {
        let mut resolve_receiver = self.reference_validate_channel_receiver;
        let handler = ReferenceResolverHandler {
            client: self.client.clone(),
            state: self.state,
            secrets_resolver: self.secrets_resolver,
            backend_references_resolver: self.backend_references_resolver,
            gateway_deployer_channel_sender: self.gateway_deployer_channel_sender,
        };

        loop {
            tokio::select! {
                Some(resolve_event) = resolve_receiver.recv() => {
                    handler.handle(resolve_event).await;
                },


                else => {
                    warn!("All listener manager channels are closed...exiting");
                }
            }
        }
    }
}
impl ReferenceResolverHandler {
    async fn handle(&self, resolve_event: ReferenceValidateRequest) {
        match resolve_event {
            ReferenceValidateRequest::New(RequestContext {
                gateway,
                kube_gateway,
                gateway_class_name,
                span,
            }) => {
                let span = span!(parent: &span, Level::INFO, "ReferenceResolverService", action = "ReferenceValidateRequest", id = %gateway.key());
                let _entered = span.enter();

                self.secrets_resolver.add_secretes(&gateway).await;

                self.backend_references_resolver.add_references(&gateway).await;

                let backend_gateway = self.process(span.clone(), gateway, &kube_gateway).await;

                let _ = self
                    .gateway_deployer_channel_sender
                    .send(GatewayDeployRequest::Deploy(
                        RequestContext::builder()
                            .gateway(backend_gateway)
                            .kube_gateway(kube_gateway)
                            .gateway_class_name(gateway_class_name)
                            .span(span.clone())
                            .build(),
                    ))
                    .await;
            }
            ReferenceValidateRequest::UpdatedGateways(gateways) => {
                let span = span!(Level::INFO, "ReferenceResolverService", action = "UpdatedGateways");
                let _entered = span.enter();
                info!("Update gateways {gateways:#?}");
                for gateway_id in gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let span = span!(Level::INFO, "ReferenceResolverService", id = %gateway_id);

                        let gateway = common::Gateway::try_from(&*kube_gateway).unwrap();
                        let gateway_class_name = kube_gateway.spec.gateway_class_name.clone();
                        let backend_gateway = self.process(span.clone(), gateway, &kube_gateway).await;

                        let kube_gateway = (*kube_gateway).clone();
                        let _ = self
                            .gateway_deployer_channel_sender
                            .send(GatewayDeployRequest::Deploy(
                                RequestContext::builder()
                                    .gateway(backend_gateway)
                                    .kube_gateway(kube_gateway)
                                    .gateway_class_name(gateway_class_name)
                                    .span(span.clone())
                                    .build(),
                            ))
                            .await;
                    }
                }
            }
        }
    }
    async fn process(&self, span: Span, gateway: common::Gateway, kube_gateway: &Gateway) -> common::Gateway {
        let backend_gateway = ListenerTlsConfigValidator::new(gateway, &self.secrets_resolver).validate().instrument(span.clone()).await;
        let resolver = RoutesResolver::builder()
            .gateway(backend_gateway)
            .kube_gateway(kube_gateway)
            .client(self.client.clone())
            .backend_reference_resolver(&self.backend_references_resolver)
            .state(&self.state)
            .build();

        resolver.validate().instrument(span.clone()).await
    }
}
