use std::{
    cmp,
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use envoy_api_rs::{
    envoy::{
        config::{
            cluster::v3::{
                Cluster as EnvoyCluster, LoadBalancingPolicy,
                cluster::{ClusterDiscoveryType, DiscoveryType, LbPolicy},
            },
            core::v3::{Http2ProtocolOptions, UpstreamHttpProtocolOptions, transport_socket::ConfigType},
            endpoint::v3::{ClusterLoadAssignment, Endpoint, LbEndpoint, LocalityLbEndpoints, lb_endpoint::HostIdentifier},
            route::v3::Route as EnvoyRoute,
        },
        extensions::{
            load_balancing_policies::{
                override_host::v3::{OverrideHost, override_host::OverrideHostSource},
                round_robin::v3::RoundRobin,
            },
            transport_sockets::tls::v3::{CommonTlsContext, TlsParameters, UpstreamTlsContext, tls_parameters::TlsProtocol},
            upstreams::http::v3::http_protocol_options::{
                ExplicitHttpConfig, UpstreamProtocolOptions, explicit_http_config::ProtocolConfig,
            },
        },
    },
    google::protobuf::UInt32Value,
};
use gateway_api::{
    common::HTTPFilterType,
    grpcroutes::GrpcRouteMatch,
    httproutes::{HttpRouteRulesMatchesPathType, PathMatch, RouteMatch},
};
use kubvernor_common::GatewayImplementationType;
use tracing::{debug, error};

use crate::{
    backends::envoy::common::{
        ClusterHolder, DurationConverter, InferenceClusterInfo, SocketAddressFactory, converters, enable_ect_proc_filter,
        get_inference_pool_configurations,
        route::{GRPCEffectiveRoutingRule, HTTPEffectiveRoutingRule},
    },
    common::{
        self, Backend, BackendType, BackendTypeConfig, DEFAULT_ROUTE_HOSTNAME, FilterHeaders, GRPCRoutingConfiguration, GRPCRoutingRule,
        HTTPRoutingConfiguration, HTTPRoutingRule, InferencePoolTypeConfig, Listener, ProtocolType, Route, RouteType, ServiceTypeConfig,
        TlsType,
    },
    controllers::HostnameMatchFilter,
};

type ListenerNameToHostname = (String, Option<String>);

fn get_http_default_rules_matches() -> RouteMatch {
    RouteMatch {
        headers: Some(vec![]),
        method: None,
        path: Some(PathMatch { r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix), value: Some("/".to_owned()) }),
        query_params: None,
    }
}

fn get_grpc_default_rules_matches() -> GrpcRouteMatch {
    GrpcRouteMatch { headers: Some(vec![]), method: None }
}

