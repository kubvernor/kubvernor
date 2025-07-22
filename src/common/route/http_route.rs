use std::{cmp, net::IpAddr};

use gateway_api::{
    common::{HTTPFilterType, HTTPHeader, HeaderModifier, RequestRedirect},
    httproutes::{HTTPBackendReference, HTTPRoute, HTTPRouteFilter, HTTPRouteRule, HTTPRouteRulesMatchesPathType, PathMatch, RouteMatch},
};
use kube::ResourceExt;
use tracing::debug;

use super::{
    get_add_headers, get_remove_headers, get_set_headers, Backend, FilterHeaders, NotResolvedReason, ResolutionStatus, ResourceKey, Route, RouteConfig, RouteType, ServiceTypeConfig,
    DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME,
};
use crate::{
    common::{
        resource_key::DEFAULT_INFERENCE_GROUP_NAME,
        route::{HeaderComparator, QueryComparator},
        BackendType, InferencePoolTypeConfig,
    },
    controllers::ControllerError,
};

fn get_http_default_rules_matches() -> RouteMatch {
    RouteMatch {
        headers: Some(vec![]),
        method: None,
        path: Some(PathMatch {
            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
            value: Some("/".to_owned()),
        }),
        query_params: None,
    }
}

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
                        Some(kind) if kind == "Service" => Backend::Maybe(BackendType::Service(ServiceTypeConfig::from((br, local_namespace)))),
                        Some(kind) if kind == "InferencePool" => Backend::Maybe(BackendType::InferencePool(InferencePoolTypeConfig::from((br, local_namespace)))),
                        _ => {
                            has_invalid_backends = true;
                            Backend::Invalid(BackendType::Invalid(ServiceTypeConfig::from((br, local_namespace))))
                        }
                    })
                    .collect(),
            })
            .collect();
        let hostnames = kube_route
            .spec
            .hostnames
            .as_ref()
            .map(|hostnames| {
                let hostnames = hostnames.iter().filter(|hostname| hostname.parse::<IpAddr>().is_err()).cloned().collect::<Vec<_>>();
                hostnames
            })
            .unwrap_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()]);

        let effective_routing_rules: Vec<_> = routing_rules
            .iter()
            .flat_map(|rr: &HTTPRoutingRule| {
                let mut matching_rules = rr.matching_rules.clone();
                if matching_rules.is_empty() {
                    matching_rules.push(get_http_default_rules_matches());
                }

                rr.matching_rules.iter().map(|matcher| HTTPEffectiveRoutingRule {
                    route_matcher: matcher.clone(),
                    backends: rr.backends.clone(),
                    name: rr.name.clone(),
                    hostnames: hostnames.clone(),
                    request_headers: rr.filter_headers(),
                    response_headers: FilterHeaders::default(),
                    redirect_filter: rr
                        .filters
                        .iter()
                        .find_map(|f| if f.r#type == HTTPFilterType::RequestRedirect { f.request_redirect.clone() } else { None }),
                })
            })
            .collect();

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
                effective_routing_rules,
            }),
        };

        Ok(Route { config })
    }
}

#[derive(Clone, Debug)]
pub struct HTTPRoutingConfiguration {
    pub routing_rules: Vec<HTTPRoutingRule>,
    pub effective_routing_rules: Vec<HTTPEffectiveRoutingRule>,
}

impl From<(&HTTPBackendReference, &str)> for ServiceTypeConfig {
    fn from((br, local_namespace): (&HTTPBackendReference, &str)) -> Self {
        ServiceTypeConfig {
            resource_key: ResourceKey::from((br, local_namespace.to_owned())),
            endpoint: if let Some(namespace) = br.namespace.as_ref() {
                if *namespace == DEFAULT_NAMESPACE_NAME {
                    br.name.clone()
                } else {
                    format!("{}.{namespace}", br.name)
                }
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
                if *namespace == DEFAULT_NAMESPACE_NAME {
                    br.name.clone()
                } else {
                    format!("{}.{namespace}", br.name)
                }
            } else if local_namespace == DEFAULT_NAMESPACE_NAME {
                br.name.clone()
            } else {
                format!("{}.{local_namespace}", br.name)
            },
            port: br.port.unwrap_or(0),
            effective_port: br.port.unwrap_or(0),
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
    fn filter_headers(&self) -> FilterHeaders {
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

#[derive(Clone, Debug, PartialEq, Default)]
pub struct HTTPEffectiveRoutingRule {
    pub route_matcher: RouteMatch,
    pub backends: Vec<Backend>,
    pub name: String,
    pub hostnames: Vec<String>,

    pub request_headers: FilterHeaders,
    pub response_headers: FilterHeaders,

    pub redirect_filter: Option<RequestRedirect>,
}

impl PartialOrd for HTTPEffectiveRoutingRule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Self::compare_matching(&self.route_matcher, &other.route_matcher))
    }
}

impl HTTPEffectiveRoutingRule {
    fn header_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        let matcher = HeaderComparator::builder().this(this.headers.as_ref()).other(other.headers.as_ref()).build();
        matcher.compare_headers()
    }

    fn query_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        let matcher = QueryComparator::builder().this(this.headers.as_ref()).other(other.headers.as_ref()).build();
        matcher.compare_queries()
    }

    fn method_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        match (this.method.as_ref(), other.method.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_method), Some(other_method)) => {
                let this_desc = this_method.clone() as isize;
                let other_desc = other_method.clone() as isize;
                this_desc.cmp(&other_desc)
            }
        }
    }
    fn path_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        match (this.path.as_ref(), other.path.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_path), Some(other_path)) => match (this_path.r#type.as_ref(), other_path.r#type.as_ref()) {
                (None, None) => this_path.value.cmp(&other_path.value),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(this_prefix_match_type), Some(other_prefix_match_type)) => {
                    let this_desc = this_prefix_match_type.clone() as isize;
                    let other_desc = other_prefix_match_type.clone() as isize;
                    let maybe_equal = this_desc.cmp(&other_desc);
                    if maybe_equal == cmp::Ordering::Equal {
                        match (&this_path.value, &other_path.value) {
                            (None, None) => std::cmp::Ordering::Equal,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (Some(this_path), Some(other_path)) => other_path.len().cmp(&this_path.len()),
                        }
                    } else {
                        maybe_equal
                    }
                }
            },
        }
    }

    fn compare_matching(this: &RouteMatch, other: &RouteMatch) -> std::cmp::Ordering {
        let path_match = Self::path_matching(this, other);
        let method_match = Self::method_matching(this, other);
        let header_match = Self::header_matching(this, other);
        let query_match = Self::query_matching(this, other);
        let result = if query_match == std::cmp::Ordering::Equal {
            if header_match == std::cmp::Ordering::Equal {
                if path_match == std::cmp::Ordering::Equal {
                    method_match
                } else {
                    path_match
                }
            } else {
                header_match
            }
        } else {
            query_match
        };
        debug!("Comparing {this:#?} {other:#?} {result:?} {path_match:?} {header_match:?} {query_match:?} {method_match:?}");
        result
    }
}
