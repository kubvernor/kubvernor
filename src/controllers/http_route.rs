use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gateways::Gateway,
    httproutes::{self, HTTPRoute, HTTPRouteParentRefs, HTTPRouteStatus, HTTPRouteStatusParents, HTTPRouteStatusParentsParentRef},
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
    mpsc::{self},
    oneshot, Mutex,
};
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    resource_handler::ResourceHandler,
    utils::{LogContext, ResourceCheckerArgs, ResourceState, RouteListenerMatcher},
    ControllerError, RECONCILE_LONG_WAIT,
};
use crate::{
    common::{self, GatewayEvent, ResourceKey, Route, VerifiyItems},
    controllers::gateway_deployer::GatewayDeployer,
    patchers::{FinalizerContext, Operation, PatchContext},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    pub controller_name: String,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    state: Arc<Mutex<State>>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    client: Client,
}

pub struct HttpRouteController {
    pub controller_name: String,
    client: kube::Client,
    state: Arc<Mutex<State>>,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
}

impl HttpRouteController {
    pub(crate) fn new(
        controller_name: String,
        client: kube::Client,
        gateway_channel_sender: mpsc::Sender<GatewayEvent>,
        state: Arc<Mutex<State>>,
        http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
        gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    ) -> Self {
        HttpRouteController {
            controller_name,
            client,
            state,
            gateway_channel_sender,
            http_route_patcher,
            gateway_patcher,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            gateway_channel_sender: self.gateway_channel_sender.clone(),
            controller_name: self.controller_name.clone(),
            state: Arc::clone(&self.state),
            http_route_patcher: self.http_route_patcher.clone(),
            gateway_patcher: self.gateway_patcher.clone(),
            client: self.client.clone(),
        });

        let api = Api::<HTTPRoute>::all(self.client.clone());
        Controller::new(api, Config::default())
            .run(Self::reconcile_http_route, Self::error_policy, Arc::clone(&context))
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<Context>) -> Action {
        Action::requeue(RECONCILE_LONG_WAIT)
    }

    async fn reconcile_http_route(resource: Arc<httproutes::HTTPRoute>, ctx: Arc<Context>) -> Result<Action> {
        let controller_name = ctx.controller_name.clone();
        let http_route_patcher = ctx.http_route_patcher.clone();
        let gateway_patcher = ctx.gateway_patcher.clone();

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload("Uid must be present".to_owned()));
        };

        let Ok(_) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload("Uid in wrong format".to_owned()));
        };

        let resource_key = ResourceKey::from(&*resource);

        let state = Arc::clone(&ctx.state);
        let version = resource.meta().resource_version.clone();

        let maybe_stored_route = {
            let state = state.lock().await;
            state.get_http_route_by_id(&resource_key).cloned()
        };

        let _ = Route::try_from(&*resource)?;

        let handler = HTTPRouteHandler {
            state: Arc::clone(&ctx.state),
            resource_key,
            controller_name: controller_name.clone(),
            resource,
            version,
            gateway_channel_sender: ctx.gateway_channel_sender.clone(),
            http_route_patcher,
            gateway_patcher,
            client: ctx.client.clone(),
        };
        handler.process(maybe_stored_route, Self::check_spec, Self::check_status).await
    }

    fn check_spec(args: ResourceCheckerArgs<HTTPRoute>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecNotChanged
        } else {
            ResourceState::SpecChanged
        }
    }

    fn check_status(args: ResourceCheckerArgs<HTTPRoute>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.status == stored_resource.status {
            ResourceState::StatusNotChanged
        } else {
            ResourceState::StatusChanged
        }
    }
}

struct HTTPRouteHandler<R> {
    state: Arc<Mutex<State>>,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    version: Option<String>,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    client: Client,
}

#[async_trait]
impl ResourceHandler<HTTPRoute> for HTTPRouteHandler<HTTPRoute> {
    fn log_context(&self) -> impl std::fmt::Display {
        LogContext::<HTTPRoute>::new(&self.controller_name, &self.resource_key, self.version.clone())
    }

    fn state(&self) -> &Arc<Mutex<State>> {
        &self.state
    }

    fn resource(&self) -> Arc<HTTPRoute> {
        Arc::clone(&self.resource)
    }

    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }

    async fn on_spec_not_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        state.save_http_route(id, resource);
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        state.maybe_save_http_route(id, resource);
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        self.on_status_changed(id, resource, state).await
    }

    async fn on_spec_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        self.on_deleted(id, resource, state).await
    }
}

