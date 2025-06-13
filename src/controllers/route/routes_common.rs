use std::{collections::BTreeSet, convert::TryFrom, sync::Arc, time::Duration};

use gateway_api::{
    common_types::{ParentRouteStatus, RouteRef, RouteStatus},
    gateways::Gateway,
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{runtime::controller::Action, Resource};
use kube_core::ObjectMeta;
use tokio::sync::mpsc;
use tracing::{debug, warn, Span};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, Backend, ReferenceValidateRequest, RequestContext, ResourceKey, Route, RouteRefKey, VerifiyItems},
    controllers::{utils::RouteListenerMatcher, ControllerError},
    services::patchers::{DeleteContext, FinalizerContext, Operation},
    state::State,
};

const CONDITION_MESSAGE: &str = "Route updated by controller";

const RECONCILE_LONG_WAIT: Duration = Duration::from_secs(3600);

pub fn generate_status_for_unknown_gateways(controller_name: &str, gateways: &[(&RouteRef, Option<Arc<Gateway>>)], generation: Option<i64>) -> Vec<ParentRouteStatus> {
    gateways
        .iter()
        .map(|(gateway, _)| ParentRouteStatus {
            conditions: Some(vec![Condition {
                last_transition_time: Time(Utc::now()),
                message: CONDITION_MESSAGE.to_owned(),
                observed_generation: generation,
                reason: "BackendNotFound".to_owned(),
                status: "False".to_owned(),
                type_: "ResolvedRefs".to_owned(),
            }]),
            controller_name: controller_name.to_owned(),
            parent_ref: RouteRef {
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

pub fn extract_references(route: &Route) -> BTreeSet<ResourceKey> {
    let mut backend_reference_keys = BTreeSet::new();

    for backend in &route.backends() {
        if let Backend::Maybe(backend_service_config) = backend {
            backend_reference_keys.insert(backend_service_config.resource_key.clone());
        }
    }

    backend_reference_keys
}

pub fn needs_finalizer<T: serde::Serialize>(resource_key: &ResourceKey, controller_name: &String, resource_meta: &ObjectMeta) -> Option<Operation<T>> {
    let has_finalizer = if let Some(finalizers) = resource_meta.finalizers.as_ref() {
        finalizers.contains(controller_name)
    } else {
        false
    };

    if has_finalizer {
        None
    } else {
        Some(Operation::PatchFinalizer(FinalizerContext {
            resource_key: resource_key.clone(),
            controller_name: controller_name.clone(),
            finalizer_name: controller_name.clone(),
            span: Span::current().clone(),
        }))
    }
}

#[derive(TypedBuilder)]
pub struct CommonRouteHandler<R: serde::Serialize> {
    pub state: State,
    pub resource_key: ResourceKey,
    pub controller_name: String,
    pub resource: Arc<R>,
    pub route_patcher_sender: mpsc::Sender<Operation<R>>,
    pub references_validator_sender: mpsc::Sender<ReferenceValidateRequest>,
    pub version: Option<String>,
}

impl<R> CommonRouteHandler<R>
where
    R: Clone + serde::Serialize + kube::Resource,
    R: TryInto<Route>,
    <R as TryInto<crate::common::Route>>::Error: std::fmt::Debug,
{
    pub async fn on_new_or_changed<T>(&self, route_key: ResourceKey, parent_gateway_refs: &[RouteRef], generation: Option<i64>, save_route: T) -> Result<Action, ControllerError>
    where
        T: Fn(&State, Option<RouteStatus>),
    {
        let route = self.resource.as_ref().clone();
        let route = route.try_into().map_err(|e| ControllerError::InvalidPayload(format!("Can't convert the route {e:?}")))?;
        let state = &self.state;
        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        let parent_gateway_refs = parent_gateway_refs_keys
            .clone()
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway(key.as_ref()).expect("We expect the lock to work")))
            .map(|i| if i.1.is_some() { Ok(i) } else { Err(i) });

        let (resolved_gateways, unknown_gateways) = VerifiyItems::verify(parent_gateway_refs);

        parent_gateway_refs_keys.for_each(|(_ref, key)| state.attach_http_route_to_gateway(key.as_ref().clone(), route_key.clone()).expect("We expect the lock to work"));

        let matching_gateways = RouteListenerMatcher::filter_matching_gateways(state, &resolved_gateways);
        let unknown_gateway_status = generate_status_for_unknown_gateways(&self.controller_name, &unknown_gateways, generation);

        save_route(state, Some(RouteStatus { parents: unknown_gateway_status }));

        let _ = self.add_finalizer(&self.resource).await?;

        let references = extract_references(&route);

        let _ = self.references_validator_sender.send(ReferenceValidateRequest::AddRoute { references, span: Span::current() }).await;

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
                .references_validator_sender
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

    pub async fn on_deleted(&self, route_key: ResourceKey, parent_gateway_refs: &[RouteRef]) -> Result<Action, ControllerError> {
        let state = &self.state;
        let controller_name = &self.controller_name;
        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        debug!("Parent keys = {parent_gateway_refs_keys:?}");
        parent_gateway_refs_keys.for_each(|(_ref, gateway_key)| state.detach_http_route_from_gateway(gateway_key.as_ref(), &route_key).expect("We expect the lock to work"));

        let Some(route) = state.delete_http_route(&route_key).expect("We expect the lock to work") else {
            return Err(ControllerError::InvalidPayload("Route doesn't exist".to_owned()));
        };
        let route = Route::try_from(&*route)?;

        let http_route = (*self.resource).clone();
        let resource_key = route_key;
        let _res = self
            .route_patcher_sender
            .send(Operation::Delete(DeleteContext {
                resource_key: resource_key.clone(),
                resource: http_route,
                controller_name: controller_name.to_owned(),
                span: Span::current().clone(),
            }))
            .await;

        let references = extract_references(&route);

        let _ = self.references_validator_sender.send(ReferenceValidateRequest::DeleteRoute { references, span: Span::current() }).await;

        Ok(Action::await_change())
    }

    async fn add_finalizer(&self, resource: &Arc<impl Resource>) -> Result<Action, ControllerError> {
        if let Some(finalizer) = needs_finalizer::<R>(&self.resource_key, &self.controller_name, resource.meta()) {
            let _res = self.route_patcher_sender.send(finalizer).await;
        }

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }
}
