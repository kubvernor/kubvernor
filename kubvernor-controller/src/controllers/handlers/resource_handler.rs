// SPDX-FileCopyrightText: © 2026 Kubvernor authors
// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Kubvernor authors.
//         This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.
//         This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//         You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
//
//

use std::sync::Arc;

use async_trait::async_trait;
use kube::{Resource, ResourceExt, runtime::controller::Action};
use kubvernor_common::ResourceKey;
use kubvernor_state::State;
use log::info;

use super::{ControllerError, ResourceChecker, ResourceState, ResourceStateChecker};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

const TARGET: &str = "kubvernor::controllers::resource_handler";

#[async_trait]
pub trait ResourceHandler<R>
where
    R: k8s_openapi::serde::de::DeserializeOwned + Clone + std::fmt::Debug,
    R: ResourceExt,
    R: Resource<DynamicType = ()>,
    R: Send + Sync + 'static,
{
    async fn process(
        &self,
        stored_resource: Option<Arc<R>>,
        resource_spec_checker: ResourceChecker<R>,
        resource_status_checker: ResourceChecker<R>,
    ) -> Result<Action> {
        let resource = &Arc::clone(&self.resource());
        let id = self.resource_key();
        let state = self.state();

        let resource_state =
            ResourceStateChecker::check_status(resource, stored_resource.clone(), resource_spec_checker, resource_status_checker);

        info!(
            target: TARGET,  "{} {} version = {} Resource state {resource_state:?}",
            self.resource_key(),
            std::any::type_name::<R>().split("::").last().unwrap_or_default(),
            self.version()
        );

        match resource_state {
            ResourceState::VersionNotChanged => self.on_version_not_changed(id, resource, state).await,
            ResourceState::SpecNotChanged => self.on_spec_not_changed(id, resource, state).await,
            ResourceState::New => self.on_new(id, resource, state).await,
            ResourceState::SpecChanged => self.on_spec_changed(id, resource, state).await,
            ResourceState::StatusChanged => self.on_status_changed(id, resource, state).await,
            ResourceState::StatusNotChanged => self.on_status_not_changed(id, resource, state).await,
            ResourceState::Deleted => self.on_deleted(id, resource, state).await,
        }
    }

    async fn on_version_not_changed(&self, key: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        info!(
            target: TARGET,  "{key} {} version = {} Version not changed",
            std::any::type_name::<R>().split("::").last().unwrap_or_default(),
            self.version()
        );
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_not_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_spec_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_deleted(&self, _: ResourceKey, _: &Arc<R>, _: &State) -> Result<Action> {
        Err(ControllerError::AlreadyAdded)
    }

    fn state(&self) -> &State;
    fn resource_key(&self) -> ResourceKey;
    fn version(&self) -> String;
    fn resource(&self) -> Arc<R>;
}
