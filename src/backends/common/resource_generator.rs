use std::{
    cmp,
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use envoy_api_rs::{
    envoy::{
        config::{
            cluster::v3::{
                cluster::{ClusterDiscoveryType, DiscoveryType, LbPolicy},
                Cluster as EnvoyCluster, LoadBalancingPolicy,
            },
            core::v3::{transport_socket::ConfigType, Http2ProtocolOptions, UpstreamHttpProtocolOptions},
            endpoint::v3::{lb_endpoint::HostIdentifier, ClusterLoadAssignment, Endpoint, LbEndpoint, LocalityLbEndpoints},
            route::v3::Route as EnvoyRoute,
        },
        extensions::{
            load_balancing_policies::{
                override_host::v3::{override_host::OverrideHostSource, OverrideHost},
                round_robin::v3::RoundRobin,
            },
            transport_sockets::tls::v3::{tls_parameters::TlsProtocol, CommonTlsContext, TlsParameters, UpstreamTlsContext},
            upstreams::http::v3::http_protocol_options::{explicit_http_config::ProtocolConfig, ExplicitHttpConfig, UpstreamProtocolOptions},
        },
    },
    google::protobuf::UInt32Value,
};
use tracing::{debug, warn};

use crate::{
    backends::common::{converters, get_inference_pool_configurations, ClusterHolder, DurationConverter, InferenceClusterInfo, SocketAddressFactory},
    common::{
        self, Backend, BackendType, BackendTypeConfig, GRPCEffectiveRoutingRule, HTTPEffectiveRoutingRule, InferencePoolTypeConfig, Listener, ProtocolType, Route, RouteType, ServiceTypeConfig,
        TlsType, DEFAULT_ROUTE_HOSTNAME,
    },
    controllers::HostnameMatchFilter,
};

type ListenerNameToHostname = (String, Option<String>);

impl Listener {
    pub fn http_matching_rules(&self) -> Vec<&HTTPEffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes
            .iter()
            .chain(unresolved.iter())
            .filter_map(|r| match &r.config.route_type {
                RouteType::Http(configuration) => Some(configuration),
                RouteType::Grpc(_) => None,
            })
            .flat_map(|r| &r.effective_routing_rules)
            .collect();
        matching_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
        matching_rules
    }

    pub fn grpc_matching_rules(&self) -> Vec<&GRPCEffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes
            .iter()
            .chain(unresolved.iter())
            .filter_map(|r| match &r.config.route_type {
                RouteType::Http(_) => None,
                RouteType::Grpc(configuration) => Some(configuration),
            })
            .flat_map(|r| &r.effective_routing_rules)
            .collect();
        matching_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
        matching_rules
    }
}

#[derive(Debug, Clone)]
pub struct EnvoyVirtualHost {
    pub name: String,
    pub effective_hostnames: Vec<String>,
    pub resolved_routes: Vec<Route>,
    pub unresolved_routes: Vec<Route>,
    pub http_routes: Vec<EnvoyRoute>,
    pub grpc_routes: Vec<EnvoyRoute>,
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
    resources: BTreeMap<i32, EnvoyListener>,
    inference_clusters: Vec<InferenceClusterInfo>,
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(effective_gateway: &'a common::Gateway) -> Self {
        Self {
            effective_gateway,
            resources: BTreeMap::new(),
            inference_clusters: vec![],
        }
    }

    pub fn generate_envoy_listeners(&mut self) -> &BTreeMap<i32, EnvoyListener> {
        let listeners = self.generate_envoy_listener_mapping();
        self.resources = listeners.clone();
        &self.resources
    }

