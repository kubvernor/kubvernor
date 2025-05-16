use std::{cmp, net::IpAddr};

use kube::ResourceExt;
use thiserror::Error;
use tracing::debug;

use super::{Backend, BackendServiceConfig, ResourceKey, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME};
use crate::controllers::ControllerError;
use gateway_api::{
    grpcroutes::{
        GRPCRoute, GRPCRouteParentRefs, GRPCRouteRules, GRPCRouteRulesFilters, GRPCRouteRulesFiltersRequestHeaderModifierAdd, GRPCRouteRulesFiltersRequestHeaderModifierSet, GRPCRouteRulesFiltersType,
        GRPCRouteRulesMatches,
    },
    httproutes::{
        HTTPRoute, HTTPRouteParentRefs, HTTPRouteRules, HTTPRouteRulesFilters, HTTPRouteRulesFiltersRequestHeaderModifierAdd, HTTPRouteRulesFiltersRequestHeaderModifierSet,
        HTTPRouteRulesFiltersRequestRedirect, HTTPRouteRulesFiltersType, HTTPRouteRulesMatches, HTTPRouteRulesMatchesPath, HTTPRouteRulesMatchesPathType,
    },
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
pub struct Route {
    pub config: RouteConfig,
}

impl Route {
    pub fn resource_key(&self) -> &ResourceKey {
        &self.config.resource_key
    }
    pub fn name(&self) -> &str {
        &self.config.resource_key.name
    }

    pub fn namespace(&self) -> &String {
        &self.config.resource_key.namespace
    }

    pub fn hostnames(&self) -> &[String] {
        &self.config.hostnames
    }

    pub fn parents(&self) -> Option<&Vec<RouteParentRefs>> {
        self.config.parents.as_ref()
    }

    pub fn backends(&self) -> Vec<&Backend> {
        self.config.backends()
    }

    pub fn config(&self) -> &RouteConfig {
        &self.config
    }

    pub fn resolution_status(&self) -> &ResolutionStatus {
        &self.config.resolution_status
    }

    pub fn config_mut(&mut self) -> &mut RouteConfig {
        &mut self.config
    }

    pub fn resolution_status_mut(&mut self) -> &mut ResolutionStatus {
        &mut self.config.resolution_status
    }

    pub fn route_type(&self) -> &RouteType {
        &self.config.route_type
    }
}

fn get_http_default_rules_matches() -> HTTPRouteRulesMatches {
    HTTPRouteRulesMatches {
        headers: Some(vec![]),
        method: None,
        path: Some(HTTPRouteRulesMatchesPath {
            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
            value: Some("/".to_owned()),
        }),
        query_params: None,
    }
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
                let mut matching_rules = rr.matching_rules.clone();
                if matching_rules.is_empty() {
                    matching_rules.push(get_http_default_rules_matches());
                }

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

        let config = RouteConfig {
            resource_key: key,
            parents: parents.map(|parents| parents.into_iter().map(RouteParentRefs::from).collect()),
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

fn get_grpc_default_rules_matches() -> GRPCRouteRulesMatches {
    GRPCRouteRulesMatches { headers: Some(vec![]), method: None }
}

impl TryFrom<&GRPCRoute> for Route {
    type Error = ControllerError;
    fn try_from(kube_route: &GRPCRoute) -> Result<Self, Self::Error> {
        let key = ResourceKey::from(kube_route);
        let parents = kube_route.spec.parent_refs.clone();
        let local_namespace = key.namespace.clone();

        let empty_rules: Vec<GRPCRouteRules> = vec![];
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
                    request_headers: FilterHeaders {
                        add: rr
                            .filters
                            .iter()
                            .filter_map(|f| {
                                if f.r#type == GRPCRouteRulesFiltersType::RequestHeaderModifier {
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
                                if f.r#type == GRPCRouteRulesFiltersType::RequestHeaderModifier {
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
                                if f.r#type == GRPCRouteRulesFiltersType::RequestHeaderModifier {
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
                })
            })
            .collect();

        let config = RouteConfig {
            resource_key: key,
            parents: parents.map(|parents| parents.into_iter().map(RouteParentRefs::from).collect()),
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
pub struct HTTPRoutingConfiguration {
    pub routing_rules: Vec<RoutingRule>,
    pub effective_routing_rules: Vec<EffectiveRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct GRPCRoutingConfiguration {
    pub routing_rules: Vec<GRPCRoutingRule>,
    pub effective_routing_rules: Vec<GRPCEffectiveRoutingRule>,
}

#[derive(Clone, Debug)]
pub enum RouteType {
    Http(HTTPRoutingConfiguration),
    Grpc(GRPCRoutingConfiguration),
}

#[derive(Clone, Debug)]
pub struct RouteConfig {
    pub resource_key: ResourceKey,
    parents: Option<Vec<RouteParentRefs>>,
    hostnames: Vec<String>,
    pub resolution_status: ResolutionStatus,
    pub route_type: RouteType,
}

impl RouteConfig {
    pub fn backends(&self) -> Vec<&Backend> {
        match &self.route_type {
            RouteType::Http(routing_rules_configuration) => routing_rules_configuration.routing_rules.iter().flat_map(|r| &r.backends).collect(),
            RouteType::Grpc(routing_rules_configuration) => routing_rules_configuration.routing_rules.iter().flat_map(|r| &r.backends).collect(),
        }
    }
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

impl RouteConfig {
    pub fn reorder_routes(&mut self) {
        match &mut self.route_type {
            RouteType::Http(HTTPRoutingConfiguration {
                routing_rules: _,
                effective_routing_rules,
            }) => effective_routing_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less)),
            RouteType::Grpc(GRPCRoutingConfiguration {
                routing_rules: _,
                effective_routing_rules,
            }) => effective_routing_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less)),
        }
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

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GRPCEffectiveRoutingRule {
    pub route_matcher: GRPCRouteRulesMatches,
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
    fn header_matching(this: &GRPCRouteRulesMatches, other: &GRPCRouteRulesMatches) -> std::cmp::Ordering {
        match (this.headers.as_ref(), other.headers.as_ref()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_headers), Some(other_headers)) => other_headers.len().cmp(&this_headers.len()),
        }
    }

    fn method_matching(this: &GRPCRouteRulesMatches, other: &GRPCRouteRulesMatches) -> std::cmp::Ordering {
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

    fn compare_matching(this: &GRPCRouteRulesMatches, other: &GRPCRouteRulesMatches) -> std::cmp::Ordering {
        let method_match = Self::method_matching(this, other);
        let header_match = Self::header_matching(this, other);

        let result = if header_match == std::cmp::Ordering::Equal { method_match } else { header_match };

        debug!("Comparing {this:#?} {other:#?} {result:?} {header_match:?} {method_match:?}");
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

impl From<&GRPCRouteRulesFiltersRequestHeaderModifierAdd> for HttpHeader {
    fn from(modifier: &GRPCRouteRulesFiltersRequestHeaderModifierAdd) -> Self {
        Self {
            name: modifier.name.clone(),
            value: modifier.value.clone(),
        }
    }
}

impl From<&GRPCRouteRulesFiltersRequestHeaderModifierSet> for HttpHeader {
    fn from(modifier: &GRPCRouteRulesFiltersRequestHeaderModifierSet) -> Self {
        Self {
            name: modifier.name.clone(),
            value: modifier.value.clone(),
        }
    }
}
