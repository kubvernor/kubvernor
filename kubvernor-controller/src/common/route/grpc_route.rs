use std::net::IpAddr;

use gateway_api::{
    common::{GRPCFilterType, GrpcRouteFilter, HTTPHeader, HeaderModifier},
    grpcroutes::{GRPCRoute, GrpcRouteMatch, GrpcRouteRule},
};
use kube::ResourceExt;

use super::{
    Backend, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME, FilterHeaders, NotResolvedReason, ResolutionStatus, ResourceKey, Route,
    RouteConfig, RouteType, ServiceTypeConfig, get_add_headers, get_remove_headers, get_set_headers,
};
use crate::{common::BackendType, controllers::ControllerError};

impl TryFrom<GRPCRoute> for Route {
    type Error = ControllerError;

    fn try_from(value: GRPCRoute) -> Result<Self, Self::Error> {
        Route::try_from(&value)
    }
}

impl TryFrom<&GRPCRoute> for Route {
    type Error = ControllerError;
    fn try_from(kube_route: &GRPCRoute) -> Result<Self, Self::Error> {
        let key = ResourceKey::from(kube_route);
        let parents = kube_route.spec.parent_refs.clone();
        let local_namespace = key.namespace.clone();

        let empty_rules: Vec<GrpcRouteRule> = vec![];
        let mut has_invalid_backends = false;
        let routing_rules = kube_route.spec.rules.as_ref().unwrap_or(&empty_rules);
        let routing_rules: Vec<GRPCRoutingRule> = routing_rules
            .iter()
            .enumerate()
            .map(|(i, rr)| GRPCRoutingRule {
                name: format!("{}-{i}", kube_route.name_any()),
                matching_rules: rr.matches.clone().unwrap_or_default(),
                filters: rr.filters.clone().unwrap_or_default(),
                backends: rr
                    .backend_refs
                    .as_ref()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|br| {
                        let config = ServiceTypeConfig {
                            resource_key: ResourceKey::from((br, local_namespace.clone())),
                            endpoint: if let Some(namespace) = br.namespace.as_ref() {
                                if *namespace == DEFAULT_NAMESPACE_NAME { br.name.clone() } else { format!("{}.{namespace}", br.name) }
                            } else if local_namespace == DEFAULT_NAMESPACE_NAME {
                                br.name.clone()
                            } else {
                                format!("{}.{local_namespace}", br.name)
                            },
                            port: br.port.unwrap_or(0),
                            effective_port: br.port.unwrap_or(0),
                            weight: br.weight.unwrap_or(1),
                        };

                        if br.kind.is_none() || br.kind == Some("Service".to_owned()) {
                            Backend::Maybe(BackendType::Service(config))
                        } else {
                            has_invalid_backends = true;
                            Backend::Invalid(BackendType::Invalid(config))
                        }
                    })
                    .collect(),
            })
            .collect();
        let hostnames = kube_route
            .spec
            .hostnames
            .as_ref()
            .map(|hostnames| hostnames.iter().filter(|hostname| hostname.parse::<IpAddr>().is_err()).cloned().collect::<Vec<_>>())
            .unwrap_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()]);

        let config = RouteConfig {
            resource_key: key,
            parents,
            hostnames,
            resolution_status: if has_invalid_backends {
                ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend)
            } else {
                ResolutionStatus::NotResolved(NotResolvedReason::Unknown)
            },
            route_type: RouteType::Grpc(GRPCRoutingConfiguration {
                routing_rules,
                //effective_routing_rules,
            }),
        };

        Ok(Route { config })
    }
}

#[derive(Clone, Debug)]
pub struct GRPCRoutingConfiguration {
    pub routing_rules: Vec<GRPCRoutingRule>,
    //pub effective_routing_rules: Vec<GRPCEffectiveRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct GRPCRoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
    pub matching_rules: Vec<GrpcRouteMatch>,
    pub filters: Vec<GrpcRouteFilter>,
}

impl GRPCRoutingRule {
    pub fn filter_headers(&self) -> FilterHeaders {
        fn iter<T: Clone, X>(routing_rule: &GRPCRoutingRule, x: X) -> Vec<T>
        where
            X: Fn(Option<&HeaderModifier>) -> Option<&Vec<T>>,
        {
            routing_rule
                .filters
                .iter()
                .filter(|f| f.r#type == GRPCFilterType::RequestHeaderModifier)
                .filter_map(|f| x(f.request_header_modifier.as_ref()))
                .map(std::iter::IntoIterator::into_iter)
                .flat_map(std::iter::Iterator::collect::<Vec<_>>)
                .cloned()
                .collect()
        }

        FilterHeaders {
            add: iter::<HTTPHeader, _>(self, get_add_headers),
            remove: iter::<String, _>(self, get_remove_headers),
            set: iter::<HTTPHeader, _>(self, get_set_headers),
        }
    }
}
