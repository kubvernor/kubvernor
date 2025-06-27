use std::{cmp, net::IpAddr};

use gateway_api::{
    common::{GRPCFilterType, GRPCRouteFilter, HTTPHeader, HeaderModifier},
    grpcroutes::{GRPCRoute, GRPCRouteRule, GRPCRouteMatch},
};
use kube::ResourceExt;
use tracing::debug;

use super::{
    get_add_headers, get_remove_headers, get_set_headers, Backend, BackendServiceConfig, FilterHeaders, NotResolvedReason, ResolutionStatus, ResourceKey, Route, RouteConfig, RouteType,
    DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME,
};
use crate::{common::route::HeaderComparator, controllers::ControllerError};

fn get_grpc_default_rules_matches() -> GRPCRouteMatch {
    GRPCRouteMatch { headers: Some(vec![]), method: None }
}

impl TryFrom<GRPCRoute> for Route {
    type Error = ControllerError;

    fn try_from(value: GRPCRoute) -> Result<Self, Self::Error> {
        Route::try_from(&value)
    }
}

impl TryFrom<&GRPCRoute> for Route {
    type Error = ControllerError;
    fn try_from(kube_route: &GRPCRoute) -> Result<Self, Self::Error> {
        let key = ResourceKey::from(kube_route);
        let parents = kube_route.spec.parent_refs.clone();
        let local_namespace = key.namespace.clone();

        let empty_rules: Vec<GRPCRouteRule> = vec![];
        let mut has_invalid_backends = false;
        let routing_rules = kube_route.spec.rules.as_ref().unwrap_or(&empty_rules);
        let routing_rules: Vec<GRPCRoutingRule> = routing_rules
            .iter()
            .enumerate()
            .map(|(i, rr)| GRPCRoutingRule {
                name: format!("{}-{i}", kube_route.name_any()),
                matching_rules: rr.matches.clone().unwrap_or_default(),
                filters: rr.filters.clone().unwrap_or_default(),
                backends: rr
                    .backend_refs
                    .as_ref()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|br| {
                        let config = BackendServiceConfig {
                            resource_key: ResourceKey::from((br, local_namespace.clone())),
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
                        };

                        if br.kind.is_none() || br.kind == Some("Service".to_owned()) {
                            Backend::Maybe(config)
                        } else {
                            has_invalid_backends = true;
                            Backend::Invalid(config)
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
            .flat_map(|rr| {
                let mut matching_rules = rr.matching_rules.clone();
                if matching_rules.is_empty() {
                    matching_rules.push(get_grpc_default_rules_matches());
                }

                matching_rules.into_iter().map(|matcher| GRPCEffectiveRoutingRule {
                    route_matcher: matcher.clone(),
                    backends: rr.backends.clone(),
                    name: rr.name.clone(),
                    hostnames: hostnames.clone(),
                    request_headers: rr.filter_headers(),
                    response_headers: FilterHeaders::default(),
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
            route_type: RouteType::Grpc(GRPCRoutingConfiguration {
                routing_rules,
                effective_routing_rules,
            }),
        };

        Ok(Route { config })
    }
}

#[derive(Clone, Debug)]
pub struct GRPCRoutingConfiguration {
    pub routing_rules: Vec<GRPCRoutingRule>,
    pub effective_routing_rules: Vec<GRPCEffectiveRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct GRPCRoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
    pub matching_rules: Vec<GRPCRouteMatch>,
    pub filters: Vec<GRPCRouteFilter>,
}

impl GRPCRoutingRule {
    fn filter_headers(&self) -> FilterHeaders {
        fn iter<T: Clone, X>(routing_rule: &GRPCRoutingRule, x: X) -> Vec<T>
        where
            X: Fn(Option<&HeaderModifier>) -> Option<&Vec<T>>,
        {
            routing_rule
                .filters
                .iter()
                .filter(|f| f.r#type == GRPCFilterType::RequestHeaderModifier)
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
pub struct GRPCEffectiveRoutingRule {
    pub route_matcher: GRPCRouteMatch,
    pub backends: Vec<Backend>,
    pub name: String,
    pub hostnames: Vec<String>,

    pub request_headers: FilterHeaders,
    pub response_headers: FilterHeaders,
}

impl PartialOrd for GRPCEffectiveRoutingRule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Self::compare_matching(&self.route_matcher, &other.route_matcher))
    }
}

impl GRPCEffectiveRoutingRule {
    fn header_matching(this: &GRPCRouteMatch, other: &GRPCRouteMatch) -> std::cmp::Ordering {
        let matcher = HeaderComparator::builder().this(this.headers.as_ref()).other(other.headers.as_ref()).build();
        matcher.compare_headers()
    }

    fn method_matching(this: &GRPCRouteMatch, other: &GRPCRouteMatch) -> std::cmp::Ordering {
        match (this.method.as_ref(), other.method.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_method), Some(other_method)) => {
                let cmp_method = this_method.method.cmp(&other_method.method);
                let cmp_service = this_method.service.cmp(&other_method.service);

                match (cmp_method, cmp_service) {
                    (cmp::Ordering::Equal, _) => cmp_service,
                    _ => cmp_method,
                }
            }
        }
    }

    fn compare_matching(this: &GRPCRouteMatch, other: &GRPCRouteMatch) -> std::cmp::Ordering {
        let method_match = Self::method_matching(this, other);
        let header_match = Self::header_matching(this, other);

        let result = if header_match == std::cmp::Ordering::Equal { method_match } else { header_match };

        debug!("Comparing {this:#?} {other:#?} {result:?} {header_match:?} {method_match:?}");
        result
    }
}
