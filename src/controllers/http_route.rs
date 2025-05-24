use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use gateway_api::{
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
use tokio::sync::mpsc::{self};
use tracing::{debug, warn, Span};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::{
    handlers::ResourceHandler,
    utils::{ResourceCheckerArgs, ResourceState, RouteListenerMatcher},
    ControllerError, RECONCILE_LONG_WAIT,
};
use crate::{
    common::{self, Backend, ReferenceValidateRequest, RequestContext, ResourceKey, Route, RouteRefKey, VerifiyItems},
    services::patchers::{DeleteContext, FinalizerContext, Operation},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;
const CONDITION_MESSAGE: &str = "Route updated by controller";

#[derive(Clone, TypedBuilder)]
pub struct HttpRouteControllerContext {
    controller_name: String,
    client: Client,
    state: State,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
}

#[derive(TypedBuilder)]
pub struct HttpRouteController {
    ctx: Arc<HttpRouteControllerContext>,
}

impl HttpRouteController {
    pub fn get_controller(&self) -> BoxFuture<()> {
        let client = self.ctx.client.clone();
        let context = &self.ctx;

        Controller::new(Api::all(client), Config::default())
            .run(Self::reconcile_http_route, Self::error_policy, Arc::clone(context))
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<HttpRouteControllerContext>) -> Action {
        Action::requeue(RECONCILE_LONG_WAIT)
    }

    async fn reconcile_http_route(resource: Arc<httproutes::HTTPRoute>, ctx: Arc<HttpRouteControllerContext>) -> Result<Action> {
        let controller_name = ctx.controller_name.clone();
        let http_route_patcher = ctx.http_route_patcher.clone();

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload("Uid must be present".to_owned()));
        };

        let Ok(_) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload("Uid in wrong format".to_owned()));
        };
        let version = resource.meta().resource_version.clone();
        let resource_key = ResourceKey::from(&*resource);

        let state = &ctx.state;

        let maybe_stored_route = state.get_http_route_by_id(&resource_key).expect("We expect the lock to work");

        let _ = Route::try_from(&*resource)?;

        let handler = HTTPRouteHandler::builder()
            .state(ctx.state.clone())
            .resource_key(resource_key)
            .controller_name(controller_name)
            .resource(resource)
            .http_route_patcher(http_route_patcher)
            .validate_references_channel_sender(ctx.validate_references_channel_sender.clone())
            .version(version)
            .build();

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

#[derive(TypedBuilder)]
struct HTTPRouteHandler<R> {
    state: State,
    resource_key: ResourceKey,
    controller_name: String,
    resource: Arc<R>,
    http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
    version: Option<String>,
}

#[async_trait]
impl ResourceHandler<HTTPRoute> for HTTPRouteHandler<HTTPRoute> {
    fn state(&self) -> &State {
        &self.state
    }

    fn version(&self) -> String {
        self.version.clone().unwrap_or_default()
    }

    fn resource(&self) -> Arc<HTTPRoute> {
        Arc::clone(&self.resource)
    }

    fn resource_key(&self) -> ResourceKey {
        self.resource_key.clone()
    }

