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
use kubvernor_common::ResourceKey;
use kubvernor_state::State;
use typed_builder::TypedBuilder;

use super::multi_references_resolver::MultiReferencesResolver;
use crate::{
    common::{Backend, Gateway, ReferenceValidateRequest},
    controllers::find_linked_routes,
};

#[derive(Clone, TypedBuilder)]
pub struct BackendReferenceResolver {
    #[builder(setter(transform = |client:Client, reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>
        | MultiReferencesResolver::builder().client(client).reference_validate_channel_sender(reference_validate_channel_sender).build()))]
    reference_resolver: MultiReferencesResolver<Service>,
    #[builder(setter(transform = |client:Client, reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>
        | MultiReferencesResolver::builder().client(client).reference_validate_channel_sender(reference_validate_channel_sender).build()))]
    inference_pool_reference_resolver: MultiReferencesResolver<InferencePool>,
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
                            .chain(&route.filter_backends())
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

    pub async fn delete_route_references(&self, route_key: &ResourceKey, reference_keys: &BTreeSet<ResourceKey>) -> BTreeSet<ResourceKey> {
        let service_references = self.reference_resolver.delete_route_references(route_key, reference_keys.iter().cloned()).await;
        let inference_pool_references =
            self.inference_pool_reference_resolver.delete_route_references(route_key, reference_keys.iter().cloned()).await;
        service_references.into_iter().chain(inference_pool_references.into_iter()).collect()
    }

    pub async fn delete_references_by_gateway(&self, gateway: &Gateway) {
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
                            .chain(&route.filter_backends())
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
