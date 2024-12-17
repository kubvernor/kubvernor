use std::sync::Arc;

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::apis::standard::{gatewayclasses::GatewayClass, gateways::Gateway};
use kube::{
    runtime::{controller::Action, watcher::Config, Controller},
    Api, Client, Resource,
};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tracing::{warn, Instrument, Span};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::{
    resource_handler::ResourceHandler,
    utils::{ResourceCheckerArgs, ResourceState},
    ControllerError, RECONCILE_ERROR_WAIT, RECONCILE_LONG_WAIT,
};
use crate::{
    common::{self, BackendGatewayEvent, DeletedContext, ReferenceResolveRequest, RequestContext, ResourceKey},
    patchers::{DeleteContext, Operation},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone, TypedBuilder)]
pub struct GatewayControllerContext {
    controller_name: String,
    client: Client,
    gateway_channel_sender: mpsc::Sender<BackendGatewayEvent>,
    state: State,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    resolve_references_channel_sender: mpsc::Sender<ReferenceResolveRequest>,
}

#[derive(TypedBuilder)]
pub struct GatewayController {
    ctx: Arc<GatewayControllerContext>,
}

impl GatewayController {
    pub fn get_controller(&self) -> BoxFuture<()> {
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
            ControllerError::UnknownGatewayClass(_) | ControllerError::ResourceInWrongState | ControllerError::ResourceHasWrongStatus => Action::requeue(RECONCILE_ERROR_WAIT),
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

        let gateway_class_name = {
            let gateway_class_name = &resource.spec.gateway_class_name;
            if !state
                .get_gateway_classes()
                .expect("We expect the lock to work")
                .into_iter()
                .any(|gc| gc.metadata.name == Some(gateway_class_name.to_string()))
            {
                warn!("reconcile_gateway: {controller_name} {name} Unknown gateway class name {gateway_class_name}");
                return Err(ControllerError::UnknownGatewayClass(gateway_class_name.clone()));
            }
            gateway_class_name.clone()
        };

        let maybe_stored_gateway_class = state.get_gateway(&resource_key).expect("We expect the lock to work");

        let handler = GatewayResourceHandler::builder()
            .state(ctx.state.clone())
            .resource_key(resource_key)
            .controller_name(controller_name)
            .gateway_class_name(gateway_class_name)
            .resource(resource)
            .version(version)
            .gateway_channel_sender(ctx.gateway_channel_sender.clone())
            .gateway_patcher(gateway_patcher)
            .gateway_class_patcher(gateway_class_patcher)
            .resolve_references_channel_sender(ctx.resolve_references_channel_sender.clone())
            .build();

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

#[derive(TypedBuilder)]
struct GatewayResourceHandler<R> {
    state: State,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    version: Option<String>,
    gateway_class_name: String,
    gateway_channel_sender: mpsc::Sender<BackendGatewayEvent>,
    gateway_patcher: mpsc::Sender<Operation<Gateway>>,
    gateway_class_patcher: mpsc::Sender<Operation<GatewayClass>>,
    resolve_references_channel_sender: mpsc::Sender<ReferenceResolveRequest>,
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
        let sender = &self.gateway_channel_sender;
        let _res = self.delete_gateway(sender, kube_gateway).await;
        let _res = self
            .gateway_patcher
            .send(Operation::Delete(DeleteContext {
                resource_key: id.clone(),
                resource: (**kube_gateway).clone(),
                controller_name: self.controller_name.clone(),
                span: Span::current().clone(),
            }))
            .await;
        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn delete_gateway(&self, sender: &Sender<BackendGatewayEvent>, kube_gateway: &Arc<Gateway>) -> Result<Gateway> {
        let maybe_gateway = common::Gateway::try_from(&**kube_gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };

        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event = BackendGatewayEvent::GatewayDeleted(
            DeletedContext::builder()
                .response_sender(response_sender)
                .gateway(backend_gateway)
                .span(Span::current().clone())
                .build(),
        );
        let _ = sender.send(listener_event).await;
        let _response = response_receiver.await;
        Ok((**kube_gateway).clone())
    }

    async fn on_new_or_changed(&self, _: ResourceKey, kube_gateway: &Arc<Gateway>, _: &State) -> Result<Action> {
        let maybe_gateway = common::Gateway::try_from(&**kube_gateway);

        let Ok(backend_gateway) = maybe_gateway else {
            warn!("Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };

        let _ = self
            .resolve_references_channel_sender
            .send(ReferenceResolveRequest::New(
                RequestContext::builder()
                    .gateway(backend_gateway)
                    .kube_gateway(Arc::clone(kube_gateway))
                    .gateway_class_name(self.gateway_class_name.clone())
                    .span(Span::current())
                    .build(),
            ))
            .await;

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn on_status_changed(&self, gateway_id: ResourceKey, resource: &Arc<Gateway>, state: &State) -> Result<Action> {
        let controller_name = &self.controller_name;
        let gateway_class_name = &self.gateway_class_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string()) {
                    let () = state.save_gateway(gateway_id.clone(), resource).expect("We expect the lock to work");
                    common::add_finalizer_to_gateway_class(&self.gateway_class_patcher, gateway_class_name, controller_name)
                        .instrument(Span::current().clone())
                        .await;
                    let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                        finalizers.iter().any(|f| f == controller_name)
                    } else {
                        false
                    };

                    if !has_finalizer {
                        let () = common::add_finalizer(&self.gateway_patcher, &gateway_id, controller_name).await;
                    }

                    return Ok(Action::requeue(RECONCILE_LONG_WAIT));
                }
            }
        }
        Err(ControllerError::ResourceHasWrongStatus)
    }
}
