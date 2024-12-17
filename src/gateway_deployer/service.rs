use std::sync::Arc;

use gateway_api::apis::standard::{gatewayclasses::GatewayClass, gateways::Gateway as KubeGateway, httproutes::HTTPRoute};
use tokio::sync::{
    mpsc::{self},
    oneshot,
};
use tracing::{span, warn, Instrument, Level, Span};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, BackendGatewayEvent, GatewayDeployRequest, RequestContext, ResourceKey},
    controllers::GatewayDeployer,
    patchers::{Operation, PatchContext},
    state::State,
};

#[derive(TypedBuilder)]
pub struct GatewayDeployerService {
    state: State,
    backend_deployer_channel_sender: tokio::sync::mpsc::Sender<BackendGatewayEvent>,
    gateway_deployer_channel_receiver: tokio::sync::mpsc::Receiver<GatewayDeployRequest>,
    gateway_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<KubeGateway>>,
    gateway_class_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<GatewayClass>>,
    http_route_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<HTTPRoute>>,
    controller_name: String,
}

impl GatewayDeployerService {
    pub async fn start(self) {
        let mut resolve_receiver = self.gateway_deployer_channel_receiver;
        let controller_name = self.controller_name.clone();
        loop {
            tokio::select! {
                Some(GatewayDeployRequest::Deploy(RequestContext{ gateway, kube_gateway, gateway_class_name, span })) = resolve_receiver.recv() => {

                    let span = span!(parent: &span, Level::INFO, "GatewayDeployerService", id = %gateway.key());
                    let _entered = span.enter();
                    let gateway_id = gateway.key();


                    let mut deployer = GatewayDeployer::builder()
                        .sender(self.backend_deployer_channel_sender.clone())
                        .gateway(gateway.clone())
                        .kube_gateway(&kube_gateway)
                        .state(&self.state)
                        .http_route_patcher(self.http_route_patcher_channel_sender.clone())
                        .controller_name(&self.controller_name)
                        .build();


                    if let Ok(updated_gateway) = deployer.deploy_gateway().instrument(span.clone()).await{
                        self.state.save_gateway(gateway_id.clone(), &kube_gateway).expect("We expect the lock to work");
                        let (sender, receiver) = oneshot::channel();
                        let _res = self
                            .gateway_patcher_channel_sender
                            .send(Operation::PatchStatus(PatchContext {
                                resource_key: gateway_id.clone(),
                                resource: updated_gateway,
                                controller_name: controller_name.clone(),
                                response_sender: sender,
                                span: Span::current().clone(),
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
                                    svc.on_version_not_changed(gateway_id, &gateway_class_name, &patched_gateway).instrument(span.clone()).await;
                                }
                                Err(e) => warn!("GatewayDeployerService Error while patching {e}"),
                            }
                        }
                    }
                },
                else => {
                    warn!("All listener manager channels are closed...exiting");
                }
            }
        }
    }
}

#[derive(TypedBuilder)]
struct GatewayDeployerServiceInternal<'a> {
    state: &'a State,
    gateway_patcher_channel_sender: mpsc::Sender<Operation<KubeGateway>>,
    gateway_class_patcher_channel_sender: mpsc::Sender<Operation<GatewayClass>>,
    controller_name: String,
}

impl<'a> GatewayDeployerServiceInternal<'a> {
    async fn on_version_not_changed(&mut self, gateway_id: &ResourceKey, gateway_class_name: &str, resource: &Arc<KubeGateway>) {
        let controller_name = &self.controller_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string()) {
                    self.state.save_gateway(gateway_id.clone(), resource).expect("We expect the lock to work");
                    common::add_finalizer_to_gateway_class(&self.gateway_class_patcher_channel_sender, gateway_class_name, controller_name)
                        .instrument(Span::current().clone())
                        .await;
                    let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                        finalizers.iter().any(|f| f == controller_name)
                    } else {
                        false
                    };

                    if !has_finalizer {
                        let () = common::add_finalizer(&self.gateway_patcher_channel_sender, gateway_id, controller_name)
                            .instrument(Span::current().clone())
                            .await;
                    };
                }
            }
        }
    }
}
