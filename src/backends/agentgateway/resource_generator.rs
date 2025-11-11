use std::{collections::BTreeMap, net::IpAddr};

use agentgateway_api_rs::agentgateway::dev::{
    resource::{
        self, BackendPolicySpec, BackendReference, Listener, Policy, PolicyTarget, Route, RouteBackend, StaticBackend,
        backend::{self},
        backend_policy_spec, policy,
    },
    workload::{self, LoadBalancing, NetworkAddress},
};
use tracing::{info, warn};

use crate::common::{self, Backend, BackendType, InferencePoolTypeConfig, ProtocolType, ResourceKey};

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

fn create_bind_name(port: i32) -> String {
    format!("bind-port-{port}")
}

#[derive(Debug, Clone, Ord, Eq, PartialEq, PartialOrd)]
// created because xds bind is not Ord
pub(crate) struct Bind {
    pub key: String,
    pub port: u32,
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(effective_gateway: &'a common::Gateway) -> Self {
        Self { effective_gateway }
    }

    pub fn generate_bindings_and_listeners(&self) -> BTreeMap<Bind, Vec<Listener>> {
        let gateway = self.effective_gateway;
        let listeners = gateway.listeners().fold(BTreeMap::<Bind, Vec<Listener>>::new(), |mut acc, listener| {
            let port = listener.port();
            let listener_name = listener.name().to_owned();
            let gateway_name = gateway.name().to_owned();

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

            info!("Generating agentgateway listener {agentgateway_listener:#?}");

            if let Some(listners) = maybe_added {
                listners.push(agentgateway_listener);
            } else {
                acc.insert(bind, vec![agentgateway_listener]);
            }
            acc
        });
        listeners
    }

    pub fn generate_routes_and_backends_and_policies(
        &self,
    ) -> (Vec<resource::Route>, Vec<resource::Backend>, Vec<resource::Policy>, Vec<workload::Service>) {
        let mut backends = BTreeMap::<String, resource::Backend>::new();
        let mut backend_services = BTreeMap::<String, workload::Service>::new();
        let mut backend_policies = BTreeMap::<String, Policy>::new();
        let gateway = self.effective_gateway;
        let routes = gateway
            .listeners()
            .flat_map(|l| {
                let (resolved, _) = l.routes();
                resolved
                    .iter()
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
                                        let (policy, backend) = create_inference_policies(&name, backend, conf);

                                        ((name.clone(), policy), backend)
                                    })
                                    .unzip();
                                info!("(Inference policies routing rule {}  {backend_policies:#?}", routing_rule.name);

                                backend_policies.extend(policies);
                                backend_services.extend(&mut routing_rule.backends.iter().filter_map(create_backend_services));
                                backends.extend(&mut routing_rule.backends.iter().flat_map(create_backends).flatten());
                                let epp_backends = epp_backends.into_iter().flatten().collect::<Vec<_>>();
                                backends.extend(epp_backends);
                                Route {
                                    key: route.name().to_owned(),
                                    listener_key: l.name().to_owned(),
                                    rule_name: routing_rule.name.clone(),
                                    route_name: route.name().to_owned(),
                                    hostnames: route.hostnames().to_vec(),
                                    matches: routing_rule.matching_rules.iter().map(convert_route_match).collect(),
                                    backends: routing_rule.backends.iter().filter_map(create_route_backend).collect(),
                                    traffic_policies: vec![],
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
            backend_services.into_values().collect::<Vec<workload::Service>>(),
        )
    }
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
        Backend::Resolved(BackendType::Service(config)) => Some(RouteBackend {
            backend: Some(agentgateway_api_rs::agentgateway::dev::resource::BackendReference {
                port: config.port as u32,
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
                kind: Some(agentgateway_api_rs::agentgateway::dev::resource::backend_reference::Kind::Service(format!(
                    "{}/{}",
                    config.resource_key.namespace, config.endpoint
                ))),
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
                    name,
                    kind: Some(backend::Kind::Static(StaticBackend { host: config.endpoint.clone(), port: config.port })),
                    inline_policies: vec![],
                },
            ))]
        },
        _ => vec![None],
    }
}

fn create_backend_services(backend: &Backend) -> Option<(String, workload::Service)> {
    match backend {
        Backend::Resolved(BackendType::InferencePool(config)) => {
            let name = format!("{}/{}", config.resource_key.namespace, config.endpoint);
            Some((
                name.clone(),
                workload::Service {
                    name: config.resource_key.name.clone(),
                    namespace: config.resource_key.namespace.clone(),
                    hostname: config.endpoint.clone(),
                    ports: config
                        .target_ports
                        .iter()
                        .map(|p| workload::Port { service_port: *p as u32, target_port: *p as u32, app_protocol: 0 })
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
        _ => None,
    }
}

fn create_inference_policies(
    policy_name: &str,
    backend: ResourceKey,
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
                    failure_mode: 0,
                })),
            })),

            name: policy_name.to_owned(),
            target: Some(PolicyTarget {
                kind: Some(resource::policy_target::Kind::Service(format!("{}/{}", backend.namespace, conf.endpoint))),
            }),
        },
        conf.inference_config.as_ref().map(|inference_config| {
            (
                epp_backend_name.clone(),
                resource::Backend {
                    name: epp_backend_name,
                    kind: Some(backend::Kind::Static(StaticBackend {
                        host: inference_config.extension_ref().name.clone(),
                        port: inference_config.extension_ref().port.as_ref().map_or_else(|| 9002, |f| f.number),
                    })),
                    inline_policies: vec![],
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
                Some(gateway_api::httproutes::HTTPRouteRulesMatchesPathType::Exact) => {
                    Some(agentgateway_api_rs::agentgateway::dev::resource::PathMatch {
                        kind: Some(agentgateway_api_rs::agentgateway::dev::resource::path_match::Kind::Exact(match_value)),
                    })
                },
                Some(gateway_api::httproutes::HTTPRouteRulesMatchesPathType::PathPrefix) => {
                    Some(agentgateway_api_rs::agentgateway::dev::resource::PathMatch {
                        kind: Some(agentgateway_api_rs::agentgateway::dev::resource::path_match::Kind::PathPrefix(match_value)),
                    })
                },
                Some(gateway_api::httproutes::HTTPRouteRulesMatchesPathType::RegularExpression) => {
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
