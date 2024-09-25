use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{Gateway, GatewayListeners, GatewayStatus, GatewayStatusListeners},
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
    utils::{self, ResourceState, SpecCheckerArgs},
    ControllerError, GATEWAY_CLASS_FINALIZER_NAME, RECONCILE_ERROR_WAIT, RECONCILE_LONG_WAIT,
};
use crate::{
    backends::{
        self,
        gateway_deployer::{
            GatewayError, GatewayEvent, GatewayProcessedPayload, GatewayResponse, Listener,
            ListenerConfig, ListenerError, ListenerStatus, Route,
        },
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

impl TryFrom<&Gateway> for backends::gateway_deployer::Gateway {
    type Error = GatewayError;

    fn try_from(gateway: &Gateway) -> std::result::Result<Self, Self::Error> {
        let id = Uuid::parse_str(&gateway.metadata.uid.clone().unwrap_or_default())
            .map_err(|_| GatewayError::ConversionProblem("Can't parse uuid".to_owned()))?;
        let name = gateway.metadata.name.clone().unwrap_or_default();
        if name.is_empty() {
            return Err(GatewayError::ConversionProblem(
                "Name can't be empty".to_owned(),
            ));
        }
        let namespace = gateway
            .metadata
            .namespace
            .clone()
            .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) =
            VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));
        if !listener_validation_errrors.is_empty() {
            return Err(GatewayError::ConversionProblem(
                "Misconfigured listeners".to_owned(),
            ));
        }

        Ok(Self {
            id,
            name,
            namespace,
            listeners,
        })
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
        let id = self.id;

        let maybe_gateway = backends::gateway_deployer::Gateway::try_from(&**gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload(
                "Misconfigured gateway".to_owned(),
            ));
        };
        let resource_key = ResourceKey::from(&**gateway);

        let linked_routes = utils::find_linked_routes(state, &resource_key);
        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event =
            GatewayEvent::GatewayChanged((response_sender, backend_gateway, linked_routes));
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;

        if let Ok(GatewayResponse::GatewayProcessed(GatewayProcessedPayload {
            gateway_status,
            routes: attached_routes,
        })) = response
        {
            let updated_gateway = self.update_gateway_resource(gateway, gateway_status.listeners);

            for attached_route in attached_routes {
                debug!("{log_context} attached route {attached_route:?}");
                let updated_routes =
                    self.update_route_parents(state, attached_route, &resource_key);
            }

            Ok(updated_gateway)
        } else {
            warn!("{log_context} {response:?} ... Problem {response:?}");
            Err(ControllerError::BackendError)
        }
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

    /// * on new gateway we need to find all the relevant data such as routes that might be referencing this gateway
    /// * send all necessary information to the backend
    /// * backend should return information whether the routes was attached to the gateway or not and to which listener/listeners
    /// * we should update gatway's status if the route count has changed for a listener
    /// * we should update the route's status and reflect that the ownership might changed
    ///
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
        gateway: &Gateway,
        processed_listeners: Vec<ListenerStatus>,
    ) -> Gateway {
        let log_context = self.log_context();
        debug!("{log_context} listener statuses {processed_listeners:?}");
        let listener_statuses: Vec<_> = processed_listeners
            .into_iter()
            .map(|status| match status {
                ListenerStatus::Accepted((name, routes)) => GatewayStatusListeners {
                    attached_routes: routes,
                    conditions: vec![Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: gateway.metadata.generation,
                        reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted
                            .to_string(),
                        status: "True".to_owned(),
                        type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                    }],
                    name,
                    supported_kinds: vec![],
                },
                ListenerStatus::Conflicted(name) => GatewayStatusListeners {
                    attached_routes: 0,
                    conditions: vec![Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: gateway.metadata.generation,
                        reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted
                            .to_string(),
                        status: "False".to_owned(),
                        type_: gateway_api::apis::standard::constants::ListenerConditionType::Conflicted.to_string(),
                    }],
                    name,
                    supported_kinds: vec![],
                },
            })
            .collect();

        let gateway_status = GatewayStatus {
            listeners: Some(listener_statuses),
            ..Default::default()
        };

        Self::update_status_conditions(((*self.resource).clone()).clone(), gateway_status)
    }

    fn update_route_parents(
        &self,
        state: &State,
        attached_route: Route,
        gateway_id: &ResourceKey,
    ) -> HTTPRoute {
        let routes = state.get_http_routes_attached_to_gateway(gateway_id);
        if let Some(routes) = routes {
            let route = routes.iter().find(|f| {
                f.metadata.name == Some(attached_route.name().to_owned())
                    && f.metadata.namespace == attached_route.namespace().cloned()
            });
            if let Some(route) = route {
                //TODO:: update status here
            }
        }

        // placeholder
        // unfortunately a route object can be created before gateway objects are created

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
