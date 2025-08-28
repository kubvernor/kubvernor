use std::collections::BTreeMap;

use agentgateway_api_rs::agentgateway::dev::resource::{Listener, Route};

use crate::{
    backends::envoy::common::InferenceClusterInfo,
    common::{self, ProtocolType},
};

pub(crate) struct ResourceGenerator<'a> {
    effective_gateway: &'a common::Gateway,
    resources: BTreeMap<i32, Listener>,
    inference_clusters: Vec<InferenceClusterInfo>,
}

impl From<ProtocolType> for i32 {
    fn from(value: ProtocolType) -> Self {
        match value {
            ProtocolType::Http => 1,
            ProtocolType::Https => 2,
            ProtocolType::Tcp => 4,
            ProtocolType::Tls => 3,
            ProtocolType::Udp => 0,
        }
    }
}

fn create_bind_name(port: i32) -> String {
    format!("bind-port-{port}")
}

#[derive(Debug, Clone, Ord, Eq, PartialEq, PartialOrd)]
struct Bind {
    key: String,
    port: u32,
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(effective_gateway: &'a common::Gateway) -> Self {
        Self { effective_gateway, resources: BTreeMap::new(), inference_clusters: vec![] }
    }

    fn generate_bindings_and_listeners(&self) -> BTreeMap<Bind, Vec<Listener>> {
        let gateway = self.effective_gateway;
        let listeners = gateway.listeners().fold(BTreeMap::<Bind, Vec<Listener>>::new(), |mut acc, listener| {
            let port = listener.port();
            let listener_name = listener.name().to_owned();
            let listener_hostname = listener.hostname().cloned();
            let gateway_name = gateway.name().to_owned();
            let protocol_type = listener.protocol();

            let bind = Bind { key: create_bind_name(port), port: port as u32 };
            let maybe_added = acc.get_mut(&bind);

            let agentgateway_listener = Listener {
                key: listener_name.clone(),
                name: listener_name,
                bind_key: create_bind_name(port),
                gateway_name,
                hostname: listener.hostname().cloned().unwrap_or_default(),
                protocol: listener.protocol().into(),
                tls: None,
            };

            if let Some(listners) = maybe_added {
                listners.push(agentgateway_listener);
            } else {
                acc.insert(bind, vec![agentgateway_listener]);
            }
            acc
        });
        listeners
    }

    fn generate_routes(&self) -> Vec<Route> {
        let gateway = self.effective_gateway;
        gateway
            .listeners()
            .flat_map(|l| {
                let (resolved, _) = l.routes();
                resolved
                    .iter()
                    .filter_map(|route| match route.route_type(){
                        common::RouteType::Http(configuration) => Some((route, configuration.routing_rules)),
                        common::RouteType::Grpc(_) => None,
                    } )
                    .map(|(route, routing_rules)| Route {
                        key: route.name().to_owned(),
                        listener_key: l.name().to_owned(),
                        rule_name: route.name().to_owned(),
                        route_name: route.name().to_owned(),
                        hostnames: route.hostnames().to_vec(),
                        matches: route.vec![],
                        filters: vec![],
                        backends: vec![],
                        traffic_policy: None,
                        inline_policies: vec![],
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}
