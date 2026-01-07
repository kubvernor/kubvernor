use gateway_api::common::ParentReference;
use kubvernor_common::ResourceKey;

pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_INFERENCE_GROUP_NAME: &str = "inference.networking.k8s.io/v1";

pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";
pub const DEFAULT_ROUTE_HOSTNAME: &str = "*";
pub const KUBERNETES_NONE: &str = "None";

impl From<(&ParentReference, String)> for RouteRefKey {
    fn from((route_parent, route_namespace): (&ParentReference, String)) -> Self {
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

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Default)]
pub struct RouteRefKey {
    pub resource_key: ResourceKey,
    pub section_name: Option<String>,
    pub port: Option<i32>,
}

#[allow(dead_code)]
impl RouteRefKey {
    pub fn new(name: &str) -> Self {
        Self { resource_key: ResourceKey::new(name), ..Default::default() }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self { resource_key: ResourceKey::namespaced(name, namespace), ..Default::default() }
    }
}

impl AsRef<ResourceKey> for RouteRefKey {
    fn as_ref(&self) -> &ResourceKey {
        &self.resource_key
    }
}
