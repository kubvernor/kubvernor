use std::sync::Arc;

use gateway_api::apis::standard::gateways::Gateway;
use k8s_openapi::api::core::v1::Service;

use kube::{Api, Client};
use tracing::debug;

use crate::{
    common::{self, calculate_attached_routes, Backend, ResolutionStatus},
    controllers::utils,
    state::State,
};

pub struct RouteResolver<'a> {
    route: common::Route,
    client: Client,
    log_context: &'a str,
}

impl<'a> RouteResolver<'a> {
    pub fn new(route: common::Route, client: Client, log_context: &'a str) -> Self {
        Self { route, client, log_context }
    }

    pub async fn resolve(self) -> common::Route {
        let mut route = self.route;
        let route_config = route.config_mut();
        let mut unresolved_count = 0;
        for rule in &mut route_config.routing_rules {
            let mut new_backends = vec![];
            for backend in &rule.backends {
                let client = self.client.clone();
                let backend_config = backend.config().clone();
                let backend_namespace = backend_config.resource_key.namespace.clone();
                let backend_name = backend_config.resource_key.name.clone();
                let service_api: Api<Service> = Api::namespaced(client, &backend_namespace);
                if (service_api.get(&backend_name).await).is_ok() {
                    new_backends.push(Backend::Resolved(backend_config));
                } else {
                    debug!("{} can't resolve {:?}", self.log_context, &backend_config.resource_key);
                    new_backends.push(Backend::Unresolved(backend_config));
                    unresolved_count += 1;
                }
            }
            rule.backends = new_backends;
        }
        route_config.resolution_status = match unresolved_count {
            0 => common::ResolutionStatus::Resolved,
            _ => common::ResolutionStatus::NotResolved,
        };
        route
    }
}

pub struct RoutesResolver<'a> {
    gateway: common::Gateway,
    client: Client,
    log_context: &'a str,
    state: &'a State,
    kube_gateway: &'a Arc<Gateway>,
}

impl<'a> RoutesResolver<'a> {
    pub fn new(gateway: common::Gateway, client: Client, log_context: &'a str, state: &'a State, kube_gateway: &'a Arc<Gateway>) -> Self {
        Self {
            gateway,
            client,
            log_context,
            state,
            kube_gateway,
        }
    }

    pub async fn validate(mut self) -> common::Gateway {
        let log_context = self.log_context;
        debug!("{log_context} Validating routes");
        let gateway_resource_key = self.gateway.key();
        let linked_routes = utils::find_linked_routes(self.state, gateway_resource_key);
        let linked_routes = utils::resolve_route_backends(self.client.clone(), linked_routes, log_context).await;
        let resolved_namespaces = utils::resolve_namespaces(self.client).await;

        let route_to_listeners_mapping = common::RouteListenerMatcher::new(self.kube_gateway, linked_routes, resolved_namespaces).filter_matching_routes();
        let per_listener_calculated_attached_routes = calculate_attached_routes(&route_to_listeners_mapping);

        for (k, routes) in per_listener_calculated_attached_routes {
            if let Some(listener) = self.gateway.listener_mut(&k) {
                let (resolved, unresolved) = routes.iter().map(|r| (**r).clone()).partition(|f| *f.resolution_status() == ResolutionStatus::Resolved);
                listener.update_routes(resolved, unresolved);
            }
        }

        self.gateway
    }
}