    pub fn generate_envoy_clusters(&self) -> Vec<EnvoyCluster> {
        let listeners = &self.resources;
        let grpc_protocol_options = envoy_api_rs::envoy::extensions::upstreams::http::v3::HttpProtocolOptions {
            upstream_protocol_options: Some(UpstreamProtocolOptions::ExplicitHttpConfig(ExplicitHttpConfig {
                protocol_config: Some(ProtocolConfig::Http2ProtocolOptions(Http2ProtocolOptions {
                    max_concurrent_streams: Some(UInt32Value { value: 10 }),
                    ..Default::default()
                })),
            })),
            ..Default::default()
        };

        let grpc_http_configuration = converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), &grpc_protocol_options));

        let clusters: BTreeSet<ClusterHolder> = listeners
            .values()
            .flat_map(|listener| {
                listener.http_listener_map.iter().flat_map(|evc| {
                    evc.resolved_routes.iter().chain(evc.unresolved_routes.iter()).flat_map(|r| {
                        let route_type = r.route_type();
                        let backends = r.backends();
                        let service_backends = backends
                            .iter()
                            .filter_map(|b| {
                                if let Backend::Resolved(BackendType::Service(backend_service_config) | BackendType::Invalid(backend_service_config)) = b {
                                    Some(backend_service_config)
                                } else {
                                    warn!("Filtering out backend not resolved {:?}", b);
                                    None
                                }
                            })
                            .filter(|b| b.weight() > 0)
                            .map(|r| create_service_cluster(r, route_type, &grpc_http_configuration));

                        let inference_backends = backends
                            .iter()
                            .filter_map(|b| {
                                if let Backend::Resolved(BackendType::InferencePool(backend_service_config)) = b {
                                    Some(backend_service_config)
                                } else {
                                    warn!("Filtering out backend not resolved {:?}", b);
                                    None
                                }
                            })
                            .filter(|b| b.weight > 0)
                            .map(|r| create_inference_cluster(r, route_type));

                        service_backends.chain(inference_backends).collect::<Vec<_>>()
                    })
                })
            })
            .collect();
        warn!("Clusters produced {:?}", clusters);
        let ext_service_clusters = self.inference_clusters.iter().filter_map(generate_ext_service_cluster);

        warn!("Ext Service Clusters produced {:?}", ext_service_clusters);
        let clusters = clusters.into_iter().chain(ext_service_clusters);
        clusters.map(|c| c.cluster).collect::<Vec<_>>()
    }

    fn generate_envoy_listener_mapping(&mut self) -> BTreeMap<i32, EnvoyListener> {
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
                        let mut new_listener = self.generate_envoy_listener(gateway_name, listener);
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
                        let envoy_listener = self.generate_envoy_listener(gateway_name, listener);
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

    fn generate_envoy_listener(&mut self, gateway_name: String, listener: &Listener) -> EnvoyListener {
        let (resolved, unresolved) = listener.routes();
        let resolved: Vec<_> = resolved.into_iter().collect();

        let mut listener_map = BTreeSet::new();
        let potential_hostnames = Self::calculate_potential_hostnames(&resolved, listener.hostname().cloned());
        debug!("generate_virtual_hosts Potential hostnames {potential_hostnames:?}");
        for potential_hostname in potential_hostnames {
            let http_matching_rules = listener
                .http_matching_rules()
                .into_iter()
                .filter(|&em| {
                    let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                    debug!("generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                    filtered
                })
                .cloned()
                .collect::<Vec<_>>();

            let grpc_matching_rules = listener
                .grpc_matching_rules()
                .into_iter()
                .filter(|&em| {
                    let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                    debug!("generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                    filtered
                })
                .cloned()
                .collect::<Vec<_>>();

            self.inference_clusters = self
                .inference_clusters
                .clone()
                .into_iter()
                .chain(http_matching_rules.iter().filter_map(get_inference_pool_configurations))
                .collect();

            listener_map.insert(EnvoyVirtualHost {
                http_routes: http_matching_rules.clone().into_iter().map(EnvoyRoute::from).collect(),
                grpc_routes: grpc_matching_rules.clone().into_iter().map(EnvoyRoute::from).collect(),
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
        calculate_hostnames_common(routes, listener_hostname, |h| vec![h])
    }

    fn calculate_effective_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
        calculate_hostnames_common(routes, listener_hostname, |h| vec![format!("{h}:*"), h])
    }
}

pub fn calculate_hostnames_common(routes: &[&Route], listener_hostname: Option<String>, create_hostnames: impl Fn(String) -> Vec<String>) -> Vec<String> {
    let routes_hostnames = routes.iter().fold(BTreeSet::new(), |mut acc, r| {
        acc.append(&mut r.hostnames().iter().cloned().collect::<BTreeSet<_>>());
        acc
    });

    match (listener_hostname, routes_hostnames.is_empty()) {
        (None, false) => Vec::from_iter(routes_hostnames),
        (Some(hostname), _) if !hostname.is_empty() && hostname != DEFAULT_ROUTE_HOSTNAME => create_hostnames(hostname),
        (None, true) | (Some(_), _) => vec![DEFAULT_ROUTE_HOSTNAME.to_owned()],
    }
}

fn create_service_cluster(config: &ServiceTypeConfig, route_type: &RouteType, grpc_http_configuration: &envoy_api_rs::google::protobuf::Any) -> ClusterHolder {
    ClusterHolder {
        name: config.cluster_name(),
        cluster: EnvoyCluster {
            name: config.cluster_name(),
            cluster_discovery_type: Some(ClusterDiscoveryType::Type(DiscoveryType::StrictDns.into())),
            lb_policy: LbPolicy::RoundRobin.into(),
            connect_timeout: Some(DurationConverter::from(std::time::Duration::from_millis(250))),
            load_assignment: Some(ClusterLoadAssignment {
                cluster_name: config.cluster_name(),
                endpoints: vec![LocalityLbEndpoints {
                    lb_endpoints: vec![LbEndpoint {
                        host_identifier: Some(HostIdentifier::Endpoint(Endpoint {
                            address: Some(SocketAddressFactory::from_backend(config)),
                            ..Default::default()
                        })),
                        load_balancing_weight: Some(UInt32Value {
                            value: config.weight().try_into().expect("For time being we expect this to work"),
                        }),

                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            typed_extension_protocol_options: match route_type {
                common::RouteType::Http(_) => HashMap::new(),
                common::RouteType::Grpc(_) => vec![("envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), grpc_http_configuration.clone())]
                    .into_iter()
                    .collect(),
            },

            ..Default::default()
        },
    }
}

fn generate_ext_service_cluster(config: &InferenceClusterInfo) -> Option<ClusterHolder> {
    let Some(extension_config) = &config.config.inference_config else { return None };

    let address_port = (
        extension_config.extension_ref().name.clone() + "." + &config.config.resource_key.namespace,
        extension_config.extension_ref().port_number.unwrap_or(9002),
    );
    let cluster_name = config.cluster_name().to_owned();

    let grpc_protocol_options = envoy_api_rs::envoy::extensions::upstreams::http::v3::HttpProtocolOptions {
        common_http_protocol_options: Some(envoy_api_rs::envoy::config::core::v3::HttpProtocolOptions {
            idle_timeout: Some(DurationConverter::from(Duration::from_secs(1))),
            ..Default::default()
        }),
        upstream_http_protocol_options: Some(UpstreamHttpProtocolOptions { auto_sni: true, ..Default::default() }),
        upstream_protocol_options: Some(UpstreamProtocolOptions::ExplicitHttpConfig(ExplicitHttpConfig {
            protocol_config: Some(ProtocolConfig::Http2ProtocolOptions(Http2ProtocolOptions {
                max_concurrent_streams: Some(UInt32Value { value: 10 }),
                ..Default::default()
            })),
        })),
        ..Default::default()
    };

    let grpc_http_configuration = converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), &grpc_protocol_options));

    let upstream_tls_context = UpstreamTlsContext {
        common_tls_context: Some(CommonTlsContext {
            tls_params: Some(TlsParameters {
                tls_minimum_protocol_version: TlsProtocol::TlSv13.into(),
                tls_maximum_protocol_version: TlsProtocol::TlSv13.into(),
                ..Default::default()
            }),
            alpn_protocols: vec!["h2".to_owned()],
            ..Default::default()
        }),
        ..Default::default()
    };

    let transport_configuration = envoy_api_rs::envoy::config::core::v3::TransportSocket {
        name: cluster_name.clone() + "_tls",
        config_type: Some(ConfigType::TypedConfig(converters::AnyTypeConverter::from((
            "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext".to_owned(),
            &upstream_tls_context,
        )))),
    };

    Some(ClusterHolder {
        name: config.cluster_name().to_owned(),
        cluster: EnvoyCluster {
            name: cluster_name.clone(),
            cluster_discovery_type: Some(ClusterDiscoveryType::Type(DiscoveryType::LogicalDns.into())),
            lb_policy: LbPolicy::RoundRobin.into(),
            connect_timeout: Some(DurationConverter::from(std::time::Duration::from_millis(250))),
            load_assignment: Some(ClusterLoadAssignment {
                cluster_name: cluster_name.clone(),
                endpoints: vec![LocalityLbEndpoints {
                    lb_endpoints: vec![LbEndpoint {
                        host_identifier: Some(HostIdentifier::Endpoint(Endpoint {
                            address: Some(SocketAddressFactory::from_address_port(address_port)),
                            ..Default::default()
                        })),
                        load_balancing_weight: Some(UInt32Value { value: 1 }),

                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            transport_socket: Some(transport_configuration),
            typed_extension_protocol_options: vec![("envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), grpc_http_configuration.clone())]
                .into_iter()
                .collect(),

            ..Default::default()
        },
    })
}

fn create_inference_cluster(config: &InferencePoolTypeConfig, _route_type: &RouteType) -> ClusterHolder {
    let fallback_policy = envoy_api_rs::envoy::config::cluster::v3::load_balancing_policy::Policy {
        typed_extension_config: Some(envoy_api_rs::envoy::config::core::v3::TypedExtensionConfig {
            name: "envoy.load_balancing_policies.round_robing".to_owned(),
            typed_config: Some(converters::AnyTypeConverter::from((
                "type.googleapis.com/envoy.extensions.load_balancing_policies.round_robin.v3.RoundRobin".to_owned(),
                &RoundRobin::default(),
            ))),
        }),
    };

    let override_host = OverrideHost {
        override_host_sources: vec![OverrideHostSource {
            header: "x-gateway-destination-endpoint".to_owned(),
            metadata: None,
        }],
        fallback_policy: Some(LoadBalancingPolicy { policies: vec![fallback_policy] }),
    };

    let lb_endpoints: Vec<_> = config
        .endpoints
        .as_ref()
        .map(|endpoints| {
            endpoints
                .iter()
                .map(|e| (e.clone(), config.effective_port))
                .map(|addr| LbEndpoint {
                    host_identifier: Some(HostIdentifier::Endpoint(Endpoint {
                        address: Some(SocketAddressFactory::from_address_port(addr)),
                        ..Default::default()
                    })),
                    load_balancing_weight: Some(UInt32Value { value: 1 }),

                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();

    ClusterHolder {
        name: config.cluster_name(),
        cluster: EnvoyCluster {
            name: config.cluster_name(),
            connect_timeout: Some(DurationConverter::from(std::time::Duration::from_millis(250))),
            load_balancing_policy: Some(LoadBalancingPolicy {
                policies: vec![envoy_api_rs::envoy::config::cluster::v3::load_balancing_policy::Policy {
                    typed_extension_config: Some(envoy_api_rs::envoy::config::core::v3::TypedExtensionConfig {
                        name: "envoy.load_balancing_policies.override_host".to_owned(),
                        typed_config: Some(converters::AnyTypeConverter::from((
                            "type.googleapis.com/envoy.extensions.load_balancing_policies.override_host.v3.OverrideHost".to_owned(),
                            &override_host,
                        ))),
                    }),
                }],
            }),
            load_assignment: Some(ClusterLoadAssignment {
                cluster_name: config.cluster_name(),
                endpoints: vec![LocalityLbEndpoints { lb_endpoints, ..Default::default() }],
                ..Default::default()
            }),

            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_effective_hostnames() {
        let routes = vec![];
        let hostname = Some("*".to_owned());
        let hostnames = ResourceGenerator::calculate_effective_hostnames(&routes, hostname);
        assert_eq!(hostnames, vec!["*".to_owned()]);
        let hostname = Some("host.blah".to_owned());
        let hostnames: BTreeSet<String> = ResourceGenerator::calculate_effective_hostnames(&routes, hostname).into_iter().collect();
        assert_eq!(hostnames, vec!["host.blah".to_owned(), "host.blah:*".to_owned()].into_iter().collect::<BTreeSet<_>>());
        let hostname = Some("host.blah".to_owned());
        let hostnames = calculate_hostnames_common(&routes, hostname, |h| vec![format!("{h}:*"), h]).into_iter().collect::<BTreeSet<_>>();
        assert_eq!(hostnames, vec!["host.blah".to_owned(), "host.blah:*".to_owned()].into_iter().collect::<BTreeSet<_>>());
    }
}
