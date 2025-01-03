use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

use crate::common::{
    gateway_api::{gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute},
    ResourceKey,
};

#[derive(thiserror::Error, Debug, PartialEq, PartialOrd)]
pub enum StorageError {
    LockingError,
}
impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Clone)]
pub struct State {
    gateway_classes: Arc<Mutex<HashMap<ResourceKey, Arc<GatewayClass>>>>,
    gateways: Arc<Mutex<HashMap<ResourceKey, Arc<Gateway>>>>,
    http_routes: Arc<Mutex<HashMap<ResourceKey, Arc<HTTPRoute>>>>,
    gateways_with_routes: Arc<Mutex<HashMap<ResourceKey, BTreeSet<ResourceKey>>>>,
}
#[allow(dead_code)]
impl State {
    pub fn new() -> Self {
        Self {
            gateway_classes: Arc::new(Mutex::new(HashMap::new())),
            gateways: Arc::new(Mutex::new(HashMap::new())),
            http_routes: Arc::new(Mutex::new(HashMap::new())),
            gateways_with_routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub fn save_gateway(&self, id: ResourceKey, gateway: &Arc<Gateway>) -> Result<(), StorageError> {
        let mut lock = self.gateways.lock().map_err(|_| StorageError::LockingError)?;
        lock.insert(id, Arc::clone(gateway));
        Ok(())
    }

    pub fn maybe_save_gateway(&self, id: ResourceKey, gateway: &Arc<Gateway>) -> Result<(), StorageError> {
        let mut lock = self.gateways.lock().map_err(|_| StorageError::LockingError)?;
        if lock.contains_key(&id) {
            lock.insert(id, Arc::clone(gateway));
        };
        Ok(())
    }

    pub fn delete_gateway(&self, id: &ResourceKey) -> Result<Option<Arc<Gateway>>, StorageError> {
        let mut lock = self.gateways.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
    }

    pub fn get_gateway(&self, id: &ResourceKey) -> Result<Option<Arc<Gateway>>, StorageError> {
        let lock = self.gateways.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn maybe_save_http_route(&self, id: ResourceKey, route: &Arc<HTTPRoute>) -> Result<(), StorageError> {
        let mut lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        if lock.contains_key(&id) {
            lock.insert(id, Arc::clone(route));
        };
        Ok(())
    }
    pub fn save_http_route(&self, id: ResourceKey, route: &Arc<HTTPRoute>) -> Result<(), StorageError> {
        let mut lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        lock.insert(id, Arc::clone(route));
        Ok(())
    }

    pub fn attach_http_route_to_gateway(&self, gateway_id: ResourceKey, route_id: ResourceKey) -> Result<(), StorageError> {
        let mut lock = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes) = lock.get_mut(&gateway_id) {
            routes.insert(route_id);
        } else {
            let mut routes = BTreeSet::new();
            routes.insert(route_id);
            lock.insert(gateway_id, routes);
        }
        Ok(())
    }

    pub fn detach_http_route_from_gateway(&self, gateway_id: &ResourceKey, route_id: &ResourceKey) -> Result<(), StorageError> {
        let mut lock = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes) = lock.get_mut(gateway_id) {
            routes.retain(|key| key != route_id);
        };
        Ok(())
    }

    pub fn get_http_routes_attached_to_gateway(&self, resource_key: &ResourceKey) -> Result<Option<Vec<Arc<HTTPRoute>>>, StorageError> {
        let lock = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        let routes_lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(resource_key).cloned().map(|keys| keys.iter().filter_map(|k| routes_lock.get(k).cloned()).collect::<Vec<_>>()))
    }

    pub fn save_gateway_class(&self, id: ResourceKey, gateway_class: &Arc<GatewayClass>) -> Result<Option<Arc<GatewayClass>>, StorageError> {
        let mut lock = self.gateway_classes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.insert(id, Arc::clone(gateway_class)))
    }

    pub fn get_gateway_class_by_id(&self, id: &ResourceKey) -> Result<Option<Arc<GatewayClass>>, StorageError> {
        let lock = self.gateway_classes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn get_gateway_classes(&self) -> Result<Vec<Arc<GatewayClass>>, StorageError> {
        let lock = self.gateway_classes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.values().cloned().collect())
    }

    pub fn delete_gateway_class(&self, id: &ResourceKey) -> Result<Option<Arc<GatewayClass>>, StorageError> {
        let mut lock = self.gateway_classes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
    }

    pub fn delete_http_route(&self, id: &ResourceKey) -> Result<Option<Arc<HTTPRoute>>, StorageError> {
        let mut lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
    }

    pub fn get_http_route_by_id(&self, id: &ResourceKey) -> Result<Option<Arc<HTTPRoute>>, StorageError> {
        let lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn get_gateways(&self) -> Result<Vec<Arc<Gateway>>, StorageError> {
        let lock = self.gateways.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.values().cloned().collect())
    }

    pub fn http_routes(&self) -> Result<MutexGuard<'_, HashMap<ResourceKey, Arc<Gateway>>>, StorageError> {
        self.gateways.lock().map_err(|_| StorageError::LockingError)
    }

    pub fn gateways_with_routes(&self) -> Result<MutexGuard<'_, HashMap<ResourceKey, BTreeSet<ResourceKey>>>, StorageError> {
        self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)
    }
}
