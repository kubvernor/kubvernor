use agentgateway_api_rs::agentgateway::dev::resource::Header;
use agentgateway_api_rs::agentgateway::dev::resource::request_redirect::Path;
use agentgateway_api_rs::agentgateway::dev::resource::traffic_policy_spec::HostRewrite;
use agentgateway_api_rs::{
    agentgateway::dev::resource::{
        self, BackendPolicySpec, BackendReference, Header, Listener, Policy, PolicyTarget, ResourceName, Route, RouteBackend, RouteName,
        StaticBackend, TlsConfig, TrafficPolicySpec, TypedResourceName,
        backend::{self},
        backend_policy_spec, bind, policy,
        policy_target::ServiceTarget,
        request_mirrors,
        request_redirect::Path,
        traffic_policy_spec::{self, HostRewrite},
    },
    istio::workload::{LoadBalancing, NetworkAddress, Port, Service},
};
use gateway_api::{
    common::{HTTPFilterType, HTTPHeader},
    httproutes::HttpRouteFilter,
};
use gateway_api_inference_extension::inferencepools::InferencePoolEndpointPickerRefFailureMode;
use kubvernor_common::ResourceKey;
use tracing::{debug, info, warn};

use crate::{
    backends::agentgateway::SecureListenerWrapper,
    common::{self, Backend, BackendType, DEFAULT_NAMESPACE_NAME, HTTPRoutingRule, InferencePoolTypeConfig, KeyData, ProtocolType},
};

pub(crate) struct ResourceGenerator<'a> {
    effective_gateway: &'a common::Gateway,
}

impl From<ProtocolType> for i32 {
    fn from(value: ProtocolType) -> Self {
        match value {
            ProtocolType::Unknown => 0,
            ProtocolType::Http => 1,
            ProtocolType::Https => 2,
            ProtocolType::Tls => 3,
            ProtocolType::Tcp => 4,
            ProtocolType::Udp => 6,
        }
    }
}

fn create_bind_name(port: i32, protocol: i32) -> String {
    format!("bind-port-{port}-protocol-{protocol}")
}

#[derive(Debug, Clone, Ord, Eq, PartialEq, PartialOrd, Default)]
// created because xds bind is not Ord
pub(crate) struct Bind {
    pub key: String,
    pub port: u32,
    pub protocol: bind::Protocol,
    pub tunnel_protocol: bind::TunnelProtocol,
}

impl From<KeyData> for TlsConfig {
    fn from(value: KeyData) -> Self {
        TlsConfig { cert: value.cert, private_key: value.private_key, root: value.root }
    }
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(effective_gateway: &'a common::Gateway) -> Self {
        Self { effective_gateway }
    }

