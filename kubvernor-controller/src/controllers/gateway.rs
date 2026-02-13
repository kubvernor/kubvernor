use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gateway_api::{constants, gatewayclasses::GatewayClass, gateways::Gateway};
use kube::{
    Api, Client, Resource,
    runtime::{Controller, controller::Action, watcher::Config},
};
use kubvernor_common::{GatewayImplementationType, ResourceKey};
use kubvernor_state::State;
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tracing::{debug, info, warn};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::{
    ControllerError, RECONCILE_ERROR_WAIT, RECONCILE_LONG_WAIT,
    handlers::ResourceHandler,
    utils::{ResourceCheckerArgs, ResourceState},
};
use crate::{
    common::{self, BackendGatewayEvent, DeletedContext, ReferenceValidateRequest, RequestContext},
    services::patchers::{DeleteContext, Operation},
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone, TypedBuilder)]
pub struct GatewayControllerContext {
    controller_name: String,
    client: Client,
    gateway_channel_senders: HashMap<GatewayImplementationType, mpsc::Sender<BackendGatewayEvent>>,
    state: State,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
}

#[derive(TypedBuilder)]
pub struct GatewayController {
    ctx: Arc<GatewayControllerContext>,
}

impl GatewayController {
    pub fn get_controller(&'_ self) -> BoxFuture<'_, ()> {
        let client = self.ctx.client.clone();
        let context = &self.ctx;

        Controller::new(Api::all(client), Config::default())
            .run(Self::reconcile_gateway, Self::error_policy, Arc::clone(context))
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, err: &ControllerError, _ctx: Arc<GatewayControllerContext>) -> Action {
        match err {
            ControllerError::PatchFailed
            | ControllerError::AlreadyAdded
            | ControllerError::InvalidPayload(_)
            | ControllerError::InvalidRecipent
            | ControllerError::FinalizerPatchFailed(_)
            | ControllerError::BackendError
            | ControllerError::UnknownResource => Action::requeue(RECONCILE_LONG_WAIT),
            ControllerError::UnknownGatewayClass(_)
            | ControllerError::UnknownGatewayType
            | ControllerError::ResourceInWrongState
            | ControllerError::ResourceHasWrongStatus => Action::requeue(RECONCILE_ERROR_WAIT),
        }
    }

    async fn reconcile_gateway(resource: Arc<Gateway>, ctx: Arc<GatewayControllerContext>) -> Result<Action> {
        let controller_name = ctx.controller_name.clone();
        let gateway_patcher = ctx.gateway_patcher.clone();
        let gateway_class_patcher = ctx.gateway_class_patcher.clone();

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

        let state = ctx.state.clone();
        let version = resource.meta().resource_version.clone();

        let (gateway_class_name, backend_type) = {
            let gateway_class_name = &resource.spec.gateway_class_name;
            let mut configured_backend_type = GatewayImplementationType::Envoy;
            if let Some(gateway_class) = state
                .get_gateway_classes()
                .expect("We expect the lock to work")
                .into_iter()
                .find(|gc| gc.metadata.name == Some(gateway_class_name.clone()))
            {
                if let Some(config_reference) = &gateway_class.spec.parameters_ref {
                    let configuration_api: Api<kubvernor_api::crds::KubvernorConfig> =
                        Api::namespaced(ctx.client.clone(), &resource_key.namespace);

                    if let Ok(configuration) = configuration_api.get(&config_reference.name).await {
                        debug!("reconcile_gateway: {controller_name} {name} retrieved configuration {:?}", configuration);
                        let Ok(backend_type) = GatewayImplementationType::try_from(configuration.spec.backendtype.as_ref()) else {
                            info!(
                                "reconcile_gateway: {controller_name} {name} Invalid backend type {:?}",
                                configuration.spec.backendtype.as_ref()
                            );
                            return Err(ControllerError::InvalidPayload("Invalid backend type".to_owned()));
                        };
                        configured_backend_type = backend_type;
                    } else {
                        debug!("reconcile_gateway: {controller_name} {name} Unable to find KubernorConfig {config_reference:?}");
                        return Err(ControllerError::InvalidPayload("Unable to find KubernorConfig".to_owned()));
                    }
                } else {
                    debug!("reconcile_gateway: {controller_name} {name} No configuration found.. using defaults ");
                }
            } else {
                warn!("reconcile_gateway: {controller_name} {name} Unknown gateway class name {gateway_class_name}");
                return Err(ControllerError::UnknownGatewayClass(gateway_class_name.clone()));
            }

            (gateway_class_name.clone(), configured_backend_type)
        };

        let maybe_stored_gateway = state.get_gateway(&resource_key).expect("We expect the lock to work");

        info!(
            "reconcile_gateway: {controller_name} {name} {backend_type:?} {:?}",
            maybe_stored_gateway.as_ref().map(|g| ResourceKey::from(&(**g)))
        );
        let handler = GatewayResourceHandler::builder()
            .state(ctx.state.clone())
            .resource_key(resource_key)
            .controller_name(controller_name)
            .gateway_class_name(gateway_class_name)
            .resource(resource)
            .version(version)
            .gateway_channel_senders(ctx.gateway_channel_senders.clone())
            .gateway_patcher(gateway_patcher)
            .gateway_class_patcher(gateway_class_patcher)
            .validate_references_channel_sender(ctx.validate_references_channel_sender.clone())
            .gateway_backend_type(backend_type)
            .build();

        handler.process(maybe_stored_gateway, Self::check_spec_changed, Self::check_status_changed).await
    }

    fn check_spec_changed(args: ResourceCheckerArgs<Gateway>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec { ResourceState::SpecNotChanged } else { ResourceState::SpecChanged }
    }

    fn check_status_changed(args: ResourceCheckerArgs<Gateway>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.status == stored_resource.status { ResourceState::StatusNotChanged } else { ResourceState::StatusChanged }
    }
}

#[derive(TypedBuilder)]
struct GatewayResourceHandler<R> {
    state: State,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    version: Option<String>,
    gateway_class_name: String,
    gateway_channel_senders: HashMap<GatewayImplementationType, mpsc::Sender<BackendGatewayEvent>>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
    gateway_backend_type: GatewayImplementationType,
}

#[async_trait]
impl ResourceHandler<Gateway> for GatewayResourceHandler<Gateway> {
    fn state(&self) -> &State {
        &self.state
    }

