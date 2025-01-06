use std::sync::Arc;

use async_trait::async_trait;
use kube::{runtime::controller::Action, Resource, ResourceExt};
use tracing::{info, instrument, span, Instrument, Level};

use super::{ControllerError, ResourceChecker, ResourceState, ResourceStateChecker};
use crate::{common::ResourceKey, state::State};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[async_trait]
pub trait ResourceHandler<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static,
{
    #[instrument(level = "info", name="ResourceHandler", skip_all, fields(id = %self.resource_key(), resource=std::any::type_name::<R>(), version=self.version()))]
    async fn process(&self, stored_resource: Option<Arc<R>>, resource_spec_checker: ResourceChecker<R>, resource_status_checker: ResourceChecker<R>) -> Result<Action> {
        let resource = &Arc::clone(&self.resource());
        let id = self.resource_key();
        let state = self.state();

        let resource_state = ResourceStateChecker::check_status(resource, stored_resource.clone(), resource_spec_checker, resource_status_checker);

        info!("Resource state {resource_state:?}");
        let span = span!(Level::INFO, "ResourceHandlerStatus", handler = format!("{resource_state:?}"));

        match resource_state {
            ResourceState::VersionNotChanged => self.on_version_not_changed(id, resource, state).instrument(span).await,
            ResourceState::SpecNotChanged => self.on_spec_not_changed(id, resource, state).instrument(span).await,
            ResourceState::New => self.on_new(id, resource, state).instrument(span).await,
            ResourceState::SpecChanged => self.on_spec_changed(id, resource, state).instrument(span).await,
            ResourceState::StatusChanged => self.on_status_changed(id, resource, state).instrument(span).await,
            ResourceState::StatusNotChanged => self.on_status_not_changed(id, resource, state).instrument(span).await,
            ResourceState::Deleted => self.on_deleted(id, resource, state).instrument(span).await,
        }
    }

    async fn on_version_not_changed(&self, key: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        info!("on_version_not_changed {key}");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_not_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_deleted(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    fn state(&self) -> &State;
    fn resource_key(&self) -> ResourceKey;
    fn version(&self) -> String;
    fn resource(&self) -> Arc<R>;
}
