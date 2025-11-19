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
use kube::{Resource, runtime::controller::Action};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};
use typed_builder::TypedBuilder;

use crate::{
    common::{self, Backend, BackendType, ReferenceValidateRequest, RequestContext, ResourceKey, Route, RouteRefKey, VerifiyItems},
    controllers::{ControllerError, inference_pool, utils::RouteListenerMatcher},
    services::patchers::{DeleteContext, Operation, PatchContext},
    state::State,
};

const CONDITION_MESSAGE: &str = "Route updated by controller";

const RECONCILE_LONG_WAIT: Duration = Duration::from_secs(3600);

pub fn generate_status_for_unknown_gateways(
    controller_name: &str,
    gateways: &[(&ParentReference, Option<Arc<Gateway>>)],
    generation: Option<i64>,
) -> Vec<ParentRouteStatus> {
    gateways
        .iter()
        .map(|(gateway, _)| ParentRouteStatus {
            conditions: vec![Condition {
                last_transition_time: Time(Utc::now()),
                message: CONDITION_MESSAGE.to_owned(),
                observed_generation: generation,
                reason: "BackendNotFound".to_owned(),
                status: "False".to_owned(),
                type_: "ResolvedRefs".to_owned(),
            }],
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
    pub async fn on_new_or_changed<T>(
        &self,
        route_key: ResourceKey,
        parent_gateway_refs: &[ParentReference],
        generation: Option<i64>,
        save_route: T,
    ) -> Result<Action, ControllerError>
    where
        T: Fn(&State, Option<RouteStatus>),
    {
        let route = self.resource.as_ref().clone();
        let route = route.try_into().map_err(|e| ControllerError::InvalidPayload(format!("Can't convert the route {e:?}")))?;
        let state = &self.state;
        let parent_gateway_refs_keys =
            parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        let parent_gateway_refs = parent_gateway_refs_keys
            .clone()
            .map(|(parent_ref, key)| (parent_ref, state.get_gateway(key.as_ref()).expect("We expect the lock to work")))
            .map(|i| if i.1.is_some() { Ok(i) } else { Err(i) });

        let (resolved_gateways, unknown_gateways) = VerifiyItems::verify(parent_gateway_refs);

        parent_gateway_refs_keys.for_each(|(_ref, key)| {
            state.attach_http_route_to_gateway(key.as_ref().clone(), route_key.clone()).expect("We expect the lock to work");
        });

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

            let mut gateway = common::Gateway::try_from(&kube_gateway).expect("We expect the lock to work");
            let Some(gateway_type) = self.state.get_gateway_type(gateway.key()).expect("We expect the lock to work") else {
                warn!(
                    "reconcile_gateway: {} {:?} Unknown gateway implementation type {gateway_class_name} {}",
                    &self.controller_name,
                    kube_gateway.meta().name,
                    gateway.key(),
                );
                return Err(ControllerError::UnknownGatewayType);
            };

            *gateway.backend_type_mut() = gateway_type;

            let _ = self
                .references_validator_sender
                .send(ReferenceValidateRequest::AddGateway(Box::new(
                    RequestContext::builder().gateway(gateway).kube_gateway(kube_gateway).gateway_class_name(gateway_class_name).build(),
                )))
                .await;
        }

        Ok(Action::await_change())
    }

    pub async fn on_deleted(&self, route_key: ResourceKey, parent_gateway_refs: &[ParentReference]) -> Result<Action, ControllerError> {
        let state = &self.state;
        let controller_name = &self.controller_name;
        let parent_gateway_refs_keys =
            parent_gateway_refs.iter().map(|parent_ref| (parent_ref, RouteRefKey::from((parent_ref, route_key.namespace.clone()))));

        debug!("Parent keys = {parent_gateway_refs_keys:?}");
        let gateway_ids = parent_gateway_refs_keys.map(|(_, r)| r.resource_key).collect::<BTreeSet<_>>();
        gateway_ids.clone().iter().for_each(|gateway_key| {
            state.detach_http_route_from_gateway(gateway_key, &route_key).expect("We expect the lock to work");
        });

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

        let _ = self.references_validator_sender.send(ReferenceValidateRequest::DeleteRoute { route_key: resource_key, references }).await;

        Ok(Action::await_change())
    }

    async fn add_finalizer(&self, resource: &Arc<impl Resource>) -> Result<Action, ControllerError> {
        if let Some(finalizer) = crate::controllers::needs_finalizer::<R>(&self.resource_key, &self.controller_name, resource.meta()) {
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use gateway_api::{common::ParentReference, gateways::Gateway};

    use super::{extract_references, generate_status_for_unknown_gateways};
    use crate::common::{
        Backend, BackendType, HTTPRoutingConfiguration, HTTPRoutingRule, NotResolvedReason, ResolutionStatus, ResourceKey, Route,
        RouteConfig, RouteType, ServiceTypeConfig,
    };

    fn create_test_parent_reference(name: &str, namespace: Option<&str>) -> ParentReference {
        ParentReference {
            group: Some("gateway.networking.k8s.io".to_owned()),
            kind: Some("Gateway".to_owned()),
            name: name.to_owned(),
            namespace: namespace.map(|s| s.to_owned()),
            port: Some(80),
            section_name: Some("http".to_owned()),
        }
    }

    fn create_test_service_backend(name: &str, namespace: &str, port: i32) -> Backend {
        Backend::Maybe(BackendType::Service(ServiceTypeConfig {
            resource_key: ResourceKey::namespaced(name, namespace),
            endpoint: format!("{}.{}", name, namespace),
            port,
            effective_port: port,
            weight: 1,
        }))
    }

    fn create_resolved_service_backend(name: &str, namespace: &str, port: i32) -> Backend {
        Backend::Resolved(BackendType::Service(ServiceTypeConfig {
            resource_key: ResourceKey::namespaced(name, namespace),
            endpoint: format!("{}.{}", name, namespace),
            port,
            effective_port: port,
            weight: 1,
        }))
    }

    fn create_test_route(name: &str, namespace: &str, backends: Vec<Backend>) -> Route {
        Route {
            config: RouteConfig {
                resource_key: ResourceKey::namespaced(name, namespace),
                parents: Some(vec![create_test_parent_reference("test-gateway", Some(namespace))]),
                hostnames: vec!["example.com".to_owned()],
                resolution_status: ResolutionStatus::NotResolved(NotResolvedReason::Unknown),
                route_type: RouteType::Http(HTTPRoutingConfiguration {
                    routing_rules: vec![HTTPRoutingRule { name: format!("{}-0", name), backends, matching_rules: vec![], filters: vec![] }],
                }),
            },
        }
    }

    #[test]
    fn test_generate_status_for_unknown_gateways_empty() {
        let controller_name = "test-controller";
        let gateways: Vec<(&ParentReference, Option<Arc<Gateway>>)> = vec![];
        let generation = Some(1);

        let result = generate_status_for_unknown_gateways(controller_name, &gateways, generation);

        assert!(result.is_empty());
    }

    #[test]
    fn test_generate_status_for_unknown_gateways_single() {
        let controller_name = "test-controller";
        let parent_ref = create_test_parent_reference("gateway-1", Some("default"));
        let gateways: Vec<(&ParentReference, Option<Arc<Gateway>>)> = vec![(&parent_ref, None)];
        let generation = Some(1);

        let result = generate_status_for_unknown_gateways(controller_name, &gateways, generation);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].controller_name, controller_name);
        assert_eq!(result[0].parent_ref.name, "gateway-1");
        assert_eq!(result[0].parent_ref.namespace, Some("default".to_owned()));
        assert_eq!(result[0].conditions.len(), 1);
        assert_eq!(result[0].conditions[0].reason, "BackendNotFound");
        assert_eq!(result[0].conditions[0].status, "False");
        assert_eq!(result[0].conditions[0].type_, "ResolvedRefs");
        assert_eq!(result[0].conditions[0].observed_generation, Some(1));
    }

    #[test]
    fn test_generate_status_for_unknown_gateways_multiple() {
        let controller_name = "test-controller";
        let parent_ref_1 = create_test_parent_reference("gateway-1", Some("ns1"));
        let parent_ref_2 = create_test_parent_reference("gateway-2", Some("ns2"));
        let gateways: Vec<(&ParentReference, Option<Arc<Gateway>>)> = vec![(&parent_ref_1, None), (&parent_ref_2, None)];
        let generation = Some(2);

        let result = generate_status_for_unknown_gateways(controller_name, &gateways, generation);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].parent_ref.name, "gateway-1");
        assert_eq!(result[0].parent_ref.namespace, Some("ns1".to_owned()));
        assert_eq!(result[1].parent_ref.name, "gateway-2");
        assert_eq!(result[1].parent_ref.namespace, Some("ns2".to_owned()));
    }

    #[test]
    fn test_generate_status_for_unknown_gateways_with_none_generation() {
        let controller_name = "test-controller";
        let parent_ref = create_test_parent_reference("gateway-1", Some("default"));
        let gateways: Vec<(&ParentReference, Option<Arc<Gateway>>)> = vec![(&parent_ref, None)];
        let generation = None;

        let result = generate_status_for_unknown_gateways(controller_name, &gateways, generation);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].conditions[0].observed_generation, None);
    }

    #[test]
    fn test_generate_status_for_unknown_gateways_preserves_parent_ref_fields() {
        let controller_name = "test-controller";
        let parent_ref = ParentReference {
            group: Some("custom.group.io".to_owned()),
            kind: Some("CustomGateway".to_owned()),
            name: "my-gateway".to_owned(),
            namespace: Some("my-namespace".to_owned()),
            port: Some(8080),
            section_name: Some("https".to_owned()),
        };
        let gateways: Vec<(&ParentReference, Option<Arc<Gateway>>)> = vec![(&parent_ref, None)];
        let generation = Some(5);

        let result = generate_status_for_unknown_gateways(controller_name, &gateways, generation);

        assert_eq!(result.len(), 1);
        let result_parent_ref = &result[0].parent_ref;
        assert_eq!(result_parent_ref.group, Some("custom.group.io".to_owned()));
        assert_eq!(result_parent_ref.kind, Some("CustomGateway".to_owned()));
        assert_eq!(result_parent_ref.name, "my-gateway");
        assert_eq!(result_parent_ref.namespace, Some("my-namespace".to_owned()));
        assert_eq!(result_parent_ref.port, Some(8080));
        assert_eq!(result_parent_ref.section_name, Some("https".to_owned()));
    }

    #[test]
    fn test_extract_references_empty_backends() {
        let route = create_test_route("test-route", "default", vec![]);

        let result = extract_references(&route);

        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_references_single_maybe_backend() {
        let backends = vec![create_test_service_backend("service-1", "default", 8080)];
        let route = create_test_route("test-route", "default", backends);

        let result = extract_references(&route);

        assert_eq!(result.len(), 1);
        let key = result.iter().next().unwrap();
        assert_eq!(key.name, "service-1");
        assert_eq!(key.namespace, "default");
    }

    #[test]
    fn test_extract_references_multiple_maybe_backends() {
        let backends =
            vec![create_test_service_backend("service-1", "default", 8080), create_test_service_backend("service-2", "other-ns", 9090)];
        let route = create_test_route("test-route", "default", backends);

        let result = extract_references(&route);

        assert_eq!(result.len(), 2);
        let keys: Vec<_> = result.iter().collect();
        let names: BTreeSet<_> = keys.iter().map(|k| k.name.as_str()).collect();
        assert!(names.contains("service-1"));
        assert!(names.contains("service-2"));
    }

    #[test]
    fn test_extract_references_ignores_resolved_backends() {
        let backends = vec![create_resolved_service_backend("resolved-service", "default", 8080)];
        let route = create_test_route("test-route", "default", backends);

        let result = extract_references(&route);

        // Resolved backends should be ignored since extract_references only looks for Backend::Maybe
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_references_mixed_backend_types() {
        let backends = vec![
            create_test_service_backend("maybe-service", "default", 8080),
            create_resolved_service_backend("resolved-service", "default", 9090),
        ];
        let route = create_test_route("test-route", "default", backends);

        let result = extract_references(&route);

        // Only Maybe backends should be extracted
        assert_eq!(result.len(), 1);
        let key = result.iter().next().unwrap();
        assert_eq!(key.name, "maybe-service");
    }

    #[test]
    fn test_extract_references_duplicate_backends() {
        let backends =
            vec![create_test_service_backend("service-1", "default", 8080), create_test_service_backend("service-1", "default", 8080)];
        let route = create_test_route("test-route", "default", backends);

        let result = extract_references(&route);

        // BTreeSet should deduplicate
        assert_eq!(result.len(), 1);
        let key = result.iter().next().unwrap();
        assert_eq!(key.name, "service-1");
    }

    #[test]
    fn test_extract_references_different_namespaces_same_name() {
        let backends = vec![create_test_service_backend("service-1", "ns-a", 8080), create_test_service_backend("service-1", "ns-b", 8080)];
        let route = create_test_route("test-route", "default", backends);

        let result = extract_references(&route);

        // Same name in different namespaces should be unique
        assert_eq!(result.len(), 2);
        let namespaces: BTreeSet<_> = result.iter().map(|k| k.namespace.as_str()).collect();
        assert!(namespaces.contains("ns-a"));
        assert!(namespaces.contains("ns-b"));
    }

    #[test]
    fn test_extract_references_with_not_allowed_backend() {
        let backend = Backend::NotAllowed(BackendType::Service(ServiceTypeConfig {
            resource_key: ResourceKey::namespaced("not-allowed-service", "default"),
            endpoint: "not-allowed-service.default".to_owned(),
            port: 8080,
            effective_port: 8080,
            weight: 1,
        }));
        let route = create_test_route("test-route", "default", vec![backend]);

        let result = extract_references(&route);

        // NotAllowed backends should be ignored
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_references_with_unresolved_backend() {
        let backend = Backend::Unresolved(BackendType::Service(ServiceTypeConfig {
            resource_key: ResourceKey::namespaced("unresolved-service", "default"),
            endpoint: "unresolved-service.default".to_owned(),
            port: 8080,
            effective_port: 8080,
            weight: 1,
        }));
        let route = create_test_route("test-route", "default", vec![backend]);

        let result = extract_references(&route);

        // Unresolved backends should be ignored
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_references_with_invalid_backend() {
        let backend = Backend::Invalid(BackendType::Service(ServiceTypeConfig {
            resource_key: ResourceKey::namespaced("invalid-service", "default"),
            endpoint: "invalid-service.default".to_owned(),
            port: 8080,
            effective_port: 8080,
            weight: 1,
        }));
        let route = create_test_route("test-route", "default", vec![backend]);

        let result = extract_references(&route);

        // Invalid backends should be ignored
        assert!(result.is_empty());
    }

    // Tests for CommonRouteHandler::on_new_or_changed
    use gateway_api::httproutes::HTTPRoute;
    use tokio::sync::mpsc;

    use super::CommonRouteHandler;
    use crate::{common::ReferenceValidateRequest, services::patchers::Operation, state::State};

    fn create_test_http_route(name: &str, namespace: &str) -> HTTPRoute {
        let yaml = format!(
            r#"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: {name}
  namespace: {namespace}
spec:
  parentRefs:
  - name: test-gateway
    namespace: {namespace}
  hostnames:
  - example.com
  rules:
  - backendRefs:
    - name: test-service
      port: 8080
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[tokio::test]
    async fn test_on_new_or_changed_with_unknown_gateway() {
        let state = State::new();
        let (route_patcher_sender, mut route_patcher_receiver) = mpsc::channel::<Operation<HTTPRoute>>(10);
        let (references_validator_sender, mut references_validator_receiver) = mpsc::channel::<ReferenceValidateRequest>(10);

        let http_route = create_test_http_route("test-route", "default");
        let resource_key = ResourceKey::namespaced("test-route", "default");

        let handler = CommonRouteHandler::builder()
            .state(state.clone())
            .resource_key(resource_key.clone())
            .controller_name("test-controller".to_owned())
            .resource(Arc::new(http_route))
            .route_patcher_sender(route_patcher_sender)
            .references_validator_sender(references_validator_sender)
            .version(Some("v1".to_owned()))
            .build();

        let parent_refs = vec![create_test_parent_reference("test-gateway", Some("default"))];
        let mut saved_status = None;

        let result = handler
            .on_new_or_changed(resource_key.clone(), &parent_refs, Some(1), |_state, status| {
                saved_status = status;
            })
            .await;

        // Should succeed even with unknown gateway
        assert!(result.is_ok());

        // Should have generated status for unknown gateway
        assert!(saved_status.is_some());
        let status = saved_status.unwrap();
        assert_eq!(status.parents.len(), 1);
        assert_eq!(status.parents[0].parent_ref.name, "test-gateway");

        // Should have sent finalizer operation
        let operation = route_patcher_receiver.try_recv();
        assert!(operation.is_ok());

        // Should have sent reference validation request
        let request = references_validator_receiver.try_recv();
        assert!(request.is_ok());
    }

    #[tokio::test]
    async fn test_on_new_or_changed_with_empty_parent_refs() {
        let state = State::new();
        let (route_patcher_sender, mut route_patcher_receiver) = mpsc::channel::<Operation<HTTPRoute>>(10);
        let (references_validator_sender, mut references_validator_receiver) = mpsc::channel::<ReferenceValidateRequest>(10);

        let http_route = create_test_http_route("test-route", "default");
        let resource_key = ResourceKey::namespaced("test-route", "default");

        let handler = CommonRouteHandler::builder()
            .state(state.clone())
            .resource_key(resource_key.clone())
            .controller_name("test-controller".to_owned())
            .resource(Arc::new(http_route))
            .route_patcher_sender(route_patcher_sender)
            .references_validator_sender(references_validator_sender)
            .version(Some("v1".to_owned()))
            .build();

        let parent_refs: Vec<ParentReference> = vec![];
        let mut saved_status = None;

        let result = handler
            .on_new_or_changed(resource_key.clone(), &parent_refs, Some(1), |_state, status| {
                saved_status = status;
            })
            .await;

        // Should succeed with empty parent refs
        assert!(result.is_ok());

        // Should have empty status
        assert!(saved_status.is_some());
        let status = saved_status.unwrap();
        assert!(status.parents.is_empty());

        // Should have sent finalizer operation
        let operation = route_patcher_receiver.try_recv();
        assert!(operation.is_ok());

        // Should have sent reference validation request
        let request = references_validator_receiver.try_recv();
        assert!(request.is_ok());
    }

    #[tokio::test]
    async fn test_on_new_or_changed_with_multiple_unknown_gateways() {
        let state = State::new();
        let (route_patcher_sender, _route_patcher_receiver) = mpsc::channel::<Operation<HTTPRoute>>(10);
        let (references_validator_sender, _references_validator_receiver) = mpsc::channel::<ReferenceValidateRequest>(10);

        let http_route = create_test_http_route("test-route", "default");
        let resource_key = ResourceKey::namespaced("test-route", "default");

        let handler = CommonRouteHandler::builder()
            .state(state.clone())
            .resource_key(resource_key.clone())
            .controller_name("test-controller".to_owned())
            .resource(Arc::new(http_route))
            .route_patcher_sender(route_patcher_sender)
            .references_validator_sender(references_validator_sender)
            .version(Some("v1".to_owned()))
            .build();

        let parent_refs =
            vec![create_test_parent_reference("gateway-1", Some("default")), create_test_parent_reference("gateway-2", Some("default"))];
        let mut saved_status = None;

        let result = handler
            .on_new_or_changed(resource_key.clone(), &parent_refs, Some(1), |_state, status| {
                saved_status = status;
            })
            .await;

        assert!(result.is_ok());

        // Should have generated status for both unknown gateways
        assert!(saved_status.is_some());
        let status = saved_status.unwrap();
        assert_eq!(status.parents.len(), 2);
    }
}
