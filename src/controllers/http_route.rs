use crate::state::{
    ResourceKey, State, DEFAULT_GROUP_NAME, DEFAULT_KIND_NAME, DEFAULT_NAMESPACE_NAME,
};

use super::utils::ResourceState;
use super::{ControllerError, RECONCILE_WAIT};

use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use gateway_api::apis::standard::httproutes::HTTPRoute;
use gateway_api::apis::standard::httproutes::{self, HTTPRouteParentRefs};

use kube::runtime::controller::Action;
use kube::runtime::watcher::Config;
use kube::runtime::Controller;
use kube::{Api, ResourceExt};
use log::{debug, info};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

type Result<T, E = ControllerError> = std::result::Result<T, E>;

#[derive(Clone)]
struct Context {
    pub client: kube::Client,
    pub controller_name: String,
    state: Arc<Mutex<State>>,
}

pub struct HttpRouteController {
    pub controller_name: String,
    client: kube::Client,
    state: Arc<Mutex<State>>,
}

impl From<(&HTTPRouteParentRefs, String)> for ResourceKey {
    fn from((route_parent, route_namespace): (&HTTPRouteParentRefs, String)) -> Self {
        Self {
            group: route_parent
                .group
                .clone()
                .unwrap_or(DEFAULT_GROUP_NAME.to_owned()),
            namespace: route_parent
                .namespace
                .clone()
                .unwrap_or(route_namespace.clone()),
            name: route_parent.name.clone(),
            kind: route_parent
                .kind
                .clone()
                .unwrap_or(DEFAULT_KIND_NAME.to_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum RouteState {
    New,
    PreviouslyProcessed,
    Added,
    Deleted,
}
impl From<ResourceState> for RouteState {
    fn from(value: ResourceState) -> Self {
        match value {
            ResourceState::Existing => RouteState::Added,
            ResourceState::NotSeenBefore => RouteState::New,
            ResourceState::Deleted => RouteState::Deleted,
        }
    }
}

impl HttpRouteController {
    pub(crate) fn new(
        controller_name: String,
        client: kube::Client,
        state: Arc<Mutex<State>>,
    ) -> Self {
        HttpRouteController {
            controller_name,
            client,
            state,
        }
    }
    pub fn get_controller(&self) -> BoxFuture<()> {
        let context = Arc::new(Context {
            client: self.client.clone(),
            controller_name: self.controller_name.clone(),
            state: Arc::clone(&self.state),
        });

        let api = Api::<HTTPRoute>::namespaced(self.client.clone(), "default");
        Controller::new(api, Config::default())
            .run(
                Self::reconcile_http_route,
                Self::error_policy,
                Arc::clone(&context),
            )
            .for_each(|_| futures::future::ready(()))
            .boxed()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn error_policy<T>(_object: Arc<T>, _err: &ControllerError, _ctx: Arc<Context>) -> Action {
        Action::requeue(RECONCILE_WAIT)
    }

    #[allow(clippy::unused_async)]
    async fn reconcile_http_route(
        route: Arc<httproutes::HTTPRoute>,
        ctx: Arc<Context>,
    ) -> Result<Action> {
        let client = &ctx.client;
        let controller_name = &ctx.controller_name;
        let resource_version = &route.metadata.resource_version;
        let resource_name = route.name_any();
        let state = &ctx.state;

        let id = Uuid::parse_str(&route.metadata.uid.clone().ok_or(
            ControllerError::InvalidPayload("Uid must be present".to_owned()),
        )?)
        .map_err(|e| ControllerError::InvalidPayload(format!("Uid in wrong format {e}")))?;

        info!("reconcile_route: {controller_name} {id} {resource_name} {resource_version:?}");
        debug!(
            "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} {route:?}",
        );

        let mut state = state.lock().await;
        let maybe_added = state.get_http_route_by_id(id);

        let route_state = Self::check_state(&route, maybe_added);
        info!(
            "reconcile_route: {controller_name} {id} {resource_name} {resource_version:?} {route_state:?}",
        );
        match route_state {
            RouteState::New | RouteState::PreviouslyProcessed => {
                let empty = vec![];
                let parent_gateway_refs = route.spec.parent_refs.as_ref().unwrap_or(&empty);
                let gateways: Vec<_> = parent_gateway_refs
                    .iter()
                    .map(|k| {
                        (
                            k,
                            route
                                .metadata
                                .namespace
                                .clone()
                                .unwrap_or(DEFAULT_NAMESPACE_NAME.to_owned()),
                        )
                    })
                    .map(ResourceKey::from)
                    .filter_map(|key| state.get_gateway_by_resource(&key))
                    .collect();

                if !gateways.is_empty() {
                    state.save_http_route(id, &route);
                }

                Ok(Action::await_change())
            }

            RouteState::Added => Err(ControllerError::AlreadyAdded),

            RouteState::Deleted => {
                state.delete_http_route(id);
                Ok(Action::await_change())
            }
        }
    }

    fn check_state(route: &HTTPRoute, maybe_added: Option<&Arc<HTTPRoute>>) -> RouteState {
        let route_state = RouteState::from(ResourceState::check_state(route, maybe_added));

        if RouteState::New != route_state {
            return route_state;
        }

        if let Some(status) = route.status.as_ref() {
            if !status.parents.is_empty() {
                return RouteState::PreviouslyProcessed;
            }
        };

        route_state
    }
}
