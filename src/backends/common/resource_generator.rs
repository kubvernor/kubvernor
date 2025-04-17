use std::collections::{BTreeMap, BTreeSet};

use tracing::debug;

use crate::{
    common::{self, EffectiveRoutingRule, Listener, ProtocolType, Route, RouteType, TlsType, DEFAULT_ROUTE_HOSTNAME},
    controllers::HostnameMatchFilter,
};

type ListenerNameToHostname = (String, Option<String>);

#[derive(Debug, Clone)]
pub struct EnvoyVirtualHost {
    pub name: String,
    pub effective_hostnames: Vec<String>,
    pub resolved_routes: Vec<Route>,
    pub unresolved_routes: Vec<Route>,
    pub effective_matching_rules: Vec<EffectiveRoutingRule>,
}

impl Ord for EnvoyVirtualHost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl Eq for EnvoyVirtualHost {}

impl PartialOrd for EnvoyVirtualHost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.name.cmp(&other.name))
    }
}

impl PartialEq for EnvoyVirtualHost {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

#[derive(Debug, Clone)]
pub struct EnvoyListener {
    pub name: String,
    pub port: i32,
    pub http_listener_map: BTreeSet<EnvoyVirtualHost>,
    pub tcp_listener_map: BTreeSet<ListenerNameToHostname>,
    pub tls_type: Option<TlsType>,
}

pub struct ResourceGenerator<'a> {
    effective_gateway: &'a common::Gateway,
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(effective_gateway: &'a common::Gateway) -> Self {
        Self { effective_gateway }
    }
    pub fn generate_resources(&self) -> BTreeMap<i32, EnvoyListener> {
        self.generate_envoy_representation()
    }

    fn generate_envoy_representation(&self) -> BTreeMap<i32, EnvoyListener> {
        let gateway = self.effective_gateway;
        let envoy_listeners = gateway.listeners().fold(BTreeMap::<i32, EnvoyListener>::new(), |mut acc, listener| {
            let port = listener.port();
            let listener_name = listener.name().to_owned();
            let listener_hostname = listener.hostname().cloned();
            let gateway_name = gateway.name().to_owned();
            let protocol_type = listener.protocol();
            let maybe_added = acc.get_mut(&port);

            if let Some(envoy_listener) = maybe_added {
                match protocol_type {
                    ProtocolType::Http | ProtocolType::Https => {
                        let mut new_listener = Self::generate_virtual_hosts(gateway_name, listener);
                        envoy_listener.http_listener_map.append(&mut new_listener.http_listener_map);
                    }
                    ProtocolType::Tcp => {
                        envoy_listener.tcp_listener_map.insert((listener_name, listener_hostname));
                    }
                    _ => (),
                }
            } else {
                match protocol_type {
                    ProtocolType::Http | ProtocolType::Https => {
                        let envoy_listener = Self::generate_virtual_hosts(gateway_name, listener);
                        acc.insert(port, envoy_listener);
                    }
                    ProtocolType::Tcp => {
                        let mut listener_map = BTreeSet::new();
                        listener_map.insert((listener_name, listener_hostname));
                        acc.insert(
                            port,
                            EnvoyListener {
                                name: gateway_name,
                                port,
                                http_listener_map: BTreeSet::new(),
                                tcp_listener_map: listener_map,
                                tls_type: None,
                            },
                        );
                    }
                    _ => (),
                }
            }

            acc
        });
        envoy_listeners
    }

    fn generate_virtual_hosts(gateway_name: String, listener: &Listener) -> EnvoyListener {
        let (resolved, unresolved) = listener.routes();
        let resolved: Vec<_> = resolved
            .into_iter()
            .filter(|r| match &r.config.route_type {
                RouteType::Http(_) => true,
                RouteType::Grpc(_) => false
            })
            .collect();

        let mut listener_map = BTreeSet::new();
        let potential_hostnames = Self::calculate_potential_hostnames(&resolved, listener.hostname().cloned());
        debug!("generate_virtual_hosts Potential hostnames {potential_hostnames:?}");
        for potential_hostname in potential_hostnames {
            let effective_matching_rules = listener
                .effective_matching_rules()
                .into_iter()
                .filter(|&em| {
                    let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                    debug!("generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                    filtered
                })
                .cloned()
                .collect::<Vec<_>>();

            listener_map.insert(EnvoyVirtualHost {
                effective_matching_rules,
                name: listener.name().to_owned() + "-" + &potential_hostname,
                effective_hostnames: Self::calculate_effective_hostnames(&resolved, Some(potential_hostname)),
                resolved_routes: resolved.iter().map(|r| (**r).clone()).collect(),
                unresolved_routes: unresolved.iter().map(|r| (**r).clone()).collect(),
            });
        }

        EnvoyListener {
            name: gateway_name,
            port: listener.port(),
            tls_type: listener.config().tls_type.clone(),
            http_listener_map: listener_map,
            tcp_listener_map: BTreeSet::new(),
        }
    }

    fn calculate_potential_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
        let routes_hostnames = routes.iter().fold(BTreeSet::new(), |mut acc, r| {
            acc.append(&mut r.hostnames().iter().cloned().collect::<BTreeSet<_>>());
            acc
        });

        match (listener_hostname.is_none(), routes_hostnames.is_empty()) {
            (true, false) => Vec::from_iter(routes_hostnames),
            (..) => listener_hostname.map_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()], |hostname| vec![hostname]),
        }
    }

    fn calculate_effective_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
        let routes_hostnames = routes.iter().fold(BTreeSet::new(), |mut acc, r| {
            acc.append(&mut r.hostnames().iter().cloned().collect::<BTreeSet<_>>());
            acc
        });

        match (listener_hostname.is_none(), routes_hostnames.is_empty()) {
            (true, false) => Vec::from_iter(routes_hostnames),
            (..) => listener_hostname.map_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()], |hostname| vec![format!("{hostname}:*"), hostname]),
        }
    }
}
