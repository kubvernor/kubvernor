use std::{fmt::Display, net::SocketAddr};

use serde::Deserialize;
use thiserror::Error;
use typed_builder::TypedBuilder;

use crate::Result;

#[derive(Clone, Debug, TypedBuilder, Deserialize)]
pub struct Address {
    pub hostname: String,
    pub port: u16,
}

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(format!("{}:{}", self.hostname, self.port).as_str())
    }
}

impl Address {
    pub fn to_ips(&self) -> Vec<SocketAddr> {
        if let Ok(socket) = self.to_string().parse::<SocketAddr>() {
            vec![socket]
        } else {
            vec![SocketAddr::from(([0, 0, 0, 0], self.port)), SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], self.port))]
        }
    }

    pub fn to_ip(&self) -> Result<SocketAddr> {
        self.to_string().parse::<SocketAddr>().map_err(std::convert::Into::into)
    }
}

#[derive(Debug, TypedBuilder, Deserialize)]
pub struct EnvoyGatewayControlPlaneConfiguration {
    pub address: Address,
}

#[derive(Debug, TypedBuilder, Deserialize)]
pub struct AgentgatewayGatewayControlPlaneConfiguration {
    pub address: Address,
}

#[derive(Clone, Debug, TypedBuilder, Deserialize)]
pub struct AdminInterfaceConfiguration {
    pub address: Address,
}

#[derive(Debug, TypedBuilder, Deserialize)]
pub struct Configuration {
    pub controller_name: String,
    pub enable_open_telemetry: Option<bool>,
    pub envoy_gateway_control_plane: Option<EnvoyGatewayControlPlaneConfiguration>,
    pub agentgateway_gateway_control_plane: Option<AgentgatewayGatewayControlPlaneConfiguration>,
    pub orion_gateway_control_plane: Option<EnvoyGatewayControlPlaneConfiguration>,
    pub admin_interface: Option<AdminInterfaceConfiguration>,
}

#[derive(Error, Debug)]
enum ConfigurationError {
    #[error("controller name must be not empty")]
    ControllerName,
    #[error("one control plane must be configured")]
    ControlPlane,
}

impl Configuration {
    pub fn validate(&self) -> Result<()> {
        if self.controller_name.is_empty() {
            return Err(ConfigurationError::ControllerName.into());
        }
        match (&self.agentgateway_gateway_control_plane, &self.envoy_gateway_control_plane) {
            (None, None) => Err(ConfigurationError::ControlPlane.into()),
            _ => Ok(()),
        }
    }
}
