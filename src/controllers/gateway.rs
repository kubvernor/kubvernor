use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{Gateway, GatewayListeners, GatewayStatusListeners},
    httproutes::{HTTPRoute, HTTPRouteStatus, HTTPRouteStatusParents, HTTPRouteStatusParentsParentRef},
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    runtime::{controller::Action, watcher::Config, Controller},
    Api, Resource, ResourceExt,
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
    common::{
        self, ChangedContext, DeletedContext, GatewayError, GatewayEvent, GatewayProcessedPayload, GatewayResponse, Listener, ListenerConfig, ListenerError, ListenerStatus, ResourceKey, Route,
        DEFAULT_NAMESPACE_NAME,
    },
    controllers::{gateway_processed_handler::GatewayProcessedHandler, utils::VerifiyItems},
    patchers::{FinalizerContext, Operation, PatchContext},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    controller_name: String,
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
}

impl TryFrom<&GatewayListeners> for Listener {
    type Error = ListenerError;

    fn try_from(gateway_listener: &GatewayListeners) -> std::result::Result<Self, Self::Error> {
        let config = ListenerConfig::new(gateway_listener.name.clone(), gateway_listener.port, gateway_listener.hostname.clone());

        match gateway_listener.protocol.as_str() {
            "HTTP" => Ok(Self::Http(config)),
            "HTTPS" => Ok(Self::Https(config)),
            "TCP" => Ok(Self::Tcp(config)),
            "TLS" => Ok(Self::Tls(config)),
            "UDP" => Ok(Self::Udp(config)),
            _ => Err(ListenerError::UnknownProtocol(gateway_listener.protocol.clone())),
        }
    }
}

impl TryFrom<&Gateway> for crate::common::Gateway {
    type Error = GatewayError;

