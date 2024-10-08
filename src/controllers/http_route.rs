use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gateways::{Gateway, GatewayListeners},
    httproutes::{self, HTTPRoute, HTTPRouteParentRefs, HTTPRouteStatus, HTTPRouteStatusParents, HTTPRouteStatusParentsParentRef},
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
    utils::{self, LogContext, ResourceCheckerArgs, ResourceState, VerifiyItems},
    ControllerError, RECONCILE_LONG_WAIT,
};
use crate::{
    common::{self, GatewayEvent, GatewayResponse, ResourceKey, Route, RouteProcessedPayload},
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
}

pub struct HttpRouteController {
    pub controller_name: String,
    client: kube::Client,
    state: Arc<Mutex<State>>,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
}

impl HttpRouteController {
    pub(crate) fn new(
        controller_name: String,
        client: kube::Client,
        gateway_channel_sender: mpsc::Sender<GatewayEvent>,
        state: Arc<Mutex<State>>,
        http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    ) -> Self {
        HttpRouteController {
            controller_name,
            client,
            state,
            gateway_channel_sender,
            http_route_patcher,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            gateway_channel_sender: self.gateway_channel_sender.clone(),
            controller_name: self.controller_name.clone(),
            state: Arc::clone(&self.state),
            http_route_patcher: self.http_route_patcher.clone(),
        });

        let api = Api::<HTTPRoute>::namespaced(self.client.clone(), "default");
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

        let handler = HTTPRouteHandler {
            state: Arc::clone(&ctx.state),
            resource_key,
            controller_name: controller_name.clone(),
            resource,
            version,
            gateway_channel_sender: ctx.gateway_channel_sender.clone(),
            http_route_patcher,
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
        state.delete_http_route(&id);
        let _res = self.http_route_patcher.send(Operation::Delete((id.clone(), (**resource).clone(), self.controller_name.clone()))).await;
        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }
}

impl HTTPRouteHandler<HTTPRoute> {
    /// * on new route we need to find all the relevant data such as gateways based on parent refs
    /// * send all necessary information to the backend
    /// * backend should return information whether the route was attached to the gateway or not and to which listener/listeners
    /// * we should update gatway's if the route count has changed for a listener
    /// * we shoudl update the route's status
    async fn on_new_or_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        let log_context = self.log_context();
        let controller_name = &self.controller_name;
        let gateway_channel_sender = &self.gateway_channel_sender;

        let empty = vec![];
        let parent_gateway_refs = resource.spec.parent_refs.as_ref().unwrap_or(&empty);
        let route = Route::from(&**resource);

        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, ResourceKey::from(parent_ref)));
        //.map(|parent_ref| (parent_ref, resource.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned())))
        //.map(|(parent_ref, namespace)| (parent_ref, ResourceKey::from((parent_ref, namespace))));

        let parent_gateway_refs = parent_gateway_refs_keys
            .clone()
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway(&key).cloned()))
            .map(|i| if i.1.is_some() { Ok(i) } else { Err(i) });

        let (resolved_gateways, unknown_gateways) = VerifiyItems::verify(parent_gateway_refs);

        parent_gateway_refs_keys.for_each(|(_ref, key)| state.attach_http_route_to_gateway(key, id.clone()));

        let matching_gateways = common::RouteListenerMatcher::filter_matching_gateways(state, &resolved_gateways);

        let mut parents = vec![];
        for gateway in matching_gateways {
            let mut linked_routes = utils::find_linked_routes(state, &ResourceKey::from(&*gateway));
            linked_routes.push(route.clone());
            warn!("Linked route {linked_routes:?}");
            let listeners_and_routes = common::RouteListenerMatcher::filter_matching_routes(&gateway, &linked_routes);
            if let Ok(status) = self.deploy_route(gateway_channel_sender, &gateway, listeners_and_routes).await {
                debug!("{log_context} Deploying route {status:?}");
                parents.push(status);
            }
        }

        parents.append(&mut self.generate_status_for_unknown_gateways(&unknown_gateways, resource.metadata.generation));
        state.save_http_route(id.clone(), resource);

        let _res = self
            .http_route_patcher
            .send(Operation::PatchStatus(PatchContext {
                resource_key: self.resource_key.clone(),
                resource: Self::update_status_conditions((**resource).clone(), HTTPRouteStatus { parents }),
                controller_name: controller_name.to_owned(),
                version: self.version.clone(),
            }))
            .await;
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

    async fn deploy_route(&self, sender: &Sender<GatewayEvent>, gateway: &Gateway, gateway_listeners: Vec<(Route, Vec<GatewayListeners>)>) -> Result<HTTPRouteStatusParents> {
        let log_context = self.log_context();
        let controller_name = &self.controller_name;
        let maybe_gateway = common::Gateway::try_from(gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };
        // let maybe_listeners = gateway_listeners.iter().map(backends::Listener::try_from);
        // let (backend_listeners, errors) = utils::VerifiyItems::verify(maybe_listeners);
        // if !errors.is_empty() {
        //     warn!("{log_context} Misconfigured  gateway listeners {backend_gateway:?} {errors:?}");
        //     return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        // }

        let gateway_name = format!("{}:{}", backend_gateway.namespace, backend_gateway.name);

        let (response_sender, response_receiver) = oneshot::channel();

        let route_event = GatewayEvent::RouteChanged((response_sender, backend_gateway, gateway_listeners));

        let _ = sender.send(route_event).await;
        let response = response_receiver.await;
        if let Ok(GatewayResponse::RouteProcessed(RouteProcessedPayload { status })) = response {
            match status {
                common::RouteStatus::Attached => {
                    debug!("{log_context} Route attached to {gateway_name}",);
                    Ok(HTTPRouteStatusParents {
                        conditions: Some(vec![Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: gateway.metadata.generation,
                            reason: "Accepted".to_owned(),
                            status: "True".to_owned(),
                            type_: "Accepted".to_owned(),
                        }]),
                        controller_name: (*controller_name).clone(),
                        parent_ref: HTTPRouteStatusParentsParentRef {
                            namespace: gateway.meta().namespace.clone(),
                            name: gateway.meta().name.clone().unwrap_or_default(),
                            ..Default::default()
                        },
                    })
                }

                common::RouteStatus::Ignored => {
                    debug!("{log_context} Route rejected by {gateway_name}",);
                    Ok(HTTPRouteStatusParents {
                        conditions: Some(vec![Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: gateway.metadata.generation,
                            reason: "NotAllowedByListeners".to_owned(),
                            status: "False".to_owned(),
                            type_: "Accepted".to_owned(),
                        }]),
                        controller_name: (*controller_name).clone(),
                        parent_ref: HTTPRouteStatusParentsParentRef {
                            namespace: gateway.meta().namespace.clone(),
                            name: gateway.meta().name.clone().unwrap_or_default(),
                            ..Default::default()
                        },
                    })
                }
            }
        } else {
            warn!("{log_context} ... Problem {response:?}");
            Err(ControllerError::BackendError)
        }
    }

    fn update_status_conditions(mut route: HTTPRoute, route_status: HTTPRouteStatus) -> HTTPRoute {
        route.status = Some(route_status);
        route.metadata.managed_fields = None;
        route
    }

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &mut State) -> Result<Action> {
        let controller_name = &self.controller_name;
        state.save_http_route(id, resource);
        if let Some(status) = &resource.status {
            if !status.parents.is_empty() {
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
