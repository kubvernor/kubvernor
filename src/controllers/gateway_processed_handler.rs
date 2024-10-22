use std::sync::Arc;

use gateway_api::apis::standard::{
    gateways::{Gateway, GatewayStatusListeners, GatewayStatusListenersSupportedKinds},
    httproutes::{HTTPRoute, HTTPRouteStatus, HTTPRouteStatusParents, HTTPRouteStatusParentsParentRef},
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{Resource, ResourceExt};
use tokio::sync::mpsc::Sender;
use tracing::debug;

use crate::{
    common::{GatewayProcessedPayload, ListenerStatus, ResourceKey, Route},
    controllers::ControllerError,
    patchers::{Operation, PatchContext},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;
pub struct GatewayProcessedHandler<'a> {
    pub gateway_processed_payload: GatewayProcessedPayload,
    pub gateway: Arc<Gateway>,
    pub state: &'a State,
    pub log_context: String,
    pub resource_key: ResourceKey,
    pub route_patcher: Sender<Operation<HTTPRoute>>,
    pub controller_name: String,
}

impl<'a> GatewayProcessedHandler<'a> {
    pub async fn deploy_gateway(&self) -> Result<Gateway> {
        let log_context = &self.log_context;
        let resource_key = &self.resource_key;
        let gateway = &(*self.gateway);
        let state = &self.state;

        let GatewayProcessedPayload {
            gateway_status,
            attached_routes,
            ignored_routes,
        } = &self.gateway_processed_payload;

        debug!("{log_context} Attached routes  {attached_routes:?}");
        let updated_gateway = self.update_gateway_resource(gateway, gateway_status.listeners.clone());
        for attached_route in attached_routes {
            let updated_route = self.update_accepted_route_parents(state, attached_route, resource_key);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let version = route.resource_version().clone();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key,
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        version,
                    }))
                    .await;
            }
        }
        debug!("{log_context} Ignored routes  {ignored_routes:?}");
        for ignored_route in ignored_routes {
            let updated_route = self.update_rejected_route_parents(state, ignored_route, resource_key);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let version = route.resource_version().clone();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key,
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        version,
                    }))
                    .await;
            }
        }

        Ok(updated_gateway)
    }

    fn update_gateway_resource(&self, gateway: &Gateway, processed_listeners: Vec<ListenerStatus>) -> Gateway {
        let log_context = &self.log_context;
        debug!("{log_context} listener statuses {processed_listeners:?}");
        let listener_statuses: Vec<_> = processed_listeners
            .into_iter()
            .map(|status| match status {
                ListenerStatus::Accepted((name, routes)) => GatewayStatusListeners {
                    attached_routes: routes,
                    conditions: vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: gateway.metadata.generation,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: gateway.metadata.generation,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                    ],
                    name,
                    supported_kinds: vec![GatewayStatusListenersSupportedKinds {
                        group: None,
                        kind: "HTTPRoute".to_owned(),
                    }],
                },
                ListenerStatus::Conflicted(name) => GatewayStatusListeners {
                    attached_routes: 0,
                    conditions: vec![Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: gateway.metadata.generation,
                        reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                        status: "False".to_owned(),
                        type_: gateway_api::apis::standard::constants::ListenerConditionType::Conflicted.to_string(),
                    }],
                    name,
                    supported_kinds: vec![],
                },
            })
            .collect();

        Self::update_status_conditions((*self.gateway).clone(), listener_statuses)
    }

    fn update_status_conditions(mut gateway: Gateway, listeners_status: Vec<GatewayStatusListeners>) -> Gateway {
        let observed_generation = gateway.metadata.generation;
        let mut status = gateway.status.unwrap_or_default();
        let mut conditions = status.conditions.unwrap_or_default();

        conditions.retain(|f| f.type_ != gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string());
        for f in &mut conditions {
            f.last_transition_time = Time(Utc::now());
            f.observed_generation = observed_generation;
            f.status = "True".to_owned();
            f.reason = gateway_api::apis::standard::constants::GatewayConditionReason::Ready.to_string();
        }

        let new_condition = Condition {
            last_transition_time: Time(Utc::now()),
            message: "Updated by controller".to_owned(),
            observed_generation,
            reason: gateway_api::apis::standard::constants::GatewayConditionReason::Ready.to_string(),
            status: "True".to_owned(),
            type_: gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string(),
        };
        conditions.push(new_condition);
        status.conditions = Some(conditions);
        status.listeners = Some(listeners_status);
        gateway.status = Some(status);
        gateway.metadata.managed_fields = None;
        gateway
    }

    fn update_accepted_route_parents(&self, state: &State, attached_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        self.update_route_parents(
            state,
            attached_route,
            gateway_id,
            (
                "Accepted",
                Condition {
                    last_transition_time: Time(Utc::now()),
                    message: "Updated by controller".to_owned(),
                    observed_generation: None,
                    reason: "Accepted".to_owned(),
                    status: "True".to_owned(),
                    type_: "Accepted".to_owned(),
                },
            ),
        )
    }

    fn update_rejected_route_parents(&self, state: &State, rejected_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        self.update_route_parents(
            state,
            rejected_route,
            gateway_id,
            (
                "Rejected",
                Condition {
                    last_transition_time: Time(Utc::now()),
                    message: "Updated by controller".to_owned(),
                    observed_generation: None,
                    reason: "Rejected".to_owned(),
                    status: "False".to_owned(),
                    type_: "Rejected".to_owned(),
                },
            ),
        )
    }

    fn update_route_parents(&self, state: &State, route: &Route, gateway_id: &ResourceKey, (new_condition_name, mut new_condition): (&'static str, Condition)) -> Option<HTTPRoute> {
        let routes = state.get_http_routes_attached_to_gateway(gateway_id);

        if let Some(routes) = routes {
            let route = routes
                .iter()
                .find(|f| f.metadata.name == Some(route.name().to_owned()) && f.metadata.namespace == Some(route.namespace().clone()));

            if let Some(mut route) = route.map(|r| (***r).clone()) {
                new_condition.observed_generation = route.meta().generation;
                let mut status = if let Some(status) = route.status { status } else { HTTPRouteStatus { parents: vec![] } };

                status
                    .parents
                    .retain(|p| !(p.controller_name == self.controller_name && p.parent_ref.namespace == self.gateway.meta().namespace && self.gateway.meta().name == Some(p.parent_ref.name.clone())));

                let route_parents = HTTPRouteStatusParents {
                    conditions: Some(vec![new_condition]),
                    controller_name: self.controller_name.clone(),
                    parent_ref: HTTPRouteStatusParentsParentRef {
                        namespace: self.gateway.meta().namespace.clone(),
                        name: self.gateway.meta().name.clone().unwrap_or_default(),
                        ..Default::default()
                    },
                };
                status.parents.push(route_parents);
                route.status = Some(status);
                route.metadata.managed_fields = None;
                return Some(route);
            }
        }
        None
    }
}
