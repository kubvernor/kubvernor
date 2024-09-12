use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gateways::{Gateway, GatewayListeners},
    httproutes::{
        self, HTTPRoute, HTTPRouteParentRefs, HTTPRouteStatus, HTTPRouteStatusParents,
        HTTPRouteStatusParentsParentRef,
    },
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    Api, Resource, ResourceExt,
};

use log::{debug, info, warn};

use tokio::sync::{
    mpsc::{self, Sender},
    oneshot, Mutex,
};
use uuid::Uuid;

use super::{
    resource_handler::ResourceHandler,
    utils::{ResourceState, SpecCheckerArgs},
    ControllerError, RECONCILE_LONG_WAIT,
};
use crate::{
    backends::gateways::{GatewayEvent, GatewayResponse, Listener, Route, RouteConfig},
    controllers::utils::{FinalizerPatcher, ResourceFinalizer},
    state::{ResourceKey, State, DEFAULT_GROUP_NAME, DEFAULT_KIND_NAME, DEFAULT_NAMESPACE_NAME},
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    pub client: kube::Client,
    pub controller_name: String,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    state: Arc<Mutex<State>>,
}

pub struct HttpRouteController {
    pub controller_name: String,
    client: kube::Client,
    state: Arc<Mutex<State>>,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
}

impl From<(&HTTPRouteParentRefs, String)> for ResourceKey {
    fn from((route_parent, route_namespace): (&HTTPRouteParentRefs, String)) -> Self {
        Self {
            group: route_parent
                .group
                .clone()
                .unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
            namespace: route_parent
                .namespace
                .clone()
                .unwrap_or(route_namespace.clone()),
            name: route_parent.name.clone(),
            kind: route_parent
                .kind
                .clone()
                .unwrap_or(DEFAULT_KIND_NAME.to_owned()),
        }
    }
}

impl HttpRouteController {
    pub(crate) fn new(
        controller_name: String,
        client: kube::Client,
        gateway_channel_sender: mpsc::Sender<GatewayEvent>,
        state: Arc<Mutex<State>>,
    ) -> Self {
        HttpRouteController {
            controller_name,
            client,
            state,
            gateway_channel_sender,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            gateway_channel_sender: self.gateway_channel_sender.clone(),
            client: self.client.clone(),
            controller_name: self.controller_name.clone(),
            state: Arc::clone(&self.state),
        });

        let api = Api::<HTTPRoute>::namespaced(self.client.clone(), "default");
        Controller::new(api, Config::default())
            .run(
                Self::reconcile_http_route,
                Self::error_policy,
                Arc::clone(&context),
            )
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<Context>) -> Action {
        Action::requeue(RECONCILE_LONG_WAIT)
    }

    async fn reconcile_http_route(
        resource: Arc<httproutes::HTTPRoute>,
        ctx: Arc<Context>,
    ) -> Result<Action> {
        let client = &ctx.client;
        let controller_name = ctx.controller_name.clone();

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
        let version = resource.meta().resource_version.clone();

        let maybe_stored_route = {
            let state = state.lock().await;
            state.get_http_route_by_id(id).cloned()
        };

        let api = Api::namespaced(
            client.clone(),
            &resource
                .meta()
                .namespace
                .clone()
                .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
        );

        let handler = HTTPRouteHandler {
            state: Arc::clone(&ctx.state),
            id,
            controller_name: controller_name.clone(),
            resource,
            name,
            version,
            api,
            gateway_channel_sender: ctx.gateway_channel_sender.clone(),
        };
        handler.process(maybe_stored_route, Self::check_spec).await
    }

    fn check_spec(args: SpecCheckerArgs<HTTPRoute>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecUnchanged
        } else {
            ResourceState::SpecChanged
        }
    }
}

struct HTTPRouteHandler<R> {
    state: Arc<Mutex<State>>,
    id: Uuid,
    controller_name: String,
    resource: Arc<R>,
    name: String,
    version: Option<String>,
    api: Api<R>,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
}