    async fn on_spec_not_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        let () = state.save_http_route(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        let () = state.maybe_save_http_route(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_status_changed(&self, _id: ResourceKey, _resource: &Arc<HTTPRoute>, _state: &State) -> Result<Action> {
        Ok(Action::await_change())
    }

    async fn on_spec_changed(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        self.on_deleted(id, resource, state).await
    }
}

impl HTTPRouteHandler<HTTPRoute> {
    async fn on_new_or_changed(&self, route_key: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        let Some(parent_gateway_refs) = resource.spec.parent_refs.as_ref() else {
            return Err(ControllerError::InvalidPayload("Route with no parents".to_owned()));
        };

        let mut http_route = (**resource).clone();
        let route = Route::try_from(&http_route)?;

        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        let parent_gateway_refs = parent_gateway_refs_keys
            .clone()
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway(key.as_ref()).expect("We expect the lock to work")))
            .map(|i| if i.1.is_some() { Ok(i) } else { Err(i) });

        let (resolved_gateways, unknown_gateways) = VerifiyItems::verify(parent_gateway_refs);

        parent_gateway_refs_keys.for_each(|(_ref, key)| state.attach_http_route_to_gateway(key.as_ref().clone(), route_key.clone()).expect("We expect the lock to work"));

        let matching_gateways = RouteListenerMatcher::filter_http_matching_gateways(state, &resolved_gateways);
        let unknown_gateway_status = self.generate_status_for_unknown_gateways(&unknown_gateways, resource.metadata.generation);

        http_route.status = Some(HTTPRouteStatus { parents: unknown_gateway_status });
        let () = state.save_http_route(route_key.clone(), &Arc::new(http_route)).expect("We expect the lock to work");

        let _ = self.add_finalizer(resource).await?;

        let references = Self::extract_references(&route);

        let _ = self
            .validate_references_channel_sender
            .send(ReferenceValidateRequest::AddRoute { references, span: Span::current() })
            .await;

        for kube_gateway in matching_gateways {
            let gateway_class_name = {
                let gateway_class_name = &kube_gateway.spec.gateway_class_name;

                if !self
                    .state
                    .get_gateway_classes()
                    .expect("We expect the lock to work")
                    .into_iter()
                    .any(|gc| gc.metadata.name == Some(gateway_class_name.to_owned()))
                {
                    warn!(
                        "reconcile_gateway: {} {:?} Unknown gateway class name {gateway_class_name}",
                        &self.controller_name,
                        kube_gateway.meta().name
                    );
                    return Err(ControllerError::UnknownGatewayClass(gateway_class_name.clone()));
                }
                gateway_class_name.clone()
            };
            let kube_gateway = (*kube_gateway).clone();
            let _ = self
                .validate_references_channel_sender
                .send(ReferenceValidateRequest::AddGateway(Box::new(
                    RequestContext::builder()
                        .gateway(common::Gateway::try_from(&kube_gateway).expect("We expect the lock to work"))
                        .kube_gateway(kube_gateway)
                        .gateway_class_name(gateway_class_name)
                        .span(Span::current())
                        .build(),
                )))
                .await;
        }

        Ok(Action::await_change())
    }

    async fn on_deleted(&self, route_key: ResourceKey, resource: &Arc<HTTPRoute>, state: &State) -> Result<Action> {
        let _ = Route::try_from(&**resource)?;

        let Some(parent_gateway_refs) = resource.spec.parent_refs.as_ref() else {
            return Err(ControllerError::InvalidPayload("Route with no parents".to_owned()));
        };

        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        debug!("Parent keys = {parent_gateway_refs_keys:?}");
        parent_gateway_refs_keys.for_each(|(_ref, gateway_key)| state.detach_http_route_from_gateway(gateway_key.as_ref(), &route_key).expect("We expect the lock to work"));

        let Some(route) = state.delete_http_route(&route_key).expect("We expect the lock to work") else {
            return Err(ControllerError::InvalidPayload("Route doesn't exist".to_owned()));
        };
        let route = Route::try_from(&*route)?;

        let http_route = (**resource).clone();
        let resource_key = route_key;
        let _res = self
            .http_route_patcher
            .send(Operation::Delete(DeleteContext {
                resource_key: resource_key.clone(),
                resource: http_route,
                controller_name: self.controller_name.clone(),
                span: Span::current().clone(),
            }))
            .await;

        let references = Self::extract_references(&route);

        let _ = self
            .validate_references_channel_sender
            .send(ReferenceValidateRequest::DeleteRoute { references, span: Span::current() })
            .await;

        Ok(Action::await_change())
    }

    fn generate_status_for_unknown_gateways(&self, gateways: &[(&HTTPRouteParentRefs, Option<Arc<Gateway>>)], generation: Option<i64>) -> Vec<HTTPRouteStatusParents> {
        gateways
            .iter()
            .map(|(gateway, _)| HTTPRouteStatusParents {
                conditions: Some(vec![Condition {
                    last_transition_time: Time(Utc::now()),
                    message: CONDITION_MESSAGE.to_owned(),
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

    async fn add_finalizer(&self, resource: &Arc<HTTPRoute>) -> Result<Action> {
        let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
            finalizers.contains(&self.controller_name)
        } else {
            false
        };

        if !has_finalizer {
            let _res = self
                .http_route_patcher
                .send(Operation::PatchFinalizer(FinalizerContext {
                    resource_key: self.resource_key.clone(),
                    controller_name: self.controller_name.clone(),
                    finalizer_name: self.controller_name.clone(),
                    span: Span::current().clone(),
                }))
                .await;
        }
        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    fn extract_references(route: &Route) -> BTreeSet<ResourceKey> {
        let mut backend_reference_keys = BTreeSet::new();

        for backend in &route.backends() {
            if let Backend::Maybe(backend_service_config) = backend {
                backend_reference_keys.insert(backend_service_config.resource_key.clone());
            }
        }

        backend_reference_keys
    }
}
