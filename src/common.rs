use std::{collections::BTreeSet, fmt::Display, sync::Arc};

use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{self, Gateway as KubeGateway, GatewayListeners},
    httproutes::{HTTPRoute, HTTPRouteParentRefs, HTTPRouteRules, HTTPRouteRulesMatches},
};
use kube::{Resource, ResourceExt};
use kube_core::ObjectMeta;
use thiserror::Error;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{controllers::ControllerError, state::State};

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum GatewayError {
    #[error("Conversion problem")]
    ConversionProblem(String),
}

#[derive(Clone, Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerStatus {
    Accepted((String, i32)),
    Conflicted(String),
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteStatus {
    Attached,
    Ignored,
}
impl std::fmt::Display for ListenerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
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
    pub certificates: Vec<ResourceKey>,
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
        Self {
            name,
            port,
            hostname,
            certificates: vec![],
        }
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
}

pub type ListenerConditions = BTreeSet<ListenerCondition>;

#[derive(Clone, Default, Debug, PartialEq, PartialOrd)]
pub struct ListenerData {
    pub config: ListenerConfig,
    pub conditions: ListenerConditions,
}

impl TryFrom<&GatewayListeners> for Listener {
    type Error = ListenerError;

    fn try_from(gateway_listener: &GatewayListeners) -> std::result::Result<Self, Self::Error> {
        let secrets = gateway_listener
            .tls
            .as_ref()
            .and_then(|tls| {
                tls.certificate_refs.as_ref().map(|refs| {
                    refs.iter()
                        .map(|r| ResourceKey::from((r.group.clone(), r.namespace.clone(), r.name.clone(), r.kind.clone())))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();

        let mut config = ListenerConfig::new(gateway_listener.name.clone(), gateway_listener.port, gateway_listener.hostname.clone());
        config.certificates = secrets;

        let condition = validate_allowed_routes(gateway_listener);

        let mut listener_conditions = ListenerConditions::new();
        _ = listener_conditions.replace(condition);
        let listener_data = ListenerData {
            config,
            conditions: listener_conditions,
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

impl TryFrom<&KubeGateway> for Gateway {
    type Error = GatewayError;

    fn try_from(gateway: &KubeGateway) -> std::result::Result<Self, Self::Error> {
        let id = Uuid::parse_str(&gateway.metadata.uid.clone().unwrap_or_default()).map_err(|_| GatewayError::ConversionProblem("Can't parse uuid".to_owned()))?;
        let name = gateway.metadata.name.clone().unwrap_or_default();
        if name.is_empty() {
            return Err(GatewayError::ConversionProblem("Name can't be empty".to_owned()));
        }
        let namespace = gateway.metadata.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) = VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));
        if !listener_validation_errrors.is_empty() {
            return Err(GatewayError::ConversionProblem("Misconfigured listeners".to_owned()));
        }

        Ok(Self { id, name, namespace, listeners })
    }
}

#[derive(Clone, Debug)]
pub struct BackendServiceConfig {
    pub endpoint: String,
    pub port: i32,
    pub weight: i32,
}

#[derive(Clone, Debug)]
pub struct RoutingRule {
    pub name: String,
    pub backends: Vec<BackendServiceConfig>,
    pub matching_rules: Vec<HTTPRouteRulesMatches>,
}

#[derive(Clone, Debug)]
pub struct RouteConfig {
    name: String,
    namespace: String,
    parents: Option<Vec<HTTPRouteParentRefs>>,
    pub routing_rules: Vec<RoutingRule>,
    hostnames: Vec<String>,
}
impl PartialEq for RouteConfig {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.namespace == other.namespace
    }
}

impl PartialOrd for RouteConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.name.partial_cmp(&other.name) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        self.namespace.partial_cmp(&other.namespace)
    }
}

impl RouteConfig {
    pub fn new(name: String, namespace: String, parents: Option<Vec<HTTPRouteParentRefs>>) -> Self {
        Self {
            name,
            namespace,
            parents,
            routing_rules: vec![],
            hostnames: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Route {
    Http(RouteConfig),
    Grpc(RouteConfig),
}

impl Route {
    pub fn name(&self) -> &str {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.name,
        }
    }

    pub fn namespace(&self) -> &String {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.namespace,
        }
    }
    pub fn parents(&self) -> &Option<Vec<HTTPRouteParentRefs>> {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.parents,
        }
    }

