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
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gateway_api::{
    common::RouteStatus,
    grpcroutes::{self, GRPCRoute},
};
use kube::{
    Api, Client, Resource,
    runtime::{Controller, controller::Action, watcher::Config},
};
use kubvernor_common::ResourceKey;
use kubvernor_state::State;
use tokio::sync::mpsc::{self};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::routes_common::CommonRouteHandler;
use crate::{
    common::{ReferenceValidateRequest, Route},
    controllers::{
        ControllerError, RECONCILE_LONG_WAIT,
        handlers::ResourceHandler,
        utils::{ResourceCheckerArgs, ResourceState},
    },
    services::patchers::{DeleteContext, Operation},
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone, TypedBuilder)]
pub struct GRPCRouteControllerContext {
    controller_name: String,
    client: Client,
    state: State,
    grpc_route_patcher: mpsc::Sender<Operation<GRPCRoute>>,
    validate_references_channel_sender: mpsc::Sender<ReferenceValidateRequest>,
}

#[derive(TypedBuilder)]
pub struct GRPCRouteController {
    ctx: Arc<GRPCRouteControllerContext>,
}

impl GRPCRouteController {
    pub fn get_controller(&'_ self) -> BoxFuture<'_, ()> {
        let client = self.ctx.client.clone();
        let context = &self.ctx;

        Controller::new(Api::all(client), Config::default())
            .run(Self::reconcile_grpc_route, Self::error_policy, Arc::clone(context))
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<GRPCRouteControllerContext>) -> Action {
        Action::requeue(RECONCILE_LONG_WAIT)
    }

    async fn reconcile_grpc_route(resource: Arc<grpcroutes::GRPCRoute>, ctx: Arc<GRPCRouteControllerContext>) -> Result<Action> {
        let controller_name = ctx.controller_name.clone();
        let grpc_route_patcher = ctx.grpc_route_patcher.clone();

        let Some(maybe_id) = resource.metadata.uid.clone() else {
            return Err(ControllerError::InvalidPayload("Uid must be present".to_owned()));
        };

        let Ok(_) = Uuid::parse_str(&maybe_id) else {
            return Err(ControllerError::InvalidPayload("Uid in wrong format".to_owned()));
        };
        let version = resource.meta().resource_version.clone();
        let resource_key = ResourceKey::from(&*resource);

        let state = &ctx.state;

        let maybe_stored_route = state.get_grpc_route_by_id(&resource_key).expect("We expect the lock to work");

        let _ = Route::try_from(&*resource)?;

        let handler = GRPCRouteHandler::builder()
            .common_handler(
                CommonRouteHandler::builder()
                    .controller_name(controller_name)
                    .references_validator_sender(ctx.validate_references_channel_sender.clone())
                    .route_patcher_sender(grpc_route_patcher)
                    .resource(resource)
                    .resource_key(resource_key)
                    .state(state.clone())
                    .version(version)
                    .build(),
            )
            .build();

        handler.process(maybe_stored_route, Self::check_spec, Self::check_status).await
    }

    fn check_spec(args: ResourceCheckerArgs<GRPCRoute>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.spec == stored_resource.spec { ResourceState::SpecNotChanged } else { ResourceState::SpecChanged }
    }

    fn check_status(args: ResourceCheckerArgs<GRPCRoute>) -> ResourceState {
        let (resource, stored_resource) = args;
        if resource.status == stored_resource.status { ResourceState::StatusNotChanged } else { ResourceState::StatusChanged }
    }
}

#[derive(TypedBuilder)]
struct GRPCRouteHandler<R: serde::Serialize> {
    common_handler: CommonRouteHandler<R>,
}

#[async_trait]
impl ResourceHandler<GRPCRoute> for GRPCRouteHandler<GRPCRoute> {
    fn state(&self) -> &State {
        &self.common_handler.state
    }

    fn version(&self) -> String {
        self.common_handler.version.clone().unwrap_or_default()
    }

    fn resource(&self) -> Arc<GRPCRoute> {
        Arc::clone(&self.common_handler.resource)
    }

    fn resource_key(&self) -> ResourceKey {
        self.common_handler.resource_key.clone()
    }

    async fn on_spec_not_changed(&self, id: ResourceKey, resource: &Arc<GRPCRoute>, state: &State) -> Result<Action> {
        let () = state.save_grpc_route(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_status_not_changed(&self, id: ResourceKey, resource: &Arc<GRPCRoute>, state: &State) -> Result<Action> {
        let () = state.maybe_save_grpc_route(id, resource).expect("We expect the lock to work");
        Err(ControllerError::AlreadyAdded)
    }

    async fn on_new(&self, id: ResourceKey, resource: &Arc<GRPCRoute>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_status_changed(&self, _id: ResourceKey, _resource: &Arc<GRPCRoute>, _state: &State) -> Result<Action> {
        Ok(Action::await_change())
    }

    async fn on_spec_changed(&self, id: ResourceKey, resource: &Arc<GRPCRoute>, state: &State) -> Result<Action> {
        self.on_new_or_changed(id, resource, state).await
    }

    async fn on_deleted(&self, id: ResourceKey, resource: &Arc<GRPCRoute>, state: &State) -> Result<Action> {
        self.on_deleted(id, resource, state).await
    }
}

impl GRPCRouteHandler<GRPCRoute> {
    async fn on_new_or_changed(&self, route_key: ResourceKey, resource: &Arc<GRPCRoute>, _state: &State) -> Result<Action> {
        let Some(parent_gateway_refs) = resource.spec.parent_refs.as_ref() else {
            return Err(ControllerError::InvalidPayload("Route with no parents".to_owned()));
        };

        self.common_handler
            .on_new_or_changed(
                route_key.clone(),
                parent_gateway_refs,
                resource.metadata.generation,
                |state: &State, route_status: Option<RouteStatus>| {
                    let mut route = (**resource).clone();
                    route.status = route_status;
                    let () = state.save_grpc_route(route_key.clone(), &Arc::new(route)).expect("We expect the lock to work");
                },
            )
            .await
    }

    async fn on_deleted(&self, route_key: ResourceKey, resource: &Arc<GRPCRoute>, _: &State) -> Result<Action> {
        let _ = Route::try_from(&**resource)?;

        let Some(parent_gateway_refs) = resource.spec.parent_refs.as_ref() else {
            // we got delete for the route that we don't know anything about... which means that there must be a finalizer with our name so let's just delte it
            let _ = self
                .common_handler
                .route_patcher_sender
                .send(Operation::Delete(DeleteContext {
                    resource_key: route_key,
                    resource: (**resource).clone(),
                    controller_name: self.common_handler.controller_name.clone(),
                }))
                .await;
            return Err(ControllerError::InvalidPayload("Route with no parents".to_owned()));
        };

        self.common_handler.on_deleted(route_key, parent_gateway_refs).await
    }
}
