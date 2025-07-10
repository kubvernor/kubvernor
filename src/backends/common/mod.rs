pub mod converters;
mod resource_generator;
use envoy_api_rs::{
    envoy::config::{
        cluster::v3::Cluster as EnvoyCluster,
        core::v3::{address, socket_address::PortSpecifier, Address, SocketAddress},
    },
    google::protobuf::Duration,
};
pub use resource_generator::{calculate_hostnames_common, EnvoyListener, EnvoyVirtualHost, ResourceGenerator};

use crate::{
    backends,
    common::{Backend, HTTPEffectiveRoutingRule, InferencePoolTypeConfig, ServiceTypeConfig},
};

pub const INFERENCE_EXT_PROC_FILTER_NAME: &str = "inference.filters.http.ext_proc";

#[derive(Debug)]
pub struct ClusterHolder {
    pub name: String,
    pub cluster: EnvoyCluster,
}
impl Eq for ClusterHolder {}

impl PartialEq for ClusterHolder {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl PartialOrd for ClusterHolder {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.name.cmp(&other.name))
    }
}
impl Ord for ClusterHolder {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

pub enum DurationConverter {}

impl DurationConverter {
    fn from(val: std::time::Duration) -> Duration {
        Duration {
            nanos: val.subsec_nanos().try_into().expect("At the moment we expect this to work"),
            seconds: val.as_secs().try_into().expect("At the moment we expect this to work"),
        }
    }
}

pub enum SocketAddressFactory {}
impl SocketAddressFactory {
    pub fn from(listener: &backends::common::EnvoyListener) -> envoy_api_rs::envoy::config::core::v3::Address {
        Address {
            address: Some(address::Address::SocketAddress(SocketAddress {
                address: "0.0.0.0".to_owned(),
                port_specifier: Some(PortSpecifier::PortValue(listener.port.try_into().expect("For time being we expect this to work"))),
                resolver_name: String::new(),
                ipv4_compat: false,
                ..Default::default()
            })),
        }
    }

    pub fn from_backend(backend: &ServiceTypeConfig) -> envoy_api_rs::envoy::config::core::v3::Address {
        Address {
            address: Some(address::Address::SocketAddress(SocketAddress {
                address: backend.endpoint.clone(),
                port_specifier: Some(PortSpecifier::PortValue(backend.effective_port.try_into().expect("For time being we expect this to work"))),
                ..Default::default()
            })),
        }
    }
}

pub struct InferenceClusterInfo {
    name: String,
    config: InferencePoolTypeConfig,
}

pub fn inference_cluster_name(route_name: String) -> String {
    route_name + "ext_svc_cluster"
}
pub fn get_inference_pool_configurations(envoy_route: &HTTPEffectiveRoutingRule) -> Option<InferenceClusterInfo> {
    get_inference_extension_configurations(&envoy_route.backends).first().map(|conf| InferenceClusterInfo {
        name: inference_cluster_name(envoy_route.name.clone()),
        config: (**conf).clone(),
    })
}

pub fn get_inference_extension_configurations(backends: &[Backend]) -> Vec<&InferencePoolTypeConfig> {
    backends
        .iter()
        .filter_map(|b| match b.backend_type() {
            crate::common::BackendType::InferencePool(inference_type_config) => Some(inference_type_config),
            _ => None,
        })
        .collect()
}
