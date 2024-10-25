use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{Gateway, GatewayStatusListeners, GatewayStatusListenersSupportedKinds},
    httproutes::HTTPRoute,
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    runtime::{controller::Action, watcher::Config, Controller},
    Api, Client, Resource,
};

use tokio::sync::{
    mpsc::{self, Sender},
    oneshot, Mutex,
};
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    resource_handler::ResourceHandler,
    utils::{self, LogContext, ResourceCheckerArgs, ResourceState},
    ControllerError, GATEWAY_CLASS_FINALIZER_NAME, RECONCILE_ERROR_WAIT, RECONCILE_LONG_WAIT,
};
use crate::{
    common::{self, ChangedContext, DeletedContext, GatewayEvent, GatewayResponse, ResourceKey},
    controllers::{gateway_processed_handler::GatewayProcessedHandler, utils::ListenerTlsConfigValidator},
    patchers::{FinalizerContext, Operation, PatchContext},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    controller_name: String,
    client: Client,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    state: Arc<Mutex<State>>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
}

pub struct GatewayController {
    pub controller_name: String,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    api: Api<Gateway>,
    state: Arc<Mutex<State>>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    client: kube::Client,
}

impl GatewayController {
    pub(crate) fn new(
        controller_name: String,
        gateway_channel_sender: mpsc::Sender<GatewayEvent>,
        client: kube::Client,
        state: Arc<Mutex<State>>,
        gateway_patcher: mpsc::Sender<Operation<Gateway>>,
        gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
        http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    ) -> Self {
        GatewayController {
            controller_name,
            gateway_channel_sender,
            api: Api::all(client.clone()),
            state,
            gateway_patcher,
            gateway_class_patcher,
            http_route_patcher,
            client,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            controller_name: self.controller_name.clone(),
            gateway_channel_sender: self.gateway_channel_sender.clone(),
            state: Arc::clone(&self.state),
            gateway_patcher: self.gateway_patcher.clone(),
            gateway_class_patcher: self.gateway_class_patcher.clone(),
            http_route_patcher: self.http_route_patcher.clone(),
            client: self.client.clone(),
        });

        Controller::new(self.api.clone(), Config::default())
            .run(Self::reconcile_gateway, Self::error_policy, Arc::clone(&context))
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
            ControllerError::UnknownGatewayClass(_) | ControllerError::ResourceInWrongState | ControllerError::ResourceHasWrongStatus => Action::requeue(RECONCILE_ERROR_WAIT),
        }
    }

    async fn reconcile_gateway(resource: Arc<Gateway>, ctx: Arc<Context>) -> Result<Action> {
        let controller_name = ctx.controller_name.clone();
        let gateway_patcher = ctx.gateway_patcher.clone();
        let gateway_class_patcher = ctx.gateway_class_patcher.clone();
        let http_route_patcher = ctx.http_route_patcher.clone();

        let Some(name) = resource.meta().name.clone() else {
            return Err(ControllerError::InvalidPayload("Resource name is not provided".to_owned()));
        };

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload("Uid must be present".to_owned()));
        };

        let Ok(_) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload("Uid in wrong format".to_owned()));
        };
        let resource_key = ResourceKey::from(&*resource);

        let state = Arc::clone(&ctx.state);
        let version = resource.meta().resource_version.clone();

        let gateway_class_name = {
            let gateway_class_name = &resource.spec.gateway_class_name;
            let state = state.lock().await;
            if !state.get_gateway_classes().any(|gc| gc.metadata.name == Some(gateway_class_name.to_string())) {
                warn!("reconcile_gateway: {controller_name} {name} Unknown gateway class name {gateway_class_name}");
                return Err(ControllerError::UnknownGatewayClass(gateway_class_name.clone()));
            }
            gateway_class_name.clone()
        };

        let maybe_stored_gateway_class = {
            let state = state.lock().await;
            state.get_gateway(&resource_key).cloned()
        };

        let handler = GatewayResourceHandler {
            state: Arc::clone(&ctx.state),
            client: ctx.client.clone(),
            resource_key,
            controller_name: controller_name.clone(),
            gateway_class_name,
            resource,
            version,
            gateway_channel_sender: ctx.gateway_channel_sender.clone(),
            gateway_patcher,
            gateway_class_patcher,
            http_route_patcher,
        };
        handler.process(maybe_stored_gateway_class, Self::check_spec_changed, Self::check_status_changed).await
    }

    fn check_spec_changed(args: ResourceCheckerArgs<Gateway>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecNotChanged
        } else {
            ResourceState::SpecChanged
        }
    }

    fn check_status_changed(args: ResourceCheckerArgs<Gateway>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.status == stored_resource.status {
            ResourceState::StatusNotChanged
        } else {
            ResourceState::StatusChanged
        }
    }
}

