use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use futures::channel::oneshot;
use log::{debug, info};
use thiserror::Error;
use tokio::sync::mpsc::{self, Receiver};

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerStatus {
    Added,
    Updated,
    AlreadyConfigured,
}
impl std::fmt::Display for ListenerStatus {
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
}

impl ListenerConfig {
    pub fn new(name: String, port: i32, hostname: Option<String>) -> Self {
        Self {
            name,
            port,
            hostname,
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
pub enum ListenerResponse {
    Processed(Vec<(String, Result<ListenerStatus, ListenerError>)>),
    Deleted,
}

pub enum ListenerEvent {
    AddListeners((oneshot::Sender<ListenerResponse>, Vec<Listener>)),
    DeleteAllListeners(oneshot::Sender<ListenerResponse>),
}

pub struct ListenerChannelHandler {
    listeners: Listeners,
    event_receiver: Receiver<ListenerEvent>,
}

impl ListenerChannelHandler {
    pub fn new() -> (mpsc::Sender<ListenerEvent>, Self) {
        let (sender, receiver) = mpsc::channel(1024);
        (
            sender,
            Self {
                listeners: Listeners::new(),
                event_receiver: receiver,
            },
        )
    }
    pub async fn start(&mut self) {
        info!("Listener handler started");
        loop {
            tokio::select! {
                    Some(event) = self.event_receiver.recv() => {
                         match event{
                            ListenerEvent::AddListeners((response_sender, listeners)) => {
                                let processed = self.listeners.update_listeners(listeners);
                                let sent = response_sender.send(ListenerResponse::Processed(processed));
                                if let Err(e) = sent{
                                    info!("Listener handler closed {e:?}");
                                    return;
                                }
                            }

                            ListenerEvent::DeleteAllListeners(response_sender) => {
                                self.listeners.delete_listeners();
                                let sent = response_sender.send(ListenerResponse::Deleted);
                                if let Err(e) = sent{
                                    info!("Listener handler closed {e:?}");
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
