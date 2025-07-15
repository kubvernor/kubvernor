use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::httproutes::{HTTPRoute, HTTPRouteRule};
use gateway_api_inference_extension::inferencepools::{InferencePool, InferencePoolStatusParent};
use k8s_openapi::{
    api::core::v1::ObjectReference,
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    runtime::{controller::Action, watcher::Config, Controller},
    Api, Client, Resource,
};
use tokio::sync::{
    mpsc::{self},
    oneshot,
};
use tracing::{warn, Span};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::{
    common::{ReferenceValidateRequest, ResourceKey},
    controllers::{
        handlers::ResourceHandler,
        utils::{ResourceCheckerArgs, ResourceState},
        ControllerError, INFERENCE_POOL_CONDITION_MESSAGE, RECONCILE_LONG_WAIT,
    },
    services::patchers::{Operation, PatchContext},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone, TypedBuilder)]
pub struct InferencePoolControllerContext {
    controller_name: String,
    client: Client,
    state: State,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
    inference_pool_patcher_sender: mpsc::Sender<Operation<InferencePool>>,
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

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload("Uid must be present".to_owned()));
        };

        let Ok(_) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload("Uid in wrong format".to_owned()));
        };
        let version = resource.meta().resource_version.clone();
        let resource_key = ResourceKey::from(&*resource);

        let state = &ctx.state;

        let maybe_stored_inference_pool = state.get_inference_pool(&resource_key).expect("We expect the lock to work");
        let handler = InferencePoolControllerHandler::builder()
            .state(ctx.state.clone())
            .resource_key(resource_key)
            .controller_name(controller_name)
            .resource(resource)
            .version(version)
            .inference_pool_patcher_sender(ctx.inference_pool_patcher_sender.clone())
            .validate_references_channel_sender(ctx.validate_references_channel_sender.clone())
            .build();
        handler.process(maybe_stored_inference_pool, Self::check_spec_changed, Self::check_status_changed).await
    }

    fn check_spec_changed(args: ResourceCheckerArgs<InferencePool>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecNotChanged
        } else {
            ResourceState::SpecChanged
        }
    }

    fn check_status_changed(args: ResourceCheckerArgs<InferencePool>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.status == stored_resource.status {
            ResourceState::StatusNotChanged
        } else {
            ResourceState::StatusChanged
        }
    }
}

#[derive(TypedBuilder)]
struct InferencePoolControllerHandler<R> {
    state: State,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    version: Option<String>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
    inference_pool_patcher_sender: mpsc::Sender<Operation<InferencePool>>,
}

#[async_trait]
impl ResourceHandler<InferencePool> for InferencePoolControllerHandler<InferencePool> {
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

impl InferencePoolControllerHandler<InferencePool> {
    async fn on_new_or_changed(&self, inference_pool_key: ResourceKey, resource: &Arc<InferencePool>, _state: &State) -> Result<Action> {
        warn!("Inference pool : New");
        let routes: Vec<_> = self
            .state
            .get_http_routes()
            .expect("We expect the lock to work")
            .into_iter()
            .filter(|route| has_inference_pool(route, &inference_pool_key))
            .collect();
        if routes.is_empty() {
            warn!("Inference pool : No routes");
            Ok(Action::requeue(Duration::from_secs(20)))
        } else {
            let gateways_ids = self
                .state
                .get_gateways_with_http_routes(&routes.iter().map(|r| ResourceKey::from(r.as_ref())).collect())
                .expect("We expect the lock to work");
            let accepted = Condition {
                last_transition_time: Time(Utc::now()),
                message: INFERENCE_POOL_CONDITION_MESSAGE.to_owned(),
                observed_generation: None,
                reason: "Accepted".to_owned(),
                status: "True".to_owned(),
                type_: "Accepted".to_owned(),
            };
            let mut inference_pool = (**resource).clone();
            let mut inference_pool_status = inference_pool.status.clone().unwrap_or_default();
            inference_pool_status.parent = Some(
                gateways_ids
                    .iter()
                    .map(|id| InferencePoolStatusParent {
                        conditions: Some(vec![accepted.clone()]),
                        parent_ref: ObjectReference {
                            kind: Some(id.kind.clone()),
                            name: Some(id.name.clone()),
                            namespace: Some(id.namespace.clone()),
                            ..Default::default()
                        },
                    })
                    .collect(),
            );
            inference_pool.status = Some(inference_pool_status);
            let (sender, receiver) = oneshot::channel();
            let _ = self
                .inference_pool_patcher_sender
                .send(Operation::PatchStatus(PatchContext {
                    resource_key: inference_pool_key.clone(),
                    resource: inference_pool,
                    controller_name: self.controller_name.clone(),
                    response_sender: sender,
                    span: Span::current(),
                }))
                .await;
            match receiver.await {
                Ok(Err(_)) | Err(_) => warn!("Could't patch status"),
                _ => (),
            }
            self.state.save_inference_pool(inference_pool_key.clone(), resource).expect("We expect the lock to work");

            let _ = self
                .validate_references_channel_sender
                .send(ReferenceValidateRequest::UpdatedGateways {
                    reference: inference_pool_key,
                    gateways: gateways_ids,
                })
                .await;
            Ok(Action::await_change())
        }
    }

    async fn on_deleted(&self, _inference_pool: ResourceKey, _resource: &Arc<InferencePool>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }
}

fn has_inference_pool(route: &HTTPRoute, inference_pool_key: &ResourceKey) -> bool {
    warn!("Inference Pool checking if route {} has a pool {}", ResourceKey::from(route), inference_pool_key);
    let empty_rules: Vec<HTTPRouteRule> = vec![];
    let routing_rules = route.spec.rules.as_ref().unwrap_or(&empty_rules);
    routing_rules.iter().enumerate().any(|(i, rr)| {
        rr.backend_refs.as_ref().unwrap_or(&vec![]).iter().any(|br| match br.kind.as_ref() {
            Some(kind) if kind == "InferencePool" => {
                br.name == inference_pool_key.name
                    && if br.namespace.is_some() {
                        br.namespace == Some(inference_pool_key.namespace.clone())
                    } else {
                        true
                    }
            }
            _ => false,
        })
    })
}
