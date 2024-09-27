use std::sync::Arc;

use async_trait::async_trait;
use kube::{runtime::controller::Action, Resource, ResourceExt};
use tokio::sync::Mutex;
use tracing::info;

use super::{
    utils::{ResourceChecker, ResourceState, ResourceStateChecker},
    ControllerError,
};
use crate::state::{ResourceKey, State};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[async_trait]
pub trait ResourceHandler<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static,
{
    async fn process(
        &self,
        stored_resource: Option<Arc<R>>,
        resource_spec_checker: ResourceChecker<R>,
        resource_status_checker: ResourceChecker<R>,
    ) -> Result<Action> {
        let resource = &Arc::clone(&self.resource());
        let id = self.resource_key();
        let state = self.state();
        let mut state = state.lock().await;
        let state = &mut state;
        let log_context = self.log_context();

        let resource_state = ResourceStateChecker::check_status(
            resource,
            stored_resource.clone(),
            resource_spec_checker,
            resource_status_checker,
        );

        info!("{log_context} Resource state {resource_state:?}");

        match resource_state {
            ResourceState::VersionNotChanged => {
                self.on_version_not_changed(id, resource, state).await
            }
            ResourceState::SpecNotChanged => self.on_spec_not_changed(id, resource, state).await,
            ResourceState::New => self.on_new(id, resource, state).await,
            ResourceState::SpecChanged => self.on_spec_changed(id, resource, state).await,
            ResourceState::StatusChanged => self.on_status_changed(id, resource, state).await,
            ResourceState::StatusNotChanged => {
                self.on_status_not_changed(id, resource, state).await
            }
            ResourceState::Deleted => self.on_deleted(id, resource, state).await,
        }
    }

    async fn on_version_not_changed(
        &self,
        _: ResourceKey,
        _: &Arc<R>,
        _: &mut State,
    ) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_not_changed(
        &self,
        _: ResourceKey,
        _: &Arc<R>,
        _: &mut State,
    ) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, _: ResourceKey, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_changed(&self, _: ResourceKey, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_changed(&self, _: ResourceKey, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(
        &self,
        _: ResourceKey,
        _: &Arc<R>,
        _: &mut State,
    ) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_deleted(&self, _: ResourceKey, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    fn state(&self) -> &Arc<Mutex<State>>;
    fn resource_key(&self) -> ResourceKey;
    fn resource(&self) -> Arc<R>;
    fn log_context(&self) -> impl std::fmt::Display + Send;
}
