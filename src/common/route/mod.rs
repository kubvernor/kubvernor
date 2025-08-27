pub mod grpc_route;
pub mod http_route;
use gateway_api::common::{HTTPHeader, HeaderModifier, ParentReference};
use thiserror::Error;

use super::{Backend, ResourceKey, ServiceTypeConfig, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME};
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
    pub hostnames: Vec<String>,
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

impl PartialEq for RouteConfig {
    fn eq(&self, other: &Self) -> bool {
        self.resource_key == other.resource_key
    }
}

impl PartialOrd for RouteConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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
