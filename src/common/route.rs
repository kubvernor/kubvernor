use std::{cmp, net::IpAddr};

use gateway_api::apis::standard::grpcroutes::{GRPCRouteParentRefs, GRPCRouteRulesFilters, GRPCRouteRulesMatches};
use kube::ResourceExt;
use thiserror::Error;
use tracing::debug;

use super::{Backend, BackendServiceConfig, ResourceKey, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME};
use crate::{
    common::gateway_api::httproutes::{
        HTTPRoute, HTTPRouteParentRefs, HTTPRouteRules, HTTPRouteRulesFilters, HTTPRouteRulesFiltersRequestHeaderModifierAdd, HTTPRouteRulesFiltersRequestHeaderModifierSet,
        HTTPRouteRulesFiltersRequestRedirect, HTTPRouteRulesFiltersType, HTTPRouteRulesMatches,
    },
    controllers::ControllerError,
};

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteStatus {
    Attached,
    Ignored,
}

impl std::fmt::Display for RouteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
pub enum Route {
    Http(RouteConfig),
    Grpc(GRPCRouteConfig),
}

impl Route {
    pub fn resource_key(&self) -> &ResourceKey {
        match self {
            Route::Http(c) => &c.resource_key,
            Route::Grpc(c) => &c.resource_key,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Route::Http(c) => &c.resource_key.name,
            Route::Grpc(c) => &c.resource_key.name,
        }
    }

    pub fn namespace(&self) -> &String {
        match self {
            Route::Http(c) => &c.resource_key.namespace,
            Route::Grpc(c) => &c.resource_key.namespace,
        }
    }

    pub fn hostnames(&self) -> &[String] {
        match self {
            Route::Http(config) => &config.hostnames,
            Route::Grpc(config) => &config.hostnames,
        }
    }

    pub fn parents(&self) -> Option<&Vec<RouteParentRefs>> {
        match self {
            Route::Http(c) => c.parents.as_ref(),
            Route::Grpc(c) => c.parents.as_ref(),
        }
    }

    pub fn backends(&self) -> Vec<&Backend> {
        match self {
            Route::Http(c) => c.routing_rules.iter().flat_map(|r| &r.backends).collect(),
            Route::Grpc(c) => c.routing_rules.iter().flat_map(|r| &r.backends).collect(),
        }
    }

    // pub fn routing_rules(&self) -> &[RoutingRule] {
    //     match self {
    //         Route::Http(c) | Route::Grpc(c) => &c.routing_rules,
    //     }
    // }

    // pub fn config(&self) -> &RouteConfig {
    //     match self {
    //         Route::Http(config) | Route::Grpc(config) => config,
    //     }
    // }

    pub fn resolution_status(&self) -> &ResolutionStatus {
        match self {
            Route::Http(config) => &config.resolution_status,
            Route::Grpc(config) => &config.resolution_status,
        }
    }

    // pub fn config_mut(&mut self) -> &mut RouteConfig {
    //     match self {
    //         Route::Http(config) | Route::Grpc(config) => config,
    //     }
    // }

    // pub fn effective_routing_rules(&self) -> &[EffectiveRoutingRule] {
    //     match self {
    //         Route::Http(c) | Route::Grpc(c) => &c.effective_routing_rules,
    //     }
    // }

    // pub fn resolution_status_mut(&mut self) -> &mut ResolutionStatus {
    //     match self {
    //         Route::Http(config) | Route::Grpc(config) => &mut config.resolution_status,
    //     }
    // }
}