impl Listener {
    fn create_http_effective_route(
        &self,
        hostnames: &[String],
        routing_configuration: &HTTPRoutingConfiguration,
    ) -> Vec<HTTPEffectiveRoutingRule> {
        routing_configuration
            .routing_rules
            .iter()
            .flat_map(|rr: &HTTPRoutingRule| {
                let mut matching_rules = rr.matching_rules.clone();
                if matching_rules.is_empty() {
                    matching_rules.push(get_http_default_rules_matches());
                }

                matching_rules.into_iter().map(|matcher| HTTPEffectiveRoutingRule {
                    listener_port: self.port(),
                    route_matcher: matcher.clone(),
                    backends: rr.backends.clone(),
                    filter_backends: rr.filter_backends.clone(),
                    name: rr.name.clone(),
                    hostnames: hostnames.to_vec(),
                    request_headers: rr.filter_headers(&HTTPFilterType::RequestHeaderModifier),
                    response_headers: rr.filter_headers(&HTTPFilterType::ResponseHeaderModifier),
                    rewrite_url_filter: rr
                        .filters
                        .iter()
                        .find_map(|f| if f.r#type == HTTPFilterType::UrlRewrite { f.url_rewrite.clone() } else { None }),
                    redirect_filter: rr
                        .filters
                        .iter()
                        .find_map(|f| if f.r#type == HTTPFilterType::RequestRedirect { f.request_redirect.clone() } else { None }),
                    mirror_filter: rr
                        .filters
                        .iter()
                        .find_map(|f| if f.r#type == HTTPFilterType::RequestMirror { f.request_mirror.clone() } else { None }),
                })
            })
            .collect()
    }

    fn create_grpc_effective_route(
        hostnames: &[String],
        routing_configuration: &GRPCRoutingConfiguration,
    ) -> Vec<GRPCEffectiveRoutingRule> {
        routing_configuration
            .routing_rules
            .iter()
            .flat_map(|rr: &GRPCRoutingRule| {
                let mut matching_rules = rr.matching_rules.clone();
                if matching_rules.is_empty() {
                    matching_rules.push(get_grpc_default_rules_matches());
                }

                matching_rules.into_iter().map(|matcher| GRPCEffectiveRoutingRule {
                    route_matcher: matcher.clone(),
                    backends: rr.backends.clone(),
                    name: rr.name.clone(),
                    hostnames: hostnames.to_vec(),
                    request_headers: rr.filter_headers(),
                    response_headers: FilterHeaders::default(),
                })
            })
            .collect()
    }

    pub fn http_matching_rules(&self) -> Vec<HTTPEffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes
            .iter()
            .chain(unresolved.iter())
            .filter_map(|r| match &r.config.route_type {
                RouteType::Http(configuration) => Some((&r.config.hostnames, configuration)),
                RouteType::Grpc(_) => None,
            })
            .flat_map(|(hostnames, config)| self.create_http_effective_route(hostnames, config))
            .collect();
        matching_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
        matching_rules
    }

    pub fn grpc_matching_rules(&self) -> Vec<GRPCEffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes
            .iter()
            .chain(unresolved.iter())
            .filter_map(|r| match &r.config.route_type {
                RouteType::Http(_) => None,
                RouteType::Grpc(configuration) => Some((&r.config.hostnames, configuration)),
            })
            .flat_map(|(hostnames, config)| Self::create_grpc_effective_route(hostnames, config))
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
        Some(self.cmp(other))
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
    pub enable_ext_proc: bool,
}

pub type HostnameCalculator = Box<dyn Fn(&str) -> Vec<String>>;

pub struct ResourceGenerator<'a> {
    effective_gateway: &'a common::Gateway,
    resources: BTreeMap<i32, EnvoyListener>,
    inference_clusters: Vec<InferenceClusterInfo>,
    gateway_implementation_type: GatewayImplementationType,
    effective_hostname_calculator: HostnameCalculator,
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(
        effective_gateway: &'a common::Gateway,
        gateway_implementation_type: GatewayImplementationType,
        effective_hostname_calculator: HostnameCalculator,
    ) -> Self {
        Self {
            effective_gateway,
            resources: BTreeMap::new(),
            inference_clusters: vec![],
            gateway_implementation_type,
            effective_hostname_calculator,
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

        let grpc_http_configuration = converters::AnyTypeConverter::from((
            "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(),
            &grpc_protocol_options,
        ));

        let clusters: BTreeSet<ClusterHolder> = listeners
            .values()
            .flat_map(|listener| {
                listener.http_listener_map.iter().flat_map(|evc| {
                    evc.resolved_routes.iter().chain(evc.unresolved_routes.iter()).flat_map(|r| {
                        debug!("Cluster  {} {}", r.backends().len(), r.filter_backends().len());
                        let route_type = r.route_type();
                        let backends = r.backends();
                        let service_backends = r.backends();
                        let filter_backends = r.filter_backends();
                        let service_backends = service_backends
                            .into_iter()
                            .filter_map(|b| {
                                if let Backend::Resolved(
                                    BackendType::Service(backend_service_config) | BackendType::Invalid(backend_service_config),
                                ) = b
                                {
                                    Some(backend_service_config)
                                } else {
                                    None
                                }
                            })
                            .filter(|b| b.weight() > 0)
                            .map(|r| create_service_cluster(r, route_type, &grpc_http_configuration));

                        let filter_backends = filter_backends
                            .into_iter()
                            .filter_map(|b| {
                                if let Backend::Resolved(
                                    BackendType::Service(backend_service_config) | BackendType::Invalid(backend_service_config),
                                ) = b
                                {
                                    Some(backend_service_config)
                                } else {
                                    debug!("Filter backend not resolved {:?}", b);
                                    None
                                }
                            })
                            .filter(|b| b.weight() > 0)
                            .map(|r| create_service_cluster(r, route_type, &grpc_http_configuration));

                        let inference_backends = backends
                            .into_iter()
                            .filter_map(|b| {
                                if let Backend::Resolved(BackendType::InferencePool(backend_service_config)) = b {
                                    Some(backend_service_config)
                                } else {
                                    None
                                }
                            })
                            .filter(|b| b.weight > 0)
                            .map(|r| create_inference_cluster(r, route_type));

                        service_backends.chain(inference_backends).chain(filter_backends).collect::<Vec<_>>()
                    })
                })
            })
            .collect();
        debug!("Clusters produced {} {:?}", clusters.len(), clusters);
        let ext_service_clusters = self.inference_clusters.iter().filter_map(|c| self.generate_ext_service_cluster(c));
        debug!("Ext Service Clusters produced {:?}", ext_service_clusters);
        let clusters = clusters.into_iter().chain(ext_service_clusters);
        clusters.map(|c| c.cluster).collect::<Vec<_>>()
    }

    fn generate_envoy_listener_mapping(&mut self) -> BTreeMap<i32, EnvoyListener> {
        let gateway = self.effective_gateway;
        gateway.listeners().fold(BTreeMap::<i32, EnvoyListener>::new(), |mut acc, listener| {
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
                    },
                    ProtocolType::Tcp => {
                        envoy_listener.tcp_listener_map.insert((listener_name, listener_hostname));
                    },
                    _ => (),
                }
            } else {
                match protocol_type {
                    ProtocolType::Http | ProtocolType::Https => {
                        let envoy_listener = self.generate_envoy_listener(gateway_name, listener);
                        acc.insert(port, envoy_listener);
                    },
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
                                enable_ext_proc: false,
                            },
                        );
                    },
                    _ => (),
                }
            }

            acc
        })
    }

    fn generate_envoy_listener(&mut self, gateway_name: String, listener: &Listener) -> EnvoyListener {
        let (resolved, unresolved) = listener.routes();
        let resolved: Vec<_> = resolved.into_iter().collect();

        let mut listener_map = BTreeSet::new();
        let potential_hostnames = Self::calculate_potential_hostnames(&resolved, listener.hostname().cloned());
        debug!("generate_virtual_hosts Potential hostnames {potential_hostnames:?}");
        let mut enable_ext_proc = false;
        for potential_hostname in potential_hostnames {
            let http_matching_rules = listener
                .http_matching_rules()
                .into_iter()
                .filter(|em| {
                    let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                    debug!("generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                    filtered
                })
                .collect::<Vec<_>>();

            let grpc_matching_rules = listener
                .grpc_matching_rules()
                .into_iter()
                .filter(|em| {
                    let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                    debug!("generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                    filtered
                })
                .collect::<Vec<_>>();

            self.inference_clusters = self
                .inference_clusters
                .clone()
                .into_iter()
                .chain(http_matching_rules.iter().filter_map(get_inference_pool_configurations))
                .collect();

            enable_ext_proc |= http_matching_rules.iter().any(enable_ect_proc_filter);

            listener_map.insert(EnvoyVirtualHost {
                http_routes: http_matching_rules.clone().into_iter().map(EnvoyRoute::from).collect(),
                grpc_routes: grpc_matching_rules.clone().into_iter().map(EnvoyRoute::from).collect(),
                name: listener.name().to_owned() + "-" + &potential_hostname,
                effective_hostnames: calculate_hostnames_common(&resolved, Some(potential_hostname), &self.effective_hostname_calculator),
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
            enable_ext_proc,
        }
    }

    fn generate_ext_service_cluster(&self, config: &InferenceClusterInfo) -> Option<ClusterHolder> {
        let Some(extension_config) = &config.config.inference_config else {
            return None;
        };

        let address_port = (
            extension_config.extension_ref().name.clone() + "." + &config.config.resource_key.namespace,
            extension_config.extension_ref().port.as_ref().map_or_else(|| 9002, |f| f.number),
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

        let grpc_http_configuration = converters::AnyTypeConverter::from((
            "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(),
            &grpc_protocol_options,
        ));

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
                cluster_discovery_type: match self.gateway_implementation_type {
                    GatewayImplementationType::Envoy => Some(ClusterDiscoveryType::Type(DiscoveryType::LogicalDns.into())),
                    GatewayImplementationType::Agentgateway => {
                        error!("Wrong type for wrong implementation type");
                        None
                    },
                    GatewayImplementationType::Orion => Some(ClusterDiscoveryType::Type(DiscoveryType::StrictDns.into())),
                },
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
                typed_extension_protocol_options: vec![(
                    "envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(),
                    grpc_http_configuration.clone(),
                )]
                .into_iter()
                .collect(),

                ..Default::default()
            },
        })
    }

    fn calculate_potential_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
        calculate_hostnames_common(routes, listener_hostname, |h| vec![h.to_owned()])
    }
}

