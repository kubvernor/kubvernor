use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{Gateway, GatewayListeners, GatewayStatus},
    httproutes::HTTPRoute,
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    Api, Client, Resource, ResourceExt,
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
    ControllerError, GATEWAY_CLASS_FINALIZER_NAME, RECONCILE_ERROR_WAIT, RECONCILE_LONG_WAIT,
};
use crate::{
    backends::gateways::{
        GatewayEvent, GatewayProcessedPayload, GatewayResponse, Listener, ListenerConfig,
        ListenerError, Route, RouteConfig,
    },
    controllers::utils::{FinalizerPatcher, ResourceFinalizer, VerifiyItems},
    state::{ResourceKey, State, DEFAULT_GROUP_NAME, DEFAULT_KIND_NAME, DEFAULT_NAMESPACE_NAME},
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    client: kube::Client,
    controller_name: String,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    state: Arc<Mutex<State>>,
}

pub struct GatewayController {
    pub controller_name: String,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
    client: kube::Client,
    api: Api<Gateway>,
    state: Arc<Mutex<State>>,
}

impl From<&Gateway> for ResourceKey {
    fn from(value: &Gateway) -> Self {
        let namespace = value
            .meta()
            .namespace
            .clone()
            .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: if namespace.is_empty() {
                DEFAULT_NAMESPACE_NAME.to_owned()
            } else {
                namespace
            },
            name: value.name_any(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl TryFrom<&GatewayListeners> for Listener {
    type Error = ListenerError;

    fn try_from(gateway_listener: &GatewayListeners) -> std::result::Result<Self, Self::Error> {
        let config = ListenerConfig::new(
            gateway_listener.name.clone(),
            gateway_listener.port,
            gateway_listener.hostname.clone(),
        );

        match gateway_listener.protocol.as_str() {
            "HTTP" => Ok(Self::Http(config)),
            "HTTPS" => Ok(Self::Https(config)),
            "TCP" => Ok(Self::Tcp(config)),
            "TLS" => Ok(Self::Tls(config)),
            "UDP" => Ok(Self::Udp(config)),
            _ => Err(ListenerError::UnknownProtocol(
                gateway_listener.protocol.clone(),
            )),
        }
    }
}

impl GatewayController {
    pub(crate) fn new(
        controller_name: String,
        gateway_channel_sender: mpsc::Sender<GatewayEvent>,
        client: kube::Client,
        state: Arc<Mutex<State>>,
    ) -> Self {
        GatewayController {
            controller_name,
            gateway_channel_sender,
            api: Api::all(client.clone()),
            client,
            state,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            client: self.client.clone(),
            controller_name: self.controller_name.clone(),
            gateway_channel_sender: self.gateway_channel_sender.clone(),
            state: Arc::clone(&self.state),
        });

        Controller::new(self.api.clone(), Config::default())
            .run(
                Self::reconcile_gateway,
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

    async fn reconcile_gateway(resource: Arc<Gateway>, ctx: Arc<Context>) -> Result<Action> {
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

        let gateway_class_name = {
            let gateway_class_name = &resource.spec.gateway_class_name;
            let state = state.lock().await;
            if !state
                .get_gateway_classes()
                .any(|gc| gc.metadata.name == Some(gateway_class_name.to_string()))
            {
                warn!("reconcile_gateway: {controller_name} {name} Unknown gateway class name {gateway_class_name}");
                return Err(ControllerError::UnknownGatewayClass(
                    gateway_class_name.clone(),
                ));
            }
            gateway_class_name.clone()
        };

        let maybe_stored_gateway_class = {
            let state = state.lock().await;
            state.get_gateway_by_id(id).cloned()
        };
        let api = Api::namespaced(
            client.clone(),
            &resource
                .meta()
                .namespace
                .clone()
                .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
        );

        let handler = GatewayResourceHandler {
            state: Arc::clone(&ctx.state),
            id,
            controller_name: controller_name.clone(),
            gateway_class_name,
            resource,
            name,
            version,
            client: client.clone(),
            api,
            gateway_channel_sender: ctx.gateway_channel_sender.clone(),
        };
        handler
            .process(maybe_stored_gateway_class, Self::check_spec)
            .await
    }

    fn check_spec(args: SpecCheckerArgs<Gateway>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec {
            ResourceState::SpecUnchanged
        } else {
            ResourceState::SpecChanged
        }
    }
}

struct GatewayResourceHandler<R> {
    state: Arc<Mutex<State>>,
    id: Uuid,
    controller_name: String,
    resource: Arc<R>,
    name: String,
    version: Option<String>,
    gateway_class_name: String,
    client: Client,
    api: Api<R>,
    gateway_channel_sender: mpsc::Sender<GatewayEvent>,
}

struct LogContext<'a> {
    controller_name: &'a str,
    gateway_class_name: &'a str,
    id: Uuid,
    name: &'a str,
    version: Option<String>,
}

impl std::fmt::Display for LogContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconcile_gateway: controller_name: {} gateway_class_name: {} id: {}, name: {} version: {:?}",
            self.controller_name, self.gateway_class_name, self.id, self.name, self.version
        )
    }
}

