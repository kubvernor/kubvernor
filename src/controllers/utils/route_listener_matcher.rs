use std::{collections::BTreeMap, sync::Arc};

use crate::{
    common::{NotResolvedReason, ResolutionStatus, ResourceKey, Route, RouteRefKey, RouteToListenersMapping},
    state::State,
};
use gateway_api::apis::standard::{
    gateways::{self, GatewayListeners, GatewayListenersAllowedRoutesNamespaces, GatewayListenersAllowedRoutesNamespacesFrom},
    httproutes::HTTPRouteParentRefs,
};
use tracing::{debug, warn};

use super::HostnameMatchFilter;

pub struct RouteListenerMatcher<'a> {
    gateway: &'a Arc<gateways::Gateway>,
    routes: Vec<Route>,
    resolved_namespaces: BTreeMap<String, BTreeMap<String, String>>,
}

impl<'a> RouteListenerMatcher<'a> {
    pub fn new(gateway: &'a Arc<gateways::Gateway>, routes: Vec<Route>, resolved_namespaces: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        Self { gateway, routes, resolved_namespaces }
    }

    pub fn filter_matching_routes(self) -> (Vec<RouteToListenersMapping>, Vec<Route>) {
        let mut routes_with_no_listeners = vec![];
        (
            self.routes
                .iter()
                .filter_map(|route| {
                    let (listeners, route_resolution_status) = self.filter_matching_route(route.parents(), route);
                    let mut updated_route = route.clone();
                    if let Some(resolution_status) = route_resolution_status {
                        *updated_route.resolution_status_mut() = resolution_status;
                    }

                    if listeners.is_empty() {
                        routes_with_no_listeners.push(updated_route);
                        None
                    } else {
                        Some(RouteToListenersMapping::new(updated_route, listeners))
                    }
                })
                .collect(),
            routes_with_no_listeners,
        )
    }

    fn filter_matching_route(&'a self, route_parents: &Option<Vec<HTTPRouteParentRefs>>, route: &'a Route) -> (Vec<GatewayListeners>, Option<ResolutionStatus>) {
        let route_key = route.resource_key();
        let mut route_resolution_status = None;
        let mut routes_and_listeners: Vec<GatewayListeners> = vec![];
        if let Some(route_parents) = route_parents {
            for route_parent in route_parents {
                let route_parent_key = RouteRefKey::from((route_parent, route_key.namespace.clone()));
                let gateway_key = ResourceKey::from(&**self.gateway);
                if route_parent_key.as_ref().name == gateway_key.name && route_parent_key.as_ref().namespace == gateway_key.namespace {
                    let matching_gateway_listeners = self.filter_listeners_by_namespace(self.gateway.spec.listeners.clone().into_iter(), gateway_key, route_key);
                    let matching_gateway_listeners = matching_gateway_listeners.collect::<Vec<_>>();
                    debug!("Matching listeners {:?}", matching_gateway_listeners);
                    if matching_gateway_listeners.is_empty() {
                        route_resolution_status = Some(ResolutionStatus::NotResolved(NotResolvedReason::NotAllowedByListeners));
                        continue;
                    }

                    let matching_gateway_listeners = Self::filter_listeners_by_hostnames(matching_gateway_listeners.into_iter(), route);
                    let matching_gateway_listeners = matching_gateway_listeners.collect::<Vec<_>>();
                    debug!("Matching listeners {:?}", matching_gateway_listeners);
                    if matching_gateway_listeners.is_empty() {
                        route_resolution_status = Some(ResolutionStatus::NotResolved(NotResolvedReason::NoMatchingListenerHostname));
                        continue;
                    }

                    let matching_gateway_listeners = matching_gateway_listeners.into_iter();
                    let mut matched: Vec<GatewayListeners> = match (route_parent.port, &route_parent.section_name) {
                        (Some(port), Some(section_name)) => {
                            let matched = filter_listeners_by_name_or_port(matching_gateway_listeners, |gl| gl.port == port && gl.name == *section_name);
                            if matched.is_empty() {
                                route_resolution_status = Some(ResolutionStatus::NotResolved(NotResolvedReason::NoMatchingParent));
                            }
                            matched
                        }
                        (Some(port), None) => filter_listeners_by_name_or_port(matching_gateway_listeners, |gl| gl.port == port),
                        (None, Some(section_name)) => {
                            let matched = filter_listeners_by_name_or_port(matching_gateway_listeners, |gl| gl.name == *section_name);
                            if matched.is_empty() {
                                route_resolution_status = Some(ResolutionStatus::NotResolved(NotResolvedReason::NoMatchingParent));
                            }
                            matched
                        }
                        (None, None) => filter_listeners_by_name_or_port(matching_gateway_listeners, |_| true),
                    };
                    debug!("Appending {route_parent:?} {matched:?}");
                    routes_and_listeners.append(&mut matched);
                }
            }
        }

        (routes_and_listeners, route_resolution_status)
    }

