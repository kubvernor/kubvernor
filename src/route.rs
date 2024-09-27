use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use futures::channel::oneshot;
use thiserror::Error;
use tokio::sync::mpsc::{self, Receiver};
use tracing::{debug, info};

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteStatus {
    Added,
    Updated,
    AlreadyConfigured,
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
pub struct RouteConfig {
    pub name: String,
    pub port: i32,
    pub hostname: Option<String>,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Route {
    Http(RouteConfig),
    Https(RouteConfig),
    Tcp(RouteConfig),
    Tls(RouteConfig),
    Udp(RouteConfig),
}

impl Route {
    fn name(&self) -> &str {
        match self {
            Route::Http(config)
            | Route::Https(config)
            | Route::Tcp(config)
            | Route::Tls(config)
            | Route::Udp(config) => config.name.as_str(),
        }
    }

    fn port(&self) -> i32 {
        match self {
            Route::Http(config)
            | Route::Https(config)
            | Route::Tcp(config)
            | Route::Tls(config)
            | Route::Udp(config) => config.port,
        }
    }

    fn protocol(&self) -> ProtocolType {
        match self {
            Route::Http(_) => ProtocolType::Http,
            Route::Https(_) => ProtocolType::Https,
            Route::Tcp(_) => ProtocolType::Tcp,
            Route::Tls(_) => ProtocolType::Tls,
            Route::Udp(_) => ProtocolType::Udp,
        }
    }
    fn hostname(&self) -> Option<&String> {
        match self {
            Route::Http(config)
            | Route::Https(config)
            | Route::Tcp(config)
            | Route::Tls(config)
            | Route::Udp(config) => config.hostname.as_ref(),
        }
    }
}

type PortProtocolHostname = (i32, ProtocolType, Option<String>);
pub struct Routes {
    listeners_by_port_protocol_hostname: HashMap<PortProtocolHostname, String>,
    listeners_by_name: HashMap<String, Route>,
}

impl Default for Routes {
    fn default() -> Self {
        Self::new()
    }
}

impl Routes {
    pub fn new() -> Self {
        Self {
            listeners_by_name: HashMap::new(),
            listeners_by_port_protocol_hostname: HashMap::new(),
        }
    }

    pub fn update_listeners(
        &mut self,
        listeners: Vec<Route>,
    ) -> Vec<(String, Result<RouteStatus, RouteError>)> {
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

    pub fn update(&mut self, listener: Route) -> Result<RouteStatus, RouteError> {
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
                Ok(RouteStatus::Added)
            }
            (None, Some(_)) => {
                let (port, protocol, hostname) = key;
                Err(RouteError::NotDistinct(name, port, protocol, hostname))
            }
            (Some(existing_listener), None) => {
                *existing_listener = listener;
                Ok(RouteStatus::Updated)
            }
            (Some(_), Some(_)) => Ok(RouteStatus::AlreadyConfigured),
        }
    }

    pub fn remove(&mut self, name: &String) -> Result<Option<Route>, RouteError> {
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
pub enum RouteResponse {
    Processed(Vec<(String, Result<RouteStatus, RouteError>)>),
}
pub struct RouteEvent {
    pub response_sender: oneshot::Sender<RouteResponse>,
    pub listeners: Vec<Route>,
}

pub struct RouteChannelHandler {
    listeners: Routes,
    event_receiver: Receiver<RouteEvent>,
}

impl RouteChannelHandler {
    pub fn new() -> (mpsc::Sender<RouteEvent>, Self) {
        let (sender, receiver) = mpsc::channel(1024);
        (
            sender,
            Self {
                listeners: Routes::new(),
                event_receiver: receiver,
            },
        )
    }
    pub async fn start(&mut self) {
        info!("Route handler started");
        loop {
            tokio::select! {
                Some(event) = self.event_receiver.recv() => {
                    let RouteEvent{ response_sender, listeners } = event;
                    let processed = self.listeners.update_listeners(listeners);
                    let sent = response_sender.send(RouteResponse::Processed(processed));
                    if let Err(e) = sent{
                        info!("Route handler closed {e:?}");
                        return;
                    }
                }
                else => {
                    break;
                }
            }
        }
    }
}

// #[cfg(test)]
// mod test{
//     use super::*;

//     impl RouteConfig{
//         fn new_for_test(name: String, port: i32, hostname: Option<String>)-> RouteConfig{
//             Self { name, port, hostname }
//         }
//     }

//     #[test]
//     fn check_unique_listeners(){
//         let mut listeners = Routes::new();
//         let l1_name = "listener1".to_string();
//         let l1_port  = 10;
//         let l1_protocol = ProtocolType::Http;

//         let l2_name = "listener2".to_string();
//         let l2_port  = 10;

//         let l3_name = "listener3".to_string();
//         let l3_port  = 10;
//         let l3_protocol = ProtocolType::Http;

//         let lc1 = RouteConfig::new_for_test(l1_name.clone(), l1_port, None);
//         let lc2 = RouteConfig::new_for_test(l2_name.clone(), l2_port, None);
//         let lc3 = RouteConfig::new_for_test(l3_name.clone(), l3_port, None);

//         let gw1 = Route::Http(lc1);
//         let gw2 = Route::Tcp(lc2);
//         let gw3 = Route::Http(lc3);

//         let added = listeners.add(gw1.clone());
//         assert!(added.is_ok());

//         let added = listeners.add(gw1.clone());
//         let err = added.err().unwrap();
//         // let err : &RouteError = err.downcast_ref().unwrap();
//         assert_eq!(err, RouteError::NotDistinct(l1_name.clone(), l1_port, l1_protocol, None));

//         let added = listeners.add(gw2);
//         assert!(added.is_ok());

//         let added = listeners.add(gw3.clone());
//         let err = added.err().unwrap();
//         //let err : &RoutesError = err.downcast_ref().unwrap();
//         assert_eq!(err, RouteError::NotDistinct(l3_name.clone(), l3_port, l3_protocol, None));

//         let old_listener = listeners.remove(l1_name).unwrap().unwrap();
//         assert_eq!(old_listener, gw1);

//         let removed = listeners.remove(l3_name);
//         let removed = removed.unwrap();
//         assert!(removed.is_none());

//         let added = listeners.add(gw3);
//         assert!(added.is_ok());

//     }

//     #[test]
//     fn check_unique_listeners_with_hostnames(){
//         let mut listeners = Routes::new();
//         let l1_name = "listener1".to_string();
//         let l1_port  = 10;
//         let l1_protocol = ProtocolType::Http;
//         let l1_hostname = Some("example.com".to_string());

//         let l2_name = "listener2".to_string();
//         let l2_port  = 10;
//         let l2_hostname = Some("example.com".to_string());

//         let l3_name = "listener3".to_string();
//         let l3_port  = 10;
//         let l3_protocol = ProtocolType::Http;
//         let l3_hostname = Some("example.com".to_string());

//         let l4_name = "listener4".to_string();
//         let l4_port  = 10;
//         let l4_hostname = Some("example.com".to_string());
//         let l4_protocol = ProtocolType::Http;

//         let l5_name = "listener5".to_string();
//         let l5_port  = 10;
//         let l5_hostname = Some("test.example.com".to_string());

//         let lc1 = RouteConfig::new_for_test(l1_name.clone(), l1_port, l1_hostname.clone());
//         let lc2 = RouteConfig::new_for_test(l2_name.clone(), l2_port, l2_hostname.clone());
//         let lc3 = RouteConfig::new_for_test(l3_name.clone(), l3_port, l3_hostname.clone());
//         let lc4 = RouteConfig::new_for_test(l4_name.clone(), l4_port, l4_hostname.clone());
//         let lc5 = RouteConfig::new_for_test(l5_name.clone(), l5_port, l5_hostname.clone());

//         let gw1 = Route::Http(lc1);
//         let gw2 = Route::Tcp(lc2);
//         let gw3 = Route::Http(lc3);
//         let gw4 = Route::Http(lc4);
//         let gw5 = Route::Http(lc5);

//         let added = listeners.add(gw1.clone());
//         assert!(added.is_ok());

//         let added = listeners.add(gw1.clone());
//         let err = added.err().unwrap();
//         //let err : &RouteError = err.downcast_ref().unwrap();
//         assert_eq!(err, RouteError::NotDistinct(l1_name.clone(), l1_port, l1_protocol, l1_hostname.clone()));

//         let added = listeners.add(gw2);
//         assert!(added.is_ok());

//         let added = listeners.add(gw3.clone());
//         let err = added.err().unwrap();
//         //let err : &RoutesError = err.downcast_ref().unwrap();
//         assert_eq!(err, RouteError::NotDistinct(l3_name.clone(), l3_port, l3_protocol, l3_hostname.clone()));

//         let old_listener = listeners.remove(l1_name).unwrap().unwrap();
//         assert_eq!(old_listener, gw1);

//         let removed = listeners.remove(l3_name);
//         let removed = removed.unwrap();
//         assert!(removed.is_none());

//         let added = listeners.add(gw3);
//         assert!(added.is_ok());

//         let added = listeners.add(gw5.clone());
//         assert!(added.is_ok());

//         let added = listeners.add(gw4.clone());
//         let err = added.err().unwrap();
//         //let err : &RoutesError = err.downcast_ref().unwrap();
//         assert_eq!(err, RouteError::NotDistinct(l4_name.clone(), l4_port, l4_protocol, l4_hostname.clone()));

//     }

// }
