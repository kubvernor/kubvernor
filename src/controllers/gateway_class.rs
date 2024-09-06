use std::{sync::Arc, time::Duration};

use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::gatewayclasses::{GatewayClass, GatewayClassStatus};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    ResourceExt,
};
use kube_core::ObjectMeta;
use log::{debug, info, warn};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{utils::ResourceState, ControllerError};
use crate::state::State;
type Result<T, E = ControllerError> = std::result::Result<T, E>;

struct Context {
    pub client: kube::Client,
    pub controller_name: String,
    state: Arc<Mutex<State>>,
}

pub struct GatewayClassController {
    controller_name: String,
    client: kube::Client,
    api: Api<GatewayClass>,
    state: Arc<Mutex<State>>,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum GatewayClassState {
    New,
    PreviouslyProcessed,
    Added,
    Deleted,
}
impl From<ResourceState> for GatewayClassState {
    fn from(value: ResourceState) -> Self {
        match value {
            ResourceState::Existing => GatewayClassState::Added,
            ResourceState::NotSeenBefore => GatewayClassState::New,
            ResourceState::Deleted => GatewayClassState::Deleted,
        }
    }
}

impl GatewayClassController {
    pub fn new(controller_name: String, client: kube::Client, state: Arc<Mutex<State>>) -> Self {
        GatewayClassController {
            controller_name,
            api: Api::all(client.clone()),
            client,
            state,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            client: self.client.clone(),
            controller_name: self.controller_name.clone(),
            state: Arc::clone(&self.state),
        });
        Controller::new(self.api.clone(), Config::default())
            .run(
                Self::reconcile_gateway_class,
                Self::error_policy,
                Arc::clone(&context),
            )
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<Context>) -> Action {
        Action::await_change()
    }

    async fn reconcile_gateway_class(
        requested_gateway_class: Arc<GatewayClass>,
        ctx: Arc<Context>,
    ) -> Result<Action> {
        let client = &ctx.client;
        let configured_controller_name = &ctx.controller_name;
        let resource_name = requested_gateway_class.name_any();
        let state = &ctx.state;

        let id = Uuid::parse_str(&requested_gateway_class.metadata.uid.clone().ok_or(
            ControllerError::InvalidPayload("Uid must be present".to_owned()),
        )?)
        .map_err(|e| ControllerError::InvalidPayload(format!("Uid in wrong format {e}")))?;

        let controller_name = &requested_gateway_class.spec.controller_name;
        let resource_version = &requested_gateway_class.metadata.resource_version;

        if *configured_controller_name != *controller_name {
            warn!("reconcile_gateway_class: Name don't match {configured_controller_name} {controller_name}");
            return Err(ControllerError::InvalidRecipent);
        };

        info!(
            "reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?}"
	    );
        debug!(
            "reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} {:?}",
            requested_gateway_class
	    );

        let mut state = state.lock().await;

        let maybe_added = { state.gateway_class_names.get(&id) };

        if let Some(stored_object) = maybe_added {
            if stored_object.metadata.resource_version == *resource_version {
                debug!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} skipping as already processed, resource version is the same");
                return Err(ControllerError::AlreadyAdded);
            }
        }

        match Self::check_state(&requested_gateway_class, maybe_added) {
            GatewayClassState::PreviouslyProcessed => {
                state
                    .gateway_class_names
                    .insert(id, Arc::clone(&requested_gateway_class));
                drop(state);
                info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} Added without update");
                Ok(Action::await_change())
            }

            GatewayClassState::Added => {
                info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} Already added");
                Err(ControllerError::AlreadyAdded)
            }

            GatewayClassState::New => {
                info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} New ");
                let updated_gateway_class = Self::update_gateway_class(&requested_gateway_class);

                debug!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} patch new class {updated_gateway_class:?}");

                let patch_params = PatchParams::apply(configured_controller_name).force();
                match Api::<GatewayClass>::all(client.clone())
                    .patch_status(
                        resource_name.trim(),
                        &patch_params,
                        &Patch::Apply(&updated_gateway_class),
                    )
                    .await
                {
                    Ok(_) => {
                        info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} patch result ok");
                        state
                            .gateway_class_names
                            .insert(id, Arc::new(updated_gateway_class));
                        drop(state);
                        Ok(Action::requeue(Duration::from_secs(10)))
                    }
                    Err(e) => {
                        warn!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} patch failed {e:?}");
                        Err(ControllerError::PatchFailed)
                    }
                }
            }

            GatewayClassState::Deleted => {
                info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} Deleted");
                let _ = state.gateway_class_names.remove(&id);

                let api = Api::all(client.clone());
                let res: std::result::Result<Action, finalizer::Error<_>> = finalizer::finalizer(
                    &api,
                    controller_name,
                    Arc::clone(&requested_gateway_class),
                    |event| async move {
                        match event {
                            finalizer::Event::Apply(_) | finalizer::Event::Cleanup(_) => {
                                Result::<Action>::Ok(Action::await_change())
                            }
                        }
                    },
                )
                .await;

                res.map_err(|e| ControllerError::FinalizerPatchFailed(e.to_string()))
            }
        }
    }

    fn update_gateway_class(requested_gateway_class: &Arc<GatewayClass>) -> GatewayClass {
        let mut conditions: Vec<Condition> = vec![];
        let new_condition = Condition {
            last_transition_time: Time(Utc::now()),
            message: "Updated by controller".to_owned(),
            observed_generation: requested_gateway_class.metadata.generation,
            reason: "AcceptedByController".to_owned(),
            status: "True".to_owned(),
            type_: "Accepted".to_owned(),
        };

        conditions.push(new_condition);
        let new_status = GatewayClassStatus {
            conditions: Some(conditions),
        };
        let mut new_gateway_class = (**requested_gateway_class).clone();
        new_gateway_class.status = Some(new_status);
        new_gateway_class.metadata = ObjectMeta::default();
        new_gateway_class
    }

    fn check_state(
        gateway_class: &GatewayClass,
        maybe_added: Option<&Arc<GatewayClass>>,
    ) -> GatewayClassState {
        let state = GatewayClassState::from(ResourceState::check_state(gateway_class, maybe_added));

        if GatewayClassState::New != state {
            return state;
        }
        if let Some(status) = &gateway_class.status {
            if let Some(existing_conditions) = &status.conditions {
                for c in existing_conditions {
                    debug!("reconcile_gateway_class: condition {c:?}");
                    if c.status == "True"
                        && c.type_ == "Accepted"
                        && c.reason == "AcceptedByController"
                    {
                        return GatewayClassState::PreviouslyProcessed;
                    }
                }
            }
        };

        state
    }
}