impl TryFrom<&HTTPRoute> for Route {
    type Error = ControllerError;
    fn try_from(kube_route: &HTTPRoute) -> Result<Self, Self::Error> {
        let key = ResourceKey::from(kube_route);
        let parents = kube_route.spec.parent_refs.clone();
        let local_namespace = key.namespace.clone();

        let empty_rules: Vec<HTTPRouteRules> = vec![];
        let mut has_invalid_backends = false;
        let routing_rules = kube_route.spec.rules.as_ref().unwrap_or(&empty_rules);
        let routing_rules: Vec<RoutingRule> = routing_rules
            .iter()
            .enumerate()
            .map(|(i, rr)| RoutingRule {
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
                rr.matching_rules.iter().map(|matcher| EffectiveRoutingRule {
                    route_matcher: matcher.clone(),
                    backends: rr.backends.clone(),
                    name: rr.name.clone(),
                    hostnames: hostnames.clone(),
                    request_headers: FilterHeaders {
                        add: rr
                            .filters
                            .iter()
                            .filter_map(|f| {
                                if f.r#type == HTTPRouteRulesFiltersType::RequestHeaderModifier {
                                    if let Some(modifier) = &f.request_header_modifier {
                                        modifier.add.as_ref().map(|to_add| to_add.iter().map(HttpHeader::from))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .flat_map(std::iter::Iterator::collect::<Vec<_>>)
                            .collect(),
                        remove: rr
                            .filters
                            .iter()
                            .filter_map(|f| {
                                if f.r#type == HTTPRouteRulesFiltersType::RequestHeaderModifier {
                                    if let Some(modifier) = &f.request_header_modifier {
                                        modifier.remove.as_ref().map(|to_remove| to_remove.clone().into_iter())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .flat_map(std::iter::Iterator::collect::<Vec<_>>)
                            .collect(),
                        set: rr
                            .filters
                            .iter()
                            .filter_map(|f| {
                                if f.r#type == HTTPRouteRulesFiltersType::RequestHeaderModifier {
                                    if let Some(modifier) = &f.request_header_modifier {
                                        modifier.set.as_ref().map(|to_set| to_set.iter().map(HttpHeader::from))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .flat_map(std::iter::Iterator::collect::<Vec<_>>)
                            .collect(),
                    },
                    response_headers: FilterHeaders::default(),
                    redirect_filter: rr.filters.iter().find_map(|f| {
                        if f.r#type == HTTPRouteRulesFiltersType::RequestRedirect {
                            f.request_redirect.clone()
                        } else {
                            None
                        }
                    }),
                })
            })
            .collect();

        let rc = RouteConfig {
            resource_key: key,
            parents: parents.map(|parents| parents.into_iter().map(RouteParentRefs::from).collect()),
            routing_rules,
            hostnames,
            resolution_status: if has_invalid_backends {
                ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend)
            } else {
                ResolutionStatus::NotResolved(NotResolvedReason::Unknown)
            },
            effective_routing_rules,
        };

        Ok(Route::Http(rc))
    }
}

#[derive(Clone, Debug)]
pub struct RouteConfig {
    pub resource_key: ResourceKey,
    parents: Option<Vec<RouteParentRefs>>,
    pub routing_rules: Vec<RoutingRule>,
    hostnames: Vec<String>,
    pub resolution_status: ResolutionStatus,
    pub effective_routing_rules: Vec<EffectiveRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct GRPCRouteConfig {
    pub resource_key: ResourceKey,
    parents: Option<Vec<RouteParentRefs>>,
    pub routing_rules: Vec<GRPCRoutingRule>,
    hostnames: Vec<String>,
    pub resolution_status: ResolutionStatus,
    pub effective_routing_rules: Vec<GRPCEffectiveRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct RouteParentRefs {
    pub group: Option<String>,
    pub kind: Option<String>,
    pub name: String,
    pub namespace: Option<String>,
    pub port: Option<i32>,
    pub section_name: Option<String>,
}

impl From<GRPCRouteParentRefs> for RouteParentRefs {
    fn from(value: GRPCRouteParentRefs) -> Self {
        let GRPCRouteParentRefs {
            group,
            kind,
            name,
            namespace,
            port,
            section_name,
        } = value;
        Self {
            group,
            kind,
            name,
            namespace,
            port,
            section_name,
        }
    }
}

impl From<HTTPRouteParentRefs> for RouteParentRefs {
    fn from(value: HTTPRouteParentRefs) -> Self {
        let HTTPRouteParentRefs {
            group,
            kind,
            name,
            namespace,
            port,
            section_name,
        } = value;
        Self {
            group,
            kind,
            name,
            namespace,
            port,
            section_name,
        }
    }
}

pub struct RouteParents {}

impl RouteConfig {
    pub fn reorder_routes(&mut self) {
        self.effective_routing_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
    }
}

impl PartialEq for RouteConfig {
    fn eq(&self, other: &Self) -> bool {
        self.resource_key == other.resource_key
    }
}

impl PartialOrd for RouteConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.resource_key.cmp(&other.resource_key))
    }
}

impl Ord for RouteConfig {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.resource_key.cmp(&other.resource_key)
    }
}

impl Eq for RouteConfig {}

impl GRPCRouteConfig {
    pub fn reorder_routes(&mut self) {
        self.effective_routing_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
    }
}

impl PartialEq for GRPCRouteConfig {
    fn eq(&self, other: &Self) -> bool {
        self.resource_key == other.resource_key
    }
}

impl PartialOrd for GRPCRouteConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.resource_key.cmp(&other.resource_key))
    }
}

impl Ord for GRPCRouteConfig {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.resource_key.cmp(&other.resource_key)
    }
}

impl Eq for GRPCRouteConfig {}

#[derive(Clone, Debug)]
pub struct RoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
    pub matching_rules: Vec<HTTPRouteRulesMatches>,
    pub filters: Vec<HTTPRouteRulesFilters>,
}

