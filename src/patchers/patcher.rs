use std::sync::Arc;

use async_trait::async_trait;

use kube::{
    api::{Patch, PatchParams},
    Api, Resource, ResourceExt,
};

use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    controllers::{ControllerError, FinalizerPatcher, ResourceFinalizer},
    state::ResourceKey,
};

pub enum Operation<R>
where
    R: Serialize,
{
    PatchStatus(PatchContext<R>),
    PatchFinalizer(FinalizerContext),
    Delete((ResourceKey, R, String)),
}

pub struct PatchContext<R>
where
    R: Serialize,
{
    pub resource_key: ResourceKey,
    pub resource: R,
    pub controller_name: String,
    pub version: Option<String>,
}

pub struct FinalizerContext {
    pub resource_key: ResourceKey,
    pub controller_name: String,
    pub finalizer_name: String,
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
    fn log_context<'a>(
        &'a self,
        resource_key: &'a ResourceKey,
        controller_name: &'a str,
        version: Option<String>,
    ) -> impl std::fmt::Display + Send;

    async fn start(&mut self) {
        while let Some(event) = self.receiver().recv().await {
            match event {
                Operation::PatchStatus(PatchContext {
                    resource_key,
                    resource,
                    controller_name,
                    version,
                }) => {
                    let api = self.api(&resource_key.namespace);
                    let patch_params = PatchParams::apply(&controller_name);
                    let log_context = self.log_context(&resource_key, &controller_name, version);
                    let res = api
                        .patch_status(&resource_key.name, &patch_params, &Patch::Apply(resource))
                        .await;
                    match res {
                        Ok(_new_gateway) => {
                            info!("{log_context} patch status result ok");
                        }
                        Err(e) => {
                            warn!("{log_context} patch status failed {e:?}");
                        }
                    }
                }
                Operation::PatchFinalizer(FinalizerContext {
                    resource_key,
                    controller_name,
                    finalizer_name,
                }) => {
                    let log_context = self.log_context(&resource_key, &controller_name, None);
                    let api = self.api(&resource_key.namespace);
                    let res = FinalizerPatcher::patch_finalizer(
                        &api,
                        &resource_key.name,
                        &controller_name,
                        &finalizer_name,
                    )
                    .await;
                    match res {
                        Ok(_new_gateway) => {
                            info!("{log_context} finalizer ok");
                        }
                        Err(e) => {
                            warn!("{log_context} finalizer failed {e:?}");
                        }
                    }
                }
                Operation::Delete((resource_key, resource, controller_name)) => {
                    let api = self.api(&resource_key.namespace);
                    let log_context = self.log_context(&resource_key, &controller_name, None);
                    let res: Result<
                        kube::runtime::controller::Action,
                        kube::runtime::finalizer::Error<ControllerError>,
                    > = ResourceFinalizer::delete_resource(
                        &api,
                        &controller_name,
                        &Arc::new(resource),
                    )
                    .await;
                    match res {
                        Ok(_new_gateway) => {
                            info!("{log_context} delete ok");
                        }
                        Err(e) => {
                            warn!("{log_context} delete failed {e:?}");
                        }
                    }
                }
            }
        }
    }
}
