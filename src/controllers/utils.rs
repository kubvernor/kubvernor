use std::{collections::BTreeSet, marker::PhantomData, sync::Arc};

use gateway_api::apis::standard::gateways::{Gateway, GatewayStatusListeners};
use k8s_openapi::{
    api::core::v1::{Secret, Service},
    apimachinery::pkg::apis::meta::v1::Condition,
};
use kube::{
    api::{Patch, PatchParams},
    runtime::{
        controller::Action,
        finalizer::{self, Error},
    },
    Api, Client, Resource, ResourceExt,
};
use kube_core::{PartialObjectMeta, PartialObjectMetaExt};
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::Serialize;
use tracing::{debug, warn};

use crate::{
    common::{self, Backend, ListenerCondition, ProtocolType, ResourceKey, Route},
    controllers::ControllerError,
    state::State,
};

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum ResourceState {
    New,
    SpecNotChanged,
    Deleted,
    SpecChanged,
    VersionNotChanged,
    StatusChanged,
    StatusNotChanged,
}

pub struct FinalizerPatcher {}

impl FinalizerPatcher {
    pub async fn patch_finalizer<T>(api: &Api<T>, resource_name: &str, controller_name: &str, finalizer_name: &str) -> std::result::Result<(), ControllerError>
    where
        T: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
        T: ResourceExt,
        T: Resource<DynamicType = ()>,
    {
        let type_name = std::any::type_name::<T>();
        let maybe_meta = api.get_metadata(resource_name).await;
        debug!("patch_finalizer {type_name} {resource_name} {controller_name}");
        if let Ok(mut meta) = maybe_meta {
            let finalizers = meta.finalizers_mut();

            if finalizers.contains(&finalizer_name.to_owned()) {
                Ok(())
            } else {
                finalizers.push(finalizer_name.to_owned());
                let mut object_meta = meta.meta().clone();
                object_meta.managed_fields = None;
                let meta: PartialObjectMeta<T> = object_meta.into_request_partial::<_>();

                match api.patch_metadata(resource_name, &PatchParams::apply(controller_name), &Patch::Apply(&meta)).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        warn!("patch_finalizer: {type_name} {controller_name} {resource_name} patch failed {e:?}",);
                        Err(ControllerError::PatchFailed)
                    }
                }
            }
        } else {
            Err(ControllerError::UnknownResource)
        }
    }
}

pub type ResourceCheckerArgs<'a, T> = (&'a Arc<T>, Arc<T>);
pub type ResourceChecker<T> = fn(args: ResourceCheckerArgs<T>) -> ResourceState;

pub struct ResourceStateChecker {}

impl ResourceStateChecker {
    pub fn check_status<R>(resource: &Arc<R>, maybe_stored_resource: Option<Arc<R>>, resource_spec_checker: ResourceChecker<R>, resource_status_checker: ResourceChecker<R>) -> ResourceState
    where
        R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
        R: ResourceExt,
        R: Resource<DynamicType = ()>,
    {
        if !resource.finalizers().is_empty() && resource.meta().deletion_timestamp.is_some() {
            return ResourceState::Deleted;
        }

        if let Some(stored_object) = maybe_stored_resource {
            if stored_object.meta().resource_version == resource.meta().resource_version {
                return ResourceState::VersionNotChanged;
            }
            let status = resource_spec_checker((resource, Arc::clone(&stored_object)));
            if ResourceState::SpecNotChanged == status {
                resource_status_checker((resource, stored_object))
            } else {
                status
            }
        } else {
            ResourceState::New
        }
    }
}

pub struct ResourceFinalizer {}

impl ResourceFinalizer {
    pub async fn delete_resource<R, ReconcileErr>(api: &Api<R>, finalizer_name: &str, resource: &Arc<R>) -> std::result::Result<Action, Error<ReconcileErr>>
    where
        R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
        R: ResourceExt,
        R: Resource<DynamicType = ()>,
        R: Serialize,
        ReconcileErr: std::error::Error + 'static,
    {
        let res: std::result::Result<Action, Error<_>> = finalizer::finalizer(api, finalizer_name, Arc::clone(resource), |event| async move {
            match event {
                finalizer::Event::Apply(_) | finalizer::Event::Cleanup(_) => Result::<Action, ReconcileErr>::Ok(Action::await_change()),
            }
        })
        .await;
        res
    }
}

pub fn find_linked_routes(state: &State, gateway_id: &ResourceKey) -> Vec<Route> {
    state
        .get_http_routes_attached_to_gateway(gateway_id)
        .map(|routes| routes.iter().filter_map(|r| Route::try_from(&***r).ok()).collect())
        .unwrap_or_default()
}

pub async fn resolve_route_backends(client: Client, routes: Vec<Route>, log_context: String) -> Vec<Route> {
    let futures: Vec<_> = routes
        .into_iter()
        .map(|route| RouteBackendResolver::new(route, client.clone(), log_context.clone()).resolve())
        .collect();
    let routes = futures::future::join_all(futures);
    routes.await
}

pub struct LogContext<'a, T> {
    pub controller_name: &'a str,
    pub resource_key: &'a ResourceKey,
    pub version: Option<String>,
    pub resource_type: PhantomData<T>,
}

impl<T> std::fmt::Display for LogContext<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let resource = format!("{:?}", &self.resource_type);
        let resource = &resource["PhantomData".len()..];
        write!(f, "{}: resource_id: {},  version: {:?}", resource, self.resource_key, self.version)
    }
}

impl<'a> LogContext<'a, Gateway> {
    pub fn new(controller_name: &'a str, resource_key: &'a ResourceKey, version: Option<String>) -> Self {
        Self {
            controller_name,
            resource_key,
            version,
            resource_type: std::marker::PhantomData,
        }
    }
}

