use kube::Client;
use tracing::{debug, info, span, warn, Instrument, Level, Span};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, gateway_api::gateways::Gateway, BackendReferenceResolver, GatewayDeployRequest, ReferenceGrantsResolver, ReferenceValidateRequest, RequestContext, SecretsResolver},
    controllers::{ListenerTlsConfigValidator, RoutesResolver},
    state::State,
};

#[derive(TypedBuilder)]
pub struct ReferenceValidatorService {
    client: Client,
    state: State,
    secrets_resolver: SecretsResolver,
    backend_references_resolver: BackendReferenceResolver,
    reference_grants_resolver: ReferenceGrantsResolver,
    reference_validate_channel_receiver: tokio::sync::mpsc::Receiver<ReferenceValidateRequest>,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
}

struct ReferenceResolverHandler {
    client: Client,
    state: State,
    secrets_resolver: SecretsResolver,
    backend_references_resolver: BackendReferenceResolver,
    reference_grants_resolver: ReferenceGrantsResolver,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
}

impl ReferenceValidatorService {
    pub async fn start(self) -> crate::Result<()> {
        let mut resolve_receiver = self.reference_validate_channel_receiver;
        let handler = ReferenceResolverHandler {
            client: self.client.clone(),
            state: self.state,
            secrets_resolver: self.secrets_resolver,
            backend_references_resolver: self.backend_references_resolver,
            reference_grants_resolver: self.reference_grants_resolver,
            gateway_deployer_channel_sender: self.gateway_deployer_channel_sender,
        };

        loop {
            tokio::select! {
                Some(resolve_event) = resolve_receiver.recv() => {
                    handler.handle(resolve_event).await;
                },


                else => {
                    warn!("All listener manager channels are closed...exiting");
                    return crate::Result::<()>::Ok(())
                }
            }
        }
    }
}
impl ReferenceResolverHandler {
    async fn handle(&self, resolve_event: ReferenceValidateRequest) {
        match resolve_event {
            ReferenceValidateRequest::AddGateway(RequestContext {
                gateway,
                kube_gateway,
                gateway_class_name,
                span,
            }) => {
                let span = span!(parent: &span, Level::INFO, "ReferenceResolverService", action = "ReferenceValidateRequest", id = %gateway.key());
                let _entered = span.enter();
                info!("Processing gateway {}", gateway.key());
                self.secrets_resolver.add_secretes_by_gateway(&gateway).await;

                self.backend_references_resolver.add_references_by_gateway(&gateway).await;
                self.reference_grants_resolver.add_references_by_gateway(&gateway).await;

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

            ReferenceValidateRequest::DeleteGateway { gateway, span: parent_span } => {
                let span = span!(parent: &parent_span, Level::INFO, "ReferenceResolverService", action = "ReferenceValidateRequest", id = %gateway.key());
                let _entered = span.enter();

                self.secrets_resolver.delete_secrets_by_gateway(&gateway).await;
                self.backend_references_resolver.delete_references_by_gateway(&gateway).await;
            }

            ReferenceValidateRequest::AddRoute { references, span: parent_span } => {
                let span = span!(parent: &parent_span, Level::INFO, "ReferenceResolverService", action = "AddRouteReferences");
                let _entered = span.enter();
                debug!("Adding route references {references:?}");
                self.backend_references_resolver.add_references(references.iter()).await;
            }

            ReferenceValidateRequest::DeleteRoute { references, span: parent_span } => {
                let span = span!(parent: &parent_span, Level::INFO, "ReferenceResolverService", action = "DeleteRouteAndValidateRequest");
                let _entered = span.enter();

                let affected_gateways = self.backend_references_resolver.delete_all_references(references.iter()).await;

                info!("Update gateways due to deleted references {references:?} {affected_gateways:?}");
                for gateway_id in affected_gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let span = span!(Level::INFO, "ReferenceResolverService", action = "DeleteRouteAndValidateRequest", id = %gateway_id);

                        let gateway = common::Gateway::try_from(&*kube_gateway).expect("We expect this to work since KubeGateway was validated");
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

            ReferenceValidateRequest::UpdatedGateways { reference, gateways } => {
                let span = span!(Level::INFO, "ReferenceResolverService", action = "UpdatedGateways");
                let _entered = span.enter();
                info!("Update gateways due to reference {reference} changing state {gateways:#?}");
                for gateway_id in gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let span = span!(Level::INFO, "ReferenceResolverService", id = %gateway_id);

                        let gateway = common::Gateway::try_from(&*kube_gateway).expect("We expect this to work since KubeGateway was validated");
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