    fn try_from(gateway: &Gateway) -> std::result::Result<Self, Self::Error> {
        let id = Uuid::parse_str(&gateway.metadata.uid.clone().unwrap_or_default()).map_err(|_| GatewayError::ConversionProblem("Can't parse uuid".to_owned()))?;
        let name = gateway.metadata.name.clone().unwrap_or_default();
        if name.is_empty() {
            return Err(GatewayError::ConversionProblem("Name can't be empty".to_owned()));
        }
        let namespace = gateway.metadata.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) = VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));
        if !listener_validation_errrors.is_empty() {
            return Err(GatewayError::ConversionProblem("Misconfigured listeners".to_owned()));
        }

        Ok(Self { id, name, namespace, listeners })
    }
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
            api: Api::all(client),
            state,
            gateway_patcher,
            gateway_class_patcher,
            http_route_patcher,
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

    async fn deploy_gateway(&self, sender: &Sender<GatewayEvent>, gateway: &Arc<Gateway>, state: &State) -> Result<Gateway> {
        let log_context = self.log_context();
        let maybe_gateway = common::Gateway::try_from(&**gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };
        let resource_key = ResourceKey::from(&**gateway);

        let linked_routes = utils::find_linked_routes(state, &resource_key);
        debug!("Linked routes {}", linked_routes.iter().fold(String::new(), |acc, r| acc + r.name()));

        let route_to_listeners_mapping = common::RouteListenerMatcher::filter_matching_routes(&self.resource, &linked_routes);

        let (response_sender, response_receiver) = oneshot::channel();

        let listener_event = GatewayEvent::GatewayChanged(ChangedContext::new(response_sender, backend_gateway, (**gateway).clone(), route_to_listeners_mapping));
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;

        if let Ok(GatewayResponse::GatewayProcessed(gateway_processed)) = response {
            let gateway_event_handler = GatewayProcessedHandler {
                gateway_processed_payload: gateway_processed,
                gateway: gateway.clone(),
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

        // if let Ok(GatewayResponse::GatewayProcessed(GatewayProcessedPayload {
        //     gateway_status,
        //     attached_routes,
        //     ignored_routes,
        // })) = response
        // {
        //     debug!("{log_context} Attached routes  {attached_routes:?}");
        //     let updated_gateway = self.update_gateway_resource(gateway, gateway_status.listeners);
        //     for attached_route in attached_routes {
        //         let updated_route = self.update_accepted_route_parents(state, &attached_route, &resource_key);
        //         if let Some(route) = updated_route {
        //             let route_resource_key = ResourceKey::from(route.meta());
        //             let version = route.resource_version().clone();
        //             let _res = self
        //                 .http_route_patcher
        //                 .send(Operation::PatchStatus(PatchContext {
        //                     resource_key: route_resource_key,
        //                     resource: route,
        //                     controller_name: self.controller_name.clone(),
        //                     version,
        //                 }))
        //                 .await;
        //         }
        //     }
        //     debug!("{log_context} Ignored routes  {ignored_routes:?}");
        //     for ignored_route in ignored_routes {
        //         let updated_route = self.update_rejected_route_parents(state, &ignored_route, &resource_key);
        //         if let Some(route) = updated_route {
        //             let route_resource_key = ResourceKey::from(route.meta());
        //             let version = route.resource_version().clone();
        //             let _res = self
        //                 .http_route_patcher
        //                 .send(Operation::PatchStatus(PatchContext {
        //                     resource_key: route_resource_key,
        //                     resource: route,
        //                     controller_name: self.controller_name.clone(),
        //                     version,
        //                 }))
        //                 .await;
        //         }
        //     }

        //     Ok(updated_gateway)
        // } else {
        //     warn!("{log_context} {response:?} ... Problem {response:?}");
        //     Err(ControllerError::BackendError)
        // }
    }

    fn update_status_conditions(mut gateway: Gateway, listeners_status: Vec<GatewayStatusListeners>) -> Gateway {
        let observed_generation = gateway.metadata.generation;
        let mut status = gateway.status.unwrap_or_default();
        let mut conditions = status.conditions.unwrap_or_default();

        conditions.retain(|f| f.type_ != gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string());
        for f in &mut conditions {
            f.last_transition_time = Time(Utc::now());
            f.observed_generation = observed_generation;
            f.status = "True".to_owned();
            f.reason = gateway_api::apis::standard::constants::GatewayConditionReason::Ready.to_string();
        }

        let new_condition = Condition {
            last_transition_time: Time(Utc::now()),
            message: "Updated by controller".to_owned(),
            observed_generation,
            reason: gateway_api::apis::standard::constants::GatewayConditionReason::Ready.to_string(),
            status: "True".to_owned(),
            type_: gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string(),
        };
        conditions.push(new_condition);
        status.conditions = Some(conditions);
        status.listeners = Some(listeners_status);
        gateway.status = Some(status);
        gateway.metadata.managed_fields = None;
        gateway
    }

    /// * on new gateway we need to find all the relevant data such as routes that might be referencing this gateway
    /// * send all necessary information to the backend
    /// * backend should return information whether the routes was attached to the gateway or not and to which listener/listeners
    /// * we should update gatway's status if the route count has changed for a listener
    /// * we should update the route's status and reflect that the ownership might changed
    ///
    async fn on_new_or_changed(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        let sender = &self.gateway_channel_sender;
        let updated_gateway = self.deploy_gateway(sender, resource, state).await?;
        state.save_gateway(id.clone(), resource);

        let _res = self
            .gateway_patcher
            .send(Operation::PatchStatus(PatchContext {
                resource_key: id,
                resource: updated_gateway,
                controller_name: self.controller_name.clone(),
                version: self.version.clone(),
            }))
            .await;
        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        let controller_name = &self.controller_name;
        let gateway_class_name = &self.gateway_class_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string()) {
                    state.save_gateway(id, resource);
                    self.add_finalizer_to_gateway_class(gateway_class_name).await;
                    let _ = self.add_finalizer(controller_name).await;
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

    // fn update_gateway_resource(&self, gateway: &Gateway, processed_listeners: Vec<ListenerStatus>) -> Gateway {
    //     let log_context = self.log_context();
    //     debug!("{log_context} listener statuses {processed_listeners:?}");
    //     let listener_statuses: Vec<_> = processed_listeners
    //         .into_iter()
    //         .map(|status| match status {
    //             ListenerStatus::Accepted((name, routes)) => GatewayStatusListeners {
    //                 attached_routes: routes,
    //                 conditions: vec![
    //                     Condition {
    //                         last_transition_time: Time(Utc::now()),
    //                         message: "Updated by controller".to_owned(),
    //                         observed_generation: gateway.metadata.generation,
    //                         reason: gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs.to_string(),
    //                         status: "True".to_owned(),
    //                         type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
    //                     },
    //                     Condition {
    //                         last_transition_time: Time(Utc::now()),
    //                         message: "Updated by controller".to_owned(),
    //                         observed_generation: gateway.metadata.generation,
    //                         reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
    //                         status: "True".to_owned(),
    //                         type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
    //                     },
    //                 ],
    //                 name,
    //                 supported_kinds: vec![GatewayStatusListenersSupportedKinds {
    //                     group: None,
    //                     kind: "HTTPRoute".to_owned(),
    //                 }],
    //             },
    //             ListenerStatus::Conflicted(name) => GatewayStatusListeners {
    //                 attached_routes: 0,
    //                 conditions: vec![Condition {
    //                     last_transition_time: Time(Utc::now()),
    //                     message: "Updated by controller".to_owned(),
    //                     observed_generation: gateway.metadata.generation,
    //                     reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
    //                     status: "False".to_owned(),
    //                     type_: gateway_api::apis::standard::constants::ListenerConditionType::Conflicted.to_string(),
    //                 }],
    //                 name,
    //                 supported_kinds: vec![],
    //             },
    //         })
    //         .collect();

    //     Self::update_status_conditions(((*self.resource).clone()).clone(), listener_statuses)
    // }

    // fn update_accepted_route_parents(&self, state: &State, attached_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
    //     self.update_route_parents(
    //         state,
    //         attached_route,
    //         gateway_id,
    //         (
    //             "Accepted",
    //             Condition {
    //                 last_transition_time: Time(Utc::now()),
    //                 message: "Updated by controller".to_owned(),
    //                 observed_generation: None,
    //                 reason: "Accepted".to_owned(),
    //                 status: "True".to_owned(),
    //                 type_: "Accepted".to_owned(),
    //             },
    //         ),
    //     )
    // }

    // fn update_rejected_route_parents(&self, state: &State, rejected_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
    //     self.update_route_parents(
    //         state,
    //         rejected_route,
    //         gateway_id,
    //         (
    //             "Rejected",
    //             Condition {
    //                 last_transition_time: Time(Utc::now()),
    //                 message: "Updated by controller".to_owned(),
    //                 observed_generation: None,
    //                 reason: "Rejected".to_owned(),
    //                 status: "False".to_owned(),
    //                 type_: "Rejected".to_owned(),
    //             },
    //         ),
    //     )
    // }

    // fn update_route_parents(&self, state: &State, route: &Route, gateway_id: &ResourceKey, (new_condition_name, mut new_condition): (&'static str, Condition)) -> Option<HTTPRoute> {
    //     let routes = state.get_http_routes_attached_to_gateway(gateway_id);

    //     if let Some(routes) = routes {
    //         let route = routes
    //             .iter()
    //             .find(|f| f.metadata.name == Some(route.name().to_owned()) && f.metadata.namespace == Some(route.namespace().clone()));

    //         if let Some(mut route) = route.map(|r| (***r).clone()) {
    //             new_condition.observed_generation = route.meta().generation;
    //             let mut status = if let Some(status) = route.status { status } else { HTTPRouteStatus { parents: vec![] } };

    //             status.parents.retain(|p| {
    //                 !(p.controller_name == self.controller_name && p.parent_ref.namespace == self.resource.meta().namespace && self.resource.meta().name == Some(p.parent_ref.name.clone()))
    //             });

    //             let route_parents = HTTPRouteStatusParents {
    //                 conditions: Some(vec![new_condition]),
    //                 controller_name: self.controller_name.clone(),
    //                 parent_ref: HTTPRouteStatusParentsParentRef {
    //                     namespace: self.resource.meta().namespace.clone(),
    //                     name: self.resource.meta().name.clone().unwrap_or_default(),
    //                     ..Default::default()
    //                 },
    //             };
    //             status.parents.push(route_parents);
    //             route.status = Some(status);
    //             route.metadata.managed_fields = None;
    //             return Some(route);
    //         }
    //     }
    //     None
    // }
}
