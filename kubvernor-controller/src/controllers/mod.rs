// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

pub mod gateway;
pub mod gateway_class;
mod handlers;
pub mod inference_pool;
pub mod route;
mod utils;

use std::time::Duration;

use kube_core::ObjectMeta;
use kubvernor_common::ResourceKey;
pub use utils::{FinalizerPatcher, HostnameMatchFilter, ListenerTlsConfigValidator, ResourceFinalizer, RoutesResolver, find_linked_routes};

use crate::services::patchers::{FinalizerContext, Operation};

#[allow(dead_code)]
#[derive(thiserror::Error, Debug, PartialEq, PartialOrd)]
pub enum ControllerError {
    PatchFailed,
    AlreadyAdded,
    InvalidPayload(String),
    InvalidRecipent,
    FinalizerPatchFailed(String),
    BackendError,
    UnknownResource,
    UnknownGatewayClass(String),
    UnknownGatewayType,
    ResourceInWrongState,
    ResourceHasWrongStatus,
}

const RECONCILE_LONG_WAIT: Duration = Duration::from_secs(3600);
const RECONCILE_ERROR_WAIT: Duration = Duration::from_secs(100);

const TARGET: &str = "kubvernor::controller";

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

pub fn needs_finalizer<T: serde::Serialize>(
    resource_key: &ResourceKey,
    controller_name: &String,
    resource_meta: &ObjectMeta,
) -> Option<Operation<T>> {
    let has_finalizer = if let Some(finalizers) = resource_meta.finalizers.as_ref() { finalizers.contains(controller_name) } else { false };

    if has_finalizer {
        None
    } else {
        Some(Operation::PatchFinalizer(FinalizerContext {
            resource_key: resource_key.clone(),
            controller_name: controller_name.clone(),
            finalizer_name: controller_name.clone(),
        }))
    }
}
