// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    hash::Hash,
    sync::{Arc, Mutex, MutexGuard},
};

use gateway_api::{gatewayclasses::GatewayClass, gateways::Gateway, grpcroutes::GRPCRoute, httproutes::HTTPRoute};
use gateway_api_inference_extension::inferencepools::InferencePool;
use kubvernor_common::{GatewayImplementationType, ResourceKey};

#[derive(thiserror::Error, Debug, PartialEq, PartialOrd)]
pub enum StorageError {
    LockingError,
}
impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Eq)]
struct RouteToInferencePool {
    route_id: ResourceKey,
    inference_pool_ids: Vec<ResourceKey>,
}
impl Hash for RouteToInferencePool {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.route_id.hash(state);
    }
}

impl PartialEq for RouteToInferencePool {
    fn eq(&self, other: &Self) -> bool {
        self.route_id == other.route_id
    }
}

impl PartialOrd for RouteToInferencePool {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.route_id.partial_cmp(&other.route_id)
    }
}

#[derive(Clone, Default)]
pub struct State {
    gateway_classes: Arc<Mutex<HashMap<ResourceKey, Arc<GatewayClass>>>>,
    gateways: Arc<Mutex<HashMap<ResourceKey, Arc<Gateway>>>>,
    http_routes: Arc<Mutex<HashMap<ResourceKey, Arc<HTTPRoute>>>>,
    grpc_routes: Arc<Mutex<HashMap<ResourceKey, Arc<GRPCRoute>>>>,
    gateways_with_routes: Arc<Mutex<HashMap<ResourceKey, BTreeSet<ResourceKey>>>>,
    inference_pools: Arc<Mutex<HashMap<ResourceKey, Arc<InferencePool>>>>,
    gateways_with_routes_with_inference_pools: Arc<Mutex<HashMap<ResourceKey, HashSet<RouteToInferencePool>>>>,
    gateway_implementation_types: Arc<Mutex<HashMap<ResourceKey, GatewayImplementationType>>>,
}
#[allow(dead_code)]
impl State {
    pub fn new() -> Self {
        Self {
            gateway_classes: Arc::new(Mutex::new(HashMap::new())),
            gateways: Arc::new(Mutex::new(HashMap::new())),
            http_routes: Arc::new(Mutex::new(HashMap::new())),
            grpc_routes: Arc::new(Mutex::new(HashMap::new())),
            gateways_with_routes: Arc::new(Mutex::new(HashMap::new())),
            inference_pools: Arc::new(Mutex::new(HashMap::new())),
            gateways_with_routes_with_inference_pools: Arc::new(Mutex::new(HashMap::new())),
            gateway_implementation_types: Arc::new(Mutex::new(HashMap::new())),
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
        }
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

    pub fn attach_http_route_to_gateway(&self, gateway_id: ResourceKey, route_id: ResourceKey) -> Result<(), StorageError> {
        let mut gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes) = gateways_with_routes.get_mut(&gateway_id) {
            routes.insert(route_id);
        } else {
            let mut routes = BTreeSet::new();
            routes.insert(route_id);
            gateways_with_routes.insert(gateway_id, routes);
        }
        Ok(())
    }

