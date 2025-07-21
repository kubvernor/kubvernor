use std::{collections::BTreeSet, convert::TryFrom, sync::Arc, time::Duration};

use gateway_api::{
    common::{ParentReference, ParentRouteStatus, RouteStatus},
    gateways::Gateway,
};
use gateway_api_inference_extension::inferencepools::InferencePool;
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{runtime::controller::Action, Resource};
use kube_core::ObjectMeta;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, Backend, BackendType, ReferenceValidateRequest, RequestContext, ResourceKey, Route, RouteRefKey, VerifiyItems},
    controllers::{inference_pool, utils::RouteListenerMatcher, ControllerError},
    services::patchers::{DeleteContext, FinalizerContext, Operation, PatchContext},
    state::State,
};

const CONDITION_MESSAGE: &str = "Route updated by controller";

const RECONCILE_LONG_WAIT: Duration = Duration::from_secs(3600);

pub fn generate_status_for_unknown_gateways(controller_name: &str, gateways: &[(&ParentReference, Option<Arc<Gateway>>)], generation: Option<i64>) -> Vec<ParentRouteStatus> {
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
            parent_ref: ParentReference {
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
        if let Backend::Maybe(backend_type) = backend {
            backend_reference_keys.insert(backend_type.resource_key());
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
    #[builder(default)]
    pub inference_pool_patcher_channel_sender: Option<mpsc::Sender<Operation<InferencePool>>>,
}

impl<R> CommonRouteHandler<R>
where
    R: Clone + serde::Serialize + kube::Resource,
    R: TryInto<Route>,
    <R as TryInto<crate::common::Route>>::Error: std::fmt::Debug,
{
    pub async fn on_new_or_changed<T>(&self, route_key: ResourceKey, parent_gateway_refs: &[ParentReference], generation: Option<i64>, save_route: T) -> Result<Action, ControllerError>
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

        let _ = self.references_validator_sender.send(ReferenceValidateRequest::AddRoute { route_key, references }).await;

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
                        .build(),
                )))
                .await;
        }

        Ok(Action::await_change())
    }

    pub async fn on_deleted(&self, route_key: ResourceKey, parent_gateway_refs: &[ParentReference]) -> Result<Action, ControllerError> {
        let state = &self.state;
        let controller_name = &self.controller_name;
        let parent_gateway_refs_keys = parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        debug!("Parent keys = {parent_gateway_refs_keys:?}");
        let gateway_ids = parent_gateway_refs_keys.map(|(_, r)| r.resource_key).collect::<BTreeSet<_>>();
        gateway_ids
            .clone()
            .iter()
            .for_each(|gateway_key| state.detach_http_route_from_gateway(gateway_key, &route_key).expect("We expect the lock to work"));

        let Some(route) = state.delete_http_route(&route_key).expect("We expect the lock to work") else {
            return Err(ControllerError::InvalidPayload("Route doesn't exist".to_owned()));
        };

        let route = Route::try_from(&*route)?;
        Self::update_inference_pools(self, &route, &gateway_ids).await;

        let http_route = (*self.resource).clone();
        let resource_key = route_key;
        let _res = self
            .route_patcher_sender
            .send(Operation::Delete(DeleteContext {
                resource_key: resource_key.clone(),
                resource: http_route,
                controller_name: controller_name.to_owned(),
            }))
            .await;

        let references = extract_references(&route);

        let _ = self
            .references_validator_sender
            .send(ReferenceValidateRequest::DeleteRoute { route_key: resource_key, references })
            .await;

        Ok(Action::await_change())
    }

    async fn add_finalizer(&self, resource: &Arc<impl Resource>) -> Result<Action, ControllerError> {
        if let Some(finalizer) = needs_finalizer::<R>(&self.resource_key, &self.controller_name, resource.meta()) {
            let _res = self.route_patcher_sender.send(finalizer).await;
        }

        Ok(Action::requeue(RECONCILE_LONG_WAIT))
    }

    async fn update_inference_pools(&self, route: &Route, gateway_ids: &BTreeSet<ResourceKey>) {
        debug!("Updating inference pools of route delete {} {gateway_ids:?}", route.name());
        if let Some(inference_pool_patcher_sender) = self.inference_pool_patcher_channel_sender.as_ref() {
            for inference_pool_backend in route.backends().iter().filter_map(|b| match b.backend_type() {
                BackendType::InferencePool(pool) => Some(&pool.resource_key),
                _ => None,
            }) {
                if let Some(inference_pool) = self.state.get_inference_pool(inference_pool_backend).expect("We expect the lock to work") {
                    debug!("Retrieved inference pool is {inference_pool:#?}");
                    let mut inference_pool = inference_pool::remove_inference_pool_parents((*inference_pool).clone(), gateway_ids);
                    inference_pool.metadata.managed_fields = None;
                    let inference_pool_resource_key = ResourceKey::from(&inference_pool);
                    let (sender, receiver) = oneshot::channel();
                    debug!("Patching updating inference pools of route delete {} {gateway_ids:?}", route.name());
                    let _ = inference_pool_patcher_sender
                        .send(Operation::PatchStatus(PatchContext {
                            resource_key: inference_pool_resource_key.clone(),
                            resource: inference_pool.clone(),
                            controller_name: self.controller_name.clone(),
                            response_sender: sender,
                        }))
                        .await;
                    match receiver.await {
                        Ok(Err(_)) | Err(_) => warn!("Could't patch status"),
                        _ => (),
                    }
                } else {
                    warn!("No inference pool - Updating inference pools of route delete {} {gateway_ids:?}", route.name());
                }
            }
        }
    }
}
