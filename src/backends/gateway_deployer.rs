use std::fmt::Display;

use log::{info, warn};
use thiserror::Error;
use tokio::sync::{
    mpsc::{self, Receiver},
    oneshot,
};

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Route is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerStatus {
    Added(i32),
    Updated(i32),
    AlreadyConfigured(i32),
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
}

impl RouteConfig {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Route {
    Http(RouteConfig),
    Grpc(RouteConfig),
}

pub struct Gateway {}

pub fn deploy_gateway(
    gateway_name: String,
    listeners: Vec<Listener>,
    routes: Vec<Route>,
) -> Vec<(String, Result<ListenerStatus, ListenerError>)> {
    info!("Got following info {gateway_name} {listeners:?} {routes:?}");
    vec![]
}

#[derive(Debug)]
pub struct GatewayProcessedPayload {
    pub listeners: Vec<(String, Result<ListenerStatus, ListenerError>)>,
    pub routes: Vec<Route>,
}

impl GatewayProcessedPayload {
    pub fn new(
        listeners: Vec<(String, Result<ListenerStatus, ListenerError>)>,
        routes: Vec<Route>,
    ) -> Self {
        Self { listeners, routes }
    }
}

#[derive(Debug)]
pub struct RouteProcessedPayload {
    pub status: RouteStatus,
    pub listeners: Vec<String>,
}

impl RouteProcessedPayload {
    pub fn new(status: RouteStatus, listeners: Vec<String>) -> Self {
        Self { status, listeners }
    }
}

#[derive(Debug)]
pub enum GatewayResponse {
    GatewayProcessed(GatewayProcessedPayload),
    GatewayDeleted,
    RouteProcessed(RouteProcessedPayload),
    RouteDeleted,
}

#[derive(Debug)]
pub enum GatewayEvent {
    GatewayChanged(
        (
            oneshot::Sender<GatewayResponse>,
            String,
            Vec<Listener>,
            Vec<Route>,
        ),
    ),
    GatewayDeleted((oneshot::Sender<GatewayResponse>, String)),
    RouteChanged(
        (
            oneshot::Sender<GatewayResponse>,
            String,
            Vec<Listener>,
            Route,
            Vec<Route>,
        ),
    ),
}

impl Display for GatewayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayEvent::GatewayChanged((_, gateway, listeners, routes)) => write!(
                f,
                "GatewayEvent::GatewayChanged {gateway} listeners {listeners:?} routes {routes:?}"
            ),
            GatewayEvent::GatewayDeleted((_, gateway)) => {
                write!(f, "GatewayEvent::GatewayDeleted {gateway}")
            }

            GatewayEvent::RouteChanged((_, gateway, listeners, routes, linked_routes)) => write!(
                f,
                "GatewayEvent::RouteChanged {gateway} listeners {listeners:?} routes {routes:?} linked_routes {linked_routes:?}"
            ),
        }
    }
}

pub struct GatewayChannelHandler {
    event_receiver: Receiver<GatewayEvent>,
}

impl GatewayChannelHandler {
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
                            GatewayEvent::GatewayChanged((response_sender, gateway, listeners, routes)) => {
                                let processed = deploy_gateway(gateway,listeners,routes.clone());
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(processed, routes.clone())));
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::GatewayDeleted((response_sender, gateway)) => {
                                let _processed = deploy_gateway(gateway,vec![],vec![]);
                                let sent = response_sender.send(GatewayResponse::GatewayDeleted);
                                if let Err(e) = sent{
                                    warn!("Gateway handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::RouteChanged((response_sender, gateway, listeners, route, mut linked_routes)) => {
                                linked_routes.push(route);
                                let all_routes = linked_routes;
                                let _processed = deploy_gateway(gateway,listeners,all_routes);

                                let sent = response_sender.send(GatewayResponse::RouteProcessed(RouteProcessedPayload::new(RouteStatus::Attached, vec![])));
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