    pub fn generate_bindings_and_listeners(&self) -> BTreeMap<Bind, Vec<SecureListenerWrapper>> {
        let gateway = self.effective_gateway;
        gateway.listeners().fold(BTreeMap::<Bind, Vec<SecureListenerWrapper>>::new(), |mut acc, listener| {
            let port = listener.port();
            let listener_name = listener.name().to_owned();
            let gateway_name = gateway.name().to_owned();
            let gateway_namespace = gateway.namespace().to_owned();

            let binding_protocol = match listener.protocol() {
                ProtocolType::Http | ProtocolType::Unknown | ProtocolType::Udp => bind::Protocol::Http,
                ProtocolType::Https | ProtocolType::Tls => bind::Protocol::Tls,
                ProtocolType::Tcp => bind::Protocol::Tcp,
            };

            let bind_key = create_bind_name(port, binding_protocol.into());

            let bind = Bind {
                key: bind_key.clone(),
                port: port as u32,
                protocol: binding_protocol,
                tunnel_protocol: bind::TunnelProtocol::Direct,
            };
            let maybe_added = acc.get_mut(&bind);

            let agentgateway_listener = Listener {
                key: listener_name.clone(),
                name: Some(resource::ListenerName { gateway_name, gateway_namespace, listener_name, listener_set: None }),
                bind_key: bind_key.clone(),
                hostname: listener.hostname().cloned().unwrap_or("*".to_owned()),
                protocol: listener.protocol().into(),
                tls: match listener.protocol() {
                    ProtocolType::Https | ProtocolType::Tls => listener.config().tls_type.as_ref().and_then(|tls_type| match tls_type {
                        common::TlsType::Terminate(certificates) => {
                            let valid_cert: Vec<TlsConfig> = certificates
                                .iter()
                                .filter_map(|c| {
                                    if let common::Certificate::ResolvedSameSpace(_, data) = c {
                                        Some(TlsConfig::from(data.clone()))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            valid_cert.first().cloned()
                        },
                        common::TlsType::Passthrough => None,
                    }),

                    _ => None,
                },
            };
            let listener = SecureListenerWrapper(agentgateway_listener);
            info!("Generating agentgateway listener {:#?}", listener);

            if let Some(listners) = maybe_added {
                listners.push(listener);
            } else {
                acc.insert(bind, vec![listener]);
            }
            acc
        })
    }

    pub fn generate_routes_and_backends_and_policies(
        &self,
    ) -> (Vec<resource::Route>, Vec<resource::Backend>, Vec<resource::Policy>, Vec<Service>) {
        let mut backends = BTreeMap::<String, resource::Backend>::new();
        let mut backend_services = BTreeMap::<String, Service>::new();
        let mut backend_policies = BTreeMap::<String, Policy>::new();
        let gateway = self.effective_gateway;

        let routes = gateway
            .listeners()
            .flat_map(|l| {
                let listener_port = l.port() as u32;
                let (resolved, unresolved) = l.routes();
                let routes = resolved.into_iter().chain(unresolved);
                routes
                    .filter_map(|route| match route.route_type() {
                        common::RouteType::Http(configuration) => Some((route, &configuration.routing_rules)),
                        common::RouteType::Grpc(_) => None,
                    })
                    .flat_map(|(route, routing_rules)| {
                        routing_rules
                            .iter()
                            .map(|routing_rule| {
                                let inference_extension_configurations: Vec<(String, ResourceKey, &InferencePoolTypeConfig)> =
                                    routing_rules
                                        .iter()
                                        .flat_map(|r| {
                                            r.backends
                                                .iter()
                                                .filter_map(|b| match b.backend_type() {
                                                    crate::common::BackendType::InferencePool(inference_type_config) => Some((
                                                        format!("Policy-{}-{}", routing_rule.name, b.resource_key()),
                                                        b.resource_key(),
                                                        inference_type_config,
                                                    )),
                                                    _ => None,
                                                })
                                                .collect::<Vec<_>>()
                                        })
                                        .collect();
                                if inference_extension_configurations.len() > 1 {
                                    warn!("Multiple external processing filter configuration per route {:?} ", route);
                                }

                                let (policies, epp_backends): (Vec<_>, Vec<_>) = inference_extension_configurations
                                    .into_iter()
                                    .map(|(name, backend, conf)| {
                                        let (policy, backend) = create_inference_policies(&name, &backend, conf);

                                        ((name.clone(), policy), backend)
                                    })
                                    .unzip();
                                info!("(Inference policies routing rule {}  {backend_policies:#?}", routing_rule.name);

                                backend_policies.extend(policies);

                                backend_services.extend(&mut routing_rule.backends.iter().filter_map(create_backend_services));
                                backends.extend(
                                    &mut routing_rule
                                        .backends
                                        .iter()
                                        .chain(&routing_rule.filter_backends)
                                        .flat_map(create_backends)
                                        .flatten(),
                                );
                                let epp_backends = epp_backends.into_iter().flatten().collect::<Vec<_>>();
                                backends.extend(epp_backends);

                                let mut traffic_policies: Vec<_> = routing_rule
                                    .filters
                                    .iter()
                                    .flat_map(|f| map_filters(f, listener_port))
                                    .chain(vec![TrafficPolicySpec {
                                        kind: Some(traffic_policy_spec::Kind::HostRewrite(HostRewrite {
                                            mode: traffic_policy_spec::host_rewrite::Mode::None.into(),
                                        })),
                                        ..Default::default()
                                    }])
                                    .collect();

                                if let Some(mirror_policy) = create_request_mirror_traffic_policy(routing_rule) {
                                    traffic_policies.push(mirror_policy);
                                }

                                debug!("Filter backends are {:?}", routing_rule.filter_backends);

                                Route {
                                    name: Some(RouteName {
                                        kind: match route.route_type() {
                                            common::RouteType::Http(_) => "HTTP".to_owned(),
                                            common::RouteType::Grpc(_) => "GRPC".to_owned(),
                                        },
                                        name: routing_rule.name.clone(),
                                        namespace: route.namespace().to_owned(),
                                        rule_name: Some(routing_rule.name.clone()),
                                    }),
                                    key: route.name().to_owned() + "-" + &routing_rule.name,
                                    listener_key: l.name().to_owned(),
                                    hostnames: route.hostnames().to_vec().into_iter().filter(|f| f != "*").collect(),
                                    matches: routing_rule.matching_rules.iter().map(convert_route_match).collect(),
                                    backends: routing_rule.backends.iter().filter_map(create_route_backend).collect(),
                                    traffic_policies,
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        (
            routes,
            backends.into_values().collect::<Vec<resource::Backend>>(),
            backend_policies.into_values().collect::<Vec<resource::Policy>>(),
            backend_services.into_values().collect::<Vec<Service>>(),
        )
    }
}

fn map_filters(filter: &HttpRouteFilter, listener_port: u32) -> Vec<TrafficPolicySpec> {
    [
        map_redirect_filter(filter, listener_port),
        map_request_header_modifier_filter(filter),
        map_response_header_modifier_filter(filter),
        map_url_rewrite_filter(filter),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[allow(clippy::cast_possible_truncation)]
fn map_url_rewrite_filter(filter: &HttpRouteFilter) -> Option<TrafficPolicySpec> {
    filter.url_rewrite.as_ref().map(|url_rewrite| TrafficPolicySpec {
        kind: Some(traffic_policy_spec::Kind::UrlRewrite(resource::UrlRewrite {
            host: url_rewrite.hostname.clone().unwrap_or_default(),
            path: match url_rewrite.path.as_ref() {
                Some(path) => match path.r#type {
                    gateway_api::common::RequestOperationType::ReplaceFullPath => {
                        Some(resource::url_rewrite::Path::Full(path.replace_full_path.clone().unwrap_or_default()))
                    },
                    gateway_api::common::RequestOperationType::ReplacePrefixMatch => {
                        Some(resource::url_rewrite::Path::Prefix(path.replace_prefix_match.clone().unwrap_or_default()))
                    },
                },
                None => None,
            },
        })),
        ..Default::default()
    })
}

#[allow(clippy::cast_possible_truncation)]
fn map_redirect_filter(filter: &HttpRouteFilter, listener_port: u32) -> Option<TrafficPolicySpec> {
    filter.request_redirect.as_ref().map(|redirect_filter| TrafficPolicySpec {
        kind: Some(traffic_policy_spec::Kind::RequestRedirect(resource::RequestRedirect {
            scheme: redirect_filter
                .scheme
                .as_ref()
                .map(|s| match s {
                    gateway_api::common::RequestRedirectScheme::Http => "HTTP".to_owned(),
                    gateway_api::common::RequestRedirectScheme::Https => "HTTPS".to_owned(),
                })
                .unwrap_or_default(),
            host: redirect_filter.hostname.clone().unwrap_or_default(),
            port: redirect_filter.port.map(|p| p as u32).map_or_else(
                || match redirect_filter.scheme {
                    Some(gateway_api::common::RequestRedirectScheme::Http) => 80,
                    Some(gateway_api::common::RequestRedirectScheme::Https) => 443,
                    None => listener_port,
                },
                |p| p,
            ),
            status: redirect_filter.status_code.unwrap_or(302) as u32,
            path: redirect_filter.path.as_ref().map(|path| match path.r#type {
                gateway_api::common::RequestOperationType::ReplaceFullPath => {
                    Path::Full(path.replace_full_path.clone().unwrap_or_default())
                },
                gateway_api::common::RequestOperationType::ReplacePrefixMatch => {
                    Path::Prefix(path.replace_prefix_match.clone().unwrap_or_default())
                },
            }),
        })),
        ..Default::default()
    })
}

fn map_request_header_modifier_filter(filter: &HttpRouteFilter) -> Option<TrafficPolicySpec> {
    filter.request_header_modifier.as_ref().map(|filter| TrafficPolicySpec {
        kind: Some(traffic_policy_spec::Kind::RequestHeaderModifier(resource::HeaderModifier {
            add: filter.add.clone().map(map_headers).unwrap_or_default(),
            set: filter.set.clone().map(map_headers).unwrap_or_default(),
            remove: filter.remove.clone().unwrap_or_default(),
        })),
        ..Default::default()
    })
}

fn create_request_mirror_traffic_policy(routing_rule: &HTTPRoutingRule) -> Option<TrafficPolicySpec> {
    let mirrors: Vec<_> = routing_rule
        .filters
        .iter()
        .filter_map(|f| if f.r#type == HTTPFilterType::RequestMirror { f.request_mirror.as_ref() } else { None })
        .map(|mirror_filter| {
            let mirror_backend_refs: Vec<_> = routing_rule
                .filter_backends
                .iter()
                .filter_map(|b| match b.backend_type() {
                    crate::common::BackendType::Service(service_type_config) | crate::common::BackendType::Invalid(service_type_config) => {
                        Some(service_type_config)
                    },
                    crate::common::BackendType::InferencePool(_) => None,
                })
                .filter(|config| {
                    debug!("Mirror backend {} {:?}", config.resource_key, mirror_filter.backend_ref);
                    config.resource_key == ResourceKey::from((&mirror_filter.backend_ref, DEFAULT_NAMESPACE_NAME.to_owned()))
                })
                .map(|config| agentgateway_api_rs::agentgateway::dev::resource::BackendReference {
                    port: config.effective_port as u32,
                    kind: Some(agentgateway_api_rs::agentgateway::dev::resource::backend_reference::Kind::Backend(format!(
                        "{}/{}",
                        config.resource_key.namespace, config.resource_key.name
                    ))),
                })
                .collect();

            debug!("Mirror backends {mirror_backend_refs:?}");

            let percentage = match (mirror_filter.percent.as_ref(), mirror_filter.fraction.as_ref()) {
                (None, None) => 100.0,
                (None, Some(fraction)) => (f64::from(fraction.numerator) / f64::from(fraction.denominator.unwrap_or(100))) * 100.0,
                (Some(percent), None) => f64::from(*percent),
                (Some(_), Some(_)) => {
                    warn!("Invalid configuration for mirror filtering ");
                    100.0
                },
            };

            request_mirrors::Mirror { backend: mirror_backend_refs.into_iter().next(), percentage }
        })
        .collect();

    if mirrors.is_empty() {
        None
    } else {
        debug!("Request mirror policy {:?}", mirrors);
        Some(TrafficPolicySpec {
            kind: Some(traffic_policy_spec::Kind::RequestMirror(resource::RequestMirrors { mirrors })),
            ..Default::default()
        })
    }
}

fn map_response_header_modifier_filter(filter: &HttpRouteFilter) -> Option<TrafficPolicySpec> {
    filter.response_header_modifier.as_ref().map(|filter| TrafficPolicySpec {
        kind: Some(traffic_policy_spec::Kind::ResponseHeaderModifier(resource::HeaderModifier {
            add: filter.add.clone().map(map_headers).unwrap_or_default(),
            set: filter.set.clone().map(map_headers).unwrap_or_default(),
            remove: filter.remove.clone().unwrap_or_default(),
        })),
        ..Default::default()
    })
}

fn convert_route_match(route_match: &gateway_api::httproutes::RouteMatch) -> agentgateway_api_rs::agentgateway::dev::resource::RouteMatch {
    agentgateway_api_rs::agentgateway::dev::resource::RouteMatch {
        path: convert_path_match(route_match.path.as_ref()),
        headers: convert_headers(route_match.headers.as_ref()),
        method: convert_method_match(route_match.method.as_ref()),
        query_params: convert_query_params(route_match.query_params.as_ref()),
    }
}

fn create_route_backend(backend: &Backend) -> Option<agentgateway_api_rs::agentgateway::dev::resource::RouteBackend> {
    match backend {
        Backend::Resolved(BackendType::Service(config))
        | Backend::Invalid(BackendType::Service(config))
        | Backend::NotAllowed(BackendType::Service(config))
        | Backend::Unresolved(BackendType::Service(config)) => Some(RouteBackend {
            backend: Some(agentgateway_api_rs::agentgateway::dev::resource::BackendReference {
                port: config.effective_port as u32,
                kind: Some(agentgateway_api_rs::agentgateway::dev::resource::backend_reference::Kind::Backend(format!(
                    "{}/{}",
                    config.resource_key.namespace, config.resource_key.name
                ))),
            }),
            weight: config.weight,
            backend_policies: vec![],
        }),
        Backend::Resolved(BackendType::InferencePool(config)) => Some(RouteBackend {
            backend: Some(agentgateway_api_rs::agentgateway::dev::resource::BackendReference {
                port: if config.target_ports.is_empty() { config.port as u32 } else { config.target_ports[0] as u32 },
                kind: Some(agentgateway_api_rs::agentgateway::dev::resource::backend_reference::Kind::Service(
                    resource::backend_reference::Service {
                        namespace: backend.resource_key().namespace.clone(),
                        hostname: config.endpoint.clone(),
                    },
                )),
            }),
            weight: config.weight,
            backend_policies: vec![],
        }),
        _ => None,
    }
}

fn create_backends(backend: &Backend) -> Vec<Option<(String, resource::Backend)>> {
    match backend {
        Backend::Resolved(BackendType::Service(config)) => {
            let name = format!("{}/{}", config.resource_key.namespace, config.resource_key.name);
            vec![Some((
                name.clone(),
                resource::Backend {
                    name: Some(ResourceName {
                        name: backend.resource_key().name.clone(),
                        namespace: backend.resource_key().namespace.clone(),
                    }),
                    kind: Some(backend::Kind::Static(StaticBackend { host: config.endpoint.clone(), port: config.effective_port })),
                    inline_policies: vec![],
                    key: name,
                },
            ))]
        },
        _ => vec![None],
    }
}

fn create_backend_services(backend: &Backend) -> Option<(String, Service)> {
    match backend {
        Backend::Resolved(BackendType::InferencePool(config)) => {
            let name = format!("{}/{}", config.resource_key.namespace, config.endpoint);
            Some((
                name.clone(),
                Service {
                    name: config.resource_key.name.clone(),
                    namespace: config.resource_key.namespace.clone(),
                    hostname: config.endpoint.clone(),
                    ports: config
                        .target_ports
                        .iter()
                        .map(|p| Port { service_port: *p as u32, target_port: *p as u32, app_protocol: 0 })
                        .collect(),
                    addresses: config.endpoints.as_ref().map_or_else(std::vec::Vec::new, |endpoints| {
                        endpoints
                            .iter()
                            .filter_map(|e| {
                                e.parse::<IpAddr>()
                                    .map(|addr| NetworkAddress {
                                        network: String::new(),
                                        address: match addr {
                                            IpAddr::V4(ipv4_addr) => ipv4_addr.octets().to_vec(),
                                            IpAddr::V6(ipv6_addr) => ipv6_addr.octets().to_vec(),
                                        },
                                    })
                                    .ok()
                            })
                            .collect()
                    }),
                    load_balancing: Some(LoadBalancing { routing_preference: vec![], mode: 0, health_policy: 1 }),
                    ..Default::default()
                },
            ))
        },
        Backend::Invalid(BackendType::Service(config)) => {
            let name = format!("{}/{}", config.resource_key.namespace, config.endpoint);
            Some((
                name.clone(),
                Service {
                    name: config.resource_key.name.clone(),
                    namespace: config.resource_key.namespace.clone(),
                    hostname: config.endpoint.clone(),
                    ..Default::default()
                },
            ))
        },

        _ => None,
    }
}

fn create_inference_policies(
    policy_name: &str,
    backend: &ResourceKey,
    conf: &InferencePoolTypeConfig,
) -> (Policy, Option<(String, resource::Backend)>) {
    let epp_backend_name = format!("epp-backend-{policy_name}");
    (
        Policy {
            kind: Some(policy::Kind::Backend(BackendPolicySpec {
                kind: Some(backend_policy_spec::Kind::InferenceRouting(backend_policy_spec::InferenceRouting {
                    endpoint_picker: conf.inference_config.as_ref().map(|inference_config| BackendReference {
                        port: inference_config.extension_ref().port.as_ref().map_or_else(|| 9002, |f| f.number) as u32,
                        kind: Some(resource::backend_reference::Kind::Backend(epp_backend_name.clone())),
                    }),
                    failure_mode: conf
                        .inference_config
                        .as_ref()
                        .map(|conf| match conf.extension_ref().failure_mode.as_ref() {
                            Some(InferencePoolEndpointPickerRefFailureMode::FailOpen) => {
                                backend_policy_spec::inference_routing::FailureMode::FailOpen.into()
                            },
                            Some(InferencePoolEndpointPickerRefFailureMode::FailClose) => {
                                backend_policy_spec::inference_routing::FailureMode::FailClosed.into()
                            },
                            _ => backend_policy_spec::inference_routing::FailureMode::Unknown.into(),
                        })
                        .unwrap_or_default(),
                })),
            })),

            name: Some(TypedResourceName { name: backend.name.clone(), namespace: backend.namespace.clone(), kind: backend.kind.clone() }),
            target: Some(PolicyTarget {
                //format!("{}/{}", backend.namespace, conf.endpoint)
                kind: Some(resource::policy_target::Kind::Service(ServiceTarget {
                    namespace: backend.namespace.clone(),
                    hostname: conf.endpoint.clone(),
                    port: None,
                })),
            }),
            key: policy_name.to_owned(),
        },
        conf.inference_config.as_ref().map(|inference_config| {
            (
                epp_backend_name.clone(),
                resource::Backend {
                    name: Some(ResourceName { name: backend.name.clone(), namespace: backend.namespace.clone() }),
                    kind: Some(backend::Kind::Static(StaticBackend {
                        host: inference_config.extension_ref().name.clone() + "." + &conf.resource_key.namespace,
                        port: inference_config.extension_ref().port.as_ref().map_or_else(|| 9002, |f| f.number),
                    })),
                    inline_policies: vec![
                        BackendPolicySpec {
                            kind: Some(backend_policy_spec::Kind::BackendTls(backend_policy_spec::BackendTls {
                                cert: None,
                                key: None,
                                root: None,
                                verification: backend_policy_spec::backend_tls::VerificationMode::InsecureAll.into(),
                                hostname: None,
                                verify_subject_alt_names: vec![],
                                alpn: None,
                            })),
                        },
                        BackendPolicySpec {
                            kind: Some(backend_policy_spec::Kind::BackendHttp(backend_policy_spec::BackendHttp {
                                version: backend_policy_spec::backend_http::HttpVersion::Http2.into(),
                                request_timeout: None,
                            })),
                        },
                    ],
                    key: epp_backend_name,
                },
            )
        }),
    )
}

fn convert_path_match(
    path_match: Option<&gateway_api::httproutes::PathMatch>,
) -> Option<agentgateway_api_rs::agentgateway::dev::resource::PathMatch> {
    match path_match {
        Some(path_match) => {
            let match_value = path_match.value.clone().unwrap_or_default();
            match path_match.r#type {
                Some(gateway_api::httproutes::HttpRouteRulesMatchesPathType::Exact) => {
                    Some(agentgateway_api_rs::agentgateway::dev::resource::PathMatch {
                        kind: Some(agentgateway_api_rs::agentgateway::dev::resource::path_match::Kind::Exact(match_value)),
                    })
                },
                Some(gateway_api::httproutes::HttpRouteRulesMatchesPathType::PathPrefix) => {
                    Some(agentgateway_api_rs::agentgateway::dev::resource::PathMatch {
                        kind: Some(agentgateway_api_rs::agentgateway::dev::resource::path_match::Kind::PathPrefix(match_value)),
                    })
                },
                Some(gateway_api::httproutes::HttpRouteRulesMatchesPathType::RegularExpression) => {
                    Some(agentgateway_api_rs::agentgateway::dev::resource::PathMatch {
                        kind: Some(agentgateway_api_rs::agentgateway::dev::resource::path_match::Kind::Regex(match_value)),
                    })
                },
                None => None,
            }
        },
        None => None,
    }
}

fn convert_headers(
    header_match: Option<&Vec<gateway_api::common::HeaderMatch>>,
) -> Vec<agentgateway_api_rs::agentgateway::dev::resource::HeaderMatch> {
    match header_match {
        Some(header_match) => header_match
            .iter()
            .cloned()
            .map(|hm| agentgateway_api_rs::agentgateway::dev::resource::HeaderMatch {
                name: hm.name,
                value: match hm.r#type {
                    Some(gateway_api::common::HeaderMatchType::Exact) => {
                        Some(agentgateway_api_rs::agentgateway::dev::resource::header_match::Value::Exact(hm.value))
                    },
                    Some(gateway_api::common::HeaderMatchType::RegularExpression) => {
                        Some(agentgateway_api_rs::agentgateway::dev::resource::header_match::Value::Regex(hm.value))
                    },
                    None => None,
                },
            })
            .collect(),
        None => vec![],
    }
}

fn convert_method_match(
    method_match: Option<&gateway_api::httproutes::HTTPMethodMatch>,
) -> Option<agentgateway_api_rs::agentgateway::dev::resource::MethodMatch> {
    method_match
        .map(|mm| agentgateway_api_rs::agentgateway::dev::resource::MethodMatch { exact: serde_json::to_string(&mm).unwrap_or_default() })
}

fn convert_query_params(
    query_match: Option<&Vec<gateway_api::common::HeaderMatch>>,
) -> Vec<agentgateway_api_rs::agentgateway::dev::resource::QueryMatch> {
    match query_match {
        Some(query_match) => query_match
            .iter()
            .cloned()
            .map(|hm| agentgateway_api_rs::agentgateway::dev::resource::QueryMatch {
                name: hm.name,
                value: match hm.r#type {
                    Some(gateway_api::common::HeaderMatchType::Exact) => {
                        Some(agentgateway_api_rs::agentgateway::dev::resource::query_match::Value::Exact(hm.value))
                    },
                    Some(gateway_api::common::HeaderMatchType::RegularExpression) => {
                        Some(agentgateway_api_rs::agentgateway::dev::resource::query_match::Value::Regex(hm.value))
                    },
                    None => None,
                },
            })
            .collect(),
        None => vec![],
    }
}

fn map_headers(headers: Vec<HTTPHeader>) -> Vec<Header> {
    headers.into_iter().map(|header| Header { name: header.name, value: header.value }).collect()
}