pub fn calculate_hostnames_common(
    routes: &[&Route],
    listener_hostname: Option<String>,
    create_hostnames: impl Fn(&str) -> Vec<String>,
) -> Vec<String> {
    let routes_hostnames = routes.iter().fold(BTreeSet::new(), |mut acc, r| {
        acc.append(&mut r.hostnames().iter().cloned().collect::<BTreeSet<_>>());
        acc
    });

    match (listener_hostname, routes_hostnames.is_empty()) {
        (None, false) => Vec::from_iter(routes_hostnames),
        (Some(hostname), _) if !hostname.is_empty() && hostname != DEFAULT_ROUTE_HOSTNAME => create_hostnames(&hostname),
        (None, true) | (Some(_), _) => vec![DEFAULT_ROUTE_HOSTNAME.to_owned()],
    }
}

fn create_service_cluster(
    config: &ServiceTypeConfig,
    route_type: &RouteType,
    grpc_http_configuration: &envoy_api_rs::google::protobuf::Any,
) -> ClusterHolder {
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
                common::RouteType::Grpc(_) => {
                    vec![("envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), grpc_http_configuration.clone())]
                        .into_iter()
                        .collect()
                },
            },

            ..Default::default()
        },
    }
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
        override_host_sources: vec![OverrideHostSource { header: "x-gateway-destination-endpoint".to_owned(), metadata: None }],
        fallback_policy: Some(LoadBalancingPolicy { policies: vec![fallback_policy] }),
    };

    let lb_endpoints: Vec<_> = config
        .endpoints
        .as_ref()
        .map(|endpoints| {
            endpoints
                .iter()
                .map(|e| (e.clone(), config.target_ports[0]))
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
