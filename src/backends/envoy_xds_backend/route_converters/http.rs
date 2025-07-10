use std::collections::HashMap;

use envoy_api_rs::{
    envoy::{
        config::{
            core::v3::{
                grpc_service::{EnvoyGrpc, TargetSpecifier},
                GrpcService,
            },
            route::v3::{
                redirect_action,
                route::Action,
                route_action::{self, ClusterSpecifier},
                route_match::PathSpecifier,
                RedirectAction, Route as EnvoyRoute, RouteAction, RouteMatch, WeightedCluster,
            },
        },
        extensions::filters::http::ext_proc::v3::{
            ext_proc_per_route::Override,
            processing_mode::{BodySendMode, HeaderSendMode},
            ExtProcOverrides, ExtProcPerRoute, ProcessingMode,
        },
        r#type::matcher::v3::RegexMatcher,
    },
    google::protobuf::BoolValue,
};
use gateway_api::httproutes;
use tracing::{debug, warn};

use crate::{
    backends::common::{converters, envoy_route_name, get_inference_extension_configurations, inference_cluster_name, INFERENCE_EXT_PROC_FILTER_NAME},
    common::HTTPEffectiveRoutingRule,
};

impl HTTPEffectiveRoutingRule {
    pub fn add_inference_filters(self, mut envoy_route: EnvoyRoute) -> EnvoyRoute {
        let inference_extension_configuration = get_inference_extension_configurations(&self.backends);
        if inference_extension_configuration.len() > 1 {
            warn!("Multiple external processing filter configuration per route {:?} ", self);
        }

        let per_route_filters = inference_extension_configuration
            .first()
            .and_then(|conf| {
                warn!("Inference Pool: setting up external service with {conf:?}");
                conf.inference_config
                    .as_ref()
                    .map(|_conf| {
                        let inference_cluster_name = inference_cluster_name(&envoy_route);
                        let ext_proc_route = ExtProcPerRoute {
                            r#override: Some(Override::Overrides(ExtProcOverrides {
                                processing_mode: Some(ProcessingMode {
                                    request_header_mode: HeaderSendMode::Send.into(),
                                    request_body_mode: BodySendMode::FullDuplexStreamed.into(),
                                    //                                    request_body_mode: BodySendMode::Buffered.into(),
                                    ..Default::default()
                                }),

                                grpc_service: Some(GrpcService {
                                    target_specifier: Some(TargetSpecifier::EnvoyGrpc(EnvoyGrpc {
                                        cluster_name: inference_cluster_name,
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                }),
                                failure_mode_allow: Some(BoolValue { value: false }),
                                ..Default::default()
                            })),
                        };
                        let mut per_route_filters = HashMap::new();
                        per_route_filters.insert(
                            INFERENCE_EXT_PROC_FILTER_NAME.to_owned(),
                            converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExtProcPerRoute".to_owned(), &ext_proc_route)),
                        );
                        per_route_filters
                    })
                    .or_else(|| {
                        warn!("Inference Pool: inference config not set {:?}", conf);
                        None
                    })
            })
            .unwrap_or_default();

        envoy_route.typed_per_filter_config = per_route_filters;
        envoy_route
    }
}

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

        debug!("Headers to match {:?}", &effective_routing_rule.route_matcher.headers);
        let headers = super::create_header_matchers(effective_routing_rule.route_matcher.headers.clone());

        let route_match = RouteMatch {
            path_specifier,
            grpc: None,
            headers,
            ..Default::default()
        };

        let request_filter_headers = effective_routing_rule.request_headers.clone();

        let request_headers_to_add = super::headers_to_add(request_filter_headers.add, request_filter_headers.set);
        let request_headers_to_remove = request_filter_headers.remove;

        let service_cluster_names: Vec<_> = super::create_cluster_weights(effective_routing_rule.backends.iter().filter_map(|b| match b.backend_type() {
            crate::common::BackendType::Service(service_type_config) | crate::common::BackendType::Invalid(service_type_config) => Some(service_type_config),
            crate::common::BackendType::InferencePool(_) => None,
        }));

        let inference_cluster_names: Vec<_> = super::create_cluster_weights(effective_routing_rule.backends.iter().filter_map(|b| match b.backend_type() {
            crate::common::BackendType::InferencePool(inference_type_config) => Some(inference_type_config),
            _ => None,
        }));

        let cluster_action = RouteAction {
            cluster_not_found_response_code: route_action::ClusterNotFoundResponseCode::InternalServerError.into(),
            cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster {
                clusters: service_cluster_names.into_iter().chain(inference_cluster_names).collect(),
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

        let route = EnvoyRoute {
            name: envoy_route_name(&effective_routing_rule),
            r#match: Some(route_match),
            request_headers_to_add,
            request_headers_to_remove,
            action: Some(action),
            ..Default::default()
        };
        effective_routing_rule.add_inference_filters(route)
    }
}
