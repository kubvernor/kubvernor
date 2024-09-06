use std::sync::Arc;

use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass,
    gateways::{Gateway, GatewayListeners, GatewayStatus},
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    Api, ResourceExt,
};
use log::{debug, info, warn};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot, Mutex,
};
use uuid::Uuid;

use super::{
    utils::ResourceState, ControllerError, GATEWAY_CLASS_FINALIZER_NAME, RECONCILE_ERROR_WAIT,
    RECONCILE_LONG_WAIT,
};
use crate::{
    backends::gateways::{GatewayEvent, GatewayResponse, Listener, ListenerConfig, ListenerError},
    controllers::utils::{MetadataPatcher, VerifiyItems},
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

    async fn reconcile_gateway(gateway: Arc<Gateway>, ctx: Arc<Context>) -> Result<Action> {
        let client = &ctx.client;
        let controller_name = &ctx.controller_name;
        let sender = &ctx.gateway_channel_sender;
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

        let mut state = state.lock().await;
        let maybe_added = state.get_gateway_by_id(id);

        if !state
            .gateway_class_names
            .values()
            .any(|gc| gc.metadata.name == Some(gateway_class_name.to_string()))
        {
            warn!("reconcile_gateway: {controller_name} {gateway_class_name} {resource_name} Unknown gateway class name {gateway_class_name}");
            return Err(ControllerError::UnknownGatewayClass(
                gateway_class_name.clone(),
            ));
        }

        let api = Api::<Gateway>::namespaced(
            client.clone(),
            &gateway
                .metadata
                .namespace
                .clone()
                .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
        );
        let resource_state = Self::check_state(&gateway, maybe_added);
        info!(
            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Resource state {resource_state:?}",                
        );
        match resource_state {
            ResourceState::Uptodate => Err(ControllerError::AlreadyAdded),

            ResourceState::New | ResourceState::Changed | ResourceState::SpecChanged => {
                let configured_gateway = (*gateway).clone();
                let gateway_status = Self::update_gateway(
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

                let updated_gateway: Gateway =
                    Self::update_gateway_status(configured_gateway, gateway_status);
                let patch_params = PatchParams::apply(controller_name);

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

                        state.save_gateway(id, &Arc::new(new_gateway));
                        let gateway_class_api = Api::<GatewayClass>::all(client.clone());
                        let res = MetadataPatcher::patch_finalizer(
                            &gateway_class_api,
                            gateway_class_name,
                            controller_name,
                            GATEWAY_CLASS_FINALIZER_NAME,
                        )
                        .await;

                        if res.is_err() {
                            warn!("reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} couldn't patch gateway class name {res:?}");
                        }

                        let res = MetadataPatcher::patch_finalizer(
                            &api,
                            &resource_name,
                            controller_name,
                            controller_name,
                        )
                        .await;

                        match res {
                            Ok(()) => {
                                info!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} meta patch status result ok");
                                if res.is_err() {
                                    warn!("reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} couldn't patch meta");
                                }
                                Ok(Action::requeue(RECONCILE_LONG_WAIT))
                            }
                            Err(e) => {
                                warn!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} patch status failed {e:?}");
                                Err(ControllerError::PatchFailed)
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                    "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} patch status failed {e:?}");
                        Err(ControllerError::PatchFailed)
                    }
                }
            }

            ResourceState::Deleted => {
                state.delete_gateway(id);
                Self::delete_gateway(
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

    fn check_state(gateway: &Gateway, maybe_added: Option<&Arc<Gateway>>) -> ResourceState {
        let state = ResourceState::check_state(gateway, maybe_added);

        if ResourceState::New != state {
            return state;
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
                        return ResourceState::Changed;
                    }
                }
            }
        };

        state
    }

    fn update_gateway_status(mut gateway: Gateway, mut gateway_status: GatewayStatus) -> Gateway {
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

    async fn update_gateway(
        ctx: &(&String, &String, Uuid, &String, &Option<String>),
        sender: &Sender<GatewayEvent>,
        gateway: &Arc<Gateway>,
    ) -> GatewayStatus {
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
        let listener_event = GatewayEvent::GatewayChanged((
            response_sender,
            (*resource_name).to_string(),
            listeners,
        ));
        let _ = sender.send(listener_event).await;
        let response = response_receiver.await;
        if let Ok(GatewayResponse::GatewayProcessed(processed)) = response {
            for (configured_name, status) in processed {
                match status {
                    Ok(good) => {
                        debug!(
                            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Gateway added listeners {configured_name} {good:?}",
                        );
                    }
                    Err(bad) => {
                        debug!(
                            "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} Gateway misconfigured listeners {configured_name} {bad:?}");
                    }
                }
            }
            GatewayStatus::default()
        } else {
            warn!(
                "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} {response:?} ... Problem"
            );
            GatewayStatus::default()
        }
    }

    async fn delete_gateway(
        ctx: &(&String, &String, Uuid, &String, &Option<String>),
        sender: &Sender<GatewayEvent>,
    ) {
        let (controller_name, gateway_class_name, id, resource_name, resource_version) = ctx;
        let (response_sender, response_receiver) = oneshot::channel();
        let gateway_event =
            GatewayEvent::GatewayDeleted((response_sender, (*resource_name).to_string()));
        let _ = sender.send(gateway_event).await;
        let response = response_receiver.await;
        if let Ok(GatewayResponse::GatewayDeleted) = response {
        } else {
            warn!(
                "reconcile_gateway: {controller_name} {gateway_class_name} {id} {resource_name} {resource_version:?} {response:?} ... Problem "
            );
        }
    }

    // async fn add_gateway_class_finalizer(
    //     client: &Client,
    //     resource_name: &str,
    //     controller_name: &str,
    // ) -> Result<()> {
    //     let api = Api::<GatewayClass>::all(client.clone());
    //     let maybe_meta = api.get_metadata(resource_name).await;
    //     debug!("add_gateway_class_finalizer {resource_name} {controller_name}");
    //     if let Ok(mut meta) = maybe_meta {
    //         let finalizers = meta.finalizers_mut();

    //         if finalizers.contains(&GATEWAY_CLASS_FINALIZER_NAME.to_owned()) {
    //             Ok(())
    //         } else {
    //             finalizers.push(GATEWAY_CLASS_FINALIZER_NAME.to_owned());
    //             let mut object_meta = meta.meta().clone();
    //             object_meta.managed_fields = None;
    //             let meta = object_meta.into_request_partial::<GatewayClass>();

    //             match api
    //                 .patch_metadata(
    //                     resource_name,
    //                     &PatchParams::apply(controller_name),
    //                     &Patch::Apply(&meta),
    //                 )
    //                 .await
    //             {
    //                 Ok(_) => Ok(()),
    //                 Err(e) => {
    //                     warn!("reconcile_gateway_class: {controller_name} {resource_name} patch failed {e:?}");
    //                     Err(ControllerError::PatchFailed)
    //                 }
    //             }
    //         }
    //     } else {
    //         Err(ControllerError::UnknownResource)
    //     }
    // }
}
