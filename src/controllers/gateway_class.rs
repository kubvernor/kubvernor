use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::gatewayclasses::{GatewayClass, GatewayClassStatus};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    Client, Resource,
};
use log::{debug, info, warn};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    utils::{ResourceState, SpecCheckerArgs},
    ControllerError, RECONCILE_ERROR_WAIT,
};
use crate::{
    controllers::{
        resource_handler::ResourceHandler, utils::ResourceFinalizer, GATEWAY_CLASS_FINALIZER_NAME,
        RECONCILE_LONG_WAIT,
    },
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

    async fn reconcile_gateway_class<'a>(
        resource: Arc<GatewayClass>,
        ctx: Arc<Context>,
    ) -> Result<Action> {
        let client = &ctx.client;
        let configured_controller_name = &ctx.controller_name;

        let Some(name) = resource.meta().name.clone() else {
            return Err(ControllerError::InvalidPayload(
                "Resource name is not provided".to_owned(),
            ));
        };

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload(
                "Uid must be present".to_owned(),
            ));
        };

        let Ok(id) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload(
                "Uid in wrong format".to_owned(),
            ));
        };

        let state = Arc::clone(&ctx.state);

        let controller_name = resource.spec.controller_name.clone();
        let version = resource.meta().resource_version.clone();

        if *configured_controller_name != *controller_name {
            warn!("reconcile_gateway_class: Name don't match {configured_controller_name} {controller_name}");
            return Err(ControllerError::InvalidRecipent);
        };

        let maybe_stored_gateway_class = {
            let state = state.lock().await;
            state.get_gateway_class_by_id(id).cloned()
        };

        let handler = GatewayClassResourceHandler {
            state: Arc::clone(&ctx.state),
            id,
            controller_name: controller_name.clone(),
            resource,
            name,
            version,
            client: client.clone(),
            api: Api::all(client.clone()),
        };
        handler
            .process(maybe_stored_gateway_class, Self::check_spec)
            .await
    }

    fn check_spec(args: SpecCheckerArgs<GatewayClass>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecUnchanged
        } else {
            ResourceState::SpecChanged
        }
    }
}

struct GatewayClassResourceHandler<R> {
    state: Arc<Mutex<State>>,
    id: Uuid,
    controller_name: String,
    resource: Arc<R>,
    name: String,
    version: Option<String>,
    client: Client,
    api: Api<R>,
}

struct LogContext<'a> {
    controller_name: &'a str,
    id: Uuid,
    name: &'a str,
    version: Option<String>,
}

impl std::fmt::Display for LogContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconcile_gateway_class: controller_name: {}  id: {}, name: {} version: {:?}",
            self.controller_name, self.id, self.name, self.version
        )
    }
}

impl GatewayClassResourceHandler<GatewayClass> {
    fn update_status_conditions(mut new_gateway_class: GatewayClass) -> GatewayClass {
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

    async fn on_new_or_changed(
        &self,
        id: Uuid,
        resource: &Arc<GatewayClass>,
        state: &mut State,
    ) -> Result<Action> {
        let log_context = self.log_context();
        let updated_gateway_class = Self::update_status_conditions((**resource).clone());
        let controller_name = &self.controller_name;
        let name = &self.name;

        let patch_params = PatchParams::apply(controller_name).force();
        match Api::<GatewayClass>::all(self.client.clone())
            .patch_status(name, &patch_params, &Patch::Apply(&updated_gateway_class))
            .await
        {
            Ok(_) => {
                info!("{log_context} patch result ok");
                state.save_gateway_class(id, resource);
                Ok(Action::await_change())
            }
            Err(e) => {
                warn!("{log_context} patch failed {e:?}");
                Err(ControllerError::PatchFailed)
            }
        }
    }
}

#[async_trait]
impl ResourceHandler<GatewayClass> for GatewayClassResourceHandler<GatewayClass> {
    fn log_context(&self) -> impl std::fmt::Display {
        LogContext {
            controller_name: &self.controller_name,
            id: self.id,
            name: &self.name,
            version: self.version.clone(),
        }
    }

    fn state(&self) -> &Arc<Mutex<State>> {
        &self.state
    }

    fn id(&self) -> Uuid {
        self.id
    }
    fn resource(&self) -> Arc<GatewayClass> {
        Arc::clone(&self.resource)
    }

    async fn on_spec_unchanged(
        &self,
        id: Uuid,
        resource: &Arc<GatewayClass>,
        state: &mut State,
    ) -> Result<Action> {
        state.save_gateway_class(id, resource);
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(
        &self,
        id: Uuid,
        resource: &Arc<GatewayClass>,
        state: &mut State,
    ) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_spec_changed(
        &self,
        id: Uuid,
        resource: &Arc<GatewayClass>,
        state: &mut State,
    ) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(
        &self,
        id: Uuid,
        resource: &Arc<GatewayClass>,
        state: &mut State,
    ) -> Result<Action> {
        let log_context = self.log_context();
        if state
            .get_gateways()
            .any(|g| *g.spec.gateway_class_name == self.name)
        {
            debug!("{log_context} can't delete since there are remaining gateways ");
            Err(ControllerError::ResourceInWrongState)
        } else {
            state.delete_gateway_class(id);
            let res = ResourceFinalizer::delete_resource(
                &self.api,
                GATEWAY_CLASS_FINALIZER_NAME,
                resource,
            )
            .await;
            res.map_err(|e: finalizer::Error<ControllerError>| {
                ControllerError::FinalizerPatchFailed(e.to_string())
            })
        }
    }
}
