// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use envoy_api_rs::{
    envoy::{
        config::{
            core::v3::{HeaderValue, HeaderValueOption, header_value_option::HeaderAppendAction},
            route::v3::{HeaderMatcher, header_matcher::HeaderMatchSpecifier, weighted_cluster::ClusterWeight},
        },
        r#type::matcher::v3::{StringMatcher, string_matcher::MatchPattern},
    },
    google::protobuf::UInt32Value,
};
use gateway_api::common::{HTTPHeader, HeaderMatch};

use crate::common::{BackendTypeConfig, InferencePoolTypeConfig, ServiceTypeConfig};

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
            header: Some(HeaderValue { key: h.name, value: h.value, ..Default::default() }),
            append_action: HeaderAppendAction::AppendIfExistsOrAdd.into(),
            ..Default::default()
        })
        .chain(to_set.into_iter().map(|h| HeaderValueOption {
            header: Some(HeaderValue { key: h.name, value: h.value, ..Default::default() }),
            append_action: HeaderAppendAction::OverwriteIfExistsOrAdd.into(),
            ..Default::default()
        }))
        .collect()
}

struct ServiceClusterWeight {
    name: String,
    weight: i32,
}

impl From<&ServiceTypeConfig> for ServiceClusterWeight {
    fn from(value: &ServiceTypeConfig) -> Self {
        ServiceClusterWeight { name: value.cluster_name(), weight: value.weight }
    }
}

impl From<&InferencePoolTypeConfig> for ServiceClusterWeight {
    fn from(value: &InferencePoolTypeConfig) -> Self {
        ServiceClusterWeight { name: value.cluster_name(), weight: value.weight }
    }
}

impl From<ServiceClusterWeight> for ClusterWeight {
    fn from(value: ServiceClusterWeight) -> Self {
        ClusterWeight {
            name: value.name,
            weight: Some(UInt32Value { value: value.weight.try_into().expect("We do expect this to work for time being") }),
            ..Default::default()
        }
    }
}

fn create_cluster_weights<I>(backends: I) -> Vec<ClusterWeight>
where
    I: Iterator<Item = ServiceClusterWeight>,
{
    backends.filter(|b| b.weight > 0).map(ClusterWeight::from).collect()
}
