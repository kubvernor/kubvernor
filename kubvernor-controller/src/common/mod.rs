// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

mod gateway;
mod listener;
mod references_resolver;
mod resource_key;
mod route;
#[cfg(test)]
mod test;

use std::{cmp, collections::BTreeSet, fmt::Display, net::IpAddr};

pub use gateway::{ChangedContext, Gateway};
pub use gateway_api::gateways::Gateway as KubeGateway;
use gateway_api::{gatewayclasses::GatewayClass, gateways::GatewayListeners};
use gateway_api_inference_extension::inferencepools::{
    InferencePoolEndpointPickerRef, InferencePoolEndpointPickerRefFailureMode, InferencePoolSpec,
};
use kubvernor_common::ResourceKey;
pub use listener::{Listener, ListenerCondition, ProtocolType, TlsType};
pub use references_resolver::{BackendReferenceResolver, ReferenceGrantRef, ReferenceGrantsResolver, SecretsResolver};
pub use resource_key::{DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME, KUBERNETES_NONE, RouteRefKey};
pub use route::{
    FilterHeaders, NotResolvedReason, ResolutionStatus, Route, RouteStatus, RouteType,
    grpc_route::{GRPCRoutingConfiguration, GRPCRoutingRule},
    http_route::{HTTPRoutingConfiguration, HTTPRoutingRule},
};
use tokio::sync::{mpsc, oneshot};
use typed_builder::TypedBuilder;

use crate::{
    Address,
    services::patchers::{FinalizerContext, Operation},
};

#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
pub enum Certificate {
    ResolvedSameSpace(ResourceKey, KeyData),
    ResolvedCrossSpace(ResourceKey, KeyData),
    NotResolved(ResourceKey),
    Invalid(ResourceKey),
}

#[derive(Clone, PartialEq, PartialOrd, Ord, Eq)]
pub struct KeyData {
    pub cert: Vec<u8>,
    pub private_key: Vec<u8>,
    pub root: Option<Vec<u8>>,
}

impl std::fmt::Debug for KeyData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyData")
            .field("cert", &"--- SENSITIVE DATA ----")
            .field("private_key", &"--- SENSITIVE DATA ----")
            .field("root", &"--- SENSITIVE DATA ----")
            .finish()
    }
}