pub struct ListenerStatusesMerger {
    all_listeners_statuses: Vec<GatewayStatusListeners>,
}
impl ListenerStatusesMerger {
    pub fn new(all_listeners_statuses: Vec<GatewayStatusListeners>) -> Self {
        Self { all_listeners_statuses }
    }
    pub fn merge(mut self, mut deployed_listeners_statuses: Vec<GatewayStatusListeners>) -> Vec<GatewayStatusListeners> {
        #[derive(Debug)]
        struct ListenerConditionHolder {
            type_: String,
            condition: Condition,
        }

        impl Eq for ListenerConditionHolder {}

        impl Ord for ListenerConditionHolder {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.type_.cmp(&other.type_)
            }
        }
        impl PartialOrd for ListenerConditionHolder {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.type_.cmp(&other.type_))
            }
        }
        impl PartialEq for ListenerConditionHolder {
            fn eq(&self, other: &Self) -> bool {
                self.type_ == other.type_
            }
        }

        for listener in &mut self.all_listeners_statuses {
            if let Some(deployed_listener) = deployed_listeners_statuses.iter_mut().find(|f| f.name == listener.name) {
                let mut listener_conditions = listener
                    .conditions
                    .iter()
                    .map(|c| ListenerConditionHolder {
                        type_: c.type_.clone(),
                        condition: c.clone(),
                    })
                    .collect::<BTreeSet<_>>();
                let deployed_conditions = deployed_listener.conditions.iter().map(|c| ListenerConditionHolder {
                    type_: c.type_.clone(),
                    condition: c.clone(),
                });
                for c in deployed_conditions {
                    let _ = listener_conditions.replace(c);
                }

                listener.conditions = listener_conditions.into_iter().map(|c| c.condition).collect();
                listener.attached_routes = deployed_listener.attached_routes;
            }
        }
        self.all_listeners_statuses
    }
}

pub struct ListenerTlsConfigValidator {
    gateway: common::Gateway,
    client: Client,
    log_context: String,
}

impl ListenerTlsConfigValidator {
    pub fn new(gateway: common::Gateway, client: Client, log_context: String) -> Self {
        Self { gateway, client, log_context }
    }

    pub async fn validate(mut self) -> common::Gateway {
        let client = self.client.clone();
        let log_context = self.log_context;
        debug!("{log_context} Validating certs");

        for listener in self.gateway.listeners.iter_mut().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
            let listener_data = listener.data_mut();
            let name = listener_data.config.name.clone();
            let conditions = &mut listener_data.conditions;
            let supported_routes = conditions.get(&ListenerCondition::Resolved(vec![])).map(ListenerCondition::supported_routes).unwrap_or_default();
            debug!("{log_context} Supported routes {name} {supported_routes:?}");
            for certificate_key in &listener_data.config.certificates {
                let secret_api: Api<Secret> = Api::namespaced(client.clone(), &certificate_key.namespace);
                if let Ok(secret) = secret_api.get(&certificate_key.name).await {
                    if secret.type_ == Some("kubernetes.io/tls".to_owned()) {
                        let supported_routes = supported_routes.clone();
                        if let Some(data) = secret.data {
                            let private_key = data.get("tls.key");
                            let certificate = data.get("tls.crt");
                            match (private_key, certificate) {
                                (Some(private_key), Some(certificate)) => {
                                    let valid_cert = CertificateDer::from_pem_slice(&certificate.0);
                                    let valid_key = PrivateKeyDer::from_pem_slice(&private_key.0);
                                    match (valid_cert, valid_key) {
                                        (Ok(_), Ok(_)) => debug!("{log_context} Private key and certificate are valid"),
                                        (Ok(_), Err(e)) => {
                                            debug!("{log_context} Key is invalid {e}");
                                            _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes));
                                        }
                                        (Err(e), Ok(_)) => {
                                            debug!("{log_context} Certificate is invalid {e}");
                                            _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes));
                                        }
                                        (Err(e_cert), Err(e_key)) => {
                                            debug!("{log_context} Key and cer certificate are invalid {e_cert}{e_key}");
                                            _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes));
                                        }
                                    };
                                }
                                _ => {
                                    _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes));
                                }
                            }
                        } else {
                            _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes));
                        }
                    } else {
                        _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes.clone()));
                    }
                } else {
                    _ = conditions.replace(ListenerCondition::InvalidCertificates(supported_routes.clone()));
                }
            }
        }
        self.gateway
    }
}

pub struct RouteBackendResolver {
    route: common::Route,
    client: Client,
    log_context: String,
}

impl RouteBackendResolver {
    pub fn new(route: common::Route, client: Client, log_context: String) -> Self {
        Self { route, client, log_context }
    }

    pub async fn resolve(self) -> common::Route {
        let mut route = self.route;
        let route_config = route.config_mut();
        let mut unresolved_count = 0;
        for rule in &mut route_config.routing_rules {
            let mut new_backends = vec![];
            for backend in &rule.backends {
                let client = self.client.clone();
                let backend_config = backend.config().clone();
                let backend_namespace = backend_config.resource_key.namespace.clone();
                let backend_name = backend_config.resource_key.name.clone();
                let service_api: Api<Service> = Api::namespaced(client, &backend_namespace);
                if (service_api.get(&backend_name).await).is_ok() {
                    new_backends.push(Backend::Resolved(backend_config));
                } else {
                    debug!("{} can't resolve {:?}", self.log_context, &backend_config.resource_key);
                    new_backends.push(Backend::Unresolved(backend_config));
                    unresolved_count += 1;
                }
            }
            rule.backends = new_backends;
        }
        route_config.resolution_status = match unresolved_count {
            0 => common::ResolutionStatus::Resolved,
            _ => common::ResolutionStatus::NotResolved,
        };
        route
    }
}