    pub fn routing_rules(&self) -> &[RoutingRule] {
        match self {
            Route::Http(c) | Route::Grpc(c) => &c.routing_rules,
        }
    }

    fn hostname(&self) -> &[String] {
        match self {
            Route::Http(config) | Route::Grpc(config) => &config.hostnames,
        }
    }
}

impl TryFrom<&HTTPRoute> for Route {
    type Error = ControllerError;
    fn try_from(value: &HTTPRoute) -> Result<Self, Self::Error> {
        let mut rc = RouteConfig::new(
            value.name_any(),
            value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
            value.spec.parent_refs.clone(),
        );
        let empty_rules: Vec<HTTPRouteRules> = vec![];
        let routing_rules = value.spec.rules.as_ref().unwrap_or(&empty_rules);
        let routing_rules = routing_rules
            .iter()
            .enumerate()
            .map(|(i, rr)| {
                RoutingRule {
                    name: format!("{}-{i}", value.name_any()),
                    matching_rules: rr.matches.clone().unwrap_or_default(),
                    backends: rr
                        .backend_refs
                        .as_ref()
                        .unwrap_or(&vec![])
                        .iter()
                        .map(|br| {
                            BackendServiceConfig {
                                endpoint: if let Some(namespace) = br.namespace.as_ref() {
                                    format!("{}.{namespace}", br.name)
                                } else {
                                    br.name.clone()
                                },
                                //endpoint: "10.110.238.122".to_owned(),
                                port: br.port.unwrap_or(0),
                                weight: br.weight.unwrap_or(1),
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        rc.routing_rules = routing_rules;

        Ok(Route::Http(rc))
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Gateway {
    pub id: Uuid,
    pub name: String,
    pub namespace: String,
    pub listeners: Vec<Listener>,
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
    pub listeners: Vec<ListenerStatus>,
    pub attached_addresses: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GatewayProcessedPayload {
    pub deployed_gateway_status: DeployedGatewayStatus,
    pub attached_routes: Vec<Route>,
    pub ignored_routes: Vec<Route>,
}

impl GatewayProcessedPayload {
    pub fn new(gateway_status: DeployedGatewayStatus, attached_routes: Vec<Route>, ignored_routes: Vec<Route>) -> Self {
        Self {
            deployed_gateway_status: gateway_status,
            attached_routes,
            ignored_routes,
        }
    }
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
    GatewayProcessed(GatewayProcessedPayload),
    GatewayDeleted(Vec<RouteStatus>),
    RouteProcessed(RouteProcessedPayload),
    GatewayProcessingError,
    RouteProcessingError,
}

#[derive(Debug)]
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

#[derive(Debug)]
pub struct ChangedContext {
    pub response_sender: oneshot::Sender<GatewayResponse>,
    pub gateway: Gateway,
    pub kube_gateway: KubeGateway,
    pub route_to_listeners_mapping: Vec<RouteToListenersMapping>,
}
impl ChangedContext {
    pub fn new(response_sender: oneshot::Sender<GatewayResponse>, gateway: Gateway, kube_gateway: KubeGateway, route_to_listeners_mapping: Vec<RouteToListenersMapping>) -> Self {
        ChangedContext {
            response_sender,
            gateway,
            kube_gateway,
            route_to_listeners_mapping,
        }
    }
}

impl Display for ChangedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Gateway {}.{} listeners = {} Routes {:?}",
            self.gateway.name,
            self.gateway.namespace,
            self.gateway.listeners.len(),
            self.route_to_listeners_mapping.iter().map(std::string::ToString::to_string).collect::<Vec<_>>()
        )
    }
}

#[derive(Debug)]
pub struct DeletedContext {
    pub response_sender: oneshot::Sender<GatewayResponse>,
    pub gateway: Gateway,
    pub routes: Vec<Route>,
}

impl DeletedContext {
    pub fn new(response_sender: oneshot::Sender<GatewayResponse>, gateway: Gateway, routes: Vec<Route>) -> Self {
        DeletedContext { response_sender, gateway, routes }
    }
}
#[derive(Debug)]
pub enum GatewayEvent {
    GatewayChanged(ChangedContext),
    GatewayDeleted(DeletedContext),
    RouteChanged(ChangedContext),
}

impl Display for GatewayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayEvent::GatewayChanged(ctx) => write!(
                f,
                "GatewayEvent::GatewayChanged
                {ctx}" // gateway {:?}
                       // routes {:?}",
                       // ctx.gateway, ctx.route_to_listeners_mapping
            ),
            GatewayEvent::GatewayDeleted(ctx) => {
                write!(
                    f,
                    "GatewayEvent::GatewayDeleted 
                gateway {:?} 
                routes {:?}",
                    ctx.gateway, ctx.routes
                )
            }

            GatewayEvent::RouteChanged(ctx) => {
                write!(
                    f,
                    "GatewayEvent::RouteChanged                 
                    {ctx}" // gateway {:?}
                           // gateway {:?}
                           // routes {:?}",
                           // ctx.gateway, ctx.route_to_listeners_mapping
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
pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";
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
            name: name.to_owned(),
            kind: kind.unwrap_or(DEFAULT_KIND_NAME.to_owned()),
        }
    }
}

impl From<&HTTPRouteParentRefs> for ResourceKey {
    fn from(route_parent: &HTTPRouteParentRefs) -> Self {
        Self {
            group: route_parent.group.clone().unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
            namespace: route_parent.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
            name: route_parent.name.clone(),
            kind: route_parent.kind.clone().unwrap_or(DEFAULT_KIND_NAME.to_owned()),
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

pub struct RouteListenerMatcher {}
impl RouteListenerMatcher {
    pub fn filter_matching_routes(gateway: &Arc<gateways::Gateway>, routes: &[Route]) -> Vec<RouteToListenersMapping> {
        routes
            .iter()
            .filter_map(|route| {
                let listeners = Self::filter_matching_route(gateway, route.parents());
                if listeners.is_empty() {
                    None
                } else {
                    Some(RouteToListenersMapping::new(route.clone(), listeners))
                }
            })
            .collect()
    }

    pub fn filter_matching_route(gateway: &Arc<gateways::Gateway>, route_parents: &Option<Vec<HTTPRouteParentRefs>>) -> Vec<GatewayListeners> {
        let mut routes_and_listeners: Vec<GatewayListeners> = vec![];
        if let Some(route_parents) = route_parents {
            for route_parent in route_parents {
                let route_key = ResourceKey::from(route_parent);
                let gateway_key = ResourceKey::from(&**gateway);
                if route_key.name == gateway_key.name && route_key.namespace == gateway_key.namespace {
                    let mut matched = match (route_parent.port, &route_parent.section_name) {
                        (Some(port), Some(section_name)) => filter_listeners_by_name_or_port(gateway, |gl| gl.port == port && gl.name == *section_name),
                        (Some(port), None) => filter_listeners_by_name_or_port(gateway, |gl| gl.port == port),
                        (None, Some(section_name)) => filter_listeners_by_name_or_port(gateway, |gl| gl.name == *section_name),
                        (None, None) => filter_listeners_by_name_or_port(gateway, |_| true),
                    };
                    routes_and_listeners.append(&mut matched);
                }
            }
        }

        routes_and_listeners
    }

    pub fn filter_matching_gateways(state: &mut State, resolved_gateways: &[(&HTTPRouteParentRefs, Option<Arc<gateways::Gateway>>)]) -> Vec<Arc<gateways::Gateway>> {
        resolved_gateways
            .iter()
            .filter_map(|(parent_ref, maybe_gateway)| {
                if let Some(gateway) = maybe_gateway {
                    let parent_ref_key = ResourceKey::from(&**parent_ref);
                    let gateway_key = ResourceKey::from(&**gateway);
                    if parent_ref_key == gateway_key {
                        state.get_gateway(&gateway_key).cloned()
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}

fn filter_listeners_by_name_or_port<F>(gateway: &Arc<gateways::Gateway>, filter: F) -> Vec<GatewayListeners>
where
    F: Fn(&GatewayListeners) -> bool,
{
    gateway.spec.listeners.iter().filter(|f| filter(f)).cloned().collect()
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

#[derive(Clone, Debug, PartialEq)]
pub enum ListenerCondition {
    Resolved(Vec<String>),
    ResolvedWithNotAllowedRoutes(Vec<String>),
    InvalidAllowedRoutes,
    InvalidCertificates(Vec<String>),
}

impl Ord for ListenerCondition {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let (_, self_type, _) = self.resolved_type();
        let (_, other_type, _) = other.resolved_type();
        self_type.to_string().cmp(&other_type.to_string())
    }
}
impl PartialOrd for ListenerCondition {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for ListenerCondition {}

impl ListenerCondition {
    pub fn resolved_type(
        &self,
    ) -> (
        &'static str,
        gateway_api::apis::standard::constants::ListenerConditionType,
        gateway_api::apis::standard::constants::ListenerConditionReason,
    ) {
        match self {
            ListenerCondition::Resolved(_) => (
                "True",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs,
            ),
            ListenerCondition::ResolvedWithNotAllowedRoutes(_) | ListenerCondition::InvalidAllowedRoutes => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::InvalidRouteKinds,
            ),
            ListenerCondition::InvalidCertificates(_) => (
                "False",
                gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs,
                gateway_api::apis::standard::constants::ListenerConditionReason::InvalidCertificateRef,
            ),
        }
    }
    pub fn supported_routes(&self) -> Vec<String> {
        match self {
            ListenerCondition::Resolved(supported_routes) | ListenerCondition::ResolvedWithNotAllowedRoutes(supported_routes) | ListenerCondition::InvalidCertificates(supported_routes) => {
                supported_routes.clone()
            }
            ListenerCondition::InvalidAllowedRoutes => vec![],
        }
    }
}

const APPROVED_ROUTES: [&str; 2] = ["HTTPRoute", "TCPRoute"];

fn validate_allowed_routes(gateway_listeners: &GatewayListeners) -> ListenerCondition {
    if let Some(ar) = gateway_listeners.allowed_routes.as_ref() {
        if let Some(kinds) = ar.kinds.as_ref() {
            let cloned_kinds = kinds.clone().into_iter().map(|k| k.kind);
            let (supported, invalid): (Vec<_>, Vec<_>) = cloned_kinds.partition(|f| APPROVED_ROUTES.contains(&f.as_str()));

            if invalid.is_empty() {
                ListenerCondition::Resolved(supported)
            } else if !supported.is_empty() {
                ListenerCondition::ResolvedWithNotAllowedRoutes(supported)
            } else {
                ListenerCondition::InvalidAllowedRoutes
            }
        } else if gateway_listeners.protocol == "HTTP" || gateway_listeners.protocol == "HTTPS" {
            ListenerCondition::Resolved(vec!["HTTPRoute".to_owned()])
        } else {
            ListenerCondition::Resolved(vec![])
        }
    } else if gateway_listeners.protocol == "HTTP" || gateway_listeners.protocol == "HTTPS" {
        ListenerCondition::Resolved(vec!["HTTPRoute".to_owned()])
    } else {
        ListenerCondition::Resolved(vec![])
    }
}
