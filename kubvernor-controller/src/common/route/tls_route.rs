// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use gateway_api_with_extensions::tlsroutes::{TLSRoute, TlsRouteRulesBackendRefs};
use kube::ResourceExt;

use super::{
    Backend, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME, NotResolvedReason, ResolutionStatus, ResourceKey, Route, RouteConfig,
    RouteType, ServiceTypeConfig,
};
use crate::{
    common::{BackendType, InferencePoolTypeConfig, resource_key::DEFAULT_INFERENCE_GROUP_NAME},
    controllers::ControllerError,
};

#[derive(Clone, Debug)]
pub struct TlsRoutingConfiguration {
    pub routing_rules: Vec<super::tls_route::TlsRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct TlsRoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
}

impl TryFrom<TLSRoute> for Route {
    type Error = ControllerError;

    fn try_from(value: TLSRoute) -> Result<Self, Self::Error> {
        Route::try_from(&value)
    }
}

impl TryFrom<&TLSRoute> for Route {
    type Error = ControllerError;
    fn try_from(kube_route: &TLSRoute) -> Result<Self, Self::Error> {
        let key = ResourceKey::from(kube_route);
        let parents = kube_route.spec.parent_refs.clone();
        let local_namespace = key.namespace.as_str();

        let mut has_invalid_backends = false;
        let routing_rules = &kube_route.spec.rules;
        let routing_rules: Vec<TlsRoutingRule> = routing_rules
            .iter()
            .enumerate()
            .map(|(i, rr)| TlsRoutingRule {
                name: format!("{}-{i}", kube_route.name_any()),

                backends: rr
                    .backend_refs
                    .iter()
                    .map(|br| match br.kind.as_ref() {
                        None => Backend::Maybe(BackendType::Service(ServiceTypeConfig::from((br, local_namespace)))),
                        Some(kind) if kind == "Service" => {
                            Backend::Maybe(BackendType::Service(ServiceTypeConfig::from((br, local_namespace))))
                        },
                        Some(kind) if kind == "InferencePool" => {
                            Backend::Maybe(BackendType::InferencePool(InferencePoolTypeConfig::from((br, local_namespace))))
                        },
                        _ => {
                            has_invalid_backends = true;
                            Backend::Invalid(BackendType::Invalid(ServiceTypeConfig::from((br, local_namespace))))
                        },
                    })
                    .collect(),
            })
            .collect();

        let hostnames =
            if kube_route.spec.hostnames.is_empty() { vec![DEFAULT_ROUTE_HOSTNAME.to_owned()] } else { kube_route.spec.hostnames.clone() };

        let config = RouteConfig {
            resource_key: key,
            parents,
            hostnames,
            resolution_status: if has_invalid_backends {
                ResolutionStatus::NotResolved(NotResolvedReason::InvalidBackend)
            } else {
                ResolutionStatus::NotResolved(NotResolvedReason::Unknown)
            },
            route_type: RouteType::Tls(TlsRoutingConfiguration { routing_rules }),
        };

        Ok(Route { config })
    }
}

impl From<(&TlsRouteRulesBackendRefs, &str)> for ServiceTypeConfig {
    fn from((br, local_namespace): (&TlsRouteRulesBackendRefs, &str)) -> Self {
        ServiceTypeConfig {
            resource_key: ResourceKey::from((br, local_namespace.to_owned())),
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
        }
    }
}

impl From<(&TlsRouteRulesBackendRefs, &str)> for InferencePoolTypeConfig {
    fn from((br, local_namespace): (&TlsRouteRulesBackendRefs, &str)) -> Self {
        let mut resource_key = ResourceKey::from((br, local_namespace.to_owned()));
        DEFAULT_INFERENCE_GROUP_NAME.clone_into(&mut resource_key.group);
        InferencePoolTypeConfig {
            resource_key,
            endpoint: if let Some(namespace) = br.namespace.as_ref() {
                if *namespace == DEFAULT_NAMESPACE_NAME { br.name.clone() } else { format!("{}.{namespace}", br.name) }
            } else if local_namespace == DEFAULT_NAMESPACE_NAME {
                br.name.clone()
            } else {
                format!("{}.{local_namespace}", br.name)
            },
            port: br.port.unwrap_or(0),
            target_ports: vec![br.port.unwrap_or(0)],
            weight: br.weight.unwrap_or(1),
            inference_config: None,
            endpoints: None,
        }
    }
}
