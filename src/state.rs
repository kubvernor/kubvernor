use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use gateway_api::apis::standard::{gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute};

use crate::common::ResourceKey;

pub struct State {
    gateway_classes: HashMap<ResourceKey, Arc<GatewayClass>>,
    gateways: HashMap<ResourceKey, Arc<Gateway>>,
    http_routes: HashMap<ResourceKey, Arc<HTTPRoute>>,
    gateways_with_routes: HashMap<ResourceKey, BTreeSet<ResourceKey>>,
}
#[allow(dead_code)]
impl State {
    pub fn new() -> Self {
        Self {
            gateway_classes: HashMap::new(),
            gateways: HashMap::new(),
            http_routes: HashMap::new(),
            gateways_with_routes: HashMap::new(),
        }
    }
    pub fn save_gateway(&mut self, id: ResourceKey, gateway: &Arc<Gateway>) {
        self.gateways.insert(id, Arc::clone(gateway));
    }

    pub fn maybe_save_gateway(&mut self, id: ResourceKey, gateway: &Arc<Gateway>) {
        if self.gateways.contains_key(&id) {
            self.gateways.insert(id, Arc::clone(gateway));
        }
    }

    pub fn delete_gateway(&mut self, id: &ResourceKey) {
        self.gateways.remove(id);
    }

    pub fn get_gateway(&self, id: &ResourceKey) -> Option<&Arc<Gateway>> {
        self.gateways.get(id)
    }

    pub fn maybe_save_http_route(&mut self, id: ResourceKey, route: &Arc<HTTPRoute>) {
        if self.http_routes.contains_key(&id) {
            self.http_routes.insert(id, Arc::clone(route));
        }
    }
    pub fn save_http_route(&mut self, id: ResourceKey, route: &Arc<HTTPRoute>) {
        self.http_routes.insert(id, Arc::clone(route));
    }

    pub fn attach_http_route_to_gateway(&mut self, gateway_id: ResourceKey, route_id: ResourceKey) {
        if let Some(routes) = self.gateways_with_routes.get_mut(&gateway_id) {
            routes.insert(route_id);
        } else {
            let mut routes = BTreeSet::new();
            routes.insert(route_id);
            self.gateways_with_routes.insert(gateway_id, routes);
        }
    }

    pub fn detach_http_route_from_gateway(&mut self, gateway_id: &ResourceKey, route_id: &ResourceKey) {
        if let Some(routes) = self.gateways_with_routes.get_mut(gateway_id) {
            routes.retain(|key| key != route_id);
        }
    }

    pub fn get_http_routes_attached_to_gateway(&self, resource_key: &ResourceKey) -> Option<Vec<&Arc<HTTPRoute>>> {
        self.gateways_with_routes
            .get(resource_key)
            .map(|keys| keys.iter().filter_map(|k| self.http_routes.get(k)).collect::<Vec<_>>())
    }

    pub fn save_gateway_class(&mut self, id: ResourceKey, gateway_class: &Arc<GatewayClass>) {
        self.gateway_classes.insert(id, Arc::clone(gateway_class));
    }

    pub fn get_gateway_class_by_id(&self, id: &ResourceKey) -> Option<&Arc<GatewayClass>> {
        self.gateway_classes.get(id)
    }

    pub fn get_gateway_classes(&self) -> std::collections::hash_map::Values<'_, ResourceKey, Arc<GatewayClass>> {
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

    pub fn get_gateways(&self) -> std::collections::hash_map::Values<'_, ResourceKey, Arc<Gateway>> {
        self.gateways.values()
    }

    pub fn http_routes(&self) -> &HashMap<ResourceKey, Arc<HTTPRoute>> {
        &self.http_routes
    }

    pub fn gateways_with_routes(&self) -> &HashMap<ResourceKey, BTreeSet<ResourceKey>> {
        &self.gateways_with_routes
    }
}
