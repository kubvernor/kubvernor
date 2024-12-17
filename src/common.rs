use std::{
    cmp,
    collections::{btree_map, BTreeMap, BTreeSet, HashMap},
    fmt::Display,
    net::IpAddr,
    sync::Arc,
};

use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{self, Gateway as KubeGateway, GatewayListeners},
    httproutes::{
        HTTPRoute, HTTPRouteParentRefs, HTTPRouteRules, HTTPRouteRulesBackendRefs, HTTPRouteRulesFilters, HTTPRouteRulesFiltersRequestHeaderModifierAdd, HTTPRouteRulesFiltersRequestHeaderModifierSet,
        HTTPRouteRulesFiltersRequestRedirect, HTTPRouteRulesFiltersType, HTTPRouteRulesMatches,
    },
};
use kube::{Resource, ResourceExt};
use kube_core::ObjectMeta;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, Span};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::{
    controllers::ControllerError,
    patchers::{FinalizerContext, Operation},
};

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
    #[error("Unknown tls mode")]
    UnknownTlsMode,
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum GatewayError {
    #[error("Conversion problem")]
    ConversionProblem(String),
}

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
#[derive(Debug, Clone, PartialEq, PartialOrd, Hash, Eq)]
pub enum ProtocolType {
    Http,
    Https,
    Tcp,
    Tls,
    Udp,
}

impl TryFrom<&String> for ProtocolType {
    type Error = ControllerError;

    fn try_from(value: &String) -> Result<Self, Self::Error> {
        Ok(match value.to_uppercase().as_str() {
            "HTTP" => Self::Http,
            "HTTPS" => Self::Https,
            "TCP" => Self::Tcp,
            "TLS" => Self::Tls,
            "UDP" => Self::Udp,
            _ => return Err(ControllerError::InvalidPayload("Wrong protocol".to_owned())),
        })
    }
}

impl Display for ProtocolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut e = format! {"{self:?}"};
        e.make_ascii_uppercase();
        write!(f, "{e}")
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListenerConfig {
    pub name: String,
    pub port: i32,
    pub hostname: Option<String>,
    pub tls_type: Option<TlsType>,
}

impl PartialOrd for ListenerConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.name.partial_cmp(&other.name) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        match self.port.partial_cmp(&other.port) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        self.hostname.partial_cmp(&other.hostname)
    }
}

impl ListenerConfig {
    pub fn new(name: String, port: i32, hostname: Option<String>) -> Self {
        Self { name, port, hostname, tls_type: None }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Listener {
    Http(ListenerData),
    Https(ListenerData),
    Tcp(ListenerData),
    Tls(ListenerData),
    Udp(ListenerData),
}

impl Listener {
    pub fn name(&self) -> &str {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => {
                listener_data.config.name.as_str()
            }
        }
    }

    pub fn port(&self) -> i32 {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => listener_data.config.port,
        }
    }

    pub fn protocol(&self) -> ProtocolType {
        match self {
            Listener::Http(_) => ProtocolType::Http,
            Listener::Https(_) => ProtocolType::Https,
            Listener::Tcp(_) => ProtocolType::Tcp,
            Listener::Tls(_) => ProtocolType::Tls,
            Listener::Udp(_) => ProtocolType::Udp,
        }
    }
    pub fn hostname(&self) -> Option<&String> {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => {
                listener_data.config.hostname.as_ref()
            }
        }
    }

    pub fn config(&self) -> &ListenerConfig {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => &listener_data.config,
        }
    }

    pub fn conditions(&self) -> impl Iterator<Item = &ListenerCondition> {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => {
                listener_data.conditions.iter()
            }
        }
    }

    pub fn conditions_mut(&mut self) -> &mut ListenerConditions {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => {
                &mut listener_data.conditions
            }
        }
    }

    pub fn data_mut(&mut self) -> &mut ListenerData {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => listener_data,
        }
    }

    pub fn routes(&self) -> (Vec<&Route>, Vec<&Route>) {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => (
                Vec::from_iter(&listener_data.resolved_routes),
                Vec::from_iter(&listener_data.unresolved_routes),
                //Vec::from_iter(&listener_data.routes_with_no_listeners),
            ),
        }
    }

    pub fn update_routes(&mut self, resolved_routes: BTreeSet<Route>, unresolved_routes: BTreeSet<Route>) {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => {
                if !unresolved_routes.is_empty() {
                    if let Some(route) = unresolved_routes.first() {
                        if *route.resolution_status() == ResolutionStatus::NotResolved(NotResolvedReason::RefNotPermitted) {
                            listener_data
                                .conditions
                                .replace(ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidBackend(APPROVED_ROUTES.iter().map(|s| (*s).to_owned()).collect())));
                        }
                    } else {
                        listener_data.conditions.replace(ListenerCondition::UnresolvedRouteRefs);
                    }
                }

                listener_data.attached_routes = unresolved_routes.len() + resolved_routes.len();
                listener_data.resolved_routes = resolved_routes;
                listener_data.unresolved_routes = unresolved_routes;
            }
        }
    }

    pub fn attached_routes(&self) -> usize {
        match self {
            Listener::Http(listener_data) | Listener::Https(listener_data) | Listener::Tcp(listener_data) | Listener::Tls(listener_data) | Listener::Udp(listener_data) => {
                listener_data.attached_routes
            }
        }
    }

    pub fn effective_matching_rules(&self) -> Vec<&EffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes.iter().chain(unresolved.iter()).flat_map(|r| r.effective_routing_rules()).collect();
        matching_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
        //matching_rules.reverse();
        matching_rules
    }
}

