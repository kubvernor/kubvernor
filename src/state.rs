use std::{collections::HashMap, fmt::Display, sync::Arc};

use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute,
};
use kube_core::ObjectMeta;
use multimap::MultiMap;

use crate::controllers::ControllerError;

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
}

impl Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.namespace, self.name)
    }
}

impl TryFrom<&ObjectMeta> for ResourceKey {
    type Error = ControllerError;
    fn try_from(value: &ObjectMeta) -> Result<Self, ControllerError> {
        let namespace = value
            .namespace
            .clone()
            .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned());

        let name = match (value.name.as_ref(), value.generate_name.as_ref()) {
            (None, None) => {
                return Err(ControllerError::InvalidPayload(
                    "Name or generated name must be set".to_owned(),
                ))
            }
            (Some(name), _) | (None, Some(name)) => name,
        };
        Ok(Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: if namespace.is_empty() {
                DEFAULT_NAMESPACE_NAME.to_owned()
            } else {
                namespace
            },
            name: name.to_string(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        })
    }
}

pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";
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

pub struct State {
    gateway_classes: HashMap<ResourceKey, Arc<GatewayClass>>,
    gateways: HashMap<ResourceKey, Arc<Gateway>>,
    http_routes: HashMap<ResourceKey, Arc<HTTPRoute>>,
    gateways_with_routes: MultiMap<ResourceKey, ResourceKey>,
}

impl State {
    pub fn new() -> Self {
        Self {
            gateway_classes: HashMap::new(),
            gateways: HashMap::new(),
            http_routes: HashMap::new(),
            gateways_with_routes: MultiMap::new(),
        }
    }
    pub fn save_gateway(&mut self, id: ResourceKey, gateway: &Arc<Gateway>) {
        self.gateways.insert(id, Arc::clone(gateway));
    }

    pub fn delete_gateway(&mut self, id: &ResourceKey) {
        self.gateways.remove(id);
    }

    pub fn get_gateways(
        &self,
    ) -> std::collections::hash_map::Values<'_, ResourceKey, Arc<Gateway>> {
        self.gateways.values()
    }

    pub fn get_gateway(&self, id: &ResourceKey) -> Option<&Arc<Gateway>> {
        self.gateways.get(id)
    }

    pub fn save_http_route(&mut self, id: ResourceKey, route: &Arc<HTTPRoute>) {
        self.http_routes.insert(id, Arc::clone(route));
    }

    pub fn attach_http_route_to_gateway(&mut self, gateway_id: ResourceKey, route_id: ResourceKey) {
        self.gateways_with_routes.insert(gateway_id, route_id);
    }

    pub fn detach_http_route_from_gateway(
        &mut self,
        gateway_id: &ResourceKey,
        route_id: &ResourceKey,
    ) {
        if let Some(routes) = self.gateways_with_routes.get_vec_mut(gateway_id) {
            routes.retain(|key| key != route_id);
        }
    }

    pub fn get_http_routes_attached_to_gateway(
        &self,
        resource_key: &ResourceKey,
    ) -> Option<Vec<&Arc<HTTPRoute>>> {
        self.gateways_with_routes.get_vec(resource_key).map(|keys| {
            keys.iter()
                .filter_map(|k| self.http_routes.get(k))
                .collect::<Vec<_>>()
        })
    }

    pub fn save_gateway_class(&mut self, id: ResourceKey, gateway_class: &Arc<GatewayClass>) {
        self.gateway_classes.insert(id, Arc::clone(gateway_class));
    }

    pub fn get_gateway_class_by_id(&self, id: &ResourceKey) -> Option<&Arc<GatewayClass>> {
        self.gateway_classes.get(id)
    }

    pub fn get_gateway_classes(
        &self,
    ) -> std::collections::hash_map::Values<'_, ResourceKey, Arc<GatewayClass>> {
        self.gateway_classes.values()
    }

    pub fn delete_gateway_class(&mut self, id: &ResourceKey) {
        self.gateway_classes.remove(id);
    }

    pub fn delete_http_route(&mut self, id: &ResourceKey) {
        self.http_routes.remove(id);
    }

    pub fn get_http_route_by_id(&self, id: &ResourceKey) -> Option<&Arc<HTTPRoute>> {
        self.http_routes.get(id)
    }
}
