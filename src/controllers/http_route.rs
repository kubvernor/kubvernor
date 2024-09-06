use std::sync::Arc;

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

use super::{utils::ResourceState, ControllerError, RECONCILE_LONG_WAIT};
use crate::{
    backends::gateways::{GatewayEvent, GatewayResponse, Listener, Route, RouteConfig},
    controllers::utils::MetadataPatcher,
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
        route: Arc<httproutes::HTTPRoute>,
        ctx: Arc<Context>,
    ) -> Result<Action> {
        let client = &ctx.client;
        let controller_name = &ctx.controller_name;
        let resource_version = &route.metadata.resource_version;
        let resource_name = route.name_any();
        let state = &ctx.state;
        let sender = &ctx.gateway_channel_sender;

        let id = Uuid::parse_str(&route.metadata.uid.clone().ok_or(
            ControllerError::InvalidPayload("Uid must be present".to_owned()),
        )?)
        .map_err(|e| ControllerError::InvalidPayload(format!("Uid in wrong format {e}")))?;

        info!("reconcile_route: {controller_name} {id} {resource_name} {resource_version:?}");

        let mut state = state.lock().await;
        let stored_route = state.get_http_route_by_id(id);

        let route_state = Self::check_state(&route, stored_route);
        info!(
            "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} {route_state:?}",
        );
        let api = Api::<HTTPRoute>::namespaced(
            client.clone(),
            &route
                .metadata
                .namespace
                .clone()
                .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
        );

        match route_state {
            ResourceState::New | ResourceState::Changed | ResourceState::SpecChanged => {
                let empty = vec![];
                let parent_gateway_refs = route.spec.parent_refs.as_ref().unwrap_or(&empty);
                let gateways: Vec<(Arc<Gateway>, Vec<GatewayListeners>)> = parent_gateway_refs
                    .iter()
                    .map(|parent_ref| {
                        (
                            parent_ref,
                            route
                                .metadata
                                .namespace
                                .clone()
                                .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
                        )
                    })
                    .map(|(parent_ref, namespace)| {
                        (parent_ref, ResourceKey::from((parent_ref, namespace)))
                    })
                    .map(|(parent_ref, key)| {
                        (parent_ref, state.get_gateway_by_resource(&key).cloned())
                    })
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
                                    Self::filter_listeners_by_name_or_port(&gateway, |gl| {
                                        gl.port == port
                                    }),
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
                let ctx = &(controller_name, id, &resource_name, resource_version);

                let mut parents = vec![];
                for (gateway, listeners) in gateways {
                    if let Ok(status) =
                        Self::update_route(ctx, sender, &gateway, &listeners, &route).await
                    {
                        debug!("Added to parents {status:?}");
                        state.save_http_route(id, &route);
                        parents.push(status);
                    }
                }
                let route_status = HTTPRouteStatus { parents };
                let mut updated_route = (*route).clone();
                updated_route.metadata.managed_fields = None;
                updated_route.status = Some(route_status);

                let patch_params = PatchParams::apply(controller_name).force();

                let res = api
                    .patch_status(
                        &route.name_any(),
                        &patch_params,
                        &Patch::Apply(updated_route),
                    )
                    .await;
                match res {
                    Ok(_updated_route) => {
                        let _res = MetadataPatcher::patch_finalizer(
                            &api,
                            &resource_name,
                            controller_name,
                            controller_name,
                        )
                        .await;
                        Ok(Action::await_change())
                    }

                    Err(e) => {
                        info!(
                            "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} {route_state:?} {e}",
                        );
                        Err(ControllerError::PatchFailed)
                    }
                }
            }

            ResourceState::Uptodate => Err(ControllerError::AlreadyAdded),

            ResourceState::Deleted => {
                state.delete_http_route(id);
                let res: std::result::Result<Action, finalizer::Error<_>> = finalizer::finalizer(
                    &api,
                    controller_name,
                    Arc::clone(&route),
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

    fn check_state(route: &HTTPRoute, stored_route: Option<&Arc<HTTPRoute>>) -> ResourceState {
        let state = ResourceState::check_state(route, stored_route);

        if ResourceState::New != state {
            return state;
        }

        if let Some(stored_route) = stored_route {
            if route.spec == stored_route.spec {
                return ResourceState::Uptodate;
            } else {
                return ResourceState::SpecChanged;
            }
        }

        if let Some(status) = route.status.as_ref() {
            if !status.parents.is_empty() {
                return ResourceState::Changed;
            }
        };

        state
    }

    async fn update_route(
        ctx: &(&String, Uuid, &String, &Option<String>),
        sender: &Sender<GatewayEvent>,
        gateway: &Gateway,
        listeners: &[GatewayListeners],
        http_route: &Arc<HTTPRoute>,
    ) -> Result<HTTPRouteStatusParents> {
        let (controller_name, id, resource_name, resource_version) = ctx;

        let gateway_name = gateway.name_any();
        let (response_sender, response_receiver) = oneshot::channel();
        let route_event = GatewayEvent::RouteChanged((
            response_sender,
            gateway_name.clone(),
            listeners
                .iter()
                .map(Listener::try_from)
                .filter_map(std::result::Result::ok)
                .collect(),
            Route::Http(RouteConfig::new(http_route.name_any())),
        ));
        let _ = sender.send(route_event).await;
        let response = response_receiver.await;
        if let Ok(GatewayResponse::RouteProcessed(route_status)) = response {
            match route_status {
                crate::backends::gateways::RouteStatus::Attached => {
                    debug!(
                            "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} Route attached to {gateway_name}",
                        );
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
                    debug!(
                        "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} Route rejected by {gateway_name}",
                    );
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
            warn!(
                "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} {response:?} ... Problem"
            );
            Err(ControllerError::BackendError)
        }
    }
}