impl Certificate {
    pub fn resolve(self: &Certificate, certifcate: Vec<u8>, key: Vec<u8>) -> Self {
        let data = KeyData { cert: certifcate, private_key: key, root: None };
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key, _)
            | Certificate::ResolvedCrossSpace(resource_key, _)
            | Certificate::NotResolved(resource_key)
            | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::ResolvedSameSpace(resource.clone(), data)
    }

    pub fn resolve_cross_space(self: &Certificate, certifcate: Vec<u8>, key: Vec<u8>) -> Self {
        let data = KeyData { cert: certifcate, private_key: key, root: None };
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key, _)
            | Certificate::ResolvedCrossSpace(resource_key, _)
            | Certificate::NotResolved(resource_key)
            | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::ResolvedCrossSpace(resource.clone(), data)
    }

    pub fn not_resolved(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key, _)
            | Certificate::ResolvedCrossSpace(resource_key, _)
            | Certificate::NotResolved(resource_key)
            | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::NotResolved(resource.clone())
    }

    pub fn invalid(self: &Certificate) -> Self {
        let resource = match self {
            Certificate::ResolvedSameSpace(resource_key, _)
            | Certificate::ResolvedCrossSpace(resource_key, _)
            | Certificate::NotResolved(resource_key)
            | Certificate::Invalid(resource_key) => resource_key,
        };
        Certificate::Invalid(resource.clone())
    }

    pub fn resouce_key(&self) -> &ResourceKey {
        match self {
            Certificate::ResolvedSameSpace(resource_key, _)
            | Certificate::ResolvedCrossSpace(resource_key, _)
            | Certificate::NotResolved(resource_key)
            | Certificate::Invalid(resource_key) => resource_key,
        }
    }

    pub fn key_data(&self) -> Option<&KeyData> {
        match self {
            Certificate::ResolvedSameSpace(_, data) | Certificate::ResolvedCrossSpace(_, data) => Some(data),
            Certificate::NotResolved(_) | Certificate::Invalid(_) => None,
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
    pub target_ports: Vec<i32>,
    pub weight: i32,
    pub inference_config: Option<InferencePoolConfig>,
    pub endpoints: Option<Vec<String>>,
}

impl InferencePoolTypeConfig {
    pub fn cluster_name(&self) -> String {
        self.resource_key.name.clone() + "." + &self.resource_key.namespace
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferencePoolConfig(pub InferencePoolSpec);

impl InferencePoolConfig {
    pub fn extension_ref(&self) -> &InferencePoolEndpointPickerRef {
        &self.0.endpoint_picker_ref
    }
}

impl PartialOrd for InferencePoolConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for InferencePoolConfig {}

#[allow(clippy::match_same_arms)]
impl Ord for InferencePoolConfig {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        let this = &self.0;
        let other = &other.0;

        let this_extension_ref = &this.endpoint_picker_ref;
        let other_extension_ref = &other.endpoint_picker_ref;

        let this_failure_mode = &this_extension_ref.failure_mode;
        let other_failure_mode = &other_extension_ref.failure_mode;
        match (this_failure_mode, other_failure_mode) {
            (None, Some(_)) => return cmp::Ordering::Less,
            (Some(_), None) => return cmp::Ordering::Greater,
            (Some(InferencePoolEndpointPickerRefFailureMode::FailOpen), Some(InferencePoolEndpointPickerRefFailureMode::FailClose)) => {
                return cmp::Ordering::Greater;
            },
            (Some(InferencePoolEndpointPickerRefFailureMode::FailClose), Some(InferencePoolEndpointPickerRefFailureMode::FailOpen)) => {
                return cmp::Ordering::Less;
            },
            _ => (),
        }

        let order = this_extension_ref.group.cmp(&other_extension_ref.group);
        if order != cmp::Ordering::Equal {
            return order;
        }
        let order = this_extension_ref.kind.cmp(&other_extension_ref.kind);
        if order != cmp::Ordering::Equal {
            return order;
        }
        let order = this_extension_ref.name.cmp(&other_extension_ref.name);
        if order != cmp::Ordering::Equal {
            return order;
        }

        match (this_extension_ref.port.as_ref(), other_extension_ref.port.as_ref()) {
            (None, None) => (),
            (None, Some(_)) => return cmp::Ordering::Less,
            (Some(_), None) => return cmp::Ordering::Greater,
            (Some(this_port_ref), Some(other_port_ref)) => {
                let order = this_port_ref.number.cmp(&other_port_ref.number);
                if order != cmp::Ordering::Equal {
                    return order;
                }
            },
        }
        cmp::Ordering::Equal

        // let order = this.target_ports.cmp(&other.target_ports);
        // if order != cmp::Ordering::Equal {
        //     return order;
        // }

        // this.selector.cmp(&other.selector)
    }
}

impl BackendTypeConfig for ServiceTypeConfig {
    fn cluster_name(&self) -> String {
        self.resource_key.name.clone() + "." + &self.resource_key.namespace
    }
    fn weight(&self) -> i32 {
        self.weight
    }

    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }
}

pub trait BackendTypeConfig {
    fn resource_key(&self) -> ResourceKey;
    fn weight(&self) -> i32;
    fn cluster_name(&self) -> String;
}

impl BackendTypeConfig for InferencePoolTypeConfig {
    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }
    fn cluster_name(&self) -> String {
        self.resource_key.name.clone() + "." + &self.resource_key.namespace
    }
    fn weight(&self) -> i32 {
        self.weight
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
            Backend::Resolved(backend_type)
            | Backend::Unresolved(backend_type)
            | Backend::NotAllowed(backend_type)
            | Backend::Maybe(backend_type)
            | Backend::Invalid(backend_type) => backend_type.resource_key(),
        }
    }

    pub fn backend_type(&self) -> &BackendType {
        match self {
            Backend::Resolved(backend_type)
            | Backend::Unresolved(backend_type)
            | Backend::NotAllowed(backend_type)
            | Backend::Maybe(backend_type)
            | Backend::Invalid(backend_type) => backend_type,
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum GatewayAddress {
    Hostname(String),
    IPAddress(IpAddr),
    NamedAddress(String),
}

#[derive(Debug)]
pub enum BackendGatewayResponse {
    Processed(Box<Gateway>),
    ProcessedWithContext { gateway: Box<Gateway>, kube_gateway: Box<KubeGateway>, gateway_class_name: String },
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
            self.listeners.iter().fold(String::new(), |acc, l| { acc + &format!("  Listener(name: {} port: {}), ", &l.name, l.port) })
        )
    }
}

#[derive(Debug, TypedBuilder)]
pub struct DeletedContext {
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
            BackendGatewayEvent::Changed(ctx) => {
                write!(f, "GatewayEvent::GatewayChanged {ctx}")
            },
            BackendGatewayEvent::Deleted(ctx) => {
                write!(f, "GatewayEvent::GatewayDeleted gateway {:?}", ctx.gateway)
            },
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
}

pub enum ReferenceValidateRequest {
    AddGateway(Box<RequestContext>),
    AddRoute { route_key: ResourceKey, references: BTreeSet<ResourceKey> },
    UpdatedGateways { reference: ResourceKey, gateways: BTreeSet<ResourceKey> },
    UpdatedRoutes { reference: ResourceKey, updated_routes: BTreeSet<ResourceKey> },
    DeleteRoute { route_key: ResourceKey, references: BTreeSet<ResourceKey> },
    DeleteGateway { gateway: Gateway },
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
        }))
        .await;
}

const GATEWAY_CLASS_FINALIZER_NAME: &str = "gateway-exists-finalizer.gateway.networking.k8s.io";

pub async fn add_finalizer_to_gateway_class(
    sender: &mpsc::Sender<Operation<GatewayClass>>,
    gateway_class_name: &str,
    controller_name: &str,
) {
    let key = ResourceKey::new(gateway_class_name);
    let _ = sender
        .send(Operation::PatchFinalizer(FinalizerContext {
            resource_key: key,
            controller_name: controller_name.to_owned(),
            finalizer_name: GATEWAY_CLASS_FINALIZER_NAME.to_owned(),
        }))
        .await;
}

pub fn create_id(name: &str, namespace: &str) -> String {
    namespace.to_owned() + "." + name
}

#[derive(Clone, Debug, TypedBuilder)]
pub struct ControlPlaneConfig {
    pub controller_name: String,
    pub listening_socket: Address,
}

impl ControlPlaneConfig {
    pub fn addr(&self) -> Address {
        self.listening_socket.clone()
    }
}
