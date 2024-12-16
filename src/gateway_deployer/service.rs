use std::sync::Arc;

use gateway_api::apis::standard::{gatewayclasses::GatewayClass, gateways::Gateway as KubeGateway, httproutes::HTTPRoute};
use kube::{Client, Resource};
use tokio::sync::{oneshot, Mutex, MutexGuard};
use tracing::{span, warn, Instrument, Level};
use typed_builder::TypedBuilder;

use crate::{
    common::{Gateway, GatewayEvent, ResourceKey},
    controllers::GatewayDeployer,
    patchers::{FinalizerContext, Operation, PatchContext},
    state::State,
};

#[derive(TypedBuilder)]
pub struct GatewayDeployerService {
    client: Client,
    state: Arc<Mutex<State>>,
    backend_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayEvent>,
    gateway_deployer_channel_receiver: tokio::sync::mpsc::Receiver<(Gateway, Arc<KubeGateway>, String)>,
    gateway_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<KubeGateway>>,
    gateway_class_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<GatewayClass>>,
    http_route_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<HTTPRoute>>,
    controller_name: String,
}

const GATEWAY_CLASS_FINALIZER_NAME: &str = "gateway-exists-finalizer.gateway.networking.k8s.io";

impl GatewayDeployerService {
    pub async fn start(self) {
        let mut resolve_receiver = self.gateway_deployer_channel_receiver;
        let controller_name = self.controller_name.clone();
        loop {
            tokio::select! {
                Some((gateway,kube_gateway,gateway_class_name)) = resolve_receiver.recv() => {
                    let span = span!(Level::INFO, "GatewayDeployerService", id = %gateway.key());
                    let gateway_id = gateway.key();
                    let version = kube_gateway.meta().resource_version.clone();
                    let mut state = self.state.lock().await;

                    let mut deployer = GatewayDeployer::builder()
                        .sender(self.backend_deployer_channel_sender.clone())
                        .gateway(gateway.clone())
                        .kube_gateway(&kube_gateway)
                        .state(&state)
                        .http_route_patcher(self.http_route_patcher_channel_sender.clone())
                        .controller_name(&self.controller_name)
                        .build();


                    if let Ok(updated_gateway) = deployer.deploy_gateway().instrument(span.clone()).await{
                        state.save_gateway(gateway_id.clone(), &kube_gateway);
                        let (sender, receiver) = oneshot::channel();
                        let _res = self
                            .gateway_patcher_channel_sender
                            .send(Operation::PatchStatus(PatchContext {
                                resource_key: gateway_id.clone(),
                                resource: updated_gateway,
                                controller_name: controller_name.clone(),
                                version,
                                response_sender: sender,
                            }))
                            .await;

                        let patched_gateway = receiver.await;
                        if let Ok(maybe_patched) = patched_gateway {
                            match maybe_patched {
                                Ok(patched_gateway) => {
                                    let patched_gateway = Arc::new(patched_gateway);
                                    state.save_gateway(gateway_id.clone(), &patched_gateway);
                                    let mut svc = GatewayDeployerServiceInternal::builder()
                                        .state(state)
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
    state: MutexGuard<'a, State>,
    gateway_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<KubeGateway>>,
    gateway_class_patcher_channel_sender: tokio::sync::mpsc::Sender<Operation<GatewayClass>>,
    controller_name: String,
}

impl<'a> GatewayDeployerServiceInternal<'a> {
    async fn on_version_not_changed(&mut self, gateway_id: &ResourceKey, gateway_class_name: &str, resource: &Arc<KubeGateway>) {
        let controller_name = &self.controller_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string()) {
                    self.state.save_gateway(gateway_id.clone(), resource);
                    self.add_finalizer_to_gateway_class(gateway_class_name, controller_name).await;
                    let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                        finalizers.iter().any(|f| f == controller_name)
                    } else {
                        false
                    };

                    if !has_finalizer {
                        let () = self.add_finalizer(gateway_id, controller_name).await;
                    };
                }
            }
        }
    }

    async fn add_finalizer(&self, gateway_id: &ResourceKey, controller_name: &str) {
        let _ = self
            .gateway_patcher_channel_sender
            .send(Operation::PatchFinalizer(FinalizerContext {
                resource_key: gateway_id.clone(),
                controller_name: controller_name.to_owned(),
                finalizer_name: controller_name.to_owned(),
            }))
            .await;
    }

    async fn add_finalizer_to_gateway_class(&self, gateway_class_name: &str, controller_name: &str) {
        let key = ResourceKey::new(gateway_class_name);
        let _ = self
            .gateway_class_patcher_channel_sender
            .send(Operation::PatchFinalizer(FinalizerContext {
                resource_key: key,
                controller_name: controller_name.to_owned(),
                finalizer_name: GATEWAY_CLASS_FINALIZER_NAME.to_owned(),
            }))
            .await;
    }
}
