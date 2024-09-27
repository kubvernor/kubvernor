use std::fmt::Display;

use thiserror::Error;
use tokio::sync::{
    mpsc::{self, Receiver},
    oneshot,
};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum GatewayError {
    #[error("Conversion problem")]
    ConversionProblem(String),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Route is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Clone, Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerStatus {
    Accepted((String, i32)),
    Conflicted(String),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteStatus {
    Attached,
    Ignored,
}
impl std::fmt::Display for ListenerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::fmt::Display for RouteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
#[derive(Debug, Clone, PartialEq, PartialOrd, Hash, Eq)]
pub enum ProtocolType {
    Http,
    Https,
    Tcp,
    Tls,
    Udp,
}

impl Display for ProtocolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut e = format! {"{self:?}"};
        e.make_ascii_uppercase();
        write!(f, "{e}")
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct ListenerConfig {
    name: String,
    port: i32,
    hostname: Option<String>,
    routes: Vec<Route>,
}

impl ListenerConfig {
    pub fn new(name: String, port: i32, hostname: Option<String>) -> Self {
        Self {
            name,
            port,
            hostname,
            routes: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Listener {
    Http(ListenerConfig),
    Https(ListenerConfig),
    Tcp(ListenerConfig),
    Tls(ListenerConfig),
    Udp(ListenerConfig),
}

// #[derive(Clone, Debug, PartialEq, PartialOrd)]
// pub enum ListenerStatus {
//     Accpeted(i32),
//     Conflicted,
// }

impl Listener {
    fn name(&self) -> &str {
        match self {
            Listener::Http(config)
            | Listener::Https(config)
            | Listener::Tcp(config)
            | Listener::Tls(config)
            | Listener::Udp(config) => config.name.as_str(),
        }
    }

    fn port(&self) -> i32 {
        match self {
            Listener::Http(config)
            | Listener::Https(config)
            | Listener::Tcp(config)
            | Listener::Tls(config)
            | Listener::Udp(config) => config.port,
        }
    }

    fn protocol(&self) -> ProtocolType {
        match self {
            Listener::Http(_) => ProtocolType::Http,
            Listener::Https(_) => ProtocolType::Https,
            Listener::Tcp(_) => ProtocolType::Tcp,
            Listener::Tls(_) => ProtocolType::Tls,
            Listener::Udp(_) => ProtocolType::Udp,
        }
    }
    fn hostname(&self) -> Option<&String> {
        match self {
            Listener::Http(config)
            | Listener::Https(config)
            | Listener::Tcp(config)
            | Listener::Tls(config)
            | Listener::Udp(config) => config.hostname.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct RouteConfig {
    name: String,
    namespace: Option<String>,
}

impl RouteConfig {
    pub fn new(name: String, namespace: Option<String>) -> Self {
        Self { name, namespace }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Route {
    Http(RouteConfig),
    Grpc(RouteConfig),
}

impl Route {
    pub fn name(&self) -> &str {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.name,
        }
    }

    pub fn namespace(&self) -> Option<&String> {
        match self {
            Route::Http(c) | Route::Grpc(c) => c.namespace.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Gateway {
    pub id: Uuid,
    pub name: String,
    pub namespace: String,
    pub listeners: Vec<Listener>,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct GatewayStatus {
    pub id: Uuid,
    pub name: String,
    pub namespace: String,
    pub listeners: Vec<ListenerStatus>,
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

pub fn deploy_route(
    route: &Route,
    linked_routes: &[Route],
    gateway: &Gateway,
) -> Vec<ListenerStatus> {
    info!("Got following route {route:?} {linked_routes:?} {gateway:?}");
    vec![]
}

#[derive(Debug)]
pub struct GatewayProcessedPayload {
    pub gateway_status: GatewayStatus,
    pub attached_routes: Vec<Route>,
    pub ignored_routes: Vec<Route>,
}

impl GatewayProcessedPayload {
    pub fn new(
        gateway_status: GatewayStatus,
        attached_routes: Vec<Route>,
        ignored_routes: Vec<Route>,
    ) -> Self {
        Self {
            gateway_status,
            attached_routes,
            ignored_routes,
        }
    }
}

#[derive(Debug)]
pub struct RouteProcessedPayload {
    pub status: RouteStatus,
    pub gateway_status: GatewayStatus,
}

impl RouteProcessedPayload {
    pub fn new(status: RouteStatus, gateway_status: GatewayStatus) -> Self {
        Self {
            status,
            gateway_status,
        }
    }
}

#[derive(Debug)]
pub enum GatewayResponse {
    GatewayProcessed(GatewayProcessedPayload),
    GatewayDeleted(Vec<RouteStatus>),
    RouteProcessed(RouteProcessedPayload),
    RouteDeleted,
}

#[derive(Debug)]
pub enum GatewayEvent {
    GatewayChanged((oneshot::Sender<GatewayResponse>, Gateway, Vec<Route>)),
    GatewayDeleted((oneshot::Sender<GatewayResponse>, Gateway, Vec<Route>)),
    RouteChanged((oneshot::Sender<GatewayResponse>, Route, Vec<Route>, Gateway)),
    RouteDeleted((oneshot::Sender<GatewayResponse>, Route, Vec<Route>, Gateway)),
}

impl Display for GatewayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayEvent::GatewayChanged((_, gateway, routes)) => write!(
                f,
                "GatewayEvent::GatewayChanged gateway {gateway:?} routes {routes:?}"
            ),
            GatewayEvent::GatewayDeleted((_, gateway, routes)) => {
                write!(
                    f,
                    "GatewayEvent::GatewayDeleted gateway {gateway:?} routes {routes:?}"
                )
            }

            GatewayEvent::RouteChanged((_, route, linked_routes, gateways)) => {
                write!(
                    f,
                    "GatewayEvent::RouteChanged route {route:?} linked_routes {linked_routes:?} gateways {gateways:?}"
                )
            }
            GatewayEvent::RouteDeleted((_, route, linked_routes, gateways)) => {
                write!(
                    f,
                    "GatewayEvent::RouteDeleted route {route:?} linked_routes {linked_routes:?} gateways {gateways:?}"
                )
            }
        }
    }
}

pub struct GatewayDeployerChannelHandler {
    event_receiver: Receiver<GatewayEvent>,
}

impl GatewayDeployerChannelHandler {
    pub fn new() -> (mpsc::Sender<GatewayEvent>, Self) {
        let (sender, receiver) = mpsc::channel(1024);
        (
            sender,
            Self {
                event_receiver: receiver,
            },
        )
    }
    pub async fn start(&mut self) {
        info!("Gateways handler started");
        loop {
            tokio::select! {
                    Some(event) = self.event_receiver.recv() => {
                        info!("Backend got gateway {event:#}");
                         match event{
                            GatewayEvent::GatewayChanged((response_sender, gateway, routes)) => {
                                let attached_routes = if routes.len() > 0{
                                    &routes[1..]
                                }else{
                                    &[]
                                };

                                let ignored_routes = if routes.len() > 1{
                                    &routes[0..1]
                                }else{
                                    &[]
                                };
                                let processed = deploy_gateway(&gateway,&routes);
                                let gateway_status = GatewayStatus{ id: gateway.id, name: gateway.name, namespace: gateway.namespace, listeners: processed};
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(gateway_status, attached_routes.to_vec(), ignored_routes.to_vec())));
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
