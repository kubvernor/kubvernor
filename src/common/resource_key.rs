use std::fmt::Display;

use gateway_api::{
    gatewayclasses::GatewayClass,
    gateways,
    grpcroutes::{GRPCRoute, GRPCRouteParentRefs, GRPCRouteRulesBackendRefs},
    httproutes::{HTTPRoute, HTTPRouteParentRefs, HTTPRouteRulesBackendRefs},
};
use k8s_openapi::api::core::v1::Service;
use kube::{Resource, ResourceExt};

use super::RouteParentRefs;
use crate::common::create_id;

pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";
pub const DEFAULT_ROUTE_HOSTNAME: &str = "*";
pub const KUBERNETES_NONE: &str = "None";

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BackendResourceKey {
    pub group: String,
    namespace: Option<String>,
    pub name: String,
    pub kind: String,
}

#[allow(dead_code)]
impl ResourceKey {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Default::default()
        }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            ..Default::default()
        }
    }
}

impl Default for ResourceKey {
    fn default() -> Self {
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: DEFAULT_NAMESPACE_NAME.to_owned(),
            name: String::default(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", create_id(&self.name, &self.namespace))
    }
}

impl From<&Service> for ResourceKey {
    fn from(service: &Service) -> Self {
        let value = &service.metadata;
        let namespace = value.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let name = match (value.name.as_ref(), value.generate_name.as_ref()) {
            (None, None) => "",
            (Some(name), _) | (None, Some(name)) => name,
        };
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: name.to_owned(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}
impl From<(Option<String>, Option<String>, String, Option<String>)> for ResourceKey {
    fn from((group, namespace, name, kind): (Option<String>, Option<String>, String, Option<String>)) -> Self {
        let namespace = namespace.unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());
        Self {
            group: group.unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
            namespace,
            name: name.clone(),
            kind: kind.unwrap_or(DEFAULT_KIND_NAME.to_owned()),
        }
    }
}

impl From<(&HTTPRouteParentRefs, String)> for RouteRefKey {
    fn from((route_parent, route_namespace): (&HTTPRouteParentRefs, String)) -> Self {
        Self {
            resource_key: ResourceKey {
                group: route_parent.group.clone().unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
                namespace: route_parent.namespace.clone().unwrap_or(route_namespace),
                name: route_parent.name.clone(),
                kind: route_parent.kind.clone().unwrap_or(DEFAULT_KIND_NAME.to_owned()),
            },
            section_name: route_parent.section_name.clone(),
            port: route_parent.port,
        }
    }
}

impl From<(&GRPCRouteParentRefs, String)> for RouteRefKey {
    fn from((route_parent, route_namespace): (&GRPCRouteParentRefs, String)) -> Self {
        Self {
            resource_key: ResourceKey {
                group: route_parent.group.clone().unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
                namespace: route_parent.namespace.clone().unwrap_or(route_namespace),
                name: route_parent.name.clone(),
                kind: route_parent.kind.clone().unwrap_or(DEFAULT_KIND_NAME.to_owned()),
            },
            section_name: route_parent.section_name.clone(),
            port: route_parent.port,
        }
    }
}

impl From<(&RouteParentRefs, String)> for RouteRefKey {
    fn from((route_parent, route_namespace): (&RouteParentRefs, String)) -> Self {
        Self {
            resource_key: ResourceKey {
                group: route_parent.group.clone().unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
                namespace: route_parent.namespace.clone().unwrap_or(route_namespace),
                name: route_parent.name.clone(),
                kind: route_parent.kind.clone().unwrap_or(DEFAULT_KIND_NAME.to_owned()),
            },
            section_name: route_parent.section_name.clone(),
            port: route_parent.port,
        }
    }
}

impl From<&GatewayClass> for ResourceKey {
    fn from(value: &GatewayClass) -> Self {
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
            name: value.name_any().clone(),
            kind: "GatewayClass".to_owned(),
        }
    }
}

impl From<&gateways::Gateway> for ResourceKey {
    fn from(value: &gateways::Gateway) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name_any(),
            kind: "Gateway".to_owned(),
        }
    }
}

impl From<&HTTPRoute> for ResourceKey {
    fn from(value: &HTTPRoute) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name_any(),
            kind: "HTTPRoute".to_owned(),
        }
    }
}

impl From<&GRPCRoute> for ResourceKey {
    fn from(value: &GRPCRoute) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name_any(),
            kind: "GRPCRoute".to_owned(),
        }
    }
}

impl From<(&HTTPRouteRulesBackendRefs, String)> for ResourceKey {
    fn from((value, gateway_namespace): (&HTTPRouteRulesBackendRefs, String)) -> Self {
        let namespace = value.namespace.clone().unwrap_or(gateway_namespace);

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name.clone(),
            kind: value.kind.clone().unwrap_or_default(),
        }
    }
}

impl From<(&GRPCRouteRulesBackendRefs, String)> for ResourceKey {
    fn from((value, gateway_namespace): (&GRPCRouteRulesBackendRefs, String)) -> Self {
        let namespace = value.namespace.clone().unwrap_or(gateway_namespace);

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name.clone(),
            kind: value.kind.clone().unwrap_or_default(),
        }
    }
}

impl From<&HTTPRouteRulesBackendRefs> for BackendResourceKey {
    fn from(value: &HTTPRouteRulesBackendRefs) -> Self {
        let namespace = value.namespace.clone();

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name.clone(),
            kind: value.kind.clone().unwrap_or_default(),
        }
    }
}

impl From<&GRPCRouteRulesBackendRefs> for BackendResourceKey {
    fn from(value: &GRPCRouteRulesBackendRefs) -> Self {
        let namespace = value.namespace.clone();

        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace,
            name: value.name.clone(),
            kind: value.kind.clone().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Default)]
pub struct RouteRefKey {
    pub resource_key: ResourceKey,
    pub section_name: Option<String>,
    pub port: Option<i32>,
}

#[allow(dead_code)]
impl RouteRefKey {
    pub fn new(name: &str) -> Self {
        Self {
            resource_key: ResourceKey::new(name),
            ..Default::default()
        }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self {
            resource_key: ResourceKey::namespaced(name, namespace),
            ..Default::default()
        }
    }
}

impl AsRef<ResourceKey> for RouteRefKey {
    fn as_ref(&self) -> &ResourceKey {
        &self.resource_key
    }
}
