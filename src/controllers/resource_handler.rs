use std::sync::Arc;

use async_trait::async_trait;
use kube::{runtime::controller::Action, Resource, ResourceExt};
use log::info;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::state::State;

use super::{
    utils::{ResourceState, ResourceStateChecker, SpecChecker},
    ControllerError,
};

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
        resource_spec_checker: SpecChecker<R>,
    ) -> Result<Action> {
        let resource = &Arc::clone(&self.resource());
        let id = self.id();
        let state = self.state();
        let mut state = state.lock().await;
        let state = &mut state;
        let log_context = self.log_context();

        let resource_state =
            ResourceStateChecker::check_status(resource, stored_resource, resource_spec_checker);

        info!("{log_context} Resource state {resource_state:?}");

        match resource_state {
            ResourceState::VersionUnchanged => self.on_version_unchanged(id, resource, state).await,
            ResourceState::SpecUnchanged => self.on_spec_unchanged(id, resource, state).await,
            ResourceState::New => self.on_new(id, resource, state).await,
            ResourceState::SpecChanged => self.on_spec_changed(id, resource, state).await,
            ResourceState::Deleted => self.on_deleted(id, resource, state).await,
        }
    }

    async fn on_version_unchanged(&self, _: Uuid, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_unchanged(&self, _: Uuid, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, _: Uuid, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_changed(&self, _: Uuid, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_deleted(&self, _: Uuid, _: &Arc<R>, _: &mut State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    fn state(&self) -> &Arc<Mutex<State>>;
    fn id(&self) -> Uuid;
    fn resource(&self) -> Arc<R>;
    fn log_context(&self) -> impl std::fmt::Display + Send;
}
