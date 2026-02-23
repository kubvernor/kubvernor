// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use std::collections::{BTreeMap, BTreeSet};

use futures::FutureExt;
use gateway_api_inference_extension::inferencepools::InferencePool;
use k8s_openapi::api::core::v1::Service;
use kube::Client;
use typed_builder::TypedBuilder;

use super::ReferencesResolver;
use crate::{
    common::{Backend, Gateway, ReferenceValidateRequest, ResourceKey},
    controllers::find_linked_routes,
    state::State,
};

#[derive(Clone, TypedBuilder)]
pub struct BackendReferenceResolver {
    #[builder(setter(transform = |client:Client, reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>| ReferencesResolver::builder().client(client).reference_validate_channel_sender(reference_validate_channel_sender).build()))]
    reference_resolver: ReferencesResolver<Service>,
    #[builder(setter(transform = |client:Client, reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>| ReferencesResolver::builder().client(client).reference_validate_channel_sender(reference_validate_channel_sender).build()))]
    inference_pool_reference_resolver: ReferencesResolver<InferencePool>,
    state: State,
}

impl BackendReferenceResolver {
    pub async fn add_references_by_gateway(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();

        let service_references = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);
            linked_routes
                .into_iter()
                .map(|route| {
                    (
                        route.resource_key().clone(),
                        route
                            .backends()
                            .iter()
                            .filter_map(|b| match b {
                                Backend::Maybe(backend_type) => Some(backend_type.resource_key()),
                                _ => None,
                            })
                            .filter(|r| r.kind == "Service")
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };

        let inference_pool_references = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);

            linked_routes
                .into_iter()
                .map(|route| {
                    (
                        route.resource_key().clone(),
                        route
                            .backends()
                            .iter()
                            .filter_map(|b| match b {
                                Backend::Maybe(backend_type) => Some(backend_type.resource_key()),
                                _ => None,
                            })
                            .filter(|r| r.kind == "InferencePool")
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };

        self.reference_resolver.add_references_for_gateway(gateway_key, service_references).await;
        self.inference_pool_reference_resolver.add_references_for_gateway(gateway_key, inference_pool_references).await;
    }

    pub async fn delete_all_references(&self, reference_keys: BTreeSet<ResourceKey>) -> BTreeSet<ResourceKey> {
        let service_references = self.reference_resolver.delete_references(reference_keys.iter().cloned()).await;
        let inference_pool_references = self.inference_pool_reference_resolver.delete_references(reference_keys.iter().cloned()).await;
        service_references.into_iter().chain(inference_pool_references.into_iter()).collect()
    }

    pub async fn add_references(&self, reference_keys: BTreeSet<ResourceKey>) {
        let service_references = reference_keys.iter().filter(|k| k.kind == "Service").cloned();
        let inference_pool_references = reference_keys.iter().filter(|k| k.kind == "InferencePool").cloned();
        self.reference_resolver.add_references(service_references).await;
        self.inference_pool_reference_resolver.add_references(inference_pool_references).await;
    }

    pub async fn delete_references_by_gateway(&self, gateway: &Gateway) {
        let gateway_key = gateway.key();
        let service_references = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);

            let mut backend_reference_keys = BTreeSet::new();
            for route in linked_routes {
                for backend in &route.backends() {
                    if let Backend::Maybe(backend_type) = backend {
                        backend_reference_keys.insert(backend_type.resource_key());
                    }
                }
            }
            backend_reference_keys
        };

        let inference_pool_references = || {
            let linked_routes = find_linked_routes(&self.state, gateway_key);

            let mut backend_reference_keys = BTreeSet::new();
            for route in linked_routes {
                for backend in &route.backends() {
                    if let Backend::Maybe(backend_type) = backend {
                        backend_reference_keys.insert(backend_type.resource_key());
                    }
                }
            }
            backend_reference_keys.into_iter().filter(|k| k.kind == "InferencePool").collect()
        };
        self.reference_resolver.delete_references_for_gateway(gateway_key, service_references).await;
        self.inference_pool_reference_resolver.delete_references_for_gateway(gateway_key, inference_pool_references).await;
    }

    pub async fn get_service_reference(&self, resource_key: &ResourceKey) -> Option<Service> {
        self.reference_resolver.get_reference(resource_key).await
    }
    pub async fn get_inference_pool_reference(&self, resource_key: &ResourceKey) -> Option<InferencePool> {
        self.inference_pool_reference_resolver.get_reference(resource_key).await
    }

    pub async fn resolve(&self) -> crate::Result<()> {
        let reference_resolver = self.reference_resolver.resolve().boxed();
        let inference_pool_resolver = self.inference_pool_reference_resolver.resolve().boxed();

        futures::future::join_all(vec![reference_resolver, inference_pool_resolver]).await;
        Ok(())
    }
}
