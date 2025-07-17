use gateway_api::gateways::Gateway;
use gateway_api_inference_extension::inferencepools::InferencePool;
use kube::Client;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, BackendReferenceResolver, GatewayDeployRequest, ReferenceGrantsResolver, ReferenceValidateRequest, RequestContext, SecretsResolver},
    controllers::{ListenerTlsConfigValidator, RoutesResolver},
    services::patchers::Operation,
    state::State,
};

#[derive(TypedBuilder)]
pub struct ReferenceValidatorService {
    controller_name: String,
    client: Client,
    state: State,
    secrets_resolver: SecretsResolver,
    backend_references_resolver: BackendReferenceResolver,
    reference_grants_resolver: ReferenceGrantsResolver,
    reference_validate_channel_receiver: tokio::sync::mpsc::Receiver<ReferenceValidateRequest>,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
    inference_pool_patcher_sender: mpsc::Sender<Operation<InferencePool>>,
}

struct ReferenceResolverHandler {
    controller_name: String,
    client: Client,
    state: State,
    secrets_resolver: SecretsResolver,
    backend_references_resolver: BackendReferenceResolver,
    reference_grants_resolver: ReferenceGrantsResolver,
    gateway_deployer_channel_sender: mpsc::Sender<GatewayDeployRequest>,
    inference_pool_patcher_sender: mpsc::Sender<Operation<InferencePool>>,
}

impl ReferenceValidatorService {
    pub async fn start(self) -> crate::Result<()> {
        let mut resolve_receiver = self.reference_validate_channel_receiver;
        let handler = ReferenceResolverHandler {
            controller_name: self.controller_name.clone(),
            client: self.client.clone(),
            state: self.state,
            secrets_resolver: self.secrets_resolver,
            backend_references_resolver: self.backend_references_resolver,
            reference_grants_resolver: self.reference_grants_resolver,
            gateway_deployer_channel_sender: self.gateway_deployer_channel_sender,
            inference_pool_patcher_sender: self.inference_pool_patcher_sender,
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
            ReferenceValidateRequest::AddGateway(boxed) => {
                let RequestContext {
                    gateway,
                    kube_gateway,
                    gateway_class_name,
                } = *boxed;
                info!("ReferenceResolverService action = AddGateway {}", gateway.key());
                let key = gateway.key().clone();
                self.secrets_resolver.add_secretes_by_gateway(&gateway).await;

                self.backend_references_resolver.add_references_by_gateway(&gateway).await;
                self.reference_grants_resolver.add_references_by_gateway(&gateway).await;

                let backend_gateway = self.process(gateway, &kube_gateway).await;

                info!("Adding gateway {} Send on channel", key);
                let _ = self
                    .gateway_deployer_channel_sender
                    .send(GatewayDeployRequest::Deploy(
                        RequestContext::builder()
                            .gateway(backend_gateway)
                            .kube_gateway(kube_gateway)
                            .gateway_class_name(gateway_class_name)
                            .build(),
                    ))
                    .await;
            }

            ReferenceValidateRequest::DeleteGateway { gateway } => {
                info!("ReferenceResolverService action = DeleteGateway {}", gateway.key());
                self.secrets_resolver.delete_secrets_by_gateway(&gateway).await;
                self.backend_references_resolver.delete_references_by_gateway(&gateway).await;
                self.reference_grants_resolver.delete_references_by_gateway(&gateway).await;
            }

            ReferenceValidateRequest::AddRoute { references } => {
                info!("ReferenceResolverService action = AddRouteReferences {references:?}");
                debug!("Adding route references {references:?}");
                self.backend_references_resolver.add_references(references).await;
            }

            ReferenceValidateRequest::DeleteRoute { references } => {
                info!("ReferenceResolverService action = DeleteRouteAndValidateRequest {references:?}");
                let affected_gateways = self.backend_references_resolver.delete_all_references(references).await;

                info!("Update gateways affected gateways  {affected_gateways:?}");
                for gateway_id in affected_gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let gateway = common::Gateway::try_from(&*kube_gateway).expect("We expect this to work since KubeGateway was validated");
                        let gateway_class_name = kube_gateway.spec.gateway_class_name.clone();
                        let backend_gateway = self.process(gateway, &kube_gateway).await;

                        let kube_gateway = (*kube_gateway).clone();
                        let _ = self
                            .gateway_deployer_channel_sender
                            .send(GatewayDeployRequest::Deploy(
                                RequestContext::builder()
                                    .gateway(backend_gateway)
                                    .kube_gateway(kube_gateway)
                                    .gateway_class_name(gateway_class_name)
                                    .build(),
                            ))
                            .await;
                    }
                }
            }

            ReferenceValidateRequest::UpdatedGateways { reference, gateways } => {
                info!("ReferenceResolverService action = UpdatedGateways {reference} {gateways:?}");
                for gateway_id in gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let gateway = common::Gateway::try_from(&*kube_gateway).expect("We expect this to work since KubeGateway was validated");
                        let key = gateway.key().clone();
                        let gateway_class_name = kube_gateway.spec.gateway_class_name.clone();
                        let backend_gateway = self.process(gateway, &kube_gateway).await;
                        let kube_gateway = (*kube_gateway).clone();
                        info!("Update gateway {} Send on channel", key);
                        let _ = self
                            .gateway_deployer_channel_sender
                            .send(GatewayDeployRequest::Deploy(
                                RequestContext::builder()
                                    .gateway(backend_gateway)
                                    .kube_gateway(kube_gateway)
                                    .gateway_class_name(gateway_class_name)
                                    .build(),
                            ))
                            .await;
                    }
                }
            }
        }
    }
    async fn process(&self, gateway: common::Gateway, kube_gateway: &Gateway) -> common::Gateway {
        let backend_gateway = ListenerTlsConfigValidator::new(gateway, &self.secrets_resolver, &self.reference_grants_resolver).validate().await;
        let resolver = RoutesResolver::builder()
            .gateway(backend_gateway)
            .kube_gateway(kube_gateway)
            .client(self.client.clone())
            .backend_reference_resolver(&self.backend_references_resolver)
            .reference_grants_resolver(&self.reference_grants_resolver)
            .state(&self.state)
            .inference_pool_patcher_sender(self.inference_pool_patcher_sender.clone())
            .controller_name(self.controller_name.clone())
            .build();

        resolver.validate().await
    }
}
