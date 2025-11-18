use std::net::IpAddr;

use gateway_api::{
    common::{HTTPFilterType, HTTPHeader, HeaderModifier},
    httproutes::{HTTPBackendReference, HTTPRoute, HTTPRouteFilter, HTTPRouteRule, RouteMatch},
};
use kube::ResourceExt;

use super::{
    Backend, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME, FilterHeaders, NotResolvedReason, ResolutionStatus, ResourceKey, Route,
    RouteConfig, RouteType, ServiceTypeConfig, get_add_headers, get_remove_headers, get_set_headers,
};
use crate::{
    common::{BackendType, InferencePoolTypeConfig, resource_key::DEFAULT_INFERENCE_GROUP_NAME},
    controllers::ControllerError,
};

impl TryFrom<HTTPRoute> for Route {
    type Error = ControllerError;

    fn try_from(value: HTTPRoute) -> Result<Self, Self::Error> {
        Route::try_from(&value)
    }
}

impl TryFrom<&HTTPRoute> for Route {
    type Error = ControllerError;
    fn try_from(kube_route: &HTTPRoute) -> Result<Self, Self::Error> {
        let key = ResourceKey::from(kube_route);
        let parents = kube_route.spec.parent_refs.clone();
        let local_namespace = key.namespace.as_str();

        let empty_rules: Vec<HTTPRouteRule> = vec![];
        let mut has_invalid_backends = false;
        let routing_rules = kube_route.spec.rules.as_ref().unwrap_or(&empty_rules);
        let routing_rules: Vec<HTTPRoutingRule> = routing_rules
            .iter()
            .enumerate()
            .map(|(i, rr)| HTTPRoutingRule {
                name: format!("{}-{i}", kube_route.name_any()),
                matching_rules: rr.matches.clone().unwrap_or_default(),
                filters: rr.filters.clone().unwrap_or_default(),
                backends: rr
                    .backend_refs
                    .as_ref()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|br| match br.kind.as_ref() {
                        None => Backend::Maybe(BackendType::Service(ServiceTypeConfig::from((br, local_namespace)))),
                        Some(kind) if kind == "Service" => {
                            Backend::Maybe(BackendType::Service(ServiceTypeConfig::from((br, local_namespace))))
                        },
                        Some(kind) if kind == "InferencePool" => {
                            Backend::Maybe(BackendType::InferencePool(InferencePoolTypeConfig::from((br, local_namespace))))
                        },
                        _ => {
                            has_invalid_backends = true;
                            Backend::Invalid(BackendType::Invalid(ServiceTypeConfig::from((br, local_namespace))))
                        },
                    })
                    .collect(),
            })
            .collect();
        let hostnames = kube_route
            .spec
            .hostnames
            .as_ref()
            .map(|hostnames| hostnames.iter().filter(|hostname| hostname.parse::<IpAddr>().is_err()).cloned().collect::<Vec<_>>())
            .unwrap_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()]);

        let config = RouteConfig {
            resource_key: key,
            parents,
            hostnames,
            resolution_status: if has_invalid_backends {
                ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend)
            } else {
                ResolutionStatus::NotResolved(NotResolvedReason::Unknown)
            },
            route_type: RouteType::Http(HTTPRoutingConfiguration {
                routing_rules,
                //effective_routing_rules,
            }),
        };

        Ok(Route { config })
    }
}

#[derive(Clone, Debug)]
pub struct HTTPRoutingConfiguration {
    pub routing_rules: Vec<HTTPRoutingRule>,
    //pub effective_routing_rules: Vec<HTTPEffectiveRoutingRule>,
}

impl From<(&HTTPBackendReference, &str)> for ServiceTypeConfig {
    fn from((br, local_namespace): (&HTTPBackendReference, &str)) -> Self {
        ServiceTypeConfig {
            resource_key: ResourceKey::from((br, local_namespace.to_owned())),
            endpoint: if let Some(namespace) = br.namespace.as_ref() {
                if *namespace == DEFAULT_NAMESPACE_NAME { br.name.clone() } else { format!("{}.{namespace}", br.name) }
            } else if local_namespace == DEFAULT_NAMESPACE_NAME {
                br.name.clone()
            } else {
                format!("{}.{local_namespace}", br.name)
            },
            port: br.port.unwrap_or(0),
            effective_port: br.port.unwrap_or(0),
            weight: br.weight.unwrap_or(1),
        }
    }
}

impl From<(&HTTPBackendReference, &str)> for InferencePoolTypeConfig {
    fn from((br, local_namespace): (&HTTPBackendReference, &str)) -> Self {
        let mut resource_key = ResourceKey::from((br, local_namespace.to_owned()));
        DEFAULT_INFERENCE_GROUP_NAME.clone_into(&mut resource_key.group);
        InferencePoolTypeConfig {
            resource_key,
            endpoint: if let Some(namespace) = br.namespace.as_ref() {
                if *namespace == DEFAULT_NAMESPACE_NAME { br.name.clone() } else { format!("{}.{namespace}", br.name) }
            } else if local_namespace == DEFAULT_NAMESPACE_NAME {
                br.name.clone()
            } else {
                format!("{}.{local_namespace}", br.name)
            },
            port: br.port.unwrap_or(0),
            target_ports: vec![br.port.unwrap_or(0)],
            weight: br.weight.unwrap_or(1),
            inference_config: None,
            endpoints: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HTTPRoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
    pub matching_rules: Vec<RouteMatch>,
    pub filters: Vec<HTTPRouteFilter>,
}

impl HTTPRoutingRule {
    pub fn filter_headers(&self) -> FilterHeaders {
        fn iter<T: Clone, X>(routing_rule: &HTTPRoutingRule, x: X) -> Vec<T>
        where
            X: Fn(Option<&HeaderModifier>) -> Option<&Vec<T>>,
        {
            routing_rule
                .filters
                .iter()
                .filter(|f| f.r#type == HTTPFilterType::RequestHeaderModifier)
                .filter_map(|f| x(f.request_header_modifier.as_ref()))
                .map(std::iter::IntoIterator::into_iter)
                .flat_map(std::iter::Iterator::collect::<Vec<_>>)
                .cloned()
                .collect()
        }

        FilterHeaders {
            add: iter::<HTTPHeader, _>(self, get_add_headers),
            remove: iter::<String, _>(self, get_remove_headers),
            set: iter::<HTTPHeader, _>(self, get_set_headers),
        }
    }
}