pub type ListenerConditions = BTreeSet<ListenerCondition>;

#[derive(Clone, Default, Debug, PartialEq, PartialOrd)]
pub struct ListenerData {
    pub config: ListenerConfig,
    pub conditions: ListenerConditions,
    pub resolved_routes: BTreeSet<Route>,
    pub unresolved_routes: BTreeSet<Route>,
    //pub routes_with_no_listeners: BTreeSet<Route>,
    pub attached_routes: usize,
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
pub enum Certificate {
    Resolved(ResourceKey),
    NotResolved(ResourceKey),
    Invalid(ResourceKey),
}

impl Certificate {
    pub fn resolve(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::Resolved(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::Resolved(resource.clone())
    }

    pub fn not_resolved(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::Resolved(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::NotResolved(resource.clone())
    }

    pub fn invalid(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::Resolved(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::Invalid(resource.clone())
    }

    pub fn resouce_key(&self) -> &ResourceKey {
        match self {
            Certificate::Resolved(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum TlsType {
    Terminate(Vec<Certificate>),
    Passthrough,
}

impl TryFrom<&GatewayListeners> for Listener {
    type Error = ListenerError;

    fn try_from(gateway_listener: &GatewayListeners) -> std::result::Result<Self, Self::Error> {
        let mut config = ListenerConfig::new(gateway_listener.name.clone(), gateway_listener.port, gateway_listener.hostname.clone());

        let condition = validate_allowed_routes(gateway_listener);

        let mut listener_conditions = ListenerConditions::new();
        _ = listener_conditions.replace(condition);

        let maybe_tls_config = gateway_listener
            .tls
            .as_ref()
            .map(|tls| match tls.mode {
                Some(gateways::GatewayListenersTlsMode::Passthrough) => Ok(TlsType::Passthrough),
                Some(gateways::GatewayListenersTlsMode::Terminate) => {
                    let secrets = tls
                        .certificate_refs
                        .as_ref()
                        .map(|refs| {
                            refs.iter()
                                .map(|r| Certificate::NotResolved(ResourceKey::from((r.group.clone(), r.namespace.clone(), r.name.clone(), r.kind.clone()))))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Ok(TlsType::Terminate(secrets))
                }
                None => Err(ListenerError::UnknownTlsMode),
            })
            .transpose();

        match maybe_tls_config {
            Ok(tls) => config.tls_type = tls,
            Err(e) => return Err(e),
        }

        let listener_data = ListenerData {
            config,
            conditions: listener_conditions,
            resolved_routes: BTreeSet::new(),
            unresolved_routes: BTreeSet::new(),
            attached_routes: 0,
        };

        match gateway_listener.protocol.as_str() {
            "HTTP" => Ok(Self::Http(listener_data)),
            "HTTPS" => Ok(Self::Https(listener_data)),
            "TCP" => Ok(Self::Tcp(listener_data)),
            "TLS" => Ok(Self::Tls(listener_data)),
            "UDP" => Ok(Self::Udp(listener_data)),
            _ => Err(ListenerError::UnknownProtocol(gateway_listener.protocol.clone())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct BackendServiceConfig {
    pub resource_key: ResourceKey,
    pub endpoint: String,
    pub port: i32,
    pub effective_port: i32,
    pub weight: i32,
}

impl BackendServiceConfig {
    pub fn cluster_name(&self) -> String {
        self.resource_key.name.clone() + "." + &self.resource_key.namespace
    }
    pub fn weight(&self) -> i32 {
        self.weight
    }
}

#[derive(Clone, Debug)]
pub struct RoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
    pub matching_rules: Vec<HTTPRouteRulesMatches>,
    pub filters: Vec<HTTPRouteRulesFilters>,
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

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum Backend {
    Resolved(BackendServiceConfig),
    Unresolved(BackendServiceConfig),
    NotAllowed(BackendServiceConfig),
    Maybe(BackendServiceConfig),
    Invalid(BackendServiceConfig),
}

impl Backend {
    pub fn config(&self) -> &BackendServiceConfig {
        match self {
            Backend::Resolved(s) | Backend::Unresolved(s) | Backend::NotAllowed(s) | Backend::Maybe(s) | Backend::Invalid(s) => s,
        }
    }
    pub fn cluster_name(&self) -> String {
        self.config().cluster_name()
    }

    pub fn weight(&self) -> i32 {
        self.config().weight()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterHeaders {
    pub add: Vec<HttpHeader>,
    pub remove: Vec<String>,
    pub set: Vec<HttpHeader>,
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

#[derive(Clone, Debug)]
pub struct RouteConfig {
    pub resource_key: ResourceKey,
    parents: Option<Vec<HTTPRouteParentRefs>>,
    pub routing_rules: Vec<RoutingRule>,
    hostnames: Vec<String>,
    pub resolution_status: ResolutionStatus,
    pub effective_routing_rules: Vec<EffectiveRoutingRule>,
}

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

#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
pub enum Route {
    Http(RouteConfig),
    Grpc(RouteConfig),
}

impl Route {
    pub fn resource_key(&self) -> &ResourceKey {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.resource_key,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.resource_key.name,
        }
    }

    pub fn namespace(&self) -> &String {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.resource_key.namespace,
        }
    }
    pub fn parents(&self) -> Option<&Vec<HTTPRouteParentRefs>> {
        match self {
            Route::Http(c) | Route::Grpc(c) => c.parents.as_ref(),
        }
    }

    pub fn routing_rules(&self) -> &[RoutingRule] {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.routing_rules,
        }
    }

    pub fn hostnames(&self) -> &[String] {
        match self {
            Route::Http(config) | Route::Grpc(config) => &config.hostnames,
        }
    }
    pub fn config(&self) -> &RouteConfig {
        match self {
            Route::Http(config) | Route::Grpc(config) => config,
        }
    }

    pub fn resolution_status(&self) -> &ResolutionStatus {
        match self {
            Route::Http(config) | Route::Grpc(config) => &config.resolution_status,
        }
    }

    pub fn config_mut(&mut self) -> &mut RouteConfig {
        match self {
            Route::Http(config) | Route::Grpc(config) => config,
        }
    }

    pub fn effective_routing_rules(&self) -> &[EffectiveRoutingRule] {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.effective_routing_rules,
        }
    }

    pub fn resolution_status_mut(&mut self) -> &mut ResolutionStatus {
        match self {
            Route::Http(config) | Route::Grpc(config) => &mut config.resolution_status,
        }
    }
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
            parents,
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

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum GatewayAddress {
    Hostname(String),
    IPAddress(IpAddr),
    NamedAddress(String),
}

#[derive(Clone, Debug)]
pub struct Gateway {
    id: Uuid,
    resource_key: ResourceKey,
    addresses: BTreeSet<GatewayAddress>,
    listeners: BTreeMap<String, Listener>,
    orphaned_routes: BTreeSet<Route>,
}

impl PartialEq for Gateway {
    fn eq(&self, other: &Self) -> bool {
        self.resource_key == other.resource_key
    }
}

impl PartialOrd for Gateway {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.resource_key.partial_cmp(&other.resource_key)
    }
}

impl Gateway {
    pub fn name(&self) -> &str {
        &self.resource_key.name
    }
    pub fn namespace(&self) -> &str {
        &self.resource_key.namespace
    }
    pub fn key(&self) -> &ResourceKey {
        &self.resource_key
    }

    pub fn id(&self) -> &Uuid {
        &self.id
    }
    pub fn listeners(&self) -> btree_map::Values<'_, String, Listener> {
        self.listeners.values()
    }

    pub fn listeners_mut(&mut self) -> btree_map::ValuesMut<'_, String, Listener> {
        self.listeners.values_mut()
    }

    pub fn listener(&self, name: &str) -> Option<&Listener> {
        self.listeners.get(name)
    }

    pub fn listener_mut(&mut self, name: &str) -> Option<&mut Listener> {
        self.listeners.get_mut(name)
    }

    pub fn addresses_mut(&mut self) -> &mut BTreeSet<GatewayAddress> {
        &mut self.addresses
    }

    pub fn addresses(&self) -> &BTreeSet<GatewayAddress> {
        &self.addresses
    }

    pub fn routes(&self) -> (BTreeSet<&Route>, BTreeSet<&Route>) {
        let mut resolved_routes = BTreeSet::new();
        let mut unresolved_routes = BTreeSet::new();

        for l in self.listeners.values() {
            let (resolved, unresolved) = l.routes();
            resolved_routes.append(&mut BTreeSet::from_iter(resolved));
            unresolved_routes.append(&mut BTreeSet::from_iter(unresolved));
        }
        (resolved_routes, unresolved_routes)
    }

    pub fn effective_matching_rules(&self) -> Vec<&EffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes.iter().chain(unresolved.iter()).flat_map(|r| r.effective_routing_rules()).collect();
        matching_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
        //matching_rules.reverse();
        matching_rules
    }

    pub fn orphaned_routes_mut(&mut self) -> &mut BTreeSet<Route> {
        &mut self.orphaned_routes
    }

    pub fn orphaned_routes(&self) -> &BTreeSet<Route> {
        &self.orphaned_routes
    }
}

impl TryFrom<&KubeGateway> for Gateway {
    type Error = GatewayError;

    fn try_from(gateway: &KubeGateway) -> std::result::Result<Self, Self::Error> {
        let id = Uuid::parse_str(&gateway.metadata.uid.clone().unwrap_or_default()).map_err(|_| GatewayError::ConversionProblem("Can't parse uuid".to_owned()))?;
        let resource_key = ResourceKey::from(gateway);

        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) = VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));
        if !listener_validation_errrors.is_empty() {
            return Err(GatewayError::ConversionProblem("Misconfigured listeners".to_owned()));
        }

        Ok(Self {
            id,
            resource_key,
            addresses: BTreeSet::new(),
            listeners: listeners.into_iter().map(|l| (l.name().to_owned(), l)).collect::<BTreeMap<String, Listener>>(),
            orphaned_routes: BTreeSet::new(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Label {
    label: String,
    value: String,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Annotation {
    label: String,
    value: String,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct DeployedGatewayStatus {
    pub id: Uuid,
    pub name: String,
    pub namespace: String,
    pub attached_addresses: Vec<String>,
}

#[derive(Debug)]
pub struct RouteProcessedPayload {
    pub route_status: RouteStatus,
    pub deployed_gateway_status: DeployedGatewayStatus,
}

impl RouteProcessedPayload {
    pub fn new(status: RouteStatus, gateway_status: DeployedGatewayStatus) -> Self {
        Self {
            route_status: status,
            deployed_gateway_status: gateway_status,
        }
    }
}

#[derive(Debug)]
pub enum GatewayResponse {
    GatewayProcessed(Gateway),
    GatewayDeleted(Vec<RouteStatus>),
    RouteProcessed(RouteProcessedPayload),
    GatewayProcessingError,
    RouteProcessingError,
}

#[derive(Debug, Clone)]
pub struct RouteToListenersMapping {
    pub route: Route,
    pub listeners: Vec<GatewayListeners>,
}

impl RouteToListenersMapping {
    pub fn new(route: Route, listeners: Vec<GatewayListeners>) -> Self {
        Self { route, listeners }
    }
}

impl Display for RouteToListenersMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "route {} -> [{}]",
            self.route.name(),
            self.listeners
                .iter()
                .fold(String::new(), |acc, l| { acc + &format!("  Listener(name: {} port: {}), ", &l.name, l.port) })
        )
    }
}

#[derive(Debug, TypedBuilder)]
pub struct ChangedContext {
    pub span: Span,
    pub response_sender: oneshot::Sender<GatewayResponse>,
    pub gateway: Gateway,
}

impl Display for ChangedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gateway {}.{} listeners = {} ", self.gateway.name(), self.gateway.namespace(), self.gateway.listeners.len(),)
    }
}

#[derive(Debug, TypedBuilder)]
pub struct DeletedContext {
    pub span: Span,
    pub response_sender: oneshot::Sender<GatewayResponse>,
    pub gateway: Gateway,
}

#[derive(Debug)]
pub enum BackendGatewayEvent {
    GatewayChanged(ChangedContext),
    GatewayDeleted(DeletedContext),
    RouteChanged(ChangedContext),
}

impl Display for BackendGatewayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendGatewayEvent::GatewayChanged(ctx) => write!(
                f,
                "GatewayEvent::GatewayChanged
                {ctx}"
            ),
            BackendGatewayEvent::GatewayDeleted(ctx) => {
                write!(
                    f,
                    "GatewayEvent::GatewayDeleted
                gateway {:?}",
                    ctx.gateway
                )
            }

            BackendGatewayEvent::RouteChanged(ctx) => {
                write!(
                    f,
                    "GatewayEvent::RouteChanged                 
                    {ctx}"
                )
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BackendResourceKey {
    pub group: String,
    namespace: Option<String>,
    pub name: String,
    pub kind: String,
}
impl BackendResourceKey {
    pub fn namespace(&self) -> Option<&String> {
        self.namespace.as_ref()
    }

    pub fn namespace_mut(&mut self) -> &mut Option<String> {
        &mut self.namespace
    }
}

#[allow(dead_code)]
impl ResourceKey {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Default::default()
        }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Default)]
pub struct RouteRefKey {
    pub resource_key: ResourceKey,
    pub section_name: Option<String>,
    pub port: Option<i32>,
}

#[allow(dead_code)]
impl RouteRefKey {
    pub fn new(name: &str) -> Self {
        Self {
            resource_key: ResourceKey::new(name),
            ..Default::default()
        }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self {
            resource_key: ResourceKey::namespaced(name, namespace),
            ..Default::default()
        }
    }
}

impl AsRef<ResourceKey> for RouteRefKey {
    fn as_ref(&self) -> &ResourceKey {
        &self.resource_key
    }
}

pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";
pub const DEFAULT_ROUTE_HOSTNAME: &str = "*";
pub const KUBERNETES_NONE: &str = "None";

impl Default for ResourceKey {
    fn default() -> Self {
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: DEFAULT_NAMESPACE_NAME.to_owned(),
            name: String::default(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.namespace, self.name)
    }
}

impl From<&ObjectMeta> for ResourceKey {
    fn from(value: &ObjectMeta) -> Self {
        let namespace = value.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let name = match (value.name.as_ref(), value.generate_name.as_ref()) {
            (None, None) => "",
            (Some(name), _) | (None, Some(name)) => name,
        };
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: name.to_owned(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}
impl From<(Option<String>, Option<String>, String, Option<String>)> for ResourceKey {
    fn from((group, namespace, name, kind): (Option<String>, Option<String>, String, Option<String>)) -> Self {
        let namespace = namespace.unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());
        Self {
            group: group.unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
            namespace,
            name: name.clone(),
            kind: kind.unwrap_or(DEFAULT_KIND_NAME.to_owned()),
        }
    }
}

// impl From<(&HTTPRouteParentRefs, String)> for ResourceKey {
//     fn from((route_parent, route_namespace): (&HTTPRouteParentRefs, String)) -> Self {
//         Self {
//             group: route_parent.group.clone().unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
//             namespace: route_parent.namespace.clone().unwrap_or(route_namespace),
//             name: route_parent.name.clone(),
//             kind: route_parent.kind.clone().unwrap_or(DEFAULT_KIND_NAME.to_owned()),
//         }
//     }
// }

impl From<(&HTTPRouteParentRefs, String)> for RouteRefKey {
    fn from((route_parent, route_namespace): (&HTTPRouteParentRefs, String)) -> Self {
        Self {
            resource_key: ResourceKey {
                group: route_parent.group.clone().unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
                namespace: route_parent.namespace.clone().unwrap_or(route_namespace),
                name: route_parent.name.clone(),
                kind: route_parent.kind.clone().unwrap_or(DEFAULT_KIND_NAME.to_owned()),
            },
            section_name: route_parent.section_name.clone(),
            port: route_parent.port,
        }
    }
}

impl From<&GatewayClass> for ResourceKey {
    fn from(value: &GatewayClass) -> Self {
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
            name: value.name_any().clone(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl From<&gateways::Gateway> for ResourceKey {
    fn from(value: &gateways::Gateway) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name_any(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl From<&HTTPRoute> for ResourceKey {
    fn from(value: &HTTPRoute) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name_any(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl From<(&HTTPRouteRulesBackendRefs, String)> for ResourceKey {
    fn from((value, gateway_namespace): (&HTTPRouteRulesBackendRefs, String)) -> Self {
        let namespace = value.namespace.clone().unwrap_or(gateway_namespace);

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name.clone(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl From<&HTTPRouteRulesBackendRefs> for BackendResourceKey {
    fn from(value: &HTTPRouteRulesBackendRefs) -> Self {
        let namespace = value.namespace.clone();

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name.clone(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

pub struct VerifiyItems;

impl VerifiyItems {
    #[allow(clippy::unwrap_used)]
    pub fn verify<I, E>(iter: impl Iterator<Item = std::result::Result<I, E>>) -> (Vec<I>, Vec<E>)
    where
        I: std::fmt::Debug,
        E: std::fmt::Debug,
    {
        let (good, bad): (Vec<_>, Vec<_>) = iter.partition(std::result::Result::is_ok);
        let good: Vec<_> = good.into_iter().map(|i| i.unwrap()).collect();
        let bad: Vec<_> = bad.into_iter().map(|i| i.unwrap_err()).collect();
        (good, bad)
    }
}

#[derive(Clone, Debug)]
#[repr(u8)]
pub enum ResolvedRefs {
    Resolved(Vec<String>),
    ResolvedWithNotAllowedRoutes(Vec<String>),
    InvalidAllowedRoutes,
    InvalidCertificates(Vec<String>),
    InvalidBackend(Vec<String>),
}

impl ResolvedRefs {
    fn discriminant(&self) -> u8 {
        unsafe { *<*const _>::from(self).cast::<u8>() }
    }
}

impl PartialOrd for ResolvedRefs {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ResolvedRefs {
    fn eq(&self, other: &Self) -> bool {
        let self_disc = self.discriminant();
        let other_disc = other.discriminant();
        self_disc == other_disc
    }
}

impl Ord for ResolvedRefs {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let self_disc = self.discriminant();
        let other_disc = other.discriminant();
        self_disc.cmp(&other_disc)
    }
}

impl Eq for ResolvedRefs {}

#[derive(Clone, Debug)]
pub enum ListenerCondition {
    ResolvedRefs(ResolvedRefs),
    UnresolvedRouteRefs,
    Accepted,
    NotAccepted,
    Programmed,
    NotProgrammed,
}

impl ListenerCondition {
    fn discriminant(&self) -> u8 {
        match self {
            ListenerCondition::ResolvedRefs(_) => 0,
            ListenerCondition::Accepted | ListenerCondition::NotAccepted => 1,
            ListenerCondition::Programmed | ListenerCondition::NotProgrammed => 2,
            ListenerCondition::UnresolvedRouteRefs => 3,
        }
    }
}

impl PartialOrd for ListenerCondition {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ListenerCondition {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let self_disc = self.discriminant();
        let other_disc = other.discriminant();
        self_disc.cmp(&other_disc)
    }
}

impl Eq for ListenerCondition {}

impl PartialEq for ListenerCondition {
    fn eq(&self, other: &Self) -> bool {
        core::mem::discriminant(self) == core::mem::discriminant(other)
    }
}

impl ListenerCondition {
    pub fn resolved_type(
        &self,
    ) -> (
        &'static str,
        gateway_api::apis::standard::constants::ListenerConditionType,
        gateway_api::apis::standard::constants::ListenerConditionReason,
    ) {
        match self {
            ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes | ResolvedRefs::ResolvedWithNotAllowedRoutes(_)) => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::InvalidRouteKinds,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidBackend(_)) => (
                "True",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::InvalidRouteKinds,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(_)) => (
                "True",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(_)) => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::InvalidCertificateRef,
            ),

            ListenerCondition::UnresolvedRouteRefs => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs,
            ),

            ListenerCondition::Accepted => (
                "True",
                gateway_api::apis::standard::constants::ListenerConditionType::Accepted,
                gateway_api::apis::standard::constants::ListenerConditionReason::Accepted,
            ),
            ListenerCondition::NotAccepted => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::Accepted,
                gateway_api::apis::standard::constants::ListenerConditionReason::Accepted,
            ),
            ListenerCondition::Programmed => (
                "True",
                gateway_api::apis::standard::constants::ListenerConditionType::Programmed,
                gateway_api::apis::standard::constants::ListenerConditionReason::Programmed,
            ),

            ListenerCondition::NotProgrammed => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::Programmed,
                gateway_api::apis::standard::constants::ListenerConditionReason::Programmed,
            ),
        }
    }
    pub fn supported_routes(&self) -> Vec<String> {
        match self {
            ListenerCondition::ResolvedRefs(
                ResolvedRefs::Resolved(supported_routes) | ResolvedRefs::ResolvedWithNotAllowedRoutes(supported_routes) | ResolvedRefs::InvalidCertificates(supported_routes),
            ) => supported_routes.clone(),
            _ => APPROVED_ROUTES.iter().map(|r| (*r).to_owned()).collect(),
        }
    }
}

const APPROVED_ROUTES: [&str; 2] = ["HTTPRoute", "TCPRoute"];

fn validate_allowed_routes(gateway_listeners: &GatewayListeners) -> ListenerCondition {
    if let Some(ar) = gateway_listeners.allowed_routes.as_ref() {
        if let Some(kinds) = ar.kinds.as_ref() {
            let cloned_kinds = kinds.clone().into_iter().map(|k| k.kind);
            let (supported, invalid): (Vec<_>, Vec<_>) = cloned_kinds.partition(|f| APPROVED_ROUTES.contains(&f.as_str()));

            match (supported.is_empty(), invalid.is_empty()) {
                (_, true) => ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(supported)),
                (true, false) => ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes),
                (false, false) => ListenerCondition::ResolvedRefs(ResolvedRefs::ResolvedWithNotAllowedRoutes(supported)),
            }
        } else if gateway_listeners.protocol == "HTTP" || gateway_listeners.protocol == "HTTPS" {
            ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec!["HTTPRoute".to_owned()]))
        } else {
            ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![]))
        }
    } else if gateway_listeners.protocol == "HTTP" || gateway_listeners.protocol == "HTTPS" {
        ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec!["HTTPRoute".to_owned()]))
    } else {
        ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![]))
    }
}

pub fn calculate_attached_routes(mapped_routes: &[RouteToListenersMapping]) -> HashMap<String, BTreeSet<&Route>> {
    let mut attached_routes: HashMap<String, BTreeSet<&Route>> = HashMap::new();

    for mapping in mapped_routes {
        mapping.listeners.iter().for_each(|l| {
            if let Some(routes) = attached_routes.get_mut(&l.name) {
                routes.insert(&mapping.route);
            } else {
                let mut routes = BTreeSet::new();
                routes.insert(&mapping.route);
                attached_routes.insert(l.name.clone(), routes);
            }
        });
    }
    attached_routes
}

#[derive(TypedBuilder)]
pub struct RequestContext {
    pub gateway: Gateway,
    pub kube_gateway: Arc<KubeGateway>,
    pub gateway_class_name: String,
    pub span: Span,
}
pub enum ReferenceResolveRequest {
    New(RequestContext),
    Remove(Gateway),
}

pub enum GatewayDeployRequest {
    Deploy(RequestContext),
}

pub async fn add_finalizer(sender: &mpsc::Sender<Operation<KubeGateway>>, gateway_id: &ResourceKey, controller_name: &str) {
    let _ = sender
        .send(Operation::PatchFinalizer(FinalizerContext {
            resource_key: gateway_id.clone(),
            controller_name: controller_name.to_owned(),
            finalizer_name: controller_name.to_owned(),
            span: Span::current().clone(),
        }))
        .await;
}

const GATEWAY_CLASS_FINALIZER_NAME: &str = "gateway-exists-finalizer.gateway.networking.k8s.io";

pub async fn add_finalizer_to_gateway_class(sender: &mpsc::Sender<Operation<GatewayClass>>, gateway_class_name: &str, controller_name: &str) {
    let key = ResourceKey::new(gateway_class_name);
    let _ = sender
        .send(Operation::PatchFinalizer(FinalizerContext {
            resource_key: key,
            controller_name: controller_name.to_owned(),
            finalizer_name: GATEWAY_CLASS_FINALIZER_NAME.to_owned(),
            span: Span::current().clone(),
        }))
        .await;
}

#[cfg(test)]
mod test {

    use std::collections::BTreeSet;

    use gateway_api::apis::standard::httproutes::{HTTPRoute, HTTPRouteRules, HTTPRouteRulesMatches, HTTPRouteRulesMatchesHeaders, HTTPRouteRulesMatchesPath, HTTPRouteRulesMatchesPathType};

    use super::{EffectiveRoutingRule, ListenerCondition};

    #[test]
    pub fn test_enums() {
        let r1 = super::ResolvedRefs::Resolved(vec!["blah".to_owned()]);
        let r2 = super::ResolvedRefs::Resolved(vec!["blah2".to_owned()]);
        let d1 = r1.discriminant();
        let d2 = r2.discriminant();
        println!("{d1} {d2} {:?}", d1.cmp(&d2));
        assert_eq!(d1, d2);
        let e1 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::Resolved(vec!["blah".to_owned()]));
        let e2 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::Resolved(vec!["blah2".to_owned()]));
        let e3 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::ResolvedWithNotAllowedRoutes(vec![]));
        let e4 = ListenerCondition::ResolvedRefs(super::ResolvedRefs::InvalidAllowedRoutes);
        let e5 = ListenerCondition::Accepted;
        assert_eq!(e1, e2);
        assert_eq!(e1, e3);
        assert_eq!(e1, e4);
        assert_ne!(e1, e5);
        let d1 = e1.discriminant();
        let d2 = e3.discriminant();

        println!("{d1:?} {d2:?} {:?}", d1.cmp(&d2));

        let mut set = BTreeSet::new();
        set.replace(e1);
        set.replace(e3);
        set.replace(e2);
        set.replace(e4);
        assert_eq!(set.len(), 1);
    }

    #[test]
    pub fn test_rule_matcher() {
        let m = r"
path:
  type: PathPrefix
  value: /v2
headers:
- name: version
  value: two
";
        let x: HTTPRouteRulesMatches = serde_yaml::from_str(m).unwrap();
        println!("{x:#?}");
    }

    #[test]
    pub fn test_route_rules() {
        let m = r"
matches:
  - path:
      type: PathPrefix
      value: /v2
  - headers:
    - name: version
      value: two
backendRefs:
  - name: infra-backend-v2
    port: 8080
";
        let x: HTTPRouteRules = serde_yaml::from_str(m).unwrap();
        println!("{x:#?}");
    }

    #[test]
    pub fn test_http_route() {
        let m = r"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: matching-part1
  namespace: gateway-conformance-infra
spec:
  parentRefs:
  - name: same-namespace
  hostnames:
  - example.com
  - example.net
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /
      headers:
      - name: version
        value: one        
      
    - headers:
      - name: version
        value: one
    backendRefs:
    - name: infra-backend-v1
      port: 8080
  - matches:
    - path:
        type: Exact
        value: blah
    - headers:
      - name: version
        value: three
    backendRefs:
    - name: infra-backend-v2
      port: 8080
";
        let x: HTTPRoute = serde_yaml::from_str(m).unwrap();
        println!("{x:#?}");
    }

    #[test]
    pub fn test_headers_sorting_rules() {
        let mut rules = vec![
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "version".to_owned(),
                        value: "one".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                name: "route1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "version".to_owned(),
                        value: "two".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                name: "route2".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![
                        HTTPRouteRulesMatchesHeaders {
                            name: "version".to_owned(),
                            value: "two".to_owned(),
                            ..Default::default()
                        },
                        HTTPRouteRulesMatchesHeaders {
                            name: "color".to_owned(),
                            value: "orange".to_owned(),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                },

                name: "route3".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "color".to_owned(),
                        value: "blue".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                name: "route4.1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "color".to_owned(),
                        value: "green".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },

                name: "route4.2".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "color".to_owned(),
                        value: "red".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },

                name: "route5.1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "color".to_owned(),
                        value: "yellow".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                name: "route5.2".to_owned(),
                ..Default::default()
            },
        ];
        let expected_names = vec!["route3", "route1", "route2", "route4.1", "route4.2", "route5.1", "route5.2"];
        println!("{rules:#?}");
        rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
        let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
        assert_eq!(expected_names, actual_names);
    }