impl GatewayResourceHandler<Gateway> {
    async fn deploy_gateway(
        &self,
        sender: &Sender<GatewayEvent>,
        gateway: &Arc<Gateway>,
        state: &State,
    ) -> Result<Gateway> {
        let log_context = self.log_context();
        let name = self.name.clone();
        let id = self.id;

        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) =
            VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));

        if !listener_validation_errrors.is_empty() {
            debug!("{log_context} Properly configured listeners {listeners:?}");
            warn!("{log_context} Misconfigured  listeners {listener_validation_errrors:?}");
        }

        let linked_routes = Self::find_linked_routes(state, id);
        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event =
            GatewayEvent::GatewayChanged((response_sender, name, listeners, linked_routes));
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;

        if let Ok(GatewayResponse::GatewayProcessed(GatewayProcessedPayload {
            listeners: processed_listeners,
            routes: attached_routes,
        })) = response
        {
            let updated_gateway = self.update_gateway_resource(processed_listeners);

            for attached_route in attached_routes {
                debug!("{log_context} attached route {attached_route:?}");
                let updated_routes = self.update_route_resource(attached_route);
            }

            Ok(updated_gateway)
        } else {
            warn!("{log_context} {response:?} ... Problem {response:?}");
            Err(ControllerError::BackendError)
        }
    }

    fn find_linked_routes(state: &State, gateway_id: Uuid) -> Vec<Route> {
        state
            .get_http_routes_attached_to_gateway(gateway_id)
            .map(|routes| {
                routes
                    .iter()
                    .map(|r| Route::Http(RouteConfig::new(r.name_any())))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn update_status_conditions(
        mut gateway: Gateway,
        mut gateway_status: GatewayStatus,
    ) -> Gateway {
        let mut conditions: Vec<Condition> = vec![];

        let new_condition = Condition {
            last_transition_time: Time(Utc::now()),
            message: "Updated by controller".to_owned(),
            observed_generation: gateway.metadata.generation,
            reason: gateway_api::apis::standard::constants::GatewayConditionReason::Ready
                .to_string(),
            status: "True".to_owned(),
            type_: gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string(),
        };
        conditions.push(new_condition);
        gateway_status.conditions = Some(conditions);
        gateway.status = Some(gateway_status);
        gateway.metadata.managed_fields = None;
        gateway
    }

    async fn on_new_or_changed(
        &self,
        id: Uuid,
        resource: &Arc<Gateway>,
        state: &mut State,
    ) -> Result<Action> {
        let log_context = self.log_context();
        let api = &self.api;
        let name = &self.name;
        let controller_name = &self.controller_name;
        let gateway_class_name = &self.gateway_class_name;
        let sender = &self.gateway_channel_sender;

        let updated_gateway = self.deploy_gateway(sender, resource, state).await?;

        let patch_params = PatchParams::apply(&self.controller_name).force();

        match api
            .patch_status(name, &patch_params, &Patch::Apply(updated_gateway))
            .await
        {
            Ok(new_gateway) => {
                info!("{log_context} patch status result ok");

                state.save_gateway(id, &Arc::new(new_gateway));
                self.change_gateway_class(gateway_class_name).await;
                self.add_finalizer(controller_name).await
            }
            Err(e) => {
                warn!("{log_context} patch status failed {e:?}");
                Err(ControllerError::PatchFailed)
            }
        }
    }

    async fn add_finalizer(&self, controller_name: &str) -> Result<Action> {
        let log_context = self.log_context();
        let api = &self.api;
        let name = &self.name;
        let res =
            FinalizerPatcher::patch_finalizer(api, name, controller_name, controller_name).await;

        match res {
            Ok(()) => {
                info!("{log_context} meta patch status result ok");
                if res.is_err() {
                    warn!("{log_context} couldn't patch meta");
                }
                Ok(Action::requeue(RECONCILE_LONG_WAIT))
            }
            Err(e) => {
                warn!("{log_context} patch status failed {e:?}");
                Err(ControllerError::PatchFailed)
            }
        }
    }

    async fn change_gateway_class(&self, gateway_class_name: &str) {
        let log_context = self.log_context();
        let gateway_class_api = Api::<GatewayClass>::all(self.client.clone());
        let res = FinalizerPatcher::patch_finalizer(
            &gateway_class_api,
            gateway_class_name,
            &self.controller_name,
            GATEWAY_CLASS_FINALIZER_NAME,
        )
        .await;

        if res.is_err() {
            warn!("{log_context} couldn't patch gateway class name {res:?}");
        }
    }

    fn update_gateway_resource(
        &self,
        processed_listeners: Vec<(
            String,
            std::result::Result<crate::backends::gateways::ListenerStatus, ListenerError>,
        )>,
    ) -> Gateway {
        let log_context = self.log_context();
        for (configured_name, status) in processed_listeners {
            match status {
                Ok(good) => {
                    debug!("{log_context} Gateway added listeners {configured_name} {good:?}",);
                }
                Err(bad) => {
                    debug!(
                        "{log_context} Gateway misconfigured listeners {configured_name} {bad:?}"
                    );
                }
            }
        }
        let gateway_status = GatewayStatus::default();
        Self::update_status_conditions(((*self.resource).clone()).clone(), gateway_status)
    }

    fn update_route_resource(&self, attached_route: Route) -> HTTPRoute {
        HTTPRoute::default()
    }
}
#[async_trait]
impl ResourceHandler<Gateway> for GatewayResourceHandler<Gateway> {
    fn log_context(&self) -> impl std::fmt::Display {
        LogContext {
            controller_name: &self.controller_name,
            gateway_class_name: &self.gateway_class_name,
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
    fn resource(&self) -> Arc<Gateway> {
        Arc::clone(&self.resource)
    }

    async fn on_spec_unchanged(
        &self,
        id: Uuid,
        resource: &Arc<Gateway>,
        state: &mut State,
    ) -> Result<Action> {
        state.save_gateway(id, resource);
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, id: Uuid, resource: &Arc<Gateway>, state: &mut State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_spec_changed(
        &self,
        id: Uuid,
        resource: &Arc<Gateway>,
        state: &mut State,
    ) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(
        &self,
        id: Uuid,
        resource: &Arc<Gateway>,
        state: &mut State,
    ) -> Result<Action> {
        let controller_name = &self.controller_name;
        state.delete_gateway(id);

        let res = ResourceFinalizer::delete_resource(&self.api, controller_name, resource).await;
        res.map_err(|e: finalizer::Error<ControllerError>| {
            ControllerError::FinalizerPatchFailed(e.to_string())
        })
    }
}
