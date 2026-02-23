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
        config::{cluster::v3::Cluster, listener::v3::Listener},
        service::discovery::v3::Resource,
    },
    prost::{self, Message},
};

use super::model::TypeUrl;
use crate::backends::envoy::common::converters;

pub fn create_cluster_resource(cluster: &Cluster) -> Resource {
    let any = converters::AnyTypeConverter::from((TypeUrl::Cluster.to_string(), cluster));
    let mut cluster_resource = Resource { ..Default::default() };
    cluster_resource.name.clone_from(&cluster.name);
    cluster_resource.resource = Some(any);
    cluster_resource
}

pub fn create_listener_resource(listener: &Listener) -> Resource {
    let mut buf: Vec<u8> = vec![];
    listener.encode(&mut buf).expect("We expect this to work");
    let any = prost::bytes::Bytes::from(buf);
    let any = envoy_api_rs::google::protobuf::Any { type_url: TypeUrl::Listener.to_string(), value: any.to_vec() };

    let mut listener_resource = Resource { ..Default::default() };
    listener_resource.name.clone_from(&listener.name);
    listener_resource.resource = Some(any);
    listener_resource
}
