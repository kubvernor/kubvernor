use gateway_api::apis::standard::gateways::GatewayListeners;
use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};

use crate::common::{Gateway, GatewayEvent, GatewayProcessedPayload, GatewayResponse, GatewayStatus, ListenerStatus, Route, RouteProcessedPayload, RouteStatus};

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
                            GatewayEvent::GatewayChanged((response_sender, gateway, routes_and_listeners)) => {
                                let (attached_routes, ignored_routes):(Vec<_>, Vec<_>) = Iterator::partition(routes_and_listeners.iter().enumerate(), |f| f.0 % 2 ==0  );
                                let attached_routes = attached_routes.into_iter().map(|i| i.1.0.clone()).collect();
                                let ignored_routes = ignored_routes.into_iter().map(|i| i.1.0.clone()).collect();
                                let processed = deploy_gateway(&gateway,&routes_and_listeners);
                                let gateway_status = GatewayStatus{ id: gateway.id, name: gateway.name, namespace: gateway.namespace, listeners: processed};
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(gateway_status, attached_routes, ignored_routes)));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::GatewayDeleted((response_sender, _gateway, _routes)) => {

                                let sent = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::RouteChanged((response_sender, gateway, _routes_and_listeners)) =>{
                                let gateway_status = GatewayStatus{ id: gateway.id, name: gateway.name, namespace: gateway.namespace, listeners: vec![]};
                                let sent = response_sender.send(GatewayResponse::RouteProcessed(RouteProcessedPayload::new(RouteStatus::Attached, &gateway_status)));
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

pub fn deploy_gateway(gateway: &Gateway, routes: &[(Route, Vec<GatewayListeners>)]) -> Vec<ListenerStatus> {
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
