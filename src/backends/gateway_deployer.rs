use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};

use crate::common::{
    ChangedContext, DeletedContext, Gateway, GatewayEvent, GatewayProcessedPayload, GatewayResponse, GatewayStatus, ListenerStatus, Route, RouteProcessedPayload, RouteStatus, RouteToListenersMapping,
};

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
                            GatewayEvent::GatewayChanged(ChangedContext{ response_sender, gateway, route_to_listeners_mapping }) => {
                                let (attached_routes, ignored_routes):(Vec<_>, Vec<_>) = Iterator::partition(route_to_listeners_mapping.iter().enumerate(), |f| f.0 % 2 ==0  );
                                let attached_routes = attached_routes.into_iter().map(|i| i.1.route.clone()).collect();
                                let ignored_routes = ignored_routes.into_iter().map(|i| i.1.route.clone()).collect();
                                let processed = deploy_gateway(&gateway,&route_to_listeners_mapping);
                                let gateway_status = GatewayStatus{ id: gateway.id, name: gateway.name, namespace: gateway.namespace, listeners: processed};
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(gateway_status, attached_routes, ignored_routes)));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::GatewayDeleted(DeletedContext{ response_sender, gateway: _, routes: _ }) => {

                                let sent = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::RouteChanged(ChangedContext{ response_sender, gateway, route_to_listeners_mapping: _ }) =>{
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

pub fn deploy_gateway(gateway: &Gateway, routes: &[RouteToListenersMapping]) -> Vec<ListenerStatus> {
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
