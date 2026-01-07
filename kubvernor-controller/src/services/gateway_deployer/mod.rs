pub mod gateway_deployer_internal;
pub mod gateway_processed_handler;

use std::{collections::HashMap, sync::Arc};

use gateway_api::{gatewayclasses::GatewayClass, grpcroutes::GRPCRoute, httproutes::HTTPRoute};
use gateway_deployer_internal::{GatewayDeployer, GatewayDeployerServiceInternal};
pub(crate) use gateway_processed_handler::GatewayProcessedHandler;
use kubvernor_common::GatewayImplementationType;
use kubvernor_state::State;
use tokio::sync::oneshot;
use tracing::{info, warn};
use typed_builder::TypedBuilder;

use crate::{
    Result,
    common::{BackendGatewayEvent, BackendGatewayResponse, GatewayDeployRequest, KubeGateway, RequestContext},
    services::patchers::{Operation, PatchContext},
};

#[derive(TypedBuilder)]
pub struct GatewayDeployerService {
    state: State,
    backend_deployer_channel_senders: HashMap<GatewayImplementationType, tokio::sync::mpsc::Sender<BackendGatewayEvent>>,
    backend_response_channel_receiver: tokio::sync::mpsc::Receiver<BackendGatewayResponse>,
    gateway_deployer_channel_receiver: tokio::sync::mpsc::Receiver<GatewayDeployRequest>,
    gateway_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<KubeGateway>>,
    gateway_class_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<GatewayClass>>,
    http_route_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<HTTPRoute>>,
    grpc_route_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<GRPCRoute>>,
    controller_name: String,
}

impl GatewayDeployerService {
    pub async fn start(self) -> Result<()> {
        let mut resolve_receiver = self.gateway_deployer_channel_receiver;
        let mut backend_response_channel_receiver = self.backend_response_channel_receiver;
        let controller_name = self.controller_name.clone();
        loop {
            tokio::select! {
                Some(
                    GatewayDeployRequest::Deploy(RequestContext{ gateway, kube_gateway, gateway_class_name, })
                ) = resolve_receiver.recv() => {
                    info!("GatewayDeployerService Deploy {}" ,gateway.key());
                    let deployer = GatewayDeployer::builder()
                        .senders(self.backend_deployer_channel_senders.clone())
                        .gateway(gateway.clone())
                        .kube_gateway(kube_gateway)
                        .gateway_class_name(gateway_class_name)
                        .build();
                    let _ = deployer.deploy_gateway().await;
                },
                Some(event) = backend_response_channel_receiver.recv() =>{
                    match event{
                        BackendGatewayResponse::Processed(_effective_gateway)=>{}
                        BackendGatewayResponse::ProcessedWithContext{gateway, kube_gateway, gateway_class_name}=>{
                            info!("GatewayDeployerService Processed {}", gateway.key());
                            let gateway_id = gateway.key().clone();
                            let gateway_event_handler = GatewayProcessedHandler {
                                effective_gateway: *gateway,
                                gateway: *kube_gateway.clone(),
                                state: &self.state.clone(),
                                http_route_patcher: self.http_route_patcher_channel_sender.clone(),
                                grpc_route_patcher: self.grpc_route_patcher_channel_sender.clone(),
                                controller_name: self.controller_name.clone(),
                            };

                            if let Ok(updated_gateway) = gateway_event_handler.deploy_gateway().await{
                                self.state.save_gateway(gateway_id.clone(), &Arc::new(*kube_gateway)).expect("We expect the lock to work");
                                let (sender, receiver) = oneshot::channel();
                                let _res = self
                                    .gateway_patcher_channel_sender
                                    .send(Operation::PatchStatus(PatchContext {
                                        resource_key: gateway_id.clone(),
                                        resource: updated_gateway,
                                        controller_name: controller_name.clone(),
                                        response_sender: sender,
                                    }))
                                    .await;

                                let patched_gateway = receiver.await;
                                if let Ok(maybe_patched) = patched_gateway {
                                    match maybe_patched {
                                        Ok(patched_gateway) => {
                                            let patched_gateway = Arc::new(patched_gateway);
                                            self.state.save_gateway(gateway_id.clone(), &patched_gateway).expect("We expect the lock to work");
                                            let mut svc = GatewayDeployerServiceInternal::builder()
                                                .state(&self.state)
                                                .gateway_patcher_channel_sender(self.gateway_patcher_channel_sender.clone())
                                                .gateway_class_patcher_channel_sender(self.gateway_class_patcher_channel_sender.clone())
                                                .controller_name(controller_name.clone()).build();
                                            svc.on_version_not_changed(&gateway_id, &gateway_class_name, &patched_gateway).await;
                                        }
                                        Err(e) => warn!("GatewayDeployerService Error while patching {e}"),
                                    }
                                }
                            }
                        }
                        BackendGatewayResponse::ProcessingError=>{}
                        BackendGatewayResponse::Deleted(_effective_gateway)=>{}
                    }

                }

                else => {
                    warn!("All listener manager channels are closed...exiting");
                    return Result::<()>::Ok(())
                }
            }
        }
    }
}
