use std::collections::{BTreeMap, HashMap};

use gateway_api::apis::standard::gateways::Gateway;

use crate::common::{ProtocolType, Route, RouteToListenersMapping};
#[derive(Debug)]
struct RdsData {
    pub route_file_name: String,
    pub rds_content: String,
}
impl RdsData {
    fn new(route_file_name: String, rds_content: String) -> Self {
        Self { route_file_name, rds_content }
    }
}

#[allow(clippy::struct_field_names)]
#[derive(Debug)]
struct XdsData {
    pub lds_content: String,
    pub rds_content: Vec<RdsData>,
    pub cds_content: String,
}

impl XdsData {
    fn new(lds_content: String, rds_content: Vec<RdsData>, cds_content: String) -> Self {
        Self {
            lds_content,
            rds_content,
            cds_content,
        }
    }
}

type ListenerNameToHostname = (String, Option<String>);
type ListenerNameToRoute = (String, Route);

type RouteKey = (String, String);

struct CollapsedListener {
    port: i32,
    listener_map: HashMap<ProtocolType, Vec<ListenerNameToHostname>>,
}

struct CollapsedRoute {
    name: String,
    namespace: String,
    listener_names: Vec<ListenerNameToRoute>,
}

fn collapse_listeners_and_routes(gateway: &Gateway, route_to_listeners_mapping: &[RouteToListenersMapping]) {
    let collapsed_listeners = gateway.spec.listeners.iter().fold(BTreeMap::<i32, CollapsedListener>::new(), |mut acc, listener| {
        let port = listener.port;
        let protocol_type = ProtocolType::try_from(listener.protocol.clone()).unwrap_or(ProtocolType::Http);
        let maybe_added = acc.get_mut(&port);

        if let Some(added) = maybe_added {
            let maybe_protocol = added.listener_map.get_mut(&protocol_type);
            if let Some(protocol_map) = maybe_protocol {
                protocol_map.push((listener.name.clone(), listener.hostname.clone()));
            } else {
                added.listener_map.insert(protocol_type, vec![(listener.name.clone(), listener.hostname.clone())]);
            }
        } else {
            let mut listener_map = HashMap::new();
            listener_map.insert(protocol_type, vec![(listener.name.clone(), listener.hostname.clone())]);
            acc.insert(port, CollapsedListener { port, listener_map });
        }

        acc
    });

    let collapsed_routes = route_to_listeners_mapping.iter().fold(BTreeMap::<RouteKey, CollapsedRoute>::new(), |mut acc, route_mapping| {
        let key = (route_mapping.route.name().to_owned(), route_mapping.route.namespace().to_owned());
        let maybe_added = acc.get_mut(&key);
        if let Some(added) = maybe_added {
            for listener in &route_mapping.listeners {
                added.listener_names.push((listener.name.clone(), route_mapping.route.clone()));
            }
        } else {
            let mut listener_names = vec![];
            for listener in &route_mapping.listeners {
                listener_names.push((listener.name.clone(), route_mapping.route.clone()));
            }
            acc.insert(
                key.clone(),
                CollapsedRoute {
                    name: key.0.clone(),
                    namespace: key.1.clone(),
                    listener_names,
                },
            );
        }
        acc
    });
}
