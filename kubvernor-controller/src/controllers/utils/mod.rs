mod hostname_match_filter;

mod route_listener_matcher;
mod routes_resolver;

mod tls_config_validator;

use std::{collections::BTreeMap, sync::Arc};

pub use hostname_match_filter::HostnameMatchFilter;
use k8s_openapi::api::core::v1::Namespace;
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::{
        controller::Action,
        finalizer::{self, Error},
    },
};
use kube_core::{PartialObjectMeta, PartialObjectMetaExt};
use kubvernor_common::ResourceKey;
use kubvernor_state::State;
pub(crate) use route_listener_matcher::RouteListenerMatcher;
pub use routes_resolver::RoutesResolver;
use serde::Serialize;
pub use tls_config_validator::ListenerTlsConfigValidator;
use tracing::{debug, warn};

use crate::{
    common::{BackendReferenceResolver, Route},
    controllers::ControllerError,
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
    pub async fn patch_finalizer<T>(
        api: &Api<T>,
        resource_name: &str,
        controller_name: &str,
        finalizer_name: &str,
    ) -> std::result::Result<(), ControllerError>
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
                    },
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
    pub fn check_status<R>(
        resource: &Arc<R>,
        maybe_stored_resource: Option<Arc<R>>,
        resource_spec_checker: ResourceChecker<R>,
        resource_status_checker: ResourceChecker<R>,
    ) -> ResourceState
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
            if ResourceState::SpecNotChanged == status { resource_status_checker((resource, stored_object)) } else { status }
        } else {
            ResourceState::New
        }
    }
}

pub struct ResourceFinalizer {}

impl ResourceFinalizer {
    pub async fn delete_resource<R, ReconcileErr>(
        api: &Api<R>,
        finalizer_name: &str,
        resource: &Arc<R>,
    ) -> std::result::Result<Action, Error<ReconcileErr>>
    where
        R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
        R: ResourceExt,
        R: Resource<DynamicType = ()>,
        R: Serialize,
        ReconcileErr: std::error::Error + 'static,
    {
        let res: std::result::Result<Action, Error<_>> =
            finalizer::finalizer(api, finalizer_name, Arc::clone(resource), |event| async move {
                match event {
                    finalizer::Event::Apply(_) | finalizer::Event::Cleanup(_) => Result::<Action, ReconcileErr>::Ok(Action::await_change()),
                }
            })
            .await;
        res
    }
}

pub fn find_linked_routes(state: &State, gateway_id: &ResourceKey) -> Vec<Route> {
    let mut http_routes: Vec<Route> = state
        .get_http_routes_attached_to_gateway(gateway_id)
        .expect("We expect the lock to work")
        .map(|routes| routes.iter().filter_map(|r| Route::try_from(&**r).ok()).collect())
        .unwrap_or_default();
    let mut grpc_routes: Vec<Route> = state
        .get_grpc_routes_attached_to_gateway(gateway_id)
        .expect("We expect the lock to work")
        .map(|routes| routes.iter().filter_map(|r| Route::try_from(&**r).ok()).collect())
        .unwrap_or_default();
    http_routes.append(&mut grpc_routes);
    http_routes
}

pub async fn resolve_namespaces(client: Client) -> BTreeMap<String, BTreeMap<String, String>> {
    let api = Api::<Namespace>::all(client);
    let lp = ListParams::default();
    let maybe_namespaces = api.list(&lp).await;
    let mut namespace_map = BTreeMap::new();
    if let Ok(namespaces) = maybe_namespaces {
        for namespace in namespaces.items {
            if let Some(labels) = namespace.metadata.labels
                && let Some(namesapce_name) = labels.get("kubernetes.io/metadata.name")
            {
                namespace_map.insert(namesapce_name.clone(), labels);
            }
        }
    }
    namespace_map
}
