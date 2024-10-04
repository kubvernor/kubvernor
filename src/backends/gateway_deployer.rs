use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};

use crate::backends::{GatewayProcessedPayload, GatewayResponse, GatewayStatus, RouteProcessedPayload, RouteStatus};

use super::common::{Gateway, GatewayEvent, ListenerStatus, Route};

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
                            GatewayEvent::GatewayChanged((response_sender, gateway, routes)) => {
                                let (attached_routes, ignored_routes):(Vec<_>, Vec<_>) = Iterator::partition(routes.iter().enumerate(), |f| f.0 % 2 ==0  );
                                let attached_routes = attached_routes.into_iter().map(|i| i.1.clone()).collect();
                                let ignored_routes = ignored_routes.into_iter().map(|i| i.1.clone()).collect();
                                let processed = deploy_gateway(&gateway,&routes);
                                let gateway_status = GatewayStatus{ id: gateway.id, name: gateway.name, namespace: gateway.namespace, listeners: processed};
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(gateway_status, attached_routes, ignored_routes)));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::GatewayDeleted((response_sender, gateway, routes)) => {
                                let _processed = deploy_gateway(&gateway, &routes);
                                let sent = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::RouteChanged((response_sender, route, linked_routes, gateway)) | GatewayEvent::RouteDeleted((response_sender, route, linked_routes, gateway))=> {
                                let processed = deploy_route(&route, &linked_routes, &gateway);
                                let gateway_status = GatewayStatus{ id: gateway.id, name: gateway.name, namespace: gateway.namespace, listeners: processed};

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

pub fn deploy_gateway(gateway: &Gateway, routes: &[Route]) -> Vec<ListenerStatus> {
    info!("Got following info {gateway:?} {routes:?}");
    gateway
        .listeners
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i % 2 == 0 {
                let routes = i32::try_from(routes.len()).unwrap_or(i32::MAX);
                ListenerStatus::Accepted((l.name().to_owned(), routes))
            } else {
                ListenerStatus::Conflicted(l.name().to_owned())
            }
        })
        .collect()
}

pub fn deploy_route(route: &Route, linked_routes: &[Route], gateway: &Gateway) -> Vec<ListenerStatus> {
    info!("Got following route {route:?} {linked_routes:?} {gateway:?}");
    vec![]
}
