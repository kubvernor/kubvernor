use std::{collections::HashMap, sync::Arc};

use gateway_api::apis::standard::{
    gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute,
};

use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
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
    pub gateway_class_names: HashMap<Uuid, Arc<GatewayClass>>,
    gateways: HashMap<Uuid, Arc<Gateway>>,
    gateways_by_id: HashMap<ResourceKey, Arc<Gateway>>,
    http_routes: HashMap<Uuid, Arc<HTTPRoute>>,
}

impl State {
    pub fn new() -> Self {
        Self {
            gateway_class_names: HashMap::new(),
            gateways: HashMap::new(),
            gateways_by_id: HashMap::new(),
            http_routes: HashMap::new(),
        }
    }
    pub fn save_gateway(&mut self, id: Uuid, gateway: &Arc<Gateway>) {
        self.gateways.insert(id, Arc::clone(gateway));
        self.gateways_by_id
            .insert(ResourceKey::from(gateway.as_ref()), Arc::clone(gateway));
    }

    pub fn delete_gateway(&mut self, id: Uuid) {
        if let Some(gateway) = self.gateways.remove(&id) {
            self.gateways_by_id
                .remove(&ResourceKey::from(gateway.as_ref()));
        }
    }

    pub fn get_gateway_by_id(&self, id: Uuid) -> Option<&Arc<Gateway>> {
        self.gateways.get(&id)
    }

    pub fn get_gateway_by_resource(&self, resource_key: &ResourceKey) -> Option<&Arc<Gateway>> {
        self.gateways_by_id.get(resource_key)
    }

    pub fn save_http_route(&mut self, id: Uuid, route: &Arc<HTTPRoute>) {
        self.http_routes.insert(id, Arc::clone(route));
        // self.gateways_by_id
        //     .insert(ResourceKey::from(gateway.as_ref()), Arc::clone(gateway));
    }

    pub fn delete_http_route(&mut self, id: Uuid) {
        self.http_routes.remove(&id);
    }

    pub fn get_http_route_by_id(&self, id: Uuid) -> Option<&Arc<HTTPRoute>> {
        self.http_routes.get(&id)
    }
}
