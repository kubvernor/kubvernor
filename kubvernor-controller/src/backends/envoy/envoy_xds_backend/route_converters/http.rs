// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use std::collections::HashMap;

use envoy_api_rs::{
    envoy::{
        config::{
            core::v3::{
                GrpcService, RuntimeFractionalPercent,
                grpc_service::{EnvoyGrpc, TargetSpecifier},
            },
            route::v3::{
                RedirectAction, Route as EnvoyRoute, RouteAction, RouteMatch, WeightedCluster,
                redirect_action::{self, PathRewriteSpecifier, SchemeRewriteSpecifier},
                route::Action,
                route_action::{self, ClusterSpecifier, HostRewriteSpecifier, RequestMirrorPolicy},
                route_match::PathSpecifier,
            },
        },
        extensions::filters::http::ext_proc::v3::{
            ExtProcOverrides, ExtProcPerRoute, ProcessingMode,
            ext_proc_per_route::Override,
            processing_mode::{BodySendMode, HeaderSendMode},
        },
        r#type::{
            matcher::v3::RegexMatcher,
            v3::{FractionalPercent, fractional_percent},
        },
    },
    google::protobuf::BoolValue,
};
use gateway_api::{common::RequestRedirectScheme, httproutes};
use gateway_api_inference_extension::inferencepools::InferencePoolEndpointPickerRefFailureMode;
use kubvernor_common::ResourceKey;
use log::{debug, info, warn};

use crate::{
    backends::envoy::{
        common::{
            HTTPEffectiveRoutingRule, INFERENCE_EXT_PROC_FILTER_NAME, converters, envoy_route_name, get_inference_extension_configurations,
            inference_cluster_name,
        },
        envoy_xds_backend::route_converters::ServiceClusterWeight,
    },
    common::{DEFAULT_NAMESPACE_NAME, InferencePoolTypeConfig},
};

const TARGET: &str = super::super::super::TARGET;

impl HTTPEffectiveRoutingRule {
    pub fn add_inference_filters(
        mut envoy_route: EnvoyRoute,
        inference_pool_config: &InferencePoolTypeConfig,
        _percentage: u32,
    ) -> EnvoyRoute {
        let inference_cluster_name = inference_cluster_name(&envoy_route, &inference_pool_config.resource_key);
        debug!(target: TARGET,"Inference Pool: setting up external service with {inference_pool_config:?} cluster name {inference_cluster_name}");

        let per_route_filters = inference_pool_config.inference_config.as_ref().map(|conf| {
            let ext_proc_route = ExtProcPerRoute {
                r#override: Some(Override::Overrides(ExtProcOverrides {
                    processing_mode: Some(ProcessingMode {
                        request_header_mode: HeaderSendMode::Send.into(),
                        request_body_mode: BodySendMode::FullDuplexStreamed.into(),
                        response_header_mode: HeaderSendMode::Skip.into(),
                        response_body_mode: BodySendMode::None.into(),
                        request_trailer_mode: HeaderSendMode::Skip.into(),
                        response_trailer_mode: HeaderSendMode::Skip.into(),
                    }),

                    grpc_service: Some(GrpcService {
                        target_specifier: Some(TargetSpecifier::EnvoyGrpc(EnvoyGrpc {
                            cluster_name: inference_cluster_name.clone(),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }),
                    failure_mode_allow: match conf.extension_ref().failure_mode.as_ref() {
                        Some(InferencePoolEndpointPickerRefFailureMode::FailOpen) => Some(BoolValue { value: true }),
                        Some(InferencePoolEndpointPickerRefFailureMode::FailClose) | None => Some(BoolValue { value: false }),
                    },
                    ..Default::default()
                })),
            };
            (
                INFERENCE_EXT_PROC_FILTER_NAME.to_owned(),
                converters::AnyTypeConverter::from((
                    "type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExtProcPerRoute".to_owned(),
                    &ext_proc_route,
                )),
            )
        });

        envoy_route.typed_per_filter_config = per_route_filters.map(|t| HashMap::from([t])).unwrap_or_default();
        envoy_route
    }
}