    #[test]
    pub fn test_path_sorting_rules() {
        let mut rules = vec![
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route3".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/one".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/two".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route2".to_owned(),
                ..Default::default()
            },
        ];
        let expected_names = vec!["route1", "route2", "route3"];
        rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
        let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
        assert_eq!(expected_names, actual_names);
    }

    #[test]
    pub fn test_path_prefix_sorting_rules() {
        let mut rules = vec![
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route3".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/one".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/two".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route2".to_owned(),
                ..Default::default()
            },
        ];
        let expected_names = vec!["route1", "route2", "route3"];
        rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
        let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
        assert_eq!(expected_names, actual_names);
    }

    #[test]
    pub fn test_path_mixed_sorting_rules() {
        let mut rules = vec![
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route0".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/one".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/two".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route2".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route3".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/blah".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route4".to_owned(),
                ..Default::default()
            },
        ];
        let expected_names = vec!["route4", "route3", "route1", "route2", "route0"];
        rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
        let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
        assert_eq!(expected_names, actual_names);
    }

    #[test]
    pub fn test_paths_and_headers_sorting_rules() {
        let mut rules = vec![
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/".to_owned()),
                    }),
                    headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                        name: "color".to_owned(),
                        value: "green".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                name: "route2".to_owned(),
                ..Default::default()
            },
        ];
        let expected_names = vec!["route2", "route1"];
        rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
        let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
        assert_eq!(expected_names, actual_names);
    }

    #[test]
    pub fn test_path_sorting_rules_extended() {
        let mut rules = vec![
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/match/exact/one".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route3".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/match/exact".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route2".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                        value: Some("/match".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route1".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/match".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route4".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/match/prefix".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route5".to_owned(),
                ..Default::default()
            },
            EffectiveRoutingRule {
                route_matcher: HTTPRouteRulesMatches {
                    path: Some(HTTPRouteRulesMatchesPath {
                        r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/match/prefix/one".to_owned()),
                    }),
                    ..Default::default()
                },
                name: "route6".to_owned(),
                ..Default::default()
            },
        ];
        let expected_names = vec!["route3", "route2", "route1", "route6", "route5", "route4"];
        rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(std::cmp::Ordering::Less));
        let actual_names: Vec<_> = rules.iter().map(|r| &r.name).collect();
        assert_eq!(expected_names, actual_names);
    }
}
