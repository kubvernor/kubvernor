use std::collections::BTreeMap;

use agentgateway_api_rs::agentgateway::dev::resource::{
    self, BackendReference, Listener, Policy, PolicySpec, PolicyTarget, Route, RouteBackend, StaticBackend,
    ai_backend::Provider,
    backend::{self},
    policy_spec::InferenceRouting,
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

    pub fn generate_routes_and_backends_and_policies(&self) -> (Vec<resource::Route>, Vec<resource::Backend>, Vec<resource::Policy>) {
        let mut backends = BTreeMap::<String, resource::Backend>::new();
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

                                let policies = inference_extension_configurations
                                    .into_iter()
                                    .map(|(name, backend, conf)| (name.clone(), create_inference_policies(name, backend, conf)))
                                    .collect::<Vec<_>>();
                                info!("(Inference policies routing rule {}  {backend_policies:#?}", routing_rule.name);

                                backend_policies.extend(policies);
                                backends.extend(&mut routing_rule.backends.iter().filter_map(create_backends));
                                Route {
                                    key: route.name().to_owned(),
                                    listener_key: l.name().to_owned(),
                                    rule_name: routing_rule.name.clone(),
                                    route_name: route.name().to_owned(),
                                    hostnames: route.hostnames().to_vec(),
                                    matches: routing_rule.matching_rules.iter().map(convert_route_match).collect(),
                                    filters: vec![],
                                    backends: routing_rule.backends.iter().filter_map(create_route_backend).collect(),
                                    traffic_policy: None,
                                    inline_policies: vec![],
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
            filters: vec![],
        }),
        Backend::Resolved(BackendType::InferencePool(config)) => Some(RouteBackend {
            backend: Some(agentgateway_api_rs::agentgateway::dev::resource::BackendReference {
                port: config.port as u32,
                kind: Some(agentgateway_api_rs::agentgateway::dev::resource::backend_reference::Kind::Backend(format!(
                    "{}/{}",
                    config.resource_key.namespace, config.resource_key.name
                ))),
            }),
            weight: config.weight,
            filters: vec![],
        }),
        _ => None,
    }
}

fn create_backends(backend: &Backend) -> Option<(String, resource::Backend)> {
    match backend {
        Backend::Resolved(BackendType::Service(config)) => {
            let name = format!("{}/{}", config.resource_key.namespace, config.resource_key.name);
            Some((
                name.clone(),
                resource::Backend {
                    name,
                    kind: Some(backend::Kind::Static(StaticBackend { host: config.endpoint.clone(), port: config.effective_port })),
                },
            ))
        },
        Backend::Resolved(BackendType::InferencePool(config)) => {
            let name = format!("{}/{}", config.resource_key.namespace, config.resource_key.name);
            Some((
                name.clone(),
                resource::Backend {
                    name,
                    kind: Some(backend::Kind::Static(StaticBackend { host: config.endpoint.clone(), port: config.effective_port })),
                },
            ))
        },
        _ => None,
    }
}

fn create_inference_policies(policy_name: String, backend: ResourceKey, conf: &InferencePoolTypeConfig) -> Policy {
    Policy {
        spec: Some(PolicySpec {
            kind: Some(resource::policy_spec::Kind::InferenceRouting(InferenceRouting {
                endpoint_picker: conf.inference_config.as_ref().map(|inference_config| BackendReference {
                    port: inference_config.extension_ref().port_number.unwrap_or(9002) as u32,
                    kind: Some(resource::backend_reference::Kind::Backend(inference_config.extension_ref().name.clone())),
                }),
                failure_mode: 0,
            })),
        }),
        name: policy_name,
        target: Some(PolicyTarget {
            kind: Some(resource::policy_target::Kind::Backend(format!("{}/{}", backend.namespace, backend.name))),
        }),
    }
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
