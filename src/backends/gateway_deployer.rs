use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};

use crate::common::{BackendGatewayEvent, ChangedContext, DeletedContext, DeployedGatewayStatus, Gateway, GatewayResponse, RouteProcessedPayload, RouteStatus};

pub struct GatewayDeployerChannelHandler {
    event_receiver: Receiver<BackendGatewayEvent>,
}

impl GatewayDeployerChannelHandler {
    pub fn new() -> (mpsc::Sender<BackendGatewayEvent>, Self) {
        let (sender, receiver) = mpsc::channel(1024);
        (sender, Self { event_receiver: receiver })
    }
    pub async fn start(&mut self) {
        info!("Gateways handler started");
        loop {
            tokio::select! {
                    Some(event) = self.event_receiver.recv() => {
                        info!("Backend got gateway {event:#}");
                         match event{
                            BackendGatewayEvent::GatewayChanged(ChangedContext{ response_sender, gateway, span: _ }) => {
                                deploy_gateway(&gateway);
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(gateway));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            BackendGatewayEvent::GatewayDeleted(DeletedContext{ response_sender, gateway: _, span: _  }) => {

                                let sent = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            BackendGatewayEvent::RouteChanged(ChangedContext{ response_sender, gateway, span:_}) =>{
                                let gateway_status = DeployedGatewayStatus{ id: *gateway.id(), name: gateway.name().to_owned(), namespace: gateway.namespace().to_owned(), attached_addresses: vec![]};
                                let sent = response_sender.send(GatewayResponse::RouteProcessed(RouteProcessedPayload::new(RouteStatus::Attached, gateway_status)));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                        }
                    }
                    else => {
                        break;
                    }
            }
        }
    }
}

pub fn deploy_gateway(gateway: &Gateway) {
    info!("Got following info {gateway:?}");
}
