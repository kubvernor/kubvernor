use std::collections::BTreeSet;

use gateway_api::gateways::Gateway;
use gateway_api_inference_extension::inferencepools::InferencePool;
use kube::Client;
use kubvernor_common::ResourceKey;
use kubvernor_state::State;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use typed_builder::TypedBuilder;

use crate::{
    common::{
        self, BackendReferenceResolver, GatewayDeployRequest, ReferenceGrantsResolver, ReferenceValidateRequest, RequestContext,
        SecretsResolver,
    },
    controllers::{ListenerTlsConfigValidator, RoutesResolver, inference_pool::clear_all_conditions},
    services::patchers::{Operation, PatchContext},
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
                let RequestContext { gateway, kube_gateway, gateway_class_name } = *boxed;
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
            },

            ReferenceValidateRequest::DeleteGateway { gateway } => {
                info!("ReferenceResolverService action = DeleteGateway {}", gateway.key());
                self.secrets_resolver.delete_secrets_by_gateway(&gateway).await;
                self.backend_references_resolver.delete_references_by_gateway(&gateway).await;
                self.reference_grants_resolver.delete_references_by_gateway(&gateway).await;
            },

            ReferenceValidateRequest::AddRoute { route_key, references } => {
                info!("ReferenceResolverService action = AddRouteReferences {route_key} {references:?}");
            },
            ReferenceValidateRequest::DeleteRoute { route_key, references } => {
                info!("ReferenceResolverService action = DeleteRouteAndValidateRequest {route_key:?} {references:?}");
                let affected_gateways = self.backend_references_resolver.delete_route_references(&route_key, &references).await;

                self.update_inference_pools(route_key, references, &affected_gateways).await;

                info!("Update gateways affected gateways  {affected_gateways:?}");
                for gateway_id in affected_gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let mut gateway =
                            common::Gateway::try_from(&*kube_gateway).expect("We expect this to work since KubeGateway was validated");
                        let Some(gateway_type) = self.state.get_gateway_type(gateway.key()).expect("We expect the lock to work") else {
                            warn!(
                                "ReferenceResolverService: {} {:?} Unknown gateway implementation type ",
                                &self.controller_name,
                                gateway.key(),
                            );
                            continue;
                        };

                        *gateway.backend_type_mut() = gateway_type;
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
            },
            ReferenceValidateRequest::UpdatedRoutes { reference, updated_routes } => {
                info!("ReferenceResolverService action = UpdatedRoutes {reference} {updated_routes:?}");
            },

            ReferenceValidateRequest::UpdatedGateways { reference, gateways } => {
                info!("ReferenceResolverService action = UpdatedGateways {reference} {gateways:?}");
                for gateway_id in gateways {
                    if let Some(kube_gateway) = self.state.get_gateway(&gateway_id).expect("We expect the lock to work") {
                        let mut gateway =
                            common::Gateway::try_from(&*kube_gateway).expect("We expect this to work since KubeGateway was validated");
                        let key = gateway.key().clone();

                        let Some(gateway_type) = self.state.get_gateway_type(&key).expect("We expect the lock to work") else {
                            warn!(
                                "ReferenceResolverService: {} {:?} Unknown gateway implementation type ",
                                &self.controller_name,
                                gateway.key(),
                            );
                            continue;
                        };

                        *gateway.backend_type_mut() = gateway_type;

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
            },
        }
    }
    async fn process(&self, gateway: common::Gateway, kube_gateway: &Gateway) -> common::Gateway {
        let backend_gateway =
            ListenerTlsConfigValidator::new(gateway, &self.secrets_resolver, &self.reference_grants_resolver).validate().await;
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

    async fn update_inference_pools(
        &self,
        route_key: ResourceKey,
        references: BTreeSet<ResourceKey>,
        affected_gateways: &BTreeSet<ResourceKey>,
    ) {
        info!("Updating inference pools for deleted route {route_key} {references:?} {affected_gateways:?}");
        for pool_reference in references.iter().filter(|r| r.kind == "InferencePool") {
            if let Some(pool) = self.state.get_inference_pool(pool_reference).expect("We expect this to work") {
                let mut pool = clear_all_conditions((*pool).clone(), affected_gateways);
                pool.metadata.managed_fields = None;
                let (sender, receiver) = oneshot::channel();
                let _ = self
                    .inference_pool_patcher_sender
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: pool_reference.clone(),
                        resource: pool,
                        controller_name: self.controller_name.clone(),
                        response_sender: sender,
                    }))
                    .await;
                match receiver.await {
                    Err(e) => error!("Sytem error  {e:?}"),
                    Ok(Err(e)) => {
                        warn!("Inference Pool: Can't update the status {e:?}");
                    },
                    _ => (),
                }
            } else {
                warn!("No pool for {:?}", pool_reference);
            }
        }
    }
}
