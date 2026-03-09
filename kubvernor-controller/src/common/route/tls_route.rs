// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use gateway_api_with_extensions::tlsroutes::{TLSRoute, TlsRouteRules, TlsRouteRulesBackendRefs};
use kube::ResourceExt;

use crate::common::resource_key::DEFAULT_GROUP_NAME;

use super::{
    Backend, DEFAULT_NAMESPACE_NAME, DEFAULT_ROUTE_HOSTNAME, NotResolvedReason, ResolutionStatus, ResourceKey, Route, RouteConfig, RouteType,
    ServiceTypeConfig,
};
use crate::{common::BackendType, controllers::ControllerError};

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
        let local_namespace = key.namespace.clone();

        let mut has_invalid_backends = false;
        let routing_rules: Vec<TLSRoutingRule> = kube_route
            .spec
            .rules
            .iter()
            .enumerate()
            .map(|(i, rr)| TLSRoutingRule {
                name: format!("{}-{i}", kube_route.name_any()),
                backends: create_backends(rr, &local_namespace, &mut has_invalid_backends),
            })
            .collect();

        let hostnames = if kube_route.spec.hostnames.is_empty() {
            vec![DEFAULT_ROUTE_HOSTNAME.to_owned()]
        } else {
            kube_route.spec.hostnames.clone()
        };

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

fn create_backends(rr: &TlsRouteRules, local_namespace: &str, has_invalid_backends: &mut bool) -> Vec<Backend> {
    rr.backend_refs.iter().map(|br| backend_from_ref(br, local_namespace, has_invalid_backends)).collect()
}

fn backend_from_ref(br: &TlsRouteRulesBackendRefs, local_namespace: &str, has_invalid_backends: &mut bool) -> Backend {
    let backend_namespace = br.namespace.clone().unwrap_or_else(|| local_namespace.to_owned());
    let resource_key = ResourceKey {
        group: DEFAULT_GROUP_NAME.to_owned(),
        namespace: backend_namespace.clone(),
        name: br.name.clone(),
        kind: br.kind.clone().unwrap_or_else(|| "Service".to_owned()),
    };
    let config = ServiceTypeConfig {
        resource_key,
        endpoint: if backend_namespace == DEFAULT_NAMESPACE_NAME { br.name.clone() } else { format!("{}.{backend_namespace}", br.name) },
        port: br.port.unwrap_or(0),
        effective_port: br.port.unwrap_or(0),
        weight: br.weight.unwrap_or(1),
    };

    if br.kind.is_none() || br.kind == Some("Service".to_owned()) {
        Backend::Maybe(BackendType::Service(config))
    } else {
        *has_invalid_backends = true;
        Backend::Invalid(BackendType::Invalid(config))
    }
}

#[derive(Clone, Debug)]
pub struct TlsRoutingConfiguration {
    pub routing_rules: Vec<TLSRoutingRule>,
}

#[derive(Clone, Debug)]
pub struct TLSRoutingRule {
    pub name: String,
    pub backends: Vec<Backend>,
}
