use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::httproutes::{HTTPRoute, HTTPRouteRule};
use gateway_api_inference_extension::inferencepools::InferencePool;
use kube::{
    runtime::{controller::Action, watcher::Config, Controller},
    Api, Client, Resource,
};
use tokio::sync::mpsc::{self};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::{
    common::{ReferenceValidateRequest, ResourceKey},
    controllers::{
        handlers::ResourceHandler,
        utils::{ResourceCheckerArgs, ResourceState},
        ControllerError, RECONCILE_LONG_WAIT,
    },
    services::patchers::Operation,
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone, TypedBuilder)]
pub struct InferencePoolControllerContext {
    controller_name: String,
    client: Client,
    state: State,
    inference_pool_patcher: mpsc::Sender<Operation<InferencePool>>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
}

#[derive(TypedBuilder)]
pub struct InferencePoolController {
    ctx: Arc<InferencePoolControllerContext>,
}

impl InferencePoolController {
    pub fn get_controller(&self) -> BoxFuture<()> {
        let client = self.ctx.client.clone();
        let context = &self.ctx;

        Controller::new(Api::all(client), Config::default())
            .run(Self::reconcile_inference_pool, Self::error_policy, Arc::clone(context))
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<InferencePoolControllerContext>) -> Action {
        Action::requeue(RECONCILE_LONG_WAIT)
    }

    async fn reconcile_inference_pool(resource: Arc<InferencePool>, ctx: Arc<InferencePoolControllerContext>) -> Result<Action> {
        let controller_name = ctx.controller_name.clone();
        let patcher = ctx.inference_pool_patcher.clone();

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload("Uid must be present".to_owned()));
        };

        let Ok(_) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload("Uid in wrong format".to_owned()));
        };
        let version = resource.meta().resource_version.clone();
        let resource_key = ResourceKey::from(&*resource);

        let state = &ctx.state;
        Err(ControllerError::AlreadyAdded)
    }

    fn check_spec(args: ResourceCheckerArgs<InferencePool>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecNotChanged
        } else {
            ResourceState::SpecChanged
        }
    }

    fn check_status(args: ResourceCheckerArgs<InferencePool>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.status == stored_resource.status {
            ResourceState::StatusNotChanged
        } else {
            ResourceState::StatusChanged
        }
    }
}

#[derive(TypedBuilder)]
struct InferencePoolResourceHandler<R> {
    state: State,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    version: Option<String>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
}

#[async_trait]
impl ResourceHandler<InferencePool> for InferencePoolResourceHandler<InferencePool> {
    fn state(&self) -> &State {
        &self.state
    }

    fn version(&self) -> String {
        self.version.clone().unwrap_or_default()
    }

    fn resource(&self) -> Arc<InferencePool> {
        Arc::clone(&self.resource)
    }

    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }

    async fn on_spec_not_changed(&self, id: ResourceKey, resource: &Arc<InferencePool>, state: &State) -> Result<Action> {
        let () = state.save_inference_pool(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(&self, id: ResourceKey, resource: &Arc<InferencePool>, state: &State) -> Result<Action> {
        let () = state.maybe_save_inference_pool(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, id: ResourceKey, resource: &Arc<InferencePool>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_status_changed(&self, _id: ResourceKey, _resource: &Arc<InferencePool>, _state: &State) -> Result<Action> {
        Ok(Action::await_change())
    }

    async fn on_spec_changed(&self, id: ResourceKey, resource: &Arc<InferencePool>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<InferencePool>, state: &State) -> Result<Action> {
        self.on_deleted(id, resource, state).await
    }
}

impl InferencePoolResourceHandler<InferencePool> {
    async fn on_new_or_changed(&self, inference_pool: ResourceKey, resource: &Arc<InferencePool>, _state: &State) -> Result<Action> {
        let routes: Vec<_> = self
            .state
            .get_http_routes()
            .expect("We expect the lock to work")
            .into_iter()
            .filter(|route| has_inference_pool(route, &inference_pool))
            .collect();
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_deleted(&self, inference_pool: ResourceKey, resource: &Arc<InferencePool>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }
}

fn has_inference_pool(route: &HTTPRoute, inference_pool_key: &ResourceKey) -> bool {
    let empty_rules: Vec<HTTPRouteRule> = vec![];
    let routing_rules = route.spec.rules.as_ref().unwrap_or(&empty_rules);
    routing_rules.iter().enumerate().any(|(i, rr)| {
        rr.backend_refs.as_ref().unwrap_or(&vec![]).iter().any(|br| match br.kind.as_ref() {
            Some(kind) if kind == "InferencePool" => br.name == inference_pool_key.name && br.namespace == Some(inference_pool_key.namespace.clone()),
            _ => false,
        })
    })
}
