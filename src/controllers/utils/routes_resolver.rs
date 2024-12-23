use std::{collections::BTreeSet, sync::Arc};

use gateway_api::apis::standard::gateways::Gateway;
use k8s_openapi::api::core::v1::Service;
use kube::Client;
use tracing::{debug, warn, Instrument, Span};
use typed_builder::TypedBuilder;

use super::BackendReferenceResolver;
use crate::{
    common::{self, calculate_attached_routes, Backend, NotResolvedReason, ResolutionStatus, KUBERNETES_NONE},
    controllers::utils::{self, RouteListenerMatcher},
    state::State,
};

#[derive(TypedBuilder)]
pub struct RouteResolver<'a> {
    gateway_namespace: &'a str,
    route: common::Route,
    backend_reference_resolver: BackendReferenceResolver,
}
struct PermittedBackends(String);
impl PermittedBackends {
    fn is_permitted(&self, route_namespace: &str, backend_namespace: &str) -> bool {
        self.0 != route_namespace || backend_namespace == self.0
    }
}

impl<'a> RouteResolver<'a> {
    pub async fn resolve(self) -> common::Route {
        let mut route = self.route;
        let route_namespace = route.namespace().to_owned();
        let route_config = route.config_mut();
        let mut route_resolution_status = ResolutionStatus::Resolved;
        for rule in &mut route_config.routing_rules {
            let mut new_backends = vec![];
            for backend in rule.backends.clone() {
                match backend {
                    Backend::Maybe(mut backend_service_config) => {
                        let backend_namespace = &backend_service_config.resource_key.namespace;

                        let maybe_service = self.backend_reference_resolver.get_reference(&backend_service_config.resource_key).await;

                        if let Some(service) = maybe_service {
                            if PermittedBackends(self.gateway_namespace.to_owned()).is_permitted(&route_namespace, backend_namespace) {
                                backend_service_config.effective_port = Self::backend_remap_port(backend_service_config.port, service);
                                new_backends.push(Backend::Resolved(backend_service_config));
                            } else {
                                debug!(
                                    "Backend is not permitted gateway namespace is {} route namespace is {} backend namespace is {}",
                                    self.gateway_namespace, &route_namespace, &backend_namespace,
                                );
                                new_backends.push(Backend::NotAllowed(backend_service_config));
                                route_resolution_status = ResolutionStatus::NotResolved(NotResolvedReason::RefNotPermitted);
                            }
                        } else {
                            debug!(
                                "can't resolve {}-{} {:?}",
                                &backend_service_config.resource_key.name, &backend_service_config.resource_key.namespace, maybe_service
                            );
                            new_backends.push(Backend::Unresolved(backend_service_config));
                            route_resolution_status = ResolutionStatus::NotResolved(NotResolvedReason::BackendNotFound);
                        }
                    }
                    Backend::Unresolved(_) | Backend::NotAllowed(_) | Backend::Invalid(_) => {
                        debug!("Skipping unresolved backend {:?}", backend);
                        route_resolution_status = ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend);
                        new_backends.push(backend);
                    }
                    Backend::Resolved(_) => {
                        debug!("Skipping resolved/unupported backend {:?}", backend);
                        new_backends.push(backend);
                    }
                }
            }
            rule.backends = new_backends;
        }
        route_config.resolution_status = route_resolution_status;
        route
    }

    fn backend_remap_port(port: i32, service: Service) -> i32 {
        if let Some(spec) = service.spec {
            if let (_, Some(cluster_ip)) = (spec.selector, spec.cluster_ip) {
                if cluster_ip != KUBERNETES_NONE {
                    return port;
                }
            }

            debug!("Spec cluster IP is NOT set ... remapping ports");
            for service_port in spec.ports.unwrap_or_default() {
                if service_port.port == port {
                    return service_port.target_port.map_or(port, |target_port| match target_port {
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(port_value) => port_value,
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(port_name) => {
                            warn!("Port names are not supported {port_name}");
                            port
                        }
                    });
                }
            }
        }
        port
    }
}

#[derive(TypedBuilder)]
pub struct RoutesResolver<'a> {
    gateway: common::Gateway,
    state: &'a State,
    kube_gateway: &'a Gateway,
    client: Client,
    backend_reference_resolver: &'a BackendReferenceResolver,
}

impl<'a> RoutesResolver<'a> {
    pub async fn validate(mut self) -> common::Gateway {
        debug!("Validating routes");
        let gateway_resource_key = self.gateway.key();
        let linked_routes = utils::find_linked_routes(self.state, gateway_resource_key);
        let linked_routes = utils::resolve_route_backends(&gateway_resource_key.namespace, self.backend_reference_resolver.clone(), linked_routes)
            .instrument(Span::current().clone())
            .await;
        let resolved_namespaces = utils::resolve_namespaces(self.client).await;

        let (route_to_listeners_mapping, routes_with_no_listeners) = RouteListenerMatcher::new(self.kube_gateway, linked_routes, resolved_namespaces).filter_matching_routes();
        let per_listener_calculated_attached_routes = calculate_attached_routes(&route_to_listeners_mapping);
        let routes_with_no_listeners = BTreeSet::from_iter(routes_with_no_listeners);
        for (k, routes) in per_listener_calculated_attached_routes {
            if let Some(listener) = self.gateway.listener_mut(&k) {
                let (resolved, unresolved): (BTreeSet<_>, BTreeSet<_>) = routes.iter().map(|r| (**r).clone()).partition(|f| *f.resolution_status() == ResolutionStatus::Resolved);
                listener.update_routes(resolved, unresolved);
            }
        }
        *self.gateway.orphaned_routes_mut() = routes_with_no_listeners;

        self.gateway
    }
}
