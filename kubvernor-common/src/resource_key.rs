use std::fmt::Display;

use gateway_api::{
    gatewayclasses::{GatewayClass, GatewayClassParametersRef},
    gateways,
    grpcroutes::{GRPCBackendReference, GRPCRoute},
    httproutes::{HTTPBackendReference, HTTPRoute},
    referencegrants::{ReferenceGrantFrom, ReferenceGrantTo},
};
use gateway_api_inference_extension::inferencepools::{InferencePool, InferencePoolStatusParentsParentRef};
use k8s_openapi::api::core::v1::Service;
use kube::{Resource, ResourceExt};

pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_INFERENCE_GROUP_NAME: &str = "inference.networking.k8s.io/v1";

pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
}

#[allow(dead_code)]
impl ResourceKey {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_owned(), ..Default::default() }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self { name: name.to_owned(), namespace: namespace.to_owned(), ..Default::default() }
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

fn create_id(name: &str, namespace: &str) -> String {
    namespace.to_owned() + "." + name
}

impl From<&Service> for ResourceKey {
    fn from(service: &Service) -> Self {
        let value = &service.metadata;
        let namespace = value.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let name = match (value.name.as_ref(), value.generate_name.as_ref()) {
            (None, None) => "",
            (Some(name), _) | (None, Some(name)) => name,
        };
        Self { group: DEFAULT_GROUP_NAME.to_owned(), namespace, name: name.to_owned(), kind: DEFAULT_KIND_NAME.to_owned() }
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

        Self { group: DEFAULT_GROUP_NAME.to_owned(), namespace, name: value.name_any(), kind: "Gateway".to_owned() }
    }
}

impl From<&HTTPRoute> for ResourceKey {
    fn from(value: &HTTPRoute) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self { group: DEFAULT_GROUP_NAME.to_owned(), namespace, name: value.name_any(), kind: "HTTPRoute".to_owned() }
    }
}

impl From<&GRPCRoute> for ResourceKey {
    fn from(value: &GRPCRoute) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self { group: DEFAULT_GROUP_NAME.to_owned(), namespace, name: value.name_any(), kind: "GRPCRoute".to_owned() }
    }
}

impl From<(&HTTPBackendReference, String)> for ResourceKey {
    fn from((value, gateway_namespace): (&HTTPBackendReference, String)) -> Self {
        let namespace = value.namespace.clone().unwrap_or(gateway_namespace);

        Self { group: DEFAULT_GROUP_NAME.to_owned(), namespace, name: value.name.clone(), kind: value.kind.clone().unwrap_or_default() }
    }
}

impl From<(&GRPCBackendReference, String)> for ResourceKey {
    fn from((value, gateway_namespace): (&GRPCBackendReference, String)) -> Self {
        let namespace = value.namespace.clone().unwrap_or(gateway_namespace);

        Self { group: DEFAULT_GROUP_NAME.to_owned(), namespace, name: value.name.clone(), kind: value.kind.clone().unwrap_or_default() }
    }
}

impl From<&InferencePool> for ResourceKey {
    fn from(value: &InferencePool) -> Self {
        let namespace = value.meta().namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self { group: DEFAULT_INFERENCE_GROUP_NAME.to_owned(), namespace, name: value.name_any(), kind: "InferencePool".to_owned() }
    }
}

impl From<&GatewayClassParametersRef> for ResourceKey {
    fn from(value: &GatewayClassParametersRef) -> Self {
        let namespace = value.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        Self { group: value.group.clone(), namespace, name: value.name.clone(), kind: value.kind.clone() }
    }
}

impl From<&InferencePoolStatusParentsParentRef> for ResourceKey {
    fn from(value: &InferencePoolStatusParentsParentRef) -> Self {
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: value.namespace.clone().unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
            name: value.name.clone(),
            kind: value.kind.clone().unwrap_or_default(),
        }
    }
}

impl From<&ReferenceGrantFrom> for ResourceKey {
    fn from(value: &ReferenceGrantFrom) -> Self {
        if value.group.is_empty() {
            ResourceKey {
                group: DEFAULT_GROUP_NAME.to_owned(),
                namespace: value.namespace.clone(),
                name: String::default(),
                kind: value.kind.clone(),
            }
        } else {
            ResourceKey {
                group: value.group.clone(),
                namespace: value.namespace.clone(),
                name: String::default(),
                kind: value.kind.clone(),
            }
        }
    }
}

impl From<&ReferenceGrantTo> for ResourceKey {
    fn from(value: &ReferenceGrantTo) -> Self {
        if value.group.is_empty() {
            ResourceKey {
                group: DEFAULT_GROUP_NAME.to_owned(),
                namespace: String::default(),
                name: value.name.as_ref().unwrap_or(&String::new()).to_owned(),
                kind: value.kind.clone(),
            }
        } else {
            ResourceKey {
                group: value.group.clone(),
                namespace: String::default(),
                name: value.name.as_ref().unwrap_or(&String::new()).to_owned(),
                kind: value.kind.clone(),
            }
        }
    }
}