    fn version(&self) -> String {
        self.version.clone().unwrap_or_default()
    }

    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }

    fn resource(&self) -> Arc<Gateway> {
        Arc::clone(&self.resource)
    }

    async fn on_spec_not_changed(&self, resource_key: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        let () = state.save_gateway(resource_key, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, resource_key: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        let () = state.save_gateway(resource_key.clone(), resource).expect("We expect the lock to work");
        self.on_new_or_changed(resource_key, resource, state).await
    }

    async fn on_spec_changed(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_status_changed(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        self.on_status_changed(id, resource, state).await
    }

    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        self.on_deleted(id, resource, state).await
    }
    async fn on_status_not_changed(&self, resource_key: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        let () = state.maybe_save_gateway(resource_key, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }
}

impl GatewayResourceHandler<Gateway> {
    async fn on_deleted(&self, id: ResourceKey, kube_gateway: &Arc<Gateway>, state: &State) -> Result<Action> {
        let _ = state.delete_gateway(&id).expect("We expect the lock to work");
        let gateway_type = state.delete_gateway_type(&id).expect("We expect the lock to work");
        let senders = &self.gateway_channel_senders;
        if let Some(gateway_type) = gateway_type {
            let _res = self.delete_gateway(senders, kube_gateway, gateway_type).await;
        } else {
            warn!("GatewayResourceHandler Controller {} Gateway {} Unknown gateway implementation type ", &self.controller_name, id);
        }

        let _res = self
            .gateway_patcher
            .send(Operation::Delete(DeleteContext {
                resource_key: id.clone(),
                resource: (**kube_gateway).clone(),
                controller_name: self.controller_name.clone(),
            }))
            .await;
        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn delete_gateway(
        &self,
        senders: &HashMap<GatewayImplementationType, Sender<BackendGatewayEvent>>,
        kube_gateway: &Arc<Gateway>,
        gateway_type: GatewayImplementationType,
    ) -> Result<Gateway> {
        let maybe_gateway = common::Gateway::try_from(&**kube_gateway);

        let Ok(mut backend_gateway) = maybe_gateway else {
            warn!("Misconfigured gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };

        *backend_gateway.backend_type_mut() = gateway_type;
        let sender = senders.get(backend_gateway.backend_type()).expect("Invalid backend type");

        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event = BackendGatewayEvent::Deleted(Box::new(
            DeletedContext::builder().response_sender(response_sender).gateway(backend_gateway.clone()).build(),
        ));

        let _ = self.validate_references_channel_sender.send(ReferenceValidateRequest::DeleteGateway { gateway: backend_gateway }).await;

        let _ = sender.send(listener_event).await;
        let _response = response_receiver.await;
        Ok((**kube_gateway).clone())
    }

    async fn on_new_or_changed(&self, _: ResourceKey, kube_gateway: &Arc<Gateway>, _: &State) -> Result<Action> {
        let maybe_gateway = common::Gateway::try_from((&**kube_gateway, self.gateway_backend_type.clone()));

        let Ok(backend_gateway) = maybe_gateway else {
            warn!("Misconfigured gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };

        let kube_gateway = (**kube_gateway).clone();
        info!("Saving gateway type");
        let _ = self.state.save_gateway_type(backend_gateway.key().clone(), self.gateway_backend_type.clone());

        let _ = self
            .validate_references_channel_sender
            .send(ReferenceValidateRequest::AddGateway(Box::new(
                RequestContext::builder()
                    .gateway(backend_gateway)
                    .kube_gateway(kube_gateway)
                    .gateway_class_name(self.gateway_class_name.clone())
                    .build(),
            )))
            .await;

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn on_status_changed(&self, gateway_id: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        let controller_name = &self.controller_name;
        let gateway_class_name = &self.gateway_class_name;
        if let Some(status) = &resource.status
            && let Some(conditions) = &status.conditions
        {
            if conditions.iter().any(|c| c.type_ == constants::GatewayConditionType::Ready.to_string()) {
                let () = state.save_gateway(gateway_id.clone(), resource).expect("We expect the lock to work");
                common::add_finalizer_to_gateway_class(&self.gateway_class_patcher, gateway_class_name, controller_name).await;
                let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                    finalizers.iter().any(|f| f == controller_name)
                } else {
                    false
                };

                if !has_finalizer {
                    let () = common::add_finalizer(&self.gateway_patcher, &gateway_id, controller_name).await;
                }

                return Ok(Action::requeue(RECONCILE_LONG_WAIT));
            } else if conditions.iter().any(|c| {
                c.type_ == constants::GatewayConditionType::Programmed.to_string()
                    && c.status == constants::GatewayConditionReason::Pending.to_string()
            }) {
                return self.on_new_or_changed(gateway_id, resource, state).await;
            }
        }
        Err(ControllerError::ResourceHasWrongStatus)
    }
}
