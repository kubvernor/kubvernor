use std::{fmt::Display, sync::Arc};

use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{self, GatewayListeners},
    httproutes::{HTTPRoute, HTTPRouteParentRefs, HTTPRouteRules},
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

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum RouteError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Route is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
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

impl Display for ProtocolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut e = format! {"{self:?}"};
        e.make_ascii_uppercase();
        write!(f, "{e}")
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct ListenerConfig {
    pub name: String,
    pub port: i32,
    pub hostname: Option<String>,
}

impl ListenerConfig {
    pub fn new(name: String, port: i32, hostname: Option<String>) -> Self {
        Self { name, port, hostname }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Listener {
    Http(ListenerConfig),
    Https(ListenerConfig),
    Tcp(ListenerConfig),
    Tls(ListenerConfig),
    Udp(ListenerConfig),
}

// #[derive(Clone, Debug, PartialEq, PartialOrd)]
// pub enum ListenerStatus {
//     Accpeted(i32),
//     Conflicted,
// }

impl Listener {
    pub fn name(&self) -> &str {
        match self {
            Listener::Http(config) | Listener::Https(config) | Listener::Tcp(config) | Listener::Tls(config) | Listener::Udp(config) => config.name.as_str(),
        }
    }

    pub fn port(&self) -> i32 {
        match self {
            Listener::Http(config) | Listener::Https(config) | Listener::Tcp(config) | Listener::Tls(config) | Listener::Udp(config) => config.port,
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
            Listener::Http(config) | Listener::Https(config) | Listener::Tcp(config) | Listener::Tls(config) | Listener::Udp(config) => config.hostname.as_ref(),
        }
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
}

#[derive(Clone, Debug)]
pub struct RouteConfig {
    name: String,
    namespace: String,
    parents: Option<Vec<HTTPRouteParentRefs>>,
    pub routing_rules: Vec<RoutingRule>,
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
        // vec![
        //     RoutingRule {
        //         name: format!("{}-r1", value.name_any()),
        //         backends: vec![
        //             BackendServiceConfig {
        //                 endpoint: "echo-service".to_owned(),
        //                 //endpoint: "10.110.238.122".to_owned(),
        //                 port: 9080,
        //                 weight: 1,
        //             },
        //             BackendServiceConfig {
        //                 endpoint: "echo-service".to_owned(),
        //                 //endpoint: "10.110.238.122".to_owned(),
        //                 port: 9080,
        //                 weight: 1,
        //             },
        //         ],
        //     },
        //     RoutingRule {
        //         name: format!("{}-r2", value.name_any()),
        //         backends: vec![BackendServiceConfig {
        //             //                    endpoint: "echo-service-2.default.svc.cluster.local".to_owned(),
        //             endpoint: "10.110.238.122".to_owned(),
        //             port: 9080,
        //             weight: 1,
        //         }],
        //     },
        // ];
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
pub struct GatewayStatus {
    pub id: Uuid,
    pub name: String,
    pub namespace: String,
    pub listeners: Vec<ListenerStatus>,
}

#[derive(Debug)]
pub struct GatewayProcessedPayload {
    pub gateway_status: GatewayStatus,
    pub attached_routes: Vec<Route>,
    pub ignored_routes: Vec<Route>,
}

impl GatewayProcessedPayload {
    pub fn new(gateway_status: GatewayStatus, attached_routes: Vec<Route>, ignored_routes: Vec<Route>) -> Self {
        Self {
            gateway_status,
            attached_routes,
            ignored_routes,
        }
    }
}

#[derive(Debug)]
pub struct RouteProcessedPayload {
    pub status: RouteStatus,
    //pub gateway_status: GatewayStatus,
}

impl RouteProcessedPayload {
    pub fn new(status: RouteStatus, _gateway_status: &GatewayStatus) -> Self {
        Self { status } // gateway_status }
    }
}

#[derive(Debug)]
pub enum GatewayResponse {
    GatewayProcessed(GatewayProcessedPayload),
    GatewayDeleted(Vec<RouteStatus>),
    RouteProcessed(RouteProcessedPayload),
    RouteDeleted,
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
    pub route_to_listeners_mapping: Vec<RouteToListenersMapping>,
}
impl ChangedContext {
    pub fn new(response_sender: oneshot::Sender<GatewayResponse>, gateway: Gateway, route_to_listeners_mapping: Vec<RouteToListenersMapping>) -> Self {
        ChangedContext {
            response_sender,
            gateway,
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

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
}

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