struct GatewayResourceHandler<R> {
    state: Arc<Mutex<State>>,
    client: Client,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    version: Option<String>,
    gateway_class_name: String,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
}

#[async_trait]
impl ResourceHandler<Gateway> for GatewayResourceHandler<Gateway> {
    fn log_context(&self) -> impl std::fmt::Display {
        LogContext::<Gateway>::new(&self.controller_name, &self.resource_key, self.version.clone())
    }

    fn state(&self) -> &Arc<Mutex<State>> {
        &self.state
    }

    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }

    fn resource(&self) -> Arc<Gateway> {
        Arc::clone(&self.resource)
    }

    async fn on_spec_not_changed(&self, resource_key: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        state.save_gateway(resource_key, resource);
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, resource_key: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        self.on_new_or_changed(resource_key, resource, state).await
    }

    async fn on_spec_changed(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        self.on_status_changed(id, resource, state).await
    }

    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        self.on_deleted(id, resource, state).await
    }
    async fn on_status_not_changed(&self, resource_key: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        state.maybe_save_gateway(resource_key, resource);
        Err(ControllerError::AlreadyAdded)
    }
}

impl GatewayResourceHandler<Gateway> {
    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        state.delete_gateway(&id);
        let sender = &self.gateway_channel_sender;
        let _res = self.delete_gateway(sender, resource, state).await;
        let _res = self.gateway_patcher.send(Operation::Delete((id.clone(), (**resource).clone(), self.controller_name.clone()))).await;
        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn delete_gateway(&self, sender: &Sender<GatewayEvent>, gateway: &Arc<Gateway>, state: &State) -> Result<Gateway> {
        let log_context = self.log_context();
        let maybe_gateway = common::Gateway::try_from(&**gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };
        let resource_key = ResourceKey::from(&**gateway);
        let linked_routes = utils::find_linked_routes(state, &resource_key);
        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event = GatewayEvent::GatewayDeleted(DeletedContext::new(response_sender, backend_gateway, linked_routes));
        let _ = sender.send(listener_event).await;
        let _response = response_receiver.await;
        Ok((**gateway).clone())
    }

    async fn deploy_gateway(&self, sender: &Sender<GatewayEvent>, gateway: &Arc<Gateway>, state: &mut State) -> Result<Gateway> {
        let log_context = self.log_context();
        let mut updated_gateway = (**gateway).clone();

        let maybe_gateway = common::Gateway::try_from(&updated_gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };
        let resource_key = ResourceKey::from(&updated_gateway);
        let backend_gateway = ListenerTlsConfigValidator::new(backend_gateway, self.client.clone(), log_context.to_string()).validate().await;

        debug!("Gateway {backend_gateway:?}");
        self.resolve_listeners_statuses(&backend_gateway, &mut updated_gateway);

        let linked_routes = utils::find_linked_routes(state, &resource_key);
        debug!("Linked routes {}", linked_routes.iter().fold(String::new(), |acc, r| acc + r.name()));

        let route_to_listeners_mapping = common::RouteListenerMatcher::filter_matching_routes(&self.resource, &linked_routes);

        let (response_sender, response_receiver) = oneshot::channel();

        let listener_event = GatewayEvent::GatewayChanged(ChangedContext::new(response_sender, backend_gateway, updated_gateway.clone(), route_to_listeners_mapping));
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;

        if let Ok(GatewayResponse::GatewayProcessed(gateway_processed)) = response {
            let gateway_event_handler = GatewayProcessedHandler {
                gateway_processed_payload: gateway_processed,
                gateway: updated_gateway,
                state,
                log_context: log_context.to_string(),
                resource_key,
                route_patcher: self.http_route_patcher.clone(),
                controller_name: self.controller_name.clone(),
            };
            gateway_event_handler.deploy_gateway().await
        } else {
            warn!("{log_context} {response:?} ... Problem {response:?}");
            Err(ControllerError::BackendError)
        }
    }

