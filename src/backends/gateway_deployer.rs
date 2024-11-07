use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};

use crate::common::{ChangedContext, DeletedContext, DeployedGatewayStatus, Gateway, GatewayEvent, GatewayProcessedPayload, GatewayResponse, RouteProcessedPayload, RouteStatus};

pub struct GatewayDeployerChannelHandler {
    event_receiver: Receiver<GatewayEvent>,
}

impl GatewayDeployerChannelHandler {
    pub fn new() -> (mpsc::Sender<GatewayEvent>, Self) {
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
                            GatewayEvent::GatewayChanged(ChangedContext{ response_sender, gateway }) => {

                                deploy_gateway(&gateway);
                                let gateway_status = DeployedGatewayStatus{ id: *gateway.id(), name: gateway.name().to_owned(), namespace: gateway.namespace().to_owned(), attached_addresses: vec![]};
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(gateway_status, gateway)));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::GatewayDeleted(DeletedContext{ response_sender, gateway: _  }) => {

                                let sent = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::RouteChanged(ChangedContext{ response_sender, gateway}) =>{
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
