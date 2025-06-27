pub mod grpc_route;
pub mod http_route;

use std::cmp;

use gateway_api::common::{HTTPHeader, HeaderModifier, HeaderMatch, ParentReference};
pub use grpc_route::GRPCEffectiveRoutingRule;
pub use http_route::HTTPEffectiveRoutingRule;
use thiserror::Error;
use typed_builder::TypedBuilder;

use super::{Backend, BackendServiceConfig, ResourceKey, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME};
use crate::common::route::{grpc_route::GRPCRoutingConfiguration, http_route::HTTPRoutingConfiguration};

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

    pub fn parents(&self) -> Option<&Vec<ParentReference>> {
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

#[derive(Clone, Debug)]
pub enum RouteType {
    Http(HTTPRoutingConfiguration),
    Grpc(GRPCRoutingConfiguration),
}

#[derive(Clone, Debug)]
pub struct RouteConfig {
    pub resource_key: ResourceKey,
    parents: Option<Vec<ParentReference>>,
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

fn get_add_headers(modifier: Option<&HeaderModifier>) -> Option<&Vec<HTTPHeader>> {
    if let Some(modifier) = modifier {
        modifier.add.as_ref()
    } else {
        None
    }
}

fn get_set_headers(modifier: Option<&HeaderModifier>) -> Option<&Vec<HTTPHeader>> {
    if let Some(modifier) = modifier {
        modifier.set.as_ref()
    } else {
        None
    }
}

fn get_remove_headers(modifier: Option<&HeaderModifier>) -> Option<&Vec<String>> {
    if let Some(modifier) = modifier {
        modifier.remove.as_ref()
    } else {
        None
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterHeaders {
    pub add: Vec<HTTPHeader>,
    pub remove: Vec<String>,
    pub set: Vec<HTTPHeader>,
}

struct Comparator<'a, T> {
    this: Option<&'a Vec<T>>,
    other: Option<&'a Vec<T>>,
}

impl<T> Comparator<'_, T> {
    pub fn compare(self) -> std::cmp::Ordering {
        match (self.this, self.other) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(this_headers), Some(other_headers)) => other_headers.len().cmp(&this_headers.len()),
        }
    }
}

#[derive(TypedBuilder)]
struct HeaderComparator<'a> {
    this: Option<&'a Vec<HeaderMatch>>,
    other: Option<&'a Vec<HeaderMatch>>,
}

impl HeaderComparator<'_> {
    pub fn compare_headers(self) -> std::cmp::Ordering {
        let comp = Comparator { this: self.this, other: self.other };
        comp.compare()
    }
}

#[derive(TypedBuilder)]
struct QueryComparator<'a> {
    this: Option<&'a Vec<HeaderMatch>>,
    other: Option<&'a Vec<HeaderMatch>>,
}
impl QueryComparator<'_> {
    pub fn compare_queries(self) -> std::cmp::Ordering {
        let comp = Comparator { this: self.this, other: self.other };
        comp.compare()
    }
}