    /// * on new gateway we need to find all the relevant data such as routes that might be referencing this gateway
    /// * send all necessary information to the backend
    /// * backend should return information whether the routes was attached to the gateway or not and to which listener/listeners
    /// * we should update gatway's status if the route count has changed for a listener
    /// * we should update the route's status and reflect that the ownership might changed
    ///
    async fn on_new_or_changed(&self, gateway_id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        let sender = &self.gateway_channel_sender;
        let updated_gateway = self.deploy_gateway(sender, resource, state).await?;
        state.save_gateway(gateway_id.clone(), resource);
        let (sender, receiver) = oneshot::channel();
        let _res = self
            .gateway_patcher
            .send(Operation::PatchStatus(PatchContext {
                resource_key: gateway_id.clone(),
                resource: updated_gateway,
                controller_name: self.controller_name.clone(),
                version: self.version.clone(),
                response_sender: sender,
            }))
            .await;
        let patched_gateway = receiver.await;
        if let Ok(maybe_patched) = patched_gateway {
            match maybe_patched {
                Ok(patched_gateway) => {
                    let patched_gateway = Arc::new(patched_gateway);
                    state.save_gateway(gateway_id.clone(), &patched_gateway);
                    _ = self.on_version_not_changed(gateway_id, &patched_gateway, state).await;
                }
                Err(e) => warn!("{} Error while patching {e}", self.log_context()),
            }
        }

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn on_status_changed(&self, gateway_id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        let controller_name = &self.controller_name;
        let gateway_class_name = &self.gateway_class_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string()) {
                    state.save_gateway(gateway_id, resource);
                    self.add_finalizer_to_gateway_class(gateway_class_name).await;
                    let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                        finalizers.iter().any(|f| f == controller_name)
                    } else {
                        false
                    };

                    if !has_finalizer {
                        let _ = self.add_finalizer(controller_name).await;
                    }

                    return Ok(Action::requeue(RECONCILE_LONG_WAIT));
                }
            }
        }
        Err(ControllerError::ResourceHasWrongStatus)
    }

    async fn on_version_not_changed(&self, gateway_id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        let controller_name = &self.controller_name;
        let gateway_class_name = &self.gateway_class_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string()) {
                    state.save_gateway(gateway_id, resource);
                    self.add_finalizer_to_gateway_class(gateway_class_name).await;
                    let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                        finalizers.iter().any(|f| f == controller_name)
                    } else {
                        false
                    };

                    if !has_finalizer {
                        let _ = self.add_finalizer(controller_name).await;
                    }

                    return Ok(Action::requeue(RECONCILE_LONG_WAIT));
                }
            }
        }
        Err(ControllerError::ResourceHasWrongStatus)
    }

    async fn add_finalizer(&self, controller_name: &str) -> Result<Action> {
        let _res = self
            .gateway_patcher
            .send(Operation::PatchFinalizer(FinalizerContext {
                resource_key: self.resource_key.clone(),
                controller_name: controller_name.to_owned(),
                finalizer_name: controller_name.to_owned(),
            }))
            .await;

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn add_finalizer_to_gateway_class(&self, gateway_class_name: &str) {
        let key = ResourceKey::new(gateway_class_name);
        let _res = self
            .gateway_class_patcher
            .send(Operation::PatchFinalizer(FinalizerContext {
                resource_key: key,
                controller_name: self.controller_name.clone(),
                finalizer_name: GATEWAY_CLASS_FINALIZER_NAME.to_owned(),
            }))
            .await;
    }

    fn resolve_listeners_statuses(&self, gateway: &common::Gateway, kube_gateway: &mut Gateway) {
        let log_context = self.log_context();
        let mut status = kube_gateway.status.clone().unwrap_or_default();
        let mut listeners_statuses = vec![];
        let generation = kube_gateway.metadata.generation;

        gateway.listeners.iter().for_each(|l| {
            let name = l.name().to_owned();
            debug!("{log_context} Processing listener {name}");
            let mut listener_status = GatewayStatusListeners { name, ..Default::default() };

            let listener_conditions = &mut listener_status.conditions;

            for condition in l.conditions() {
                let (status, type_, reason) = condition.resolved_type();
                let status = status.to_owned();
                let type_ = type_.to_string();
                let reason = reason.to_string();
                match condition {
                    common::ListenerCondition::Resolved(allowed_routes)
                    | common::ListenerCondition::ResolvedWithNotAllowedRoutes(allowed_routes)
                    | common::ListenerCondition::InvalidCertificates(allowed_routes) => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                        listener_status.supported_kinds = allowed_routes.iter().map(|r| GatewayStatusListenersSupportedKinds { group: None, kind: r.clone() }).collect();
                    }

                    common::ListenerCondition::InvalidAllowedRoutes => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                        listener_status.supported_kinds = vec![];
                    }
                }
            }
            listeners_statuses.push(listener_status);
        });
        status.listeners = Some(listeners_statuses);
        kube_gateway.status = Some(status);
    }
}
