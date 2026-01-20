use std::collections::{BTreeSet, HashMap};

use gateway_api::gateways::Gateway;
use gateway_api_inference_extension::inferencepools::{InferencePool, InferencePoolSpec};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::{Api, Client, api::ListParams};
use kube_core::{Expression, Selector, object::HasSpec};
use kubvernor_common::ResourceKey;
use kubvernor_state::State;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use typed_builder::TypedBuilder;

use super::BackendReferenceResolver;
use crate::{
    common::{
        self, Backend, BackendType, BackendTypeConfig, InferencePoolConfig, KUBERNETES_NONE, NotResolvedReason, ReferenceGrantRef,
        ReferenceGrantsResolver, ResolutionStatus, Route, RouteToListenersMapping,
    },
    controllers::{
        inference_pool::{self},
        utils::{self, RouteListenerMatcher},
    },
    services::patchers::{Operation, PatchContext},
};

#[derive(TypedBuilder)]
pub struct RouteResolver<'a> {
    client: Client,
    gateway_resource_key: &'a ResourceKey,
    route: common::Route,
    backend_reference_resolver: BackendReferenceResolver,
    reference_grants_resolver: ReferenceGrantsResolver,
    inference_pool_patcher_sender: mpsc::Sender<Operation<InferencePool>>,
    controller_name: String,
}
struct PermittedBackends(String);
impl PermittedBackends {
    fn is_permitted(&self, route_namespace: &str, backend_namespace: &str) -> bool {
        self.0 != route_namespace || backend_namespace == self.0
    }
}

