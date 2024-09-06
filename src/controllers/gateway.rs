use std::sync::Arc;

use futures::{channel::oneshot, future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::gateways::{Gateway, GatewayListeners, GatewayStatus};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    Api, ResourceExt,
};
use kube_core::ObjectMeta;
use log::{debug, info, warn};
use tokio::sync::{mpsc, mpsc::Sender, Mutex};
use uuid::Uuid;

use super::{utils::ResourceState, ControllerError, RECONCILE_WAIT};
use crate::{
    controllers::utils::VerifiyItems,
    listener::{Listener, ListenerConfig, ListenerError, ListenerEvent},
    state::{ResourceKey, State, DEFAULT_GROUP_NAME, DEFAULT_KIND_NAME, DEFAULT_NAMESPACE_NAME},
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    client: kube::Client,
    controller_name: String,
    listener_sender: mpsc::Sender<ListenerEvent>,
    state: Arc<Mutex<State>>,
}

pub struct GatewayController {
    pub controller_name: String,
    listener_sender: mpsc::Sender<ListenerEvent>,
    client: kube::Client,
    api: Api<Gateway>,
    state: Arc<Mutex<State>>,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum GatewayState {
    New,
    PreviouslyProcessed,
    Added,
    Deleted,
}
impl From<ResourceState> for GatewayState {
    fn from(value: ResourceState) -> Self {
        match value {
            ResourceState::Existing => GatewayState::Added,
            ResourceState::NotSeenBefore => GatewayState::New,
            ResourceState::Deleted => GatewayState::Deleted,
        }
    }
}
impl From<&Gateway> for ResourceKey {
    fn from(value: &Gateway) -> Self {
        let namespace = value
            .namespace()
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
        listener_sender: mpsc::Sender<ListenerEvent>,
        client: kube::Client,
        state: Arc<Mutex<State>>,
    ) -> Self {
        GatewayController {
            controller_name,
            listener_sender,
            api: Api::all(client.clone()),
            client,
            state,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            client: self.client.clone(),
            controller_name: self.controller_name.clone(),
            listener_sender: self.listener_sender.clone(),
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
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<Context>) -> Action {
        Action::requeue(RECONCILE_WAIT)
    }

    async fn reconcile_gateway(gateway: Arc<Gateway>, ctx: Arc<Context>) -> Result<Action> {
        let client = &ctx.client;
        let controller_name = &ctx.controller_name;
        let sender = &ctx.listener_sender;
        let gateway_class_name = &gateway.spec.gateway_class_name;
        let resource_name = gateway.name_any();
        let resource_version = &gateway.metadata.resource_version;

        let state = &ctx.state;

        let id = Uuid::parse_str(&gateway.metadata.uid.clone().ok_or(
            ControllerError::InvalidPayload("Uid must be present".to_owned()),
        )?)
        .map_err(|e| ControllerError::InvalidPayload(format!("Uid in wrong format {e}")))?;

        info!(
            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?}",
        );
        debug!(
            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} {:?}",
            gateway
        );

        let mut state = state.lock().await;
        let maybe_added = state.get_gateway_by_id(id);

        if !state
            .gateway_class_names
            .values()
            .any(|gc| gc.metadata.name == Some(gateway_class_name.to_string()))
        {
            warn!("reconcile_gateway: {controller_name} {gateway_class_name} {resource_name} Unknown gateway class name {gateway_class_name}");
            return Err(ControllerError::InvalidPayload(format!(
                "Unknown gateway class name {gateway_class_name}"
            )));
        }

        let api: Api<Gateway> = Api::namespaced(client.clone(), "default");

        match Self::check_state(&gateway, maybe_added) {
            GatewayState::PreviouslyProcessed => {
                info!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} previously processed",                
                );
                Self::process_listeners(
                    &(
                        controller_name,
                        gateway_class_name,
                        id,
                        &resource_name,
                        resource_version,
                    ),
                    sender,
                    &gateway,
                )
                .await;

                state.save_gateway(id, &Arc::clone(&gateway));

                Ok(Action::await_change())
            }