    pub fn filter_matching_gateways(state: &mut State, resolved_gateways: &[(&HTTPRouteParentRefs, Option<Arc<gateways::Gateway>>)]) -> Vec<Arc<gateways::Gateway>> {
        resolved_gateways
            .iter()
            .filter_map(|(parent_ref, maybe_gateway)| {
                if let Some(gateway) = maybe_gateway {
                    let gateway_key = ResourceKey::from(&**gateway);
                    let parent_ref_key = RouteRefKey::from((&**parent_ref, gateway_key.namespace.clone()));

                    if *parent_ref_key.as_ref() == gateway_key {
                        state.get_gateway(&gateway_key).cloned()
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    fn filter_listeners_by_hostnames(listeners: impl Iterator<Item = GatewayListeners> + 'a, route: &'a Route) -> impl Iterator<Item = GatewayListeners> + 'a {
        let route_hostnames = route.hostnames();
        listeners.filter(move |listener| {
            debug!("Filtering by hostname {:?} {:?}", &listener.hostname, &route_hostnames);
            if let Some(hostname) = &listener.hostname {
                if hostname.is_empty() {
                    true
                } else {
                    HostnameMatchFilter::new(hostname, route_hostnames).filter()
                }
            } else {
                true
            }
        })
    }
    fn filter_listeners_by_namespace(
        &'a self,
        listeners: impl Iterator<Item = GatewayListeners> + 'a,
        gateway_key: ResourceKey,
        route_key: &'a ResourceKey,
    ) -> impl Iterator<Item = GatewayListeners> + 'a {
        listeners.filter(move |l| {
            let mut is_allowed = true;
            if let Some(allowed_routes) = &l.allowed_routes {
                if let Some(allowed_kinds) = &allowed_routes.kinds {
                    if !allowed_kinds.is_empty() {
                        is_allowed = allowed_kinds.iter().map(|k| &k.kind).any(|f| f == "HTTPRoute");
                    }
                }

                if let Some(GatewayListenersAllowedRoutesNamespaces { from: Some(selector_type), selector }) = &allowed_routes.namespaces {
                    match selector_type {
                        GatewayListenersAllowedRoutesNamespacesFrom::All => {}
                        GatewayListenersAllowedRoutesNamespacesFrom::Selector => {
                            // namespace selector
                            warn!("Selector {selector:?}");
                            is_allowed = false;
                            if let Some(selector) = selector {
                                if let Some(selector_labels) = &selector.match_labels {
                                    let resolved_namespaces = self.resolved_namespaces.get(&route_key.namespace);
                                    warn!("Selector labales {resolved_namespaces:#?}");
                                    if let Some(labels) = resolved_namespaces {
                                        for (selector_k, selector_v) in selector_labels {
                                            if labels.get(selector_k) == Some(selector_v) {
                                                is_allowed = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        GatewayListenersAllowedRoutesNamespacesFrom::Same => {
                            if route_key.namespace != gateway_key.namespace {
                                is_allowed = false;
                            }
                        }
                    }
                }
            }
            is_allowed
        })
    }
}

fn filter_listeners_by_name_or_port<F>(gateway_listeners: impl Iterator<Item = GatewayListeners>, filter: F) -> Vec<GatewayListeners>
where
    F: Fn(&GatewayListeners) -> bool,
{
    gateway_listeners.filter(|f| filter(f)).collect()
}
