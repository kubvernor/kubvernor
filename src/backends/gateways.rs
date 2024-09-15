use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use log::{debug, info, warn};
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
    Added,
    Updated,
    AlreadyConfigured,
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

pub struct Gateway {
    listeners: Listeners,
}
pub struct Gateways {
    gateways: HashMap<String, Gateway>,
}

impl Gateways {
    fn new() -> Self {
        Self {
            gateways: HashMap::new(),
        }
    }

    pub fn update_listeners(
        &mut self,
        gateway_name: String,
        listeners: Vec<Listener>,
    ) -> Vec<(String, Result<ListenerStatus, ListenerError>)> {
        if let Some(gateway) = self.gateways.get_mut(&gateway_name) {
            gateway.listeners.update_listeners(listeners)
        } else {
            let mut gateway = Gateway {
                listeners: Listeners::new(),
            };
            let updated = gateway.listeners.update_listeners(listeners);
            self.gateways.insert(gateway_name, gateway);
            updated
        }
    }

    pub fn remove_listeners(&mut self, gateway: &str) {
        if let Some(mut gateway) = self.gateways.remove(gateway) {
            gateway.listeners.delete_listeners();
        };
    }
}

type PortProtocolHostname = (i32, ProtocolType, Option<String>);
pub struct Listeners {
    listeners_by_port_protocol_hostname: HashMap<PortProtocolHostname, String>,
    listeners_by_name: HashMap<String, Listener>,
}

impl Default for Listeners {
    fn default() -> Self {
        Self::new()
    }
}

impl Listeners {
    pub fn new() -> Self {
        Self {
            listeners_by_name: HashMap::new(),
            listeners_by_port_protocol_hostname: HashMap::new(),
        }
    }

    pub fn update_listeners(
        &mut self,
        listeners: Vec<Listener>,
    ) -> Vec<(String, Result<ListenerStatus, ListenerError>)> {
        debug!("Updating listeners {}", listeners.len());
        let existing_keys: HashSet<_> = self
            .listeners_by_name
            .keys()
            .map(std::borrow::ToOwned::to_owned)
            .collect();
        let new_keys: HashSet<_> = listeners.iter().map(|l| l.name().to_owned()).collect();
        for name in existing_keys.difference(&new_keys) {
            debug!("Removing listener {name}");
            let _ = self.remove(&name.to_string());
        }

        listeners
            .into_iter()
            .map(|l| (l.name().to_owned(), self.update(l)))
            .collect()
    }

    pub fn delete_listeners(&mut self) {
        debug!("Deleting listeners");
        self.listeners_by_name.clear();
        self.listeners_by_port_protocol_hostname.clear();
    }

    pub fn update(&mut self, listener: Listener) -> Result<ListenerStatus, ListenerError> {
        let name = listener.name().to_owned();
        let protocol = listener.protocol();
        let port = listener.port();
        let hostname = listener.hostname().cloned();
        let key = (port, protocol, hostname);

        let listener_exists = self.listeners_by_name.get_mut(&name);
        let listener_duplicate = self.listeners_by_port_protocol_hostname.get(&key);

        match (listener_exists, listener_duplicate) {
            (None, None) => {
                self.listeners_by_port_protocol_hostname
                    .insert(key, name.clone());
                self.listeners_by_name.insert(name, listener);
                Ok(ListenerStatus::Added)
            }
            (None, Some(_)) => {
                let (port, protocol, hostname) = key;
                Err(ListenerError::NotDistinct(name, port, protocol, hostname))
            }
            (Some(existing_listener), None) => {
                *existing_listener = listener;
                Ok(ListenerStatus::Updated)
            }
            (Some(_), Some(_)) => Ok(ListenerStatus::AlreadyConfigured),
        }
    }

    pub fn remove(&mut self, name: &String) -> Result<Option<Listener>, ListenerError> {
        if let Some(gateway) = self.listeners_by_name.remove(name) {
            let key = &(
                gateway.port(),
                gateway.protocol(),
                gateway.hostname().cloned(),
            );
            self.listeners_by_port_protocol_hostname.remove(key);
            Ok(Some(gateway))
        } else {
            Ok(None)
        }
    }
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

            GatewayEvent::RouteChanged((_, gateway, listeners, routes)) => write!(
                f,
                "GatewayEvent::RouteChanged {gateway} listeners {listeners:?} routes {routes:?}"
            ),
        }
    }
}

pub struct GatewayChannelHandler {
    gateways: Gateways,
    event_receiver: Receiver<GatewayEvent>,
}

impl GatewayChannelHandler {
    pub fn new() -> (mpsc::Sender<GatewayEvent>, Self) {
        let (sender, receiver) = mpsc::channel(1024);
        (
            sender,
            Self {
                gateways: Gateways::new(),
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
                                let processed = self.gateways.update_listeners(gateway,listeners);
                                let sent = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload::new(processed, routes)));
                                if let Err(e) = sent{
                                    info!("Listener handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::GatewayDeleted((response_sender, gateway)) => {
                                self.gateways.remove_listeners(&gateway);
                                let sent = response_sender.send(GatewayResponse::GatewayDeleted);
                                if let Err(e) = sent{
                                    info!("Listener handler closed {e:?}");
                                    return;
                                }
                            }

                            GatewayEvent::RouteChanged((response_sender, gateway, listeners, route)) => {
                                let gateway_name = gateway;

                                let sent = response_sender.send(GatewayResponse::RouteProcessed(RouteProcessedPayload::new(RouteStatus::Attached, vec![])));
                                if let Err(e) = sent{
                                    info!("Listener handler closed {e:?}");
                                    return;
                                }
                                // let processed = self.gateways.update_listeners(gateway,listeners);
                                // let sent = response_sender.send(GatewayResponse::Processed(processed));
                                // if let Err(e) = sent{
                                //     info!("Listener handler closed {e:?}");
                                //     return;
                                // }
                            }

                            // GatewayEvent::RouteRemoved((response_sender, gateway, route)) => {
                            //     warn!("Route added {gateway} {route:?}");
                            //     let sent = response_sender.send(GatewayResponse::RouteDeleted);
                            //     if let Err(e) = sent{
                            //         info!("Listener handler closed {e:?}");
                            //         return;
                            //     }
                            //     // let processed = self.gateways.update_listeners(gateway,listeners);
                            //     // let sent = response_sender.send(GatewayResponse::Processed(processed));
                            //     // if let Err(e) = sent{
                            //     //     info!("Listener handler closed {e:?}");
                            //     //     return;
                            //     // }
                            // }
                        }
                    }
                    else => {
                        break;
                    }
            }
        }
    }
}
