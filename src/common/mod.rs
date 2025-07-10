mod gateway;
mod listener;
mod references_resolver;
mod resource_key;
mod route;
#[cfg(test)]
mod test;

use std::{
    collections::BTreeSet,
    fmt::Display,
    net::{IpAddr, SocketAddr},
};

pub use gateway::{ChangedContext, Gateway};
pub use gateway_api::gateways::Gateway as KubeGateway;
use gateway_api::{gatewayclasses::GatewayClass, gateways::GatewayListeners};
pub use listener::{Listener, ListenerCondition, ProtocolType, TlsType};
pub use references_resolver::{BackendReferenceResolver, ReferenceGrantRef, ReferenceGrantsResolver, SecretsResolver};
pub use resource_key::{ResourceKey, RouteRefKey, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME, KUBERNETES_NONE};
pub use route::{GRPCEffectiveRoutingRule, HTTPEffectiveRoutingRule, NotResolvedReason, ResolutionStatus, Route, RouteStatus, RouteType};
use tokio::sync::{mpsc, oneshot};
use tracing::Span;
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::services::patchers::{FinalizerContext, Operation};

#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
pub enum Certificate {
    ResolvedSameSpace(ResourceKey),
    ResolvedCrossSpace(ResourceKey),
    NotResolved(ResourceKey),
    Invalid(ResourceKey),
}

impl Certificate {
    pub fn resolve(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key) | Certificate::ResolvedCrossSpace(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::ResolvedSameSpace(resource.clone())
    }

    pub fn resolve_cross_space(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key) | Certificate::ResolvedCrossSpace(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::ResolvedCrossSpace(resource.clone())
    }

    pub fn not_resolved(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key) | Certificate::ResolvedCrossSpace(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::NotResolved(resource.clone())
    }

    pub fn invalid(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key) | Certificate::ResolvedCrossSpace(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::Invalid(resource.clone())
    }

    pub fn resouce_key(&self) -> &ResourceKey {
        match self {
            Certificate::ResolvedSameSpace(resource_key) | Certificate::ResolvedCrossSpace(resource_key) | Certificate::NotResolved(resource_key) | Certificate::Invalid(resource_key) => resource_key,
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct ServiceTypeConfig {
    pub resource_key: ResourceKey,
    pub endpoint: String,
    pub port: i32,
    pub effective_port: i32,
    pub weight: i32,
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct InferencePoolTypeConfig {
    pub resource_key: ResourceKey,
    pub endpoint: String,
    pub port: i32,
    pub effective_port: i32,
    pub weight: i32,
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct InvalidTypeConfig {
    pub resource_key: ResourceKey,
}

impl ServiceTypeConfig {
    pub fn cluster_name(&self) -> String {
        self.resource_key.name.clone() + "." + &self.resource_key.namespace
    }
    pub fn weight(&self) -> i32 {
        self.weight
    }

    pub fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }
}

impl InferencePoolTypeConfig {
    pub fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }
}
impl InvalidTypeConfig {
    pub fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum Backend {
    Resolved(BackendType),
    Unresolved(BackendType),
    NotAllowed(BackendType),
    Maybe(BackendType),
    Invalid(BackendType),
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum BackendType {
    Service(ServiceTypeConfig),
    InferencePool(InferencePoolTypeConfig),
    Invalid(ServiceTypeConfig),
}

impl BackendType {
    pub fn resource_key(&self) -> ResourceKey {
        match self {
            BackendType::Service(service_type_config) => service_type_config.resource_key(),
            BackendType::InferencePool(inference_pool_type_config) => inference_pool_type_config.resource_key(),
            BackendType::Invalid(invalid_type_config) => invalid_type_config.resource_key(),
        }
    }
}

impl Backend {
    pub fn resource_key(&self) -> ResourceKey {
        match self {
            Backend::Resolved(backend_type) | Backend::Unresolved(backend_type) | Backend::NotAllowed(backend_type) | Backend::Maybe(backend_type) | Backend::Invalid(backend_type) => {
                backend_type.resource_key()
            }
        }
    }

    pub fn backend_type(&self) -> &BackendType {
        match self {
            Backend::Resolved(backend_type) | Backend::Unresolved(backend_type) | Backend::NotAllowed(backend_type) | Backend::Maybe(backend_type) | Backend::Invalid(backend_type) => backend_type,
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum GatewayAddress {
    Hostname(String),
    IPAddress(IpAddr),
    NamedAddress(String),
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
pub enum BackendGatewayResponse {
    Processed(Box<Gateway>),
    ProcessedWithContext {
        gateway: Box<Gateway>,
        kube_gateway: Box<KubeGateway>,
        span: Span,
        gateway_class_name: String,
    },
    Deleted(Vec<RouteStatus>),
    ProcessingError,
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
pub struct DeletedContext {
    pub span: Span,
    pub response_sender: oneshot::Sender<BackendGatewayResponse>,
    pub gateway: Gateway,
}

#[derive(Debug)]
pub enum BackendGatewayEvent {
    Changed(Box<ChangedContext>),
    Deleted(Box<DeletedContext>),
}

impl Display for BackendGatewayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendGatewayEvent::Changed(ctx) => write!(f, "GatewayEvent::GatewayChanged {ctx}"),
            BackendGatewayEvent::Deleted(ctx) => {
                write!(f, "GatewayEvent::GatewayDeleted gateway {:?}", ctx.gateway)
            }
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
    RefNotPermitted(Vec<String>),
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

#[derive(TypedBuilder)]
pub struct RequestContext {
    pub gateway: Gateway,
    pub kube_gateway: KubeGateway,
    pub gateway_class_name: String,
    pub span: Span,
}

pub enum ReferenceValidateRequest {
    AddGateway(Box<RequestContext>),
    AddRoute { references: BTreeSet<ResourceKey>, span: Span },
    UpdatedGateways { reference: ResourceKey, gateways: BTreeSet<ResourceKey> },
    DeleteRoute { references: BTreeSet<ResourceKey>, span: Span },
    DeleteGateway { gateway: Gateway, span: Span },
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

pub fn create_id(name: &str, namespace: &str) -> String {
    namespace.to_owned() + "." + name
}

#[derive(Clone, Debug, TypedBuilder)]
pub struct ControlPlaneConfig {
    pub host: String,
    pub port: u32,
    pub controller_name: String,
    pub listening_socket: SocketAddr,
}

impl ControlPlaneConfig {
    pub fn addr(&self) -> SocketAddr {
        self.listening_socket
    }
}
