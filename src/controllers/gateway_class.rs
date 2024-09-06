use std::sync::Arc;

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
use log::{debug, info, warn};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{utils::ResourceState, ControllerError, RECONCILE_ERROR_WAIT};
use crate::{
    controllers::{GATEWAY_CLASS_FINALIZER_NAME, RECONCILE_LONG_WAIT},
    state::State,
};
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
    fn error_policy<T>(_object: Arc<T>, err: &ControllerError, _ctx: Arc<Context>) -> Action {
        match err {
            ControllerError::PatchFailed
            | ControllerError::AlreadyAdded
            | ControllerError::InvalidPayload(_)
            | ControllerError::InvalidRecipent
            | ControllerError::FinalizerPatchFailed(_)
            | ControllerError::BackendError
            | ControllerError::UnknownResource => Action::requeue(RECONCILE_LONG_WAIT),
            ControllerError::UnknownGatewayClass(_) | ControllerError::ResourceInWrongState => {
                Action::requeue(RECONCILE_ERROR_WAIT)
            }
        }
    }

    async fn reconcile_gateway_class(
        gateway_class: Arc<GatewayClass>,
        ctx: Arc<Context>,
    ) -> Result<Action> {
        let client = &ctx.client;
        let configured_controller_name = &ctx.controller_name;
        let resource_name = gateway_class.name_any();
        let state = &ctx.state;

        let id = Uuid::parse_str(&gateway_class.metadata.uid.clone().ok_or(
            ControllerError::InvalidPayload("Uid must be present".to_owned()),
        )?)
        .map_err(|e| ControllerError::InvalidPayload(format!("Uid in wrong format {e}")))?;

        let controller_name = &gateway_class.spec.controller_name;
        let resource_version = &gateway_class.metadata.resource_version;

        if *configured_controller_name != *controller_name {
            warn!("reconcile_gateway_class: Name don't match {configured_controller_name} {controller_name}");
            return Err(ControllerError::InvalidRecipent);
        };

        info!(
            "reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?}"
	    );

        let mut state = state.lock().await;

        let maybe_added = { state.gateway_class_names.get(&id) };

        if let Some(stored_object) = maybe_added {
            if stored_object.metadata.resource_version == *resource_version {
                debug!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} skipping as already processed, resource version is the same");
                return Err(ControllerError::AlreadyAdded);
            }
        }

        let resource_state = Self::check_state(&gateway_class, maybe_added);
        info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} Resource state {resource_state:?}");
        match resource_state {
            ResourceState::Uptodate => Err(ControllerError::AlreadyAdded),

            ResourceState::New | ResourceState::Changed | ResourceState::SpecChanged => {
                let updated_gateway_class = Self::update_gateway_class((*gateway_class).clone());

                debug!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} patch new ");

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
                        info!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} patch result ok");
                        state
                            .gateway_class_names
                            .insert(id, Arc::new(updated_gateway_class));
                        drop(state);
                        Ok(Action::await_change())
                    }
                    Err(e) => {
                        warn!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} patch failed {e:?}");
                        Err(ControllerError::PatchFailed)
                    }
                }
            }

            ResourceState::Deleted => {
                if state
                    .get_gateways()
                    .any(|g| *g.spec.gateway_class_name == resource_name)
                {
                    debug!("reconcile_gateway_class: {configured_controller_name} {controller_name} {id} {resource_name} {resource_version:?} can't delete since there are remaining gateways ");
                    Err(ControllerError::ResourceInWrongState)
                } else {
                    let _ = state.gateway_class_names.remove(&id);

                    let api = Api::all(client.clone());
                    let res: std::result::Result<Action, finalizer::Error<_>> =
                        finalizer::finalizer(
                            &api,
                            GATEWAY_CLASS_FINALIZER_NAME,
                            Arc::clone(&gateway_class),
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
    }

    fn update_gateway_class(mut new_gateway_class: GatewayClass) -> GatewayClass {
        let mut conditions: Vec<Condition> = vec![];
        let new_condition = Condition {
            last_transition_time: Time(Utc::now()),
            message: "Updated by controller".to_owned(),
            observed_generation: new_gateway_class.metadata.generation,
            reason: "AcceptedByController".to_owned(),
            status: "True".to_owned(),
            type_: "Accepted".to_owned(),
        };

        conditions.push(new_condition);
        let new_status = GatewayClassStatus {
            conditions: Some(conditions),
        };
        new_gateway_class.status = Some(new_status);
        new_gateway_class.metadata.managed_fields = None;
        new_gateway_class
    }

    fn check_state(
        gateway_class: &GatewayClass,
        maybe_added: Option<&Arc<GatewayClass>>,
    ) -> ResourceState {
        let state = ResourceState::check_state(gateway_class, maybe_added);

        if ResourceState::New != state {
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
                        return ResourceState::Changed;
                    }
                }
            }
        };

        state
    }
}