    pub fn attach_grpc_route_to_gateway(&self, gateway_id: ResourceKey, route_id: ResourceKey) -> Result<(), StorageError> {
        let mut gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes) = gateways_with_routes.get_mut(&gateway_id) {
            routes.insert(route_id);
        } else {
            let mut routes = BTreeSet::new();
            routes.insert(route_id);
            gateways_with_routes.insert(gateway_id, routes);
        }
        Ok(())
    }

    pub fn detach_http_route_from_gateway(&self, gateway_id: &ResourceKey, route_id: &ResourceKey) -> Result<(), StorageError> {
        let mut gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes) = gateways_with_routes.get_mut(gateway_id) {
            routes.retain(|key| key != route_id);
        }
        Ok(())
    }

    pub fn detach_grpc_route_from_gateway(&self, gateway_id: &ResourceKey, route_id: &ResourceKey) -> Result<(), StorageError> {
        let mut gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes) = gateways_with_routes.get_mut(gateway_id) {
            routes.retain(|key| key != route_id);
        }
        Ok(())
    }

    pub fn get_http_routes_attached_to_gateway(&self, gateway_key: &ResourceKey) -> Result<Option<Vec<Arc<HTTPRoute>>>, StorageError> {
        let gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        let http_routes = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(gateways_with_routes
            .get(gateway_key)
            .cloned()
            .map(|keys| keys.iter().filter_map(|k| http_routes.get(k).cloned()).collect::<Vec<_>>()))
    }

    pub fn get_grpc_routes_attached_to_gateway(&self, gateway_key: &ResourceKey) -> Result<Option<Vec<Arc<GRPCRoute>>>, StorageError> {
        let gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        let grpc_routes = self.grpc_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(gateways_with_routes
            .get(gateway_key)
            .cloned()
            .map(|keys| keys.iter().filter_map(|k| grpc_routes.get(k).cloned()).collect::<Vec<_>>()))
    }

    pub fn save_gateway_class(
        &self,
        id: ResourceKey,
        gateway_class: &Arc<GatewayClass>,
    ) -> Result<Option<Arc<GatewayClass>>, StorageError> {
        let mut lock = self.gateway_classes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.insert(id, Arc::clone(gateway_class)))
    }

    pub fn save_gateway_type(
        &self,
        id: ResourceKey,
        gateway_implementation_type: GatewayImplementationType,
    ) -> Result<Option<GatewayImplementationType>, StorageError> {
        let mut lock = self.gateway_implementation_types.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.insert(id, gateway_implementation_type))
    }

    pub fn get_gateway_type(&self, id: &ResourceKey) -> Result<Option<GatewayImplementationType>, StorageError> {
        let lock = self.gateway_implementation_types.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn delete_gateway_type(&self, id: &ResourceKey) -> Result<Option<GatewayImplementationType>, StorageError> {
        let mut lock = self.gateway_implementation_types.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
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

    pub fn maybe_save_http_route(&self, id: ResourceKey, route: &Arc<HTTPRoute>) -> Result<(), StorageError> {
        let mut lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        if lock.contains_key(&id) {
            lock.insert(id, Arc::clone(route));
        }
        Ok(())
    }
    pub fn save_http_route(&self, id: ResourceKey, route: &Arc<HTTPRoute>) -> Result<(), StorageError> {
        let mut lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        lock.insert(id, Arc::clone(route));
        Ok(())
    }

    pub fn delete_http_route(&self, id: &ResourceKey) -> Result<Option<Arc<HTTPRoute>>, StorageError> {
        let mut lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
    }

    pub fn get_http_route_by_id(&self, id: &ResourceKey) -> Result<Option<Arc<HTTPRoute>>, StorageError> {
        let lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn maybe_save_grpc_route(&self, id: ResourceKey, route: &Arc<GRPCRoute>) -> Result<(), StorageError> {
        let mut lock = self.grpc_routes.lock().map_err(|_| StorageError::LockingError)?;
        if lock.contains_key(&id) {
            lock.insert(id, Arc::clone(route));
        }
        Ok(())
    }
    pub fn save_grpc_route(&self, id: ResourceKey, route: &Arc<GRPCRoute>) -> Result<(), StorageError> {
        let mut lock = self.grpc_routes.lock().map_err(|_| StorageError::LockingError)?;
        lock.insert(id, Arc::clone(route));
        Ok(())
    }

    pub fn delete_grpc_route(&self, id: &ResourceKey) -> Result<Option<Arc<GRPCRoute>>, StorageError> {
        let mut lock = self.grpc_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
    }

    pub fn get_grpc_route_by_id(&self, id: &ResourceKey) -> Result<Option<Arc<GRPCRoute>>, StorageError> {
        let lock = self.grpc_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn get_gateways(&self) -> Result<Vec<Arc<Gateway>>, StorageError> {
        let lock = self.gateways.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.values().cloned().collect())
    }

    pub fn gateways_with_routes(&self) -> Result<MutexGuard<'_, HashMap<ResourceKey, BTreeSet<ResourceKey>>>, StorageError> {
        self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)
    }

    pub fn save_inference_pool(&self, id: ResourceKey, inference_pool: &Arc<InferencePool>) -> Result<(), StorageError> {
        let mut lock = self.inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        lock.insert(id, Arc::clone(inference_pool));
        Ok(())
    }

    pub fn maybe_save_inference_pool(&self, id: ResourceKey, inference_pool: &Arc<InferencePool>) -> Result<(), StorageError> {
        let mut lock = self.inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        if lock.contains_key(&id) {
            lock.insert(id, Arc::clone(inference_pool));
        }
        Ok(())
    }

    pub fn get_inference_pool(&self, id: &ResourceKey) -> Result<Option<Arc<InferencePool>>, StorageError> {
        let lock = self.inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.get(id).cloned())
    }

    pub fn delete_inference_pool(&self, id: &ResourceKey) -> Result<Option<Arc<InferencePool>>, StorageError> {
        let mut lock = self.inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.remove(id))
    }

    pub fn get_http_routes(&self) -> Result<Vec<Arc<HTTPRoute>>, StorageError> {
        let lock = self.http_routes.lock().map_err(|_| StorageError::LockingError)?;
        Ok(lock.values().cloned().collect())
    }

    pub fn get_gateways_with_http_routes(&self, routes: &BTreeSet<ResourceKey>) -> Result<BTreeSet<ResourceKey>, StorageError> {
        let mut owned_gateways = BTreeSet::new();
        let gateways_with_routes = self.gateways_with_routes.lock().map_err(|_| StorageError::LockingError)?;
        for (gateway_id, stored_routes) in gateways_with_routes.iter() {
            let mut instesection = stored_routes.intersection(routes);
            if instesection.next().is_some() {
                owned_gateways.insert(gateway_id.clone());
            }
        }
        Ok(owned_gateways)
    }

    pub fn find_gateways_by_inference_pool(
        &self,
        inference_pool_resource_key: &ResourceKey,
    ) -> Result<BTreeSet<ResourceKey>, StorageError> {
        let gateways_with_routes_with_inference_pools =
            self.gateways_with_routes_with_inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        let mut gateway_ids = BTreeSet::new();
        for (gateway, routes) in gateways_with_routes_with_inference_pools.iter() {
            for RouteToInferencePool { route_id: _, inference_pool_ids } in routes {
                if inference_pool_ids.contains(inference_pool_resource_key) {
                    gateway_ids.insert(gateway.clone());
                }
            }
        }
        Ok(gateway_ids)
    }

    pub fn find_gateways_by_route_and_inference_pool(&self, route_id: &ResourceKey) -> Result<BTreeSet<ResourceKey>, StorageError> {
        let gateways_with_routes_with_inference_pools =
            self.gateways_with_routes_with_inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        let mut gateway_ids = BTreeSet::new();

        let inference_pools_associated_with_route = gateways_with_routes_with_inference_pools
            .values()
            .filter_map(|p| {
                p.get(&RouteToInferencePool { route_id: route_id.clone(), inference_pool_ids: vec![] })
                    .as_ref()
                    .map(|f| &f.inference_pool_ids)
            })
            .flatten()
            .collect::<BTreeSet<_>>();

        for (gateway, routes) in gateways_with_routes_with_inference_pools.iter() {
            for RouteToInferencePool { route_id: _, inference_pool_ids } in routes {
                for inference_pool in &inference_pools_associated_with_route {
                    if inference_pool_ids.contains(inference_pool) {
                        gateway_ids.insert(gateway.clone());
                    }
                }
            }
        }

        Ok(gateway_ids)
    }

    pub fn attach_inference_pool_to_gateway(
        &self,
        gateway_id: ResourceKey,
        route_id: ResourceKey,
        inference_pool_ids: Vec<ResourceKey>,
    ) -> Result<(), StorageError> {
        let mut gateways_with_routes_with_inference_pools =
            self.gateways_with_routes_with_inference_pools.lock().map_err(|_| StorageError::LockingError)?;
        if let Some(routes_with_pools) = gateways_with_routes_with_inference_pools.get_mut(&gateway_id) {
            routes_with_pools.insert(RouteToInferencePool { route_id, inference_pool_ids });
        } else {
            let mut routes_with_pools = HashSet::new();
            routes_with_pools.insert(RouteToInferencePool { route_id, inference_pool_ids });
            gateways_with_routes_with_inference_pools.insert(gateway_id, routes_with_pools);
        }

        Ok(())
    }

    pub fn detach_infererence_pool_from_gateway(
        &self,
        gateway_id: &ResourceKey,
        route_id: &ResourceKey,
    ) -> Result<Vec<ResourceKey>, StorageError> {
        {
            let mut gateways_with_routes_with_inference_pools =
                self.gateways_with_routes_with_inference_pools.lock().map_err(|_| StorageError::LockingError)?;
            if let Some(routes_with_pools) = gateways_with_routes_with_inference_pools.get_mut(gateway_id)
                && let Some(inference_pools) =
                    routes_with_pools.take(&RouteToInferencePool { route_id: route_id.clone(), inference_pool_ids: vec![] })
            {
                return Ok(inference_pools.inference_pool_ids);
            }
        }

        Ok(vec![])
    }
}