impl HTTPRouteHandler<HTTPRoute> {
    /// * on new route we need to find all the relevant data such as gateways based on parent refs
    /// * send all necessary information to the backend
    /// * backend should return information whether the route was attached to the gateway or not and to which listener/listeners
    /// * we should update gatway's if the route count has changed for a listener
    /// * we shoudl update the route's status
    async fn on_new_or_changed(&self, route_id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        let log_context = self.log_context().to_string();
        let gateway_channel_sender = &self.gateway_channel_sender;

        let Some(parent_gateway_refs) = resource.spec.parent_refs.as_ref() else {
            return Err(ControllerError::InvalidPayload("Route with no parents".to_owned()));
        };

        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, ResourceKey::from(parent_ref)));

        let parent_gateway_refs = parent_gateway_refs_keys
            .clone()
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway(&key).cloned()))
            .map(|i| if i.1.is_some() { Ok(i) } else { Err(i) });

        let (resolved_gateways, unknown_gateways) = VerifiyItems::verify(parent_gateway_refs);

        parent_gateway_refs_keys.for_each(|(_ref, key)| state.attach_http_route_to_gateway(key, route_id.clone()));

        let matching_gateways = RouteListenerMatcher::filter_matching_gateways(state, &resolved_gateways);
        let unknown_gateway_status = self.generate_status_for_unknown_gateways(&unknown_gateways, resource.metadata.generation);
        let mut http_route = (**resource).clone();
        http_route.status = Some(HTTPRouteStatus { parents: unknown_gateway_status });
        state.save_http_route(route_id.clone(), &Arc::new(http_route));

        for gateway in matching_gateways {
            let gateway_id = ResourceKey::from(&*gateway);

            let mut deployer = GatewayDeployer {
                client: self.client.clone(),
                log_context: &log_context,
                sender: gateway_channel_sender.clone(),
                kube_gateway: &gateway,
                state,
                http_route_patcher: self.http_route_patcher.clone(),
                controller_name: &self.controller_name,
            };

            if let Ok(updated_gateway) = deployer.deploy_gateway().await {
                debug!("{log_context} Deploying gateway {updated_gateway:?}");
                state.save_gateway(gateway_id.clone(), &Arc::new(updated_gateway.clone()));
                let (sender, receiver) = oneshot::channel();
                let gateway_id = ResourceKey::from(&updated_gateway);
                let version = updated_gateway.metadata.resource_version.clone();
                let _res = self
                    .gateway_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: gateway_id.clone(),
                        resource: updated_gateway,
                        controller_name: self.controller_name.clone(),
                        version,
                        response_sender: sender,
                    }))
                    .await;
                let patched_gateway = receiver.await;
                if let Ok(maybe_patched) = patched_gateway {
                    match maybe_patched {
                        Ok(patched_gateway) => {
                            state.save_gateway(gateway_id.clone(), &Arc::new(patched_gateway));
                        }
                        Err(e) => {
                            warn!("{} Error while patching {e}", self.log_context());
                        }
                    }
                }
            }
        }
        Ok(Action::await_change())
    }

    async fn on_deleted(&self, route_id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        let log_context = self.log_context().to_string();
        let gateway_channel_sender = &self.gateway_channel_sender;
        let _route = Route::try_from(&**resource)?;

        let Some(parent_gateway_refs) = resource.spec.parent_refs.as_ref() else {
            return Err(ControllerError::InvalidPayload("Route with no parents".to_owned()));
        };

        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, ResourceKey::from(parent_ref)));

        let parent_gateway_refs = parent_gateway_refs_keys
            .clone()
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway(&key).cloned()))
            .map(|i| if i.1.is_some() { Ok(i) } else { Err(i) });

        let (resolved_gateways, _unknown_gateways) = VerifiyItems::verify(parent_gateway_refs);
        debug!("Parent keys = {parent_gateway_refs_keys:?}");
        parent_gateway_refs_keys.for_each(|(_ref, gateway_key)| state.detach_http_route_from_gateway(&gateway_key, &route_id));
        state.delete_http_route(&route_id);

        let matching_gateways = RouteListenerMatcher::filter_matching_gateways(state, &resolved_gateways);

        for gateway in matching_gateways {
            let gateway_id = ResourceKey::from(&*gateway);

            let mut deployer = GatewayDeployer {
                client: self.client.clone(),
                log_context: &log_context,
                sender: gateway_channel_sender.clone(),
                kube_gateway: &gateway,
                state,
                http_route_patcher: self.http_route_patcher.clone(),
                controller_name: &self.controller_name,
            };

            if let Ok(updated_gateway) = deployer.deploy_gateway().await {
                debug!("{log_context} Deploying gateway {updated_gateway:?}");
                state.save_gateway(gateway_id.clone(), &Arc::new(updated_gateway.clone()));
                let (sender, receiver) = oneshot::channel();
                let gateway_id = ResourceKey::from(&updated_gateway);
                let version = updated_gateway.metadata.resource_version.clone();
                let _res = self
                    .gateway_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: gateway_id.clone(),
                        resource: updated_gateway,
                        controller_name: self.controller_name.clone(),
                        version,
                        response_sender: sender,
                    }))
                    .await;
                let patched_gateway = receiver.await;
                if let Ok(maybe_patched) = patched_gateway {
                    match maybe_patched {
                        Ok(patched_gateway) => {
                            state.save_gateway(gateway_id.clone(), &Arc::new(patched_gateway));
                        }
                        Err(e) => {
                            warn!("{} Error while patching {e}", self.log_context());
                        }
                    }
                }
            }
        }

        let http_route = (**resource).clone();
        let resource_key = route_id;
        let _res = self.http_route_patcher.send(Operation::Delete((resource_key, http_route, self.controller_name.clone()))).await;

        Ok(Action::await_change())
    }

    fn generate_status_for_unknown_gateways(&self, gateways: &[(&HTTPRouteParentRefs, Option<Arc<Gateway>>)], generation: Option<i64>) -> Vec<HTTPRouteStatusParents> {
        gateways
            .iter()
            .map(|(gateway, _)| HTTPRouteStatusParents {
                conditions: Some(vec![Condition {
                    last_transition_time: Time(Utc::now()),
                    message: "Updated by controller".to_owned(),
                    observed_generation: generation,
                    reason: "BackendNotFound".to_owned(),
                    status: "False".to_owned(),
                    type_: "ResolvedRefs".to_owned(),
                }]),
                controller_name: self.controller_name.clone(),
                parent_ref: HTTPRouteStatusParentsParentRef {
                    group: gateway.group.clone(),
                    kind: gateway.kind.clone(),
                    name: gateway.name.clone(),
                    namespace: gateway.namespace.clone(),
                    port: gateway.port,
                    section_name: gateway.section_name.clone(),
                },
            })
            .collect()
    }

    // async fn deploy_route(
    //     &self,
    //     sender: &Sender<GatewayEvent>,
    //     state: &mut State,
    //     gateway: Arc<Gateway>,
    //     route_to_listeners_mapping: Vec<RouteToListenersMapping>,
    //     mut unresolved_linked_routes: Vec<Route>,
    //     per_listener_calculated_attached_routes: HashMap<String, u32>,
    // ) -> Result<Gateway> {
    //     let log_context = self.log_context();

    //     let maybe_gateway = common::Gateway::try_from(&*gateway);
    //     let Ok(backend_gateway) = maybe_gateway else {
    //         warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
    //         return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
    //     };

    //     let resource_key = ResourceKey::from(&*gateway);

    //     let (response_sender, response_receiver) = oneshot::channel();

    //     let route_event = GatewayEvent::RouteChanged(ChangedContext::new(response_sender, backend_gateway, (*gateway).clone(), route_to_listeners_mapping));

    //     let _ = sender.send(route_event).await;
    //     let response = response_receiver.await;

    //     if let Ok(GatewayResponse::GatewayProcessed(mut gateway_processed)) = response {
    //         gateway_processed.ignored_routes.append(&mut unresolved_linked_routes);
    //         let gateway_event_handler = GatewayProcessedHandler {
    //             gateway_processed_payload: gateway_processed,
    //             gateway: (*gateway).clone(),
    //             state,
    //             log_context: log_context.to_string(),
    //             resource_key,
    //             route_patcher: self.http_route_patcher.clone(),
    //             controller_name: self.controller_name.clone(),
    //             per_listener_calculated_attached_routes,
    //         };
    //         gateway_event_handler.deploy_gateway().await
    //     } else {
    //         warn!("{log_context} {response:?} ... Problem {response:?}");
    //         Err(ControllerError::BackendError)
    //     }
    // }

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        let controller_name = &self.controller_name;
        state.save_http_route(id, resource);
        if let Some(status) = &resource.status {
            let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                finalizers.iter().any(|f| f == controller_name)
            } else {
                false
            };

            if !status.parents.is_empty() && !has_finalizer {
                let _res = self.add_finalizer(controller_name).await;
                return Ok(Action::requeue(RECONCILE_LONG_WAIT));
            }
        }
        Err(ControllerError::ResourceHasWrongStatus)
    }

    async fn add_finalizer(&self, controller_name: &str) -> Result<Action> {
        let _res = self
            .http_route_patcher
            .send(Operation::PatchFinalizer(FinalizerContext {
                resource_key: self.resource_key.clone(),
                controller_name: controller_name.to_owned(),
                finalizer_name: controller_name.to_owned(),
            }))
            .await;

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }
}

impl<'a> LogContext<'a, HTTPRoute> {
    pub fn new(controller_name: &'a str, resource_key: &'a ResourceKey, version: Option<String>) -> Self {
        Self {
            controller_name,
            resource_key,
            version,
            resource_type: std::marker::PhantomData,
        }
    }
}
