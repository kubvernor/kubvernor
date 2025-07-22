use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::httproutes::{HTTPRoute, HTTPRouteRule};
use gateway_api_inference_extension::inferencepools::{InferencePool, InferencePoolStatus, InferencePoolStatusParent, InferencePoolStatusParentParentRef};
use k8s_openapi::{
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
use tracing::{debug, info, warn};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::{
    common::{ReferenceValidateRequest, ResourceKey},
    controllers::{
        handlers::ResourceHandler,
        utils::{ResourceCheckerArgs, ResourceState},
        ControllerError, RECONCILE_LONG_WAIT,
    },
    services::patchers::{DeleteContext, Operation, PatchContext},
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

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<InferencePool>, state: &State) -> Result<Action> {
        let () = state.maybe_save_inference_pool(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
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
        let routes: Vec<_> = self
            .state
            .get_http_routes()
            .expect("We expect the lock to work")
            .into_iter()
            .filter(|route| has_inference_pool(route, &inference_pool_key))
            .collect();
        if routes.is_empty() {
            Ok(Action::requeue(Duration::from_secs(20)))
        } else {
            let gateways_ids = self
                .state
                .get_gateways_with_http_routes(&routes.iter().map(|r| ResourceKey::from(r.as_ref())).collect())
                .expect("We expect the lock to work");

            let mut inference_pool = create_accepted_inference_pool_status((**resource).clone(), &gateways_ids);

            self.state
                .save_inference_pool(inference_pool_key.clone(), &Arc::new(inference_pool.clone()))
                .expect("We expect the lock to work");

            inference_pool.metadata.managed_fields = None;
            let (sender, receiver) = oneshot::channel();
            let _ = self
                .inference_pool_patcher_sender
                .send(Operation::PatchStatus(PatchContext {
                    resource_key: inference_pool_key.clone(),
                    resource: inference_pool,
                    controller_name: self.controller_name.clone(),
                    response_sender: sender,
                }))
                .await;
            match receiver.await {
                Ok(Err(_)) | Err(_) => warn!("Could't patch status"),
                _ => (),
            }

            let _ = self.add_finalizer(&self.resource).await?;

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

    async fn on_deleted(&self, inference_pool_key: ResourceKey, resource: &Arc<InferencePool>, _: &State) -> Result<Action> {
        let deleted = self.state.delete_inference_pool(&inference_pool_key).expect("We expect the lock to work");
        if deleted.is_none() {
            warn!("Unable to delete {inference_pool_key:?}");
        }

        let inference_pool = (**resource).clone();

        let _res = self
            .inference_pool_patcher_sender
            .send(Operation::Delete(DeleteContext {
                resource_key: inference_pool_key.clone(),
                resource: inference_pool,
                controller_name: self.controller_name.clone(),
            }))
            .await;

        let routes: Vec<_> = self
            .state
            .get_http_routes()
            .expect("We expect the lock to work")
            .into_iter()
            .filter(|route| has_inference_pool(route, &inference_pool_key))
            .collect();
        if !routes.is_empty() {
            let gateways_ids = self
                .state
                .get_gateways_with_http_routes(&routes.iter().map(|r| ResourceKey::from(r.as_ref())).collect())
                .expect("We expect the lock to work");

            let _ = self
                .validate_references_channel_sender
                .send(ReferenceValidateRequest::UpdatedGateways {
                    reference: inference_pool_key,
                    gateways: gateways_ids,
                })
                .await;
        }
        Ok(Action::await_change())
    }

    async fn add_finalizer(&self, resource: &Arc<impl Resource>) -> Result<Action, ControllerError> {
        if let Some(finalizer) = super::needs_finalizer(&self.resource_key, &self.controller_name, resource.meta()) {
            let _res = self.inference_pool_patcher_sender.send(finalizer).await;
        }

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }
}

fn has_inference_pool(route: &HTTPRoute, inference_pool_key: &ResourceKey) -> bool {
    let empty_rules: Vec<HTTPRouteRule> = vec![];
    let routing_rules = route.spec.rules.as_ref().unwrap_or(&empty_rules);
    routing_rules.iter().any(|rr| {
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

fn create_accepted_inference_pool_status(mut inference_pool: InferencePool, gateways_ids: &BTreeSet<ResourceKey>) -> InferencePool {
    inference_pool.status = None;
    create_inference_pool_status_with_conditions(inference_pool, gateways_ids, &[InferencePoolCondition::Accepted(None).get_condition()])
}

pub fn clear_all_conditions(mut inference_pool: InferencePool, gateways_ids: &BTreeSet<ResourceKey>) -> InferencePool {
    if let Some(status) = inference_pool.status.as_mut() {
        if let Some(parents) = status.parent.as_ref() {
            let new_parents = parents.iter().filter(|parent| {
                let key = ResourceKey::from(&parent.parent_ref);
                gateways_ids.contains(&key)
            });

            status.parent = Some(new_parents.cloned().collect());
        }
    }
    inference_pool
}

fn create_inference_pool_status_with_conditions(mut inference_pool: InferencePool, gateways_ids: &BTreeSet<ResourceKey>, conditions: &[Condition]) -> InferencePool {
    let mut inference_pool_status = inference_pool.status.clone().unwrap_or_default();
    inference_pool_status.parent = Some(
        gateways_ids
            .iter()
            .map(|id| InferencePoolStatusParent {
                conditions: Some(conditions.to_vec()),
                parent_ref: InferencePoolStatusParentParentRef {
                    kind: Some(id.kind.clone()),
                    name: id.name.clone(),
                    namespace: Some(id.namespace.clone()),
                    ..Default::default()
                },
            })
            .collect(),
    );

    inference_pool.status = Some(inference_pool_status);
    inference_pool
}

pub fn update_inference_pool_parents(gateway: &ResourceKey, mut inference_pool: InferencePool, resolved_endpoint_picker: bool) -> InferencePool {
    let gateway_name = gateway.name.clone();
    let gateway_namespace = gateway.namespace.clone();

    let mut parents = inference_pool.status.clone().map(|s| s.parent.unwrap_or_default()).unwrap_or_default();

    let conditions = if resolved_endpoint_picker {
        vec![
            InferencePoolCondition::Accepted(inference_pool.metadata.generation).get_condition(),
            InferencePoolCondition::Resolved.get_condition(),
        ]
    } else {
        vec![
            InferencePoolCondition::Accepted(inference_pool.metadata.generation).get_condition(),
            InferencePoolCondition::NotResolved.get_condition(),
        ]
    };

    if let Some(parent) = parents
        .iter_mut()
        .find(|p| p.parent_ref.kind == Some("Gateway".to_owned()) && p.parent_ref.name == gateway_name.clone() && p.parent_ref.namespace == Some(gateway_namespace.clone()))
    {
        update_conditions(parent, conditions);
        info!("Updating conditions {gateway} {parent:?}");
    } else {
        let new_parent = InferencePoolStatusParent {
            conditions: Some(conditions),
            parent_ref: InferencePoolStatusParentParentRef {
                name: gateway_name.clone(),
                namespace: Some(gateway_namespace.clone()),
                kind: Some("Gateway".to_owned()),
                ..Default::default()
            },
        };
        parents.push(new_parent);
    }

    let parents = parents.into_iter().filter(|p| p.parent_ref.kind == Some("Gateway".to_owned())).collect();

    inference_pool.status = Some(InferencePoolStatus { parent: Some(parents) });
    inference_pool
}

fn update_conditions(parent: &mut InferencePoolStatusParent, new_conditions: Vec<Condition>) {
    if let Some(conditions) = &parent.conditions {
        let conditions: BTreeSet<_> = conditions.clone().into_iter().map(ConditionHolder).collect();
        let new_conditions: BTreeSet<_> = new_conditions.into_iter().map(ConditionHolder).collect();

        // common conditions such as accepted
        let same_conditions: BTreeSet<ConditionHolder> = conditions.intersection(&new_conditions).cloned().collect();

        // new unique conditions
        let new_conditions: BTreeSet<ConditionHolder> = new_conditions.difference(&same_conditions).cloned().collect();

        let conditions = same_conditions.union(&new_conditions).cloned().map(std::convert::Into::into).collect();

        parent.conditions = Some(conditions);
    } else {
        parent.conditions = Some(new_conditions);
    }
}
#[derive(Debug, Clone)]
struct ConditionHolder(Condition);

impl From<ConditionHolder> for Condition {
    fn from(val: ConditionHolder) -> Self {
        val.0
    }
}

impl From<Condition> for ConditionHolder {
    fn from(value: Condition) -> Self {
        ConditionHolder(value)
    }
}
impl Ord for ConditionHolder {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.reason.cmp(&other.0.reason)
    }
}

impl PartialOrd for ConditionHolder {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for ConditionHolder {}

impl PartialEq for ConditionHolder {
    fn eq(&self, other: &Self) -> bool {
        self.0.reason == other.0.reason
    }
}

pub fn remove_inference_pool_parents(mut inference_pool: InferencePool, gateways_ids: &BTreeSet<ResourceKey>) -> InferencePool {
    let parents = inference_pool.status.clone().map(|s| s.parent.unwrap_or_default()).unwrap_or_default();
    let parents = parents
        .into_iter()
        .filter(|p| {
            let key = ResourceKey::from(&p.parent_ref);
            let contains = !gateways_ids.contains(&key);
            info!("Filtering inference pools {key:?} in {gateways_ids:?} {contains}");
            !gateways_ids.contains(&key)
        })
        .collect();

    inference_pool.status = Some(InferencePoolStatus { parent: Some(parents) });
    inference_pool
}

pub enum InferencePoolCondition {
    Accepted(Option<i64>),
    Resolved,
    NotResolved,
}

const INFERENCE_POOL_CONDITION_MESSAGE: &str = "Inference Pool status updated by controller";

impl InferencePoolCondition {
    pub fn get_condition(self) -> Condition {
        match self {
            InferencePoolCondition::Accepted(generation) => Condition {
                last_transition_time: Time(Utc::now()),
                message: INFERENCE_POOL_CONDITION_MESSAGE.to_owned(),
                observed_generation: generation,
                reason: "Accepted".to_owned(),
                status: "True".to_owned(),
                type_: "Accepted".to_owned(),
            },
            InferencePoolCondition::Resolved => Condition {
                last_transition_time: Time(Utc::now()),
                message: INFERENCE_POOL_CONDITION_MESSAGE.to_owned(),
                observed_generation: None,
                reason: "ResolvedRefs".to_owned(),
                status: "True".to_owned(),
                type_: "ResolvedRefs".to_owned(),
            },
            InferencePoolCondition::NotResolved => Condition {
                last_transition_time: Time(Utc::now()),
                message: INFERENCE_POOL_CONDITION_MESSAGE.to_owned(),
                observed_generation: None,
                reason: "ResolvedRefs".to_owned(),
                status: "False".to_owned(),
                type_: "ResolvedRefs".to_owned(),
            },
        }
    }
}
