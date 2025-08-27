use envoy_api_rs::{
    envoy::{
        config::route::v3::{
            header_matcher::HeaderMatchSpecifier,
            route::Action,
            route_action::{self, ClusterSpecifier},
            route_match::PathSpecifier,
            weighted_cluster::ClusterWeight,
            HeaderMatcher, Route as EnvoyRoute, RouteAction, RouteMatch, WeightedCluster,
        },
        r#type::matcher::v3::{string_matcher::MatchPattern, RegexMatcher, StringMatcher},
    },
    google::protobuf::UInt32Value,
};
use gateway_api::common::{self};
use tracing::warn;

use crate::{backends::common::GRPCEffectiveRoutingRule, common::BackendTypeConfig};

impl From<GRPCEffectiveRoutingRule> for EnvoyRoute {
    fn from(effective_routing_rule: GRPCEffectiveRoutingRule) -> Self {
        warn!("Headers to match {:?}", effective_routing_rule.route_matcher.headers);

        let headers = effective_routing_rule.route_matcher.headers.map_or(vec![], |headers| {
            headers
                .iter()
                .map(|header| HeaderMatcher {
                    name: header.name.clone(),
                    header_match_specifier: Some(HeaderMatchSpecifier::StringMatch(StringMatcher {
                        match_pattern: Some(MatchPattern::Exact(header.value.clone())),
                        ..Default::default()
                    })),
                    ..Default::default()
                })
                .collect()
        });

        let path_specifier = effective_routing_rule.route_matcher.method.clone().and_then(|matcher| {
            let service = matcher.service.clone().map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v });

            let path = if let Some(method) = matcher.method {
                "/".to_owned() + &service + "/" + &method
            } else {
                "/".to_owned() + &service
            };

            matcher.r#type.map(|t| match t {
                common::HeaderMatchType::Exact => PathSpecifier::Path(path),
                common::HeaderMatchType::RegularExpression => PathSpecifier::SafeRegex(RegexMatcher { regex: path, ..Default::default() }),
            })
        });

        let path_specifier = if path_specifier.is_none() { Some(PathSpecifier::Prefix("/".to_owned())) } else { path_specifier };

        let route_match = RouteMatch {
            headers,
            path_specifier,
            grpc: None,
            ..Default::default()
        };

        let request_filter_headers = effective_routing_rule.request_headers;
        let request_headers_to_add = super::headers_to_add(request_filter_headers.add, request_filter_headers.set);
        let request_headers_to_remove = request_filter_headers.remove;

        let cluster_names: Vec<_> = effective_routing_rule
            .backends
            .iter()
            .filter_map(|b| match b.backend_type() {
                crate::common::BackendType::Service(service_type_config) => Some(service_type_config),
                _ => None,
            })
            .filter(|b| b.weight() > 0)
            .map(|b| ClusterWeight {
                name: b.cluster_name(),
                weight: Some(UInt32Value {
                    value: b.weight().try_into().expect("We do expect this to work for time being"),
                }),
                ..Default::default()
            })
            .collect();
        let cluster_action = RouteAction {
            cluster_not_found_response_code: route_action::ClusterNotFoundResponseCode::NotFound.into(),
            cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster {
                clusters: cluster_names,
                ..Default::default()
            })),
            ..Default::default()
        };

        let action: Action = Action::Route(cluster_action);

        EnvoyRoute {
            name: format!("{}-grpc-route", effective_routing_rule.name),
            r#match: Some(route_match),
            request_headers_to_add,
            request_headers_to_remove,
            action: Some(action),
            ..Default::default()
        }
    }
}