impl HTTPRouteHandler<HTTPRoute> {
    async fn on_new_or_changed(
        &self,
        id: Uuid,
        resource: &Arc<HTTPRoute>,
        state: &mut State,
    ) -> Result<Action> {
        let log_context = self.log_context();
        let controller_name = &self.controller_name;
        let api = &self.api;
        let name = &self.name;
        let gateway_channel_sender = &self.gateway_channel_sender;

        let empty = vec![];
        let parent_gateway_refs = resource.spec.parent_refs.as_ref().unwrap_or(&empty);
        let gateways: Vec<(Arc<Gateway>, Vec<GatewayListeners>)> = parent_gateway_refs
            .iter()
            .map(|parent_ref| {
                (
                    parent_ref,
                    resource
                        .meta()
                        .namespace
                        .clone()
                        .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
                )
            })
            .map(|(parent_ref, namespace)| (parent_ref, ResourceKey::from((parent_ref, namespace))))
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway_by_resource(&key).cloned()))
            .filter_map(|(parent_ref, maybe_gateway)| {
                if let Some(gateway) = maybe_gateway {
                    match (parent_ref.port, &parent_ref.section_name) {
                        (Some(port), Some(section_name)) => Some((
                            Arc::clone(&gateway),
                            Self::filter_listeners_by_name_or_port(&gateway, |gl| {
                                gl.port == port && gl.name == *section_name
                            }),
                        )),
                        (Some(port), None) => Some((
                            Arc::clone(&gateway),
                            Self::filter_listeners_by_name_or_port(&gateway, |gl| gl.port == port),
                        )),
                        (None, Some(section_name)) => Some((
                            Arc::clone(&gateway),
                            Self::filter_listeners_by_name_or_port(&gateway, |gl| {
                                gl.name == *section_name
                            }),
                        )),
                        (None, None) => Some((
                            Arc::clone(&gateway),
                            Self::filter_listeners_by_name_or_port(&gateway, |_| true),
                        )),
                    }
                } else {
                    None
                }
            })
            .collect();

        let mut parents = vec![];
        for (gateway, listeners) in gateways {
            if let Ok(status) = self
                .update_route(gateway_channel_sender, &gateway, &listeners, resource)
                .await
            {
                debug!("{log_context} Added to parents {status:?}");
                state.save_http_route(id, resource);
                parents.push(status);
            }
        }

        let res = api
            .patch_status(
                &resource.name_any(),
                &PatchParams::apply(controller_name),
                &Patch::Apply(Self::update_status_conditions(
                    (**resource).clone(),
                    HTTPRouteStatus { parents },
                )),
            )
            .await;
        match res {
            Ok(_updated_route) => {
                let _res =
                    FinalizerPatcher::patch_finalizer(api, name, controller_name, controller_name)
                        .await;
                Ok(Action::await_change())
            }

            Err(e) => {
                info!("{log_context} {e}",);
                Err(ControllerError::PatchFailed)
            }
        }
    }

    async fn update_route(
        &self,
        sender: &Sender<GatewayEvent>,
        gateway: &Gateway,
        listeners: &[GatewayListeners],
        http_route: &Arc<HTTPRoute>,
    ) -> Result<HTTPRouteStatusParents> {
        let log_context = self.log_context();
        let controller_name = &self.controller_name;
        let gateway_name = gateway.name_any();
        let (response_sender, response_receiver) = oneshot::channel();
        let listeners = listeners
            .iter()
            .map(Listener::try_from)
            .filter_map(std::result::Result::ok)
            .collect::<Vec<_>>();
        let route_event = GatewayEvent::RouteChanged((
            response_sender,
            gateway_name.clone(),
            listeners,
            Route::Http(RouteConfig::new(http_route.name_any())),
        ));
        let _ = sender.send(route_event).await;
        let response = response_receiver.await;
        if let Ok(GatewayResponse::RouteProcessed(route_status)) = response {
            match route_status {
                crate::backends::gateways::RouteStatus::Attached => {
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

                crate::backends::gateways::RouteStatus::Ignored => {
                    debug!("{log_context} Route rejected by {gateway_name}",);
                    Ok(HTTPRouteStatusParents {
                        conditions: Some(vec![Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: gateway.metadata.generation,
                            reason: "Rejected".to_owned(),
                            status: "False".to_owned(),
                            type_: "Rejected".to_owned(),
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

    fn filter_listeners_by_name_or_port<F>(
        gateway: &Arc<Gateway>,
        filter: F,
    ) -> Vec<GatewayListeners>
    where
        F: Fn(&GatewayListeners) -> bool,
    {
        gateway
            .spec
            .listeners
            .iter()
            .filter(|f| filter(f))
            .cloned()
            .collect()
    }
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
            "reconcile_http_route: controller_name: {} id: {}, name: {} version: {:?}",
            self.controller_name, self.id, self.name, self.version
        )
    }
}

#[async_trait]
impl ResourceHandler<HTTPRoute> for HTTPRouteHandler<HTTPRoute> {
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
    fn resource(&self) -> Arc<HTTPRoute> {
        Arc::clone(&self.resource)
    }

    async fn on_spec_unchanged(
        &self,
        id: Uuid,
        resource: &Arc<HTTPRoute>,
        state: &mut State,
    ) -> Result<Action> {
        state.save_http_route(id, resource);
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(
        &self,
        id: Uuid,
        resource: &Arc<HTTPRoute>,
        state: &mut State,
    ) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_spec_changed(
        &self,
        id: Uuid,
        resource: &Arc<HTTPRoute>,
        state: &mut State,
    ) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(
        &self,
        id: Uuid,
        resource: &Arc<HTTPRoute>,
        state: &mut State,
    ) -> Result<Action> {
        let controller_name = &self.controller_name;
        let api = &self.api;

        state.delete_http_route(id);
        let res = ResourceFinalizer::delete_resource(api, controller_name, resource).await;
        res.map_err(|e: finalizer::Error<ControllerError>| {
            ControllerError::FinalizerPatchFailed(e.to_string())
        })
    }
}