impl RouteResolver<'_> {
    pub async fn resolve(self) -> common::Route {
        let mut route = self.route.clone();
        let route_resource_key = &route.resource_key().clone();
        let route_config = route.config_mut();
        let mut route_resolution_status = ResolutionStatus::Resolved;

        match &mut route_config.route_type {
            common::RouteType::Http(httprouting_configuration) => {
                for rule in &mut httprouting_configuration.routing_rules {
                    let (new_service_backends, resolution_status) = self.process_backends(route_resource_key, rule.backends.clone()).await;
                    if resolution_status != ResolutionStatus::Resolved {
                        route_resolution_status = resolution_status;
                    }
                    let (new_filter_backends, resolution_status) =
                        self.process_backends(route_resource_key, rule.filter_backends.clone()).await;
                    if resolution_status != ResolutionStatus::Resolved {
                        route_resolution_status = resolution_status;
                    }

                    rule.backends.clone_from(&new_service_backends);
                    rule.filter_backends.clone_from(&new_filter_backends);
                }
            },
            common::RouteType::Grpc(grpcrouting_configuration) => {
                for rule in &mut grpcrouting_configuration.routing_rules {
                    let (new_backends, resolution_status) = self.process_backends(route_resource_key, rule.backends.clone()).await;
                    if resolution_status != ResolutionStatus::Resolved {
                        route_resolution_status = resolution_status;
                    }
                    rule.backends.clone_from(&new_backends);
                }
            },
        }

        route_config.resolution_status = route_resolution_status;
        route
    }

    fn backend_remap_port(port: i32, service: Service) -> i32 {
        if let Some(spec) = service.spec {
            if let (_, Some(cluster_ip)) = (spec.selector, spec.cluster_ip)
                && cluster_ip != KUBERNETES_NONE
            {
                return port;
            }

            debug!("Spec cluster IP is NOT set ... remapping ports");
            for service_port in spec.ports.unwrap_or_default() {
                if service_port.port == port {
                    return service_port.target_port.map_or(port, |target_port| match target_port {
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port_value) => port_value,
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(port_name) => {
                            warn!("Port names are not supported {port_name}");
                            port
                        },
                    });
                }
            }
        }
        port
    }

    async fn process_backends(&self, route_resource_key: &ResourceKey, backends: Vec<Backend>) -> (Vec<Backend>, ResolutionStatus) {
        let mut new_backends = vec![];
        let mut route_resolution_status = ResolutionStatus::Resolved;
        let (service_backends, inference_pool_backends): (Vec<_>, Vec<_>) = backends.into_iter().partition(|b| match b.backend_type() {
            BackendType::Service(_) | BackendType::Invalid(_) => true,
            BackendType::InferencePool(_) => false,
        });

        for backend in service_backends {
            let (new_backend, resolution_status) = self.process_service_backend(route_resource_key, backend).await;
            if resolution_status != ResolutionStatus::Resolved {
                route_resolution_status = resolution_status;
            }
            new_backends.push(new_backend);
        }

        for backend in inference_pool_backends {
            let (new_backend, resolution_status) = self.process_inference_pool_backend(route_resource_key, backend).await;
            if resolution_status != ResolutionStatus::Resolved {
                route_resolution_status = resolution_status;
            }
            new_backends.push(new_backend);
        }
        (new_backends, route_resolution_status)
    }

    async fn process_service_backend(&self, route_resource_key: &ResourceKey, backend: Backend) -> (Backend, ResolutionStatus) {
        let gateway_resource_key = self.gateway_resource_key;
        let gateway_namespace = &self.gateway_resource_key.namespace;
        let route_namespace = &route_resource_key.namespace;
        match backend {
            Backend::Maybe(BackendType::Service(mut backend_config) | BackendType::Invalid(mut backend_config)) => {
                let reference_grant_allowed = self
                    .reference_grants_resolver
                    .is_allowed(route_resource_key, &backend_config.resource_key(), gateway_resource_key)
                    .await;

                let grant_ref = ReferenceGrantRef::builder()
                    .namespace(backend_config.resource_key().namespace)
                    .from(route_resource_key.into())
                    .to((&backend_config.resource_key()).into())
                    .gateway_key(gateway_resource_key.clone())
                    .build();

                info!("Allowed because of reference grant {grant_ref:?} {reference_grant_allowed}");

                let backend_resource_key = backend_config.resource_key();
                let backend_namespace = &backend_resource_key.namespace;
                if reference_grant_allowed
                    || PermittedBackends(gateway_namespace.to_owned()).is_permitted(route_namespace, backend_namespace)
                {
                    let maybe_service = self.backend_reference_resolver.get_service_reference(&backend_resource_key).await;

                    if let Some(service) = maybe_service {
                        backend_config.effective_port = Self::backend_remap_port(backend_config.port, service);
                        (Backend::Resolved(BackendType::Service(backend_config)), ResolutionStatus::Resolved)
                    } else {
                        debug!("can't resolve {:?} {:?}", &backend_resource_key, maybe_service);
                        (
                            Backend::Unresolved(BackendType::Service(backend_config)),
                            ResolutionStatus::NotResolved(NotResolvedReason::BackendNotFound),
                        )
                    }
                } else {
                    debug!(
                        "Backend is not permitted gateway namespace is {} route namespace is {} backend namespace is {}",
                        gateway_namespace, &route_namespace, &backend_namespace,
                    );
                    (
                        Backend::NotAllowed(BackendType::Service(backend_config)),
                        ResolutionStatus::NotResolved(NotResolvedReason::RefNotPermitted),
                    )
                }
            },
            Backend::Unresolved(_) | Backend::NotAllowed(_) | Backend::Invalid(_) => {
                debug!("Skipping unresolved backend {:?}", backend);
                (backend, ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend))
            },
            Backend::Maybe(_) => {
                warn!("This should not be processed here {:?}", backend);
                (backend, ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend))
            },
            Backend::Resolved(_) => {
                debug!("Skipping resolved/unupported backend {:?}", backend);
                (backend, ResolutionStatus::Resolved)
            },
        }
    }

    async fn get_inference_extension(client: Client, namespace: &str, inference_pool_spec: &InferencePoolSpec) -> bool {
        let service_api: Api<Service> = Api::namespaced(client, namespace);
        if let Ok(service) = service_api.get(&inference_pool_spec.endpoint_picker_ref.name).await {
            debug!("Inference Pool: Endpoint picker found with IP {:?} {:?} ", service.metadata.name, service.spec.map(|s| s.cluster_ips));
            true
        } else {
            debug!("Inference Pool: Can't get endpoint picker service {:?} ", inference_pool_spec.endpoint_picker_ref);
            false
        }
    }

    async fn get_model_endpoints(client: Client, namespace: &str, inference_pool_spec: &InferencePoolSpec) -> Vec<String> {
        let mut selector: Selector = Selector::default();

        for (k, v) in &inference_pool_spec.selector.match_labels {
            selector.extend(Expression::Equal(k.to_owned(), v.to_owned()));
        }

        let list_params = ListParams::default().labels_from(&selector);
        let api: Api<Pod> = Api::namespaced(client, namespace);

        if let Ok(pods) = api.list(&list_params).await {
            pods.iter().filter_map(|pod| pod.status.as_ref()).filter_map(|status| status.pod_ip.as_ref()).cloned().collect()
        } else {
            vec![]
        }
    }

    async fn process_inference_pool_backend(&self, route_resource_key: &ResourceKey, backend: Backend) -> (Backend, ResolutionStatus) {
        let gateway_namespace = &self.gateway_resource_key.namespace;
        let route_namespace = &route_resource_key.namespace;
        match backend {
            Backend::Maybe(BackendType::InferencePool(mut backend_config)) => {
                let backend_resource_key = backend_config.resource_key();
                let backend_namespace = &backend_resource_key.namespace;
                if PermittedBackends(gateway_namespace.to_owned()).is_permitted(route_namespace, backend_namespace) {
                    let maybe_inference_pool: Option<gateway_api_inference_extension::inferencepools::InferencePool> =
                        self.backend_reference_resolver.get_inference_pool_reference(&backend_resource_key).await;

                    if let Some(inference_pool) = maybe_inference_pool {
                        let inference_pool_spec = inference_pool.spec();
                        let model_endpoints =
                            Self::get_model_endpoints(self.client.clone(), &route_resource_key.namespace, inference_pool_spec).await;

                        debug!("Inference Pool: got model endpoints {:?}", model_endpoints);

                        let resolved_endpoint_picker = match inference_pool_spec.endpoint_picker_ref.kind.as_ref() {
                            None => {
                                Self::get_inference_extension(self.client.clone(), &route_resource_key.namespace, inference_pool_spec).await
                            },
                            Some(val) if val == "Service" => {
                                Self::get_inference_extension(self.client.clone(), &route_resource_key.namespace, inference_pool_spec).await
                            },
                            _ => false,
                        };

                        backend_config.inference_config = Some(InferencePoolConfig(inference_pool_spec.clone()));
                        backend_config.target_ports = inference_pool_spec.target_ports.iter().map(|p| p.number).collect();
                        backend_config.endpoints = Some(model_endpoints);

                        debug!("Inference Pool: Setting backend config {backend_resource_key:?} {inference_pool_spec:?}",);

                        if inference_pool.metadata.name.is_some() {
                            let mut inference_pool = inference_pool::update_inference_pool_parents(
                                &self.controller_name,
                                self.gateway_resource_key,
                                inference_pool,
                                resolved_endpoint_picker,
                            );
                            inference_pool.metadata.managed_fields = None;
                            let (sender, receiver) = oneshot::channel();
                            let _ = self
                                .inference_pool_patcher_sender
                                .send(Operation::PatchStatus(PatchContext {
                                    resource_key: ResourceKey::from(&inference_pool),
                                    resource: inference_pool,
                                    controller_name: self.controller_name.clone(),
                                    response_sender: sender,
                                }))
                                .await;

                            if let Ok(Ok(_)) = receiver.await {
                                (Backend::Resolved(BackendType::InferencePool(backend_config)), ResolutionStatus::Resolved)
                            } else {
                                warn!("Inference Pool: Can't update the status");
                                (
                                    Backend::Unresolved(BackendType::InferencePool(backend_config)),
                                    ResolutionStatus::NotResolved(NotResolvedReason::BackendNotFound),
                                )
                            }
                        } else {
                            info!("Inference Pool: No name");
                            (
                                Backend::Unresolved(BackendType::InferencePool(backend_config)),
                                ResolutionStatus::NotResolved(NotResolvedReason::BackendNotFound),
                            )
                        }
                    } else {
                        info!("Inference Pool: Backend can't resolve {backend_resource_key:?} {maybe_inference_pool:?}",);
                        (
                            Backend::Unresolved(BackendType::InferencePool(backend_config)),
                            ResolutionStatus::NotResolved(NotResolvedReason::BackendNotFound),
                        )
                    }
                } else {
                    debug!(
                        "Inference Backend is not permitted gateway namespace is {} route namespace is {} backend namespace is {}",
                        gateway_namespace, &route_namespace, &backend_namespace,
                    );
                    (
                        Backend::NotAllowed(BackendType::InferencePool(backend_config)),
                        ResolutionStatus::NotResolved(NotResolvedReason::RefNotPermitted),
                    )
                }
            },
            Backend::Unresolved(_) | Backend::NotAllowed(_) | Backend::Invalid(_) => {
                debug!("Skipping unresolved backend {:?}", backend);
                (backend, ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend))
            },
            Backend::Maybe(_) => {
                warn!("This should not be processed here {:?}", backend);
                (backend, ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend))
            },
            Backend::Resolved(_) => {
                debug!("Skipping resolved/unupported backend {:?}", backend);
                (backend, ResolutionStatus::Resolved)
            },
        }
    }
}

