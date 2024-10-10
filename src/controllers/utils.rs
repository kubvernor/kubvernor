use std::{marker::PhantomData, sync::Arc};

use gateway_api::apis::standard::gateways::Gateway;
use kube::{
    api::{Patch, PatchParams},
    runtime::{
        controller::Action,
        finalizer::{self, Error},
    },
    Api, Resource, ResourceExt,
};
use kube_core::{PartialObjectMeta, PartialObjectMetaExt};
use serde::Serialize;
use tracing::{debug, warn};

use crate::{
    common::{ResourceKey, Route, RouteConfig, DEFAULT_NAMESPACE_NAME},
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

pub struct LogContext<'a, T> {
    pub controller_name: &'a str,
    pub resource_key: &'a ResourceKey,
    pub version: Option<String>,
    pub resource_type: PhantomData<T>,
}

impl<T> std::fmt::Display for LogContext<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: resource_id: {},  version: {:?}", self.resource_type, self.resource_key, self.version)
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
