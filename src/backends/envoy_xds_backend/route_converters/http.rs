use envoy_api_rs::envoy::{
    config::route::v3::{
        redirect_action,
        route::Action,
        route_action::{self, ClusterSpecifier},
        route_match::PathSpecifier,
        RedirectAction, Route as EnvoyRoute, RouteAction, RouteMatch, WeightedCluster,
    },
    r#type::matcher::v3::RegexMatcher,
};
use gateway_api::httproutes;
use tracing::warn;

use crate::common::HTTPEffectiveRoutingRule;

impl From<HTTPEffectiveRoutingRule> for EnvoyRoute {
    fn from(effective_routing_rule: HTTPEffectiveRoutingRule) -> Self {
        let path_specifier = effective_routing_rule.route_matcher.path.clone().and_then(|matcher| {
            let value = matcher.value.clone().map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v });
            matcher.r#type.map(|t| match t {
                httproutes::HTTPRouteRulesMatchesPathType::Exact => PathSpecifier::Path(value),
                httproutes::HTTPRouteRulesMatchesPathType::PathPrefix => {
                    if let Some(val) = matcher.value {
                        if val == "/" {
                            PathSpecifier::Prefix(value)
                        } else {
                            PathSpecifier::PathSeparatedPrefix(value)
                        }
                    } else {
                        PathSpecifier::Prefix(value)
                    }
                }
                httproutes::HTTPRouteRulesMatchesPathType::RegularExpression => PathSpecifier::SafeRegex(RegexMatcher { regex: value, ..Default::default() }),
            })
        });

        let path_specifier = if path_specifier.is_none() { Some(PathSpecifier::Path("/".to_owned())) } else { path_specifier };

        warn!("Headers to match {:?}", effective_routing_rule.route_matcher.headers);
        let headers = super::create_header_matchers(effective_routing_rule.route_matcher.headers);

        let route_match = RouteMatch {
            path_specifier,
            grpc: None,
            headers,
            ..Default::default()
        };

        let request_filter_headers = effective_routing_rule.request_headers;

        let request_headers_to_add = super::headers_to_add(request_filter_headers.add, request_filter_headers.set);
        let request_headers_to_remove = request_filter_headers.remove;

        let cluster_names: Vec<_> = super::create_cluster_weights(&effective_routing_rule.backends);

        let cluster_action = RouteAction {
            cluster_not_found_response_code: route_action::ClusterNotFoundResponseCode::InternalServerError.into(),
            cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster {
                clusters: cluster_names,
                ..Default::default()
            })),
            ..Default::default()
        };

        let action: Action = if let Some(redirect_action) = effective_routing_rule.redirect_filter.clone().map(|f| RedirectAction {
            host_redirect: f.hostname.unwrap_or_default(),

            response_code: match f.status_code {
                Some(302) => redirect_action::RedirectResponseCode::Found.into(),
                Some(303) => redirect_action::RedirectResponseCode::SeeOther.into(),
                Some(307) => redirect_action::RedirectResponseCode::TemporaryRedirect.into(),
                Some(308) => redirect_action::RedirectResponseCode::PermanentRedirect.into(),
                Some(301 | _) | None => redirect_action::RedirectResponseCode::MovedPermanently.into(),
            },
            ..Default::default()
        }) {
            Action::Redirect(redirect_action)
        } else {
            Action::Route(cluster_action)
        };

        EnvoyRoute {
            name: format!("{}-route", effective_routing_rule.name),
            r#match: Some(route_match),
            request_headers_to_add,
            request_headers_to_remove,
            action: Some(action),
            ..Default::default()
        }
    }
}