impl From<HTTPEffectiveRoutingRule> for Vec<EnvoyRoute> {
    #[allow(clippy::cast_possible_truncation)]
    fn from(effective_routing_rule: HTTPEffectiveRoutingRule) -> Self {
        let inference_extension_configuration: Vec<InferencePoolTypeConfig> =
            get_inference_extension_configurations(&effective_routing_rule.backends);

        let envoy_route = EnvoyRoute::from(effective_routing_rule);

        if inference_extension_configuration.is_empty() {
            vec![envoy_route]
        } else {
            let total_weight = inference_extension_configuration.iter().fold(0, |acc, x| acc + x.weight);
            let mut new_routes = vec![];
            for i in inference_extension_configuration {
                let envoy_route_with_inference = envoy_route.clone();
                let percentage = ((f64::from(i.weight) / f64::from(total_weight)) * 100.0) as u32;
                debug!(target: TARGET,"Configuring Envoy Route for Inference Backend with route match runtime fraction {percentage}");
                new_routes.push(HTTPEffectiveRoutingRule::add_inference_filters(envoy_route_with_inference, &i, percentage));
            }
            new_routes
        }
    }
}

impl From<HTTPEffectiveRoutingRule> for EnvoyRoute {
    #[allow(clippy::cast_possible_truncation)]
    fn from(effective_routing_rule: HTTPEffectiveRoutingRule) -> Self {
        let path_specifier = effective_routing_rule.route_matcher.path.clone().and_then(|matcher| {
            let value = matcher.value.clone().map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v });
            matcher.r#type.map(|t| match t {
                httproutes::HttpRouteRulesMatchesPathType::Exact => PathSpecifier::Path(value),
                httproutes::HttpRouteRulesMatchesPathType::PathPrefix => {
                    if let Some(val) = matcher.value {
                        if val == "/" { PathSpecifier::Prefix(value) } else { PathSpecifier::PathSeparatedPrefix(value) }
                    } else {
                        PathSpecifier::Prefix(value)
                    }
                },
                httproutes::HttpRouteRulesMatchesPathType::RegularExpression => {
                    PathSpecifier::SafeRegex(RegexMatcher { regex: value, ..Default::default() })
                },
            })
        });

        let path_specifier = if path_specifier.is_none() { Some(PathSpecifier::Path("/".to_owned())) } else { path_specifier };

        debug!(target: TARGET,"Headers to match {:?}", &effective_routing_rule.route_matcher.headers);
        let headers = super::create_header_matchers(effective_routing_rule.route_matcher.headers.clone());

        let route_match = RouteMatch { path_specifier, grpc: None, headers, ..Default::default() };

        let request_filter_headers = effective_routing_rule.request_headers.clone();
        let request_headers_to_add = super::headers_to_add(request_filter_headers.add, request_filter_headers.set);
        let request_headers_to_remove = request_filter_headers.remove;

        let response_filter_headers = effective_routing_rule.response_headers.clone();
        let response_headers_to_add = super::headers_to_add(response_filter_headers.add, response_filter_headers.set);
        let response_headers_to_remove = response_filter_headers.remove;

        let service_cluster_names: Vec<_> =
            super::create_cluster_weights(effective_routing_rule.backends.iter().map(|b| match b.backend_type() {
                crate::common::BackendType::Service(service_type_config) | crate::common::BackendType::Invalid(service_type_config) => {
                    ServiceClusterWeight::from(service_type_config)
                },
                crate::common::BackendType::InferencePool(infrence_config) => ServiceClusterWeight::from(infrence_config),
            }));

        let mirror_policy = effective_routing_rule.mirror_filter.as_ref().map(|mirror_filter| {
            let mirror_backends: Vec<String> = effective_routing_rule
                .filter_backends
                .iter()
                .filter_map(|b| match b.backend_type() {
                    crate::common::BackendType::Service(service_type_config) | crate::common::BackendType::Invalid(service_type_config) => {
                        Some(service_type_config)
                    },
                    crate::common::BackendType::InferencePool(_) => None,
                })
                .filter(|config| {
                    debug!(target: TARGET,"Mirror backend {} {:?}", config.resource_key, mirror_filter.backend_ref);
                    config.resource_key == ResourceKey::from((&mirror_filter.backend_ref, DEFAULT_NAMESPACE_NAME.to_owned()))
                })
                .map(crate::common::BackendTypeConfig::cluster_name)
                .collect();

            debug!(target: TARGET,"Mirror backends {mirror_backends:?}");

            let fraction = match (mirror_filter.percent.as_ref(), mirror_filter.fraction.as_ref()) {
                (None, None) => FractionalPercent { numerator: 100, denominator: fractional_percent::DenominatorType::Hundred.into() },
                (None, Some(fraction)) => FractionalPercent {
                    numerator: ((f64::from(fraction.numerator) / f64::from(fraction.denominator.unwrap_or(100))) * 100.0) as u32,
                    denominator: fractional_percent::DenominatorType::Hundred.into(),
                },
                (Some(percent), None) => {
                    FractionalPercent { numerator: *percent as u32, denominator: fractional_percent::DenominatorType::Hundred.into() }
                },
                (Some(_), Some(_)) => {
                    warn!(target: TARGET,"Invalid configuration for mirror filtering ");
                    FractionalPercent { numerator: 100, denominator: fractional_percent::DenominatorType::Hundred.into() }
                },
            };

            mirror_backends
                .into_iter()
                .map(|cluster_name| RequestMirrorPolicy {
                    cluster: cluster_name.clone(),
                    runtime_fraction: Some(RuntimeFractionalPercent { default_value: Some(fraction), ..Default::default() }),
                    ..Default::default()
                })
                .collect()
        });

        info!(target: TARGET,"Mirror policies {:?} {mirror_policy:?}", mirror_policy.as_ref().map(|p: &Vec<_>| p.len()));

        let host_rewrite_specifier = effective_routing_rule
            .rewrite_url_filter
            .as_ref()
            .and_then(|f| f.hostname.as_ref().map(|host| HostRewriteSpecifier::HostRewriteLiteral(host.clone())));
        info!(target: TARGET,"Hostname rewrite options {host_rewrite_specifier:?}");

        let cluster_action = RouteAction {
            cluster_not_found_response_code: route_action::ClusterNotFoundResponseCode::InternalServerError.into(),
            cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster {
                clusters: service_cluster_names.into_iter().collect(),
                ..Default::default()
            })),
            host_rewrite_specifier,
            request_mirror_policies: mirror_policy.unwrap_or_default(),
            ..Default::default()
        };

        let action: Action = if let Some(redirect_action) = effective_routing_rule.redirect_filter.clone().map(|f| RedirectAction {
            host_redirect: f.hostname.unwrap_or_default(),
            scheme_rewrite_specifier: f.scheme.clone().map(|s| match s {
                gateway_api::common::RequestRedirectScheme::Http => SchemeRewriteSpecifier::SchemeRedirect("http".to_owned()),
                gateway_api::common::RequestRedirectScheme::Https => SchemeRewriteSpecifier::HttpsRedirect(true),
            }),
            path_rewrite_specifier: f.path.map(|p| match p.r#type {
                gateway_api::common::RequestOperationType::ReplaceFullPath => {
                    PathRewriteSpecifier::PathRedirect(p.replace_full_path.unwrap_or_default())
                },
                gateway_api::common::RequestOperationType::ReplacePrefixMatch => {
                    PathRewriteSpecifier::PrefixRewrite(p.replace_prefix_match.unwrap_or_default())
                },
            }),

            port_redirect: derive_port(f.port, f.scheme, effective_routing_rule.listener_port),

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
            name: envoy_route_name(&effective_routing_rule),
            r#match: Some(route_match),
            request_headers_to_add,
            request_headers_to_remove,
            response_headers_to_add,
            response_headers_to_remove,

            action: Some(action),
            ..Default::default()
        }
    }
}

fn derive_port(port: Option<i32>, scheme: Option<RequestRedirectScheme>, listener_port: i32) -> u32 {
    if let Some(port) = port {
        port as u32
    } else if let Some(scheme) = scheme {
        match scheme {
            RequestRedirectScheme::Https => 443,
            RequestRedirectScheme::Http => 80,
        }
    } else {
        listener_port as u32
    }
}