#[derive(TypedBuilder)]
pub struct RoutesResolver<'a> {
    gateway: common::Gateway,
    state: &'a State,
    kube_gateway: &'a Gateway,
    client: Client,
    backend_reference_resolver: &'a BackendReferenceResolver,
    reference_grants_resolver: &'a ReferenceGrantsResolver,
    inference_pool_patcher_sender: mpsc::Sender<Operation<InferencePool>>,
    controller_name: String,
}

impl RoutesResolver<'_> {
    pub async fn validate(mut self) -> common::Gateway {
        debug!("Validating routes");
        let gateway_resource_key = self.gateway.key();
        let linked_routes = utils::find_linked_routes(self.state, gateway_resource_key);
        let linked_routes = linked_routes.into_iter().map(|route| {
            RouteResolver::builder()
                .gateway_resource_key(gateway_resource_key)
                .route(route)
                .backend_reference_resolver(self.backend_reference_resolver.clone())
                .reference_grants_resolver(self.reference_grants_resolver.clone())
                .client(self.client.clone())
                .inference_pool_patcher_sender(self.inference_pool_patcher_sender.clone())
                .controller_name(self.controller_name.clone())
                .build()
                .resolve()
        });
        let linked_routes = futures::future::join_all(linked_routes).await;

        let resolved_namespaces = utils::resolve_namespaces(self.client).await;

        info!("Linked routes {:?}", linked_routes.iter().map(Route::resource_key));

        let (route_to_listeners_mapping, routes_with_no_listeners) =
            RouteListenerMatcher::new(self.kube_gateway, linked_routes, resolved_namespaces).filter_matching_routes();
        let per_listener_calculated_attached_routes = calculate_attached_routes(&route_to_listeners_mapping);
        let routes_with_no_listeners = BTreeSet::from_iter(routes_with_no_listeners);
        for (k, routes) in per_listener_calculated_attached_routes {
            if let Some(listener) = self.gateway.listener_mut(&k) {
                let (resolved, unresolved): (BTreeSet<_>, BTreeSet<_>) =
                    routes.iter().map(|r| (**r).clone()).partition(|f| *f.resolution_status() == ResolutionStatus::Resolved);
                listener.update_routes(resolved, unresolved);
            }
        }
        *self.gateway.orphaned_routes_mut() = routes_with_no_listeners;

        self.gateway
    }
}

pub fn calculate_attached_routes(mapped_routes: &[RouteToListenersMapping]) -> HashMap<String, BTreeSet<&Route>> {
    let mut attached_routes: HashMap<String, BTreeSet<&Route>> = HashMap::new();

    for mapping in mapped_routes {
        mapping.listeners.iter().for_each(|l| {
            if let Some(routes) = attached_routes.get_mut(&l.name) {
                routes.insert(&mapping.route);
            } else {
                let mut routes = BTreeSet::new();
                routes.insert(&mapping.route);
                attached_routes.insert(l.name.clone(), routes);
            }
        });
    }
    attached_routes
}
