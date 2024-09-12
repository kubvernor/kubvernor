use std::sync::Arc;

use kube::{
    api::{Patch, PatchParams},
    runtime::{
        controller::Action,
        finalizer::{self, Error},
    },
    Api, Resource, ResourceExt,
};
use kube_core::{PartialObjectMeta, PartialObjectMetaExt};
use log::{debug, warn};
use serde::Serialize;

use crate::controllers::ControllerError;

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum ResourceState {
    New,
    SpecUnchanged,
    Deleted,
    SpecChanged,
    VersionUnchanged,
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

                match api
                    .patch_metadata(
                        resource_name,
                        &PatchParams::apply(controller_name),
                        &Patch::Apply(&meta),
                    )
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        warn!(
                            "patch_finalizer: {type_name} {controller_name} {resource_name} patch failed {e:?}", 
                        );
                        Err(ControllerError::PatchFailed)
                    }
                }
            }
        } else {
            Err(ControllerError::UnknownResource)
        }
    }
}

pub type SpecCheckerArgs<'a, T> = (&'a Arc<T>, Arc<T>);
pub type SpecChecker<T> = fn(args: SpecCheckerArgs<T>) -> ResourceState;

pub struct ResourceStateChecker {}

impl ResourceStateChecker {
    pub fn check_status<R>(
        resource: &Arc<R>,
        maybe_stored_resource: Option<Arc<R>>,
        resource_spec_checker: SpecChecker<R>,
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
                return ResourceState::VersionUnchanged;
            }
            resource_spec_checker((resource, stored_object))
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
        let res: std::result::Result<Action, Error<_>> = finalizer::finalizer(
            api,
            finalizer_name,
            Arc::clone(resource),
            |event| async move {
                match event {
                    finalizer::Event::Apply(_) | finalizer::Event::Cleanup(_) => {
                        Result::<Action, ReconcileErr>::Ok(Action::await_change())
                    }
                }
            },
        )
        .await;
        res
    }
}
