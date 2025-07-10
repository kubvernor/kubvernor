use envoy_api_rs::{
    envoy::{
        config::{
            core::v3::{header_value_option::HeaderAppendAction, HeaderValue, HeaderValueOption},
            route::v3::{header_matcher::HeaderMatchSpecifier, weighted_cluster::ClusterWeight, HeaderMatcher},
        },
        r#type::matcher::v3::{string_matcher::MatchPattern, StringMatcher},
    },
    google::protobuf::UInt32Value,
};
use gateway_api::common::{HTTPHeader, HeaderMatch};

use crate::common::Backend;

mod grpc;
mod http;

fn create_header_matchers(headers: Option<Vec<HeaderMatch>>) -> Vec<HeaderMatcher> {
    headers.map_or(vec![], |headers| {
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
    })
}

fn headers_to_add(to_add: Vec<HTTPHeader>, to_set: Vec<HTTPHeader>) -> Vec<HeaderValueOption> {
    to_add
        .into_iter()
        .map(|h| HeaderValueOption {
            header: Some(HeaderValue {
                key: h.name,
                value: h.value,
                ..Default::default()
            }),
            append_action: HeaderAppendAction::AppendIfExistsOrAdd.into(),
            ..Default::default()
        })
        .chain(to_set.into_iter().map(|h| HeaderValueOption {
            header: Some(HeaderValue {
                key: h.name,
                value: h.value,
                ..Default::default()
            }),
            append_action: HeaderAppendAction::OverwriteIfExistsOrAdd.into(),
            ..Default::default()
        }))
        .collect()
}

fn create_cluster_weights(backends: &[Backend]) -> Vec<ClusterWeight> {
    backends
        .iter()
        .filter_map(|b| match b.backend_type() {
            crate::common::BackendType::Service(service_type_config) | crate::common::BackendType::Invalid(service_type_config) => Some(service_type_config),
            crate::common::BackendType::InferencePool(_) => None,
        })
        .filter(|b| b.weight() > 0)
        .map(|b| ClusterWeight {
            name: b.cluster_name(),
            weight: Some(UInt32Value {
                value: b.weight().try_into().expect("We do expect this to work for time being"),
            }),
            ..Default::default()
        })
        .collect()
}