#[derive(Clone, Debug)]
pub struct GRPCRoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
    pub matching_rules: Vec<GRPCRouteRulesMatches>,
    pub filters: Vec<GRPCRouteRulesFilters>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct EffectiveRoutingRule {
    pub route_matcher: HTTPRouteRulesMatches,
    pub backends: Vec<Backend>,
    pub name: String,
    pub hostnames: Vec<String>,

    pub request_headers: FilterHeaders,
    pub response_headers: FilterHeaders,

    pub redirect_filter: Option<HTTPRouteRulesFiltersRequestRedirect>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GRPCEffectiveRoutingRule {
    pub route_matcher: GRPCRouteRulesMatches,
    pub backends: Vec<Backend>,
    pub name: String,
    pub hostnames: Vec<String>,

    pub request_headers: FilterHeaders,
    pub response_headers: FilterHeaders,
}
impl PartialOrd for EffectiveRoutingRule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Self::compare_matching(&self.route_matcher, &other.route_matcher))
    }
}

impl EffectiveRoutingRule {
    fn header_matching(this: &HTTPRouteRulesMatches, other: &HTTPRouteRulesMatches) -> std::cmp::Ordering {
        match (this.headers.as_ref(), other.headers.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_headers), Some(other_headers)) => other_headers.len().cmp(&this_headers.len()),
        }
    }

    fn query_matching(this: &HTTPRouteRulesMatches, other: &HTTPRouteRulesMatches) -> std::cmp::Ordering {
        match (this.query_params.as_ref(), other.query_params.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_query_params), Some(other_query_params)) => this_query_params.len().cmp(&other_query_params.len()),
        }
    }

    fn method_matching(this: &HTTPRouteRulesMatches, other: &HTTPRouteRulesMatches) -> std::cmp::Ordering {
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
    fn path_matching(this: &HTTPRouteRulesMatches, other: &HTTPRouteRulesMatches) -> std::cmp::Ordering {
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

    fn compare_matching(this: &HTTPRouteRulesMatches, other: &HTTPRouteRulesMatches) -> std::cmp::Ordering {
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

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum NotResolvedReason {
    Unknown,
    NotAllowedByListeners,
    NoMatchingListenerHostname,
    InvalidBackend,
    BackendNotFound,
    RefNotPermitted,
    NoMatchingParent,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum ResolutionStatus {
    Resolved,
    NotResolved(NotResolvedReason),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterHeaders {
    pub add: Vec<HttpHeader>,
    pub remove: Vec<String>,
    pub set: Vec<HttpHeader>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

impl From<&HTTPRouteRulesFiltersRequestHeaderModifierAdd> for HttpHeader {
    fn from(modifier: &HTTPRouteRulesFiltersRequestHeaderModifierAdd) -> Self {
        Self {
            name: modifier.name.clone(),
            value: modifier.value.clone(),
        }
    }
}

impl From<&HTTPRouteRulesFiltersRequestHeaderModifierSet> for HttpHeader {
    fn from(modifier: &HTTPRouteRulesFiltersRequestHeaderModifierSet) -> Self {
        Self {
            name: modifier.name.clone(),
            value: modifier.value.clone(),
        }
    }
}
