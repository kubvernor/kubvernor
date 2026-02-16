use std::sync::Arc;

use async_trait::async_trait;
use kube::{
    Api, Resource, ResourceExt,
    api::{Patch, PatchParams},
};
use kubvernor_common::ResourceKey;
use log::{debug, error, info};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::controllers::{ControllerError, FinalizerPatcher, ResourceFinalizer};

pub enum Operation<R>
where
    R: Serialize,
{
    PatchStatus(PatchContext<R>),
    PatchFinalizer(FinalizerContext),
    Delete(DeleteContext<R>),
}

pub struct PatchContext<R>
where
    R: Serialize,
{
    pub resource_key: ResourceKey,
    pub resource: R,
    pub controller_name: String,
    pub response_sender: tokio::sync::oneshot::Sender<Result<R, kube::Error>>,
}

pub struct FinalizerContext {
    pub resource_key: ResourceKey,
    pub controller_name: String,
    pub finalizer_name: String,
}

pub struct DeleteContext<R> {
    pub resource_key: ResourceKey,
    pub resource: R,
    pub controller_name: String,
}

const TARGET: &str = "Services::PatcherService";

#[async_trait]
pub trait Patcher<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug + Serialize,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static,
{
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<R>>;
    fn api(&self, namespace: &str) -> Api<R>;

    async fn start(&mut self) -> crate::Result<()> {
        while let Some(event) = self.receiver().recv().await {
            match event {
                Operation::PatchStatus(PatchContext { resource_key, mut resource, controller_name, response_sender }) => {
                    info!(target: TARGET, "{} PatchStatus {}", kubvernor_common::format_resource::<R>(), resource_key);
                    resource.meta_mut().resource_version = Option::<String>::None;
                    let api = self.api(&resource_key.namespace);
                    let patch_params = PatchParams::apply(&controller_name).force();

                    let res = api.patch_status(&resource_key.name, &patch_params, &Patch::Apply(resource)).await;
                    match &res {
                        Ok(_new_gateway) => debug!(target: TARGET,"patch status result ok"),
                        Err(e) => error!(target: TARGET,"patch status failed {e:?}"),
                    }
                    let _ = response_sender.send(res);
                },
                Operation::PatchFinalizer(FinalizerContext { resource_key, controller_name, finalizer_name }) => {
                    info!(target: TARGET, "{} PatchFinalizer {}", kubvernor_common::format_resource::<R>(), resource_key);

                    let api = self.api(&resource_key.namespace);
                    let res = FinalizerPatcher::patch_finalizer(&api, &resource_key.name, &controller_name, &finalizer_name).await;
                    match &res {
                        Ok(_new_gateway) => debug!(target: TARGET, "finalizer ok"),
                        Err(e) => {
                            error!(target: TARGET, "finalizer failed {resource_key} {controller_name} {finalizer_name} {e:?}");
                        },
                    }
                },
                Operation::Delete(DeleteContext { resource_key, resource, controller_name }) => {
                    info!(target: TARGET, "{} PatchDelete {}", kubvernor_common::format_resource::<R>(), resource_key);
                    let api = self.api(&resource_key.namespace);
                    let res: Result<kube::runtime::controller::Action, kube::runtime::finalizer::Error<ControllerError>> =
                        ResourceFinalizer::delete_resource(&api, &controller_name, &Arc::new(resource)).await;
                    match res {
                        Ok(_new_gateway) => debug!(target: TARGET, "delete result ok"),
                        Err(e) => error!(target: TARGET, "{resource_key} {controller_name} delete failed {e:?}"),
                    }
                },
            }
        }
        crate::Result::<()>::Ok(())
    }
}
