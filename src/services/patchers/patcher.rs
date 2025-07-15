use std::sync::Arc;

use async_trait::async_trait;
use kube::{
    api::{Patch, PatchParams},
    Api, Resource, ResourceExt,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{info, span, warn, Instrument, Level, Span};

use crate::{
    common::ResourceKey,
    controllers::{ControllerError, FinalizerPatcher, ResourceFinalizer},
};

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
    pub span: Span,
}

pub struct FinalizerContext {
    pub resource_key: ResourceKey,
    pub controller_name: String,
    pub finalizer_name: String,
    pub span: Span,
}

pub struct DeleteContext<R> {
    pub resource_key: ResourceKey,
    pub resource: R,
    pub controller_name: String,
    pub span: Span,
}

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
                Operation::PatchStatus(PatchContext {
                    resource_key,
                    mut resource,
                    controller_name,
                    response_sender,
                    span,
                }) => {
                    let span = span!(parent: &span, Level::INFO, "PatcherService", resource= %std::any::type_name_of_val(&resource), operation="PatchStatus", id = %resource_key);
                    resource.meta_mut().managed_fields = None;
                    resource.meta_mut().resource_version = Option::<String>::None;
                    let api = self.api(&resource_key.namespace);
                    let patch_params = PatchParams::apply(&controller_name).force();

                    let res = api.patch_status(&resource_key.name, &patch_params, &Patch::Apply(resource)).instrument(span.clone()).await;
                    match &res {
                        Ok(_new_gateway) => span.in_scope(|| {
                            info!("patch status result ok");
                        }),
                        Err(e) => span.in_scope(|| {
                            warn!("patch status failed {e:?}");
                        }),
                    }
                    let _ = response_sender.send(res);
                }
                Operation::PatchFinalizer(FinalizerContext {
                    resource_key,
                    controller_name,
                    finalizer_name,
                    span,
                }) => {
                    let span = span!(parent: &span, Level::INFO, "PatcherService",  operation="PatchFinalizer", id = %resource_key);
                    let api = self.api(&resource_key.namespace);
                    let res = FinalizerPatcher::patch_finalizer(&api, &resource_key.name, &controller_name, &finalizer_name)
                        .instrument(span.clone())
                        .await;
                    match &res {
                        Ok(_new_gateway) => span.in_scope(|| {
                            info!("finalizer ok");
                        }),
                        Err(e) => span.in_scope(|| {
                            warn!("finalizer failed {resource_key} {controller_name} {finalizer_name} {e:?}");
                        }),
                    }
                }
                Operation::Delete(DeleteContext {
                    resource_key,
                    resource,
                    controller_name,
                    span,
                }) => {
                    let span = span!(parent: &span, Level::INFO, "PatcherService", resource= %std::any::type_name_of_val(&resource), operation="PatchDelete", id = %resource_key);
                    let api = self.api(&resource_key.namespace);
                    let res: Result<kube::runtime::controller::Action, kube::runtime::finalizer::Error<ControllerError>> =
                        ResourceFinalizer::delete_resource(&api, &controller_name, &Arc::new(resource)).instrument(span.clone()).await;
                    match res {
                        Ok(_new_gateway) => span.in_scope(|| {
                            info!("delete result ok");
                        }),
                        Err(e) => span.in_scope(|| {
                            warn!("delete failed {e:?}");
                        }),
                    }
                }
            }
        }
        crate::Result::<()>::Ok(())
    }
}