            GatewayState::Added => {
                info!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} already added",                
                );
                Err(ControllerError::AlreadyAdded)
            }

            GatewayState::New => {
                info!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} new",                
                );

                let updated_gateway = Self::update_gateway(&gateway);
                debug!(
            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} patch new gateway {updated_gateway:?}");

                let patch_params = PatchParams::apply(controller_name).force();

                match api
                    .patch_status(
                        &resource_name,
                        &patch_params,
                        &Patch::Apply(updated_gateway),
                    )
                    .await
                {
                    Ok(new_gateway) => {
                        info!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} patch status result ok");
                        Self::process_listeners(
                            &(
                                controller_name,
                                gateway_class_name,
                                id,
                                &resource_name,
                                resource_version,
                            ),
                            sender,
                            &gateway,
                        )
                        .await;
                        state.save_gateway(id, &Arc::new(new_gateway));
                        Ok(Action::requeue(RECONCILE_WAIT))
                    }
                    Err(e) => {
                        warn!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} patch status failed {e:?}");
                        Err(ControllerError::PatchFailed)
                    }
                }
            }

            GatewayState::Deleted => {
                info!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} deleted",                
                );
                state.delete_gateway(id);
                Self::delete_listeners(
                    &(
                        controller_name,
                        gateway_class_name,
                        id,
                        &resource_name,
                        resource_version,
                    ),
                    sender,
                )
                .await;

                let res: std::result::Result<Action, finalizer::Error<_>> = finalizer::finalizer(
                    &api,
                    controller_name,
                    Arc::clone(&gateway),
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

    fn check_state(gateway: &Gateway, maybe_added: Option<&Arc<Gateway>>) -> GatewayState {
        let gateway_state = GatewayState::from(ResourceState::check_state(gateway, maybe_added));

        if GatewayState::New != gateway_state {
            return gateway_state;
        }

        if let Some(status) = gateway.status.as_ref() {
            if let Some(existing_conditions) = &status.conditions {
                for c in existing_conditions {
                    debug!("reconcile_gateway_class: condition {c:?}");
                    if c.status == "True"
                        && c.type_
                            == gateway_api::apis::standard::constants::GatewayConditionType::Ready
                                .to_string()
                        && c.reason
                            == gateway_api::apis::standard::constants::GatewayConditionReason::Ready
                                .to_string()
                    {
                        return GatewayState::PreviouslyProcessed;
                    }
                }
            }
        };

        gateway_state
    }

    fn update_gateway(gateway: &Arc<Gateway>) -> Gateway {
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
        let new_status = GatewayStatus {
            conditions: Some(conditions),
            addresses: None,
            listeners: Some(vec![]),
        };
        let mut configured_gateway = (**gateway).clone();
        configured_gateway.status = Some(new_status);
        configured_gateway.metadata = ObjectMeta::default();
        configured_gateway
    }

    async fn process_listeners(
        ctx: &(&String, &String, Uuid, &String, &Option<String>),
        sender: &Sender<ListenerEvent>,
        gateway: &Arc<Gateway>,
    ) {
        let (controller_name, gateway_class_name, id, resource_name, resource_version) = ctx;
        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) =
            VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));

        if !listener_validation_errrors.is_empty() {
            debug!(
                "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Properly configured listeners {listeners:?}");
            warn!(
                "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Misconfigured  listeners {listener_validation_errrors:?}"
            );
        }

        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event = ListenerEvent::AddListeners((response_sender, listeners));
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;
        if let Ok(crate::listener::ListenerResponse::Processed(processed)) = response {
            for (_configured_name, status) in processed {
                match status {
                    Ok(good) => {
                        debug!(
                            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Gateway added listeners {good:?}",
                        );
                    }
                    Err(bad) => {
                        debug!(
                            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Gateway misconfigured listeners {bad:?}");
                    }
                }
            }
        } else {
            warn!(
                "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} {response:?} ... Problem"
            );
        }
    }

    async fn delete_listeners(
        ctx: &(&String, &String, Uuid, &String, &Option<String>),
        sender: &Sender<ListenerEvent>,
    ) {
        let (controller_name, gateway_class_name, id, resource_name, resource_version) = ctx;
        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event = ListenerEvent::DeleteAllListeners(response_sender);
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;
        if let Ok(crate::listener::ListenerResponse::Deleted) = response {
        } else {
            warn!(
                "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} {response:?} ... Problem "
            );
        }
    }
}
