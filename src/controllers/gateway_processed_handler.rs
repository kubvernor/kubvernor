use gateway_api::apis::standard::{
    gateways::{Gateway, GatewayStatusAddresses, GatewayStatusListeners, GatewayStatusListenersSupportedKinds},
    httproutes::{HTTPRoute, HTTPRouteStatus, HTTPRouteStatusParents, HTTPRouteStatusParentsParentRef},
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::{Resource, ResourceExt};
use tokio::sync::{mpsc::Sender, oneshot};
use tracing::{debug, error, warn};

use crate::{
    common::{DeployedGatewayStatus, GatewayProcessedPayload, ListenerStatus, ResourceKey, Route},
    controllers::ControllerError,
    patchers::{Operation, PatchContext},
    state::State,
};

use super::utils::ListenerStatusesMerger;

type Result<T, E = ControllerError> = std::result::Result<T, E>;
pub struct GatewayProcessedHandler<'a> {
    pub gateway_processed_payload: GatewayProcessedPayload,
    pub gateway: Gateway,
    pub state: &'a State,
    pub log_context: &'a str,
    pub resource_key: &'a ResourceKey,
    pub route_patcher: Sender<Operation<HTTPRoute>>,
    pub controller_name: String,
}

impl<'a> GatewayProcessedHandler<'a> {
    pub async fn deploy_gateway(mut self) -> Result<Gateway> {
        let GatewayProcessedPayload {
            deployed_gateway_status: gateway_status,
            attached_routes,
            ignored_routes,
        } = &self.gateway_processed_payload.clone();

        debug!("{} Attached routes {attached_routes:?}", &self.log_context);
        self.update_gateway_resource(gateway_status);
        self.update_routes(attached_routes, ignored_routes).await;
        Ok(self.gateway)
    }

    fn update_gateway_resource(&mut self, deployed_gateway_status: &DeployedGatewayStatus) {
        let log_context = &self.log_context;

        debug!("{log_context} listener statuses {:?}", &deployed_gateway_status.listeners);

        //let listener_statuses = Self::generate_processed_listeners_conditions(gateway.metadata.generation, deployed_gateway_status);
        let listener_statuses = vec![];
        Self::update_gateway_status_conditions(&mut self.gateway, listener_statuses);
        Self::update_gateway_status_addresses(&mut self.gateway, &deployed_gateway_status.attached_addresses);
    }

    fn update_gateway_status_conditions(gateway: &mut Gateway, listeners_status: Vec<GatewayStatusListeners>) {
        let observed_generation = gateway.metadata.generation;
        let mut status = gateway.status.clone().unwrap_or_default();
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
        if status.listeners.is_none() && !listeners_status.is_empty() {
            error!("Status listeners should not be empty here");
        }
        status.listeners = Some(ListenerStatusesMerger::new(status.listeners.clone().unwrap_or_default()).merge(listeners_status));
        gateway.status = Some(status);
        gateway.metadata.managed_fields = None;
    }

    async fn update_routes(&self, attached_routes: &[Route], ignored_routes: &[Route]) {
        let gateway_id = &self.resource_key;
        let log_context = &self.log_context;
        for attached_route in attached_routes {
            let updated_route = self.update_accepted_route_parents(attached_route, gateway_id);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let version = route.resource_version().clone();
                let (sender, receiver) = oneshot::channel();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key.clone(),
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        version,
                        response_sender: sender,
                    }))
                    .await;
                let patched_route = receiver.await;
                if let Ok(maybe_patched) = patched_route {
                    match maybe_patched {
                        Ok(_patched_route) => {}
                        Err(e) => {
                            warn!("{log_context} Error while patching {e}");
                        }
                    }
                }
            }
        }
        debug!("{log_context} Ignored routes  {ignored_routes:?}");
        for ignored_route in ignored_routes {
            let updated_route = self.update_rejected_route_parents(ignored_route, gateway_id);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let version = route.resource_version().clone();
                let (sender, receiver) = oneshot::channel();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key.clone(),
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        version,
                        response_sender: sender,
                    }))
                    .await;

                let patched_route = receiver.await;
                if let Ok(maybe_patched) = patched_route {
                    match maybe_patched {
                        Ok(_patched_route) => {
                            //patched_route.metadata.resource_version = None;
                            //self.state.save_http_route(route_resource_key, &Arc::new(patched_route));
                        }
                        Err(e) => {
                            warn!("{log_context} Error while patching {e}");
                        }
                    }
                }
            }
        }
    }

    fn update_accepted_route_parents(&self, attached_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        self.update_route_parents(
            attached_route,
            gateway_id,
            (
                "Accepted",
                vec![
                    Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: None,
                        reason: "Accepted".to_owned(),
                        status: "True".to_owned(),
                        type_: "Accepted".to_owned(),
                    },
                    Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: None,
                        reason: "ResolvedRefs".to_owned(),
                        status: "True".to_owned(),
                        type_: "ResolvedRefs".to_owned(),
                    },
                ],
            ),
        )
    }

    fn update_rejected_route_parents(&self, rejected_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        let conditions = match rejected_route.resolution_status() {
            crate::common::ResolutionStatus::Resolved => vec![Condition {
                last_transition_time: Time(Utc::now()),
                message: "Updated by controller".to_owned(),
                observed_generation: None,
                reason: gateway_api::apis::standard::constants::ListenerConditionReason::Invalid.to_string(),
                status: "False".to_owned(),
                type_: gateway_api::apis::standard::constants::ListenerConditionType::Conflicted.to_string(),
            }],

            crate::common::ResolutionStatus::PartiallyResolved | crate::common::ResolutionStatus::NotResolved => {
                vec![
                    Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: None,
                        reason: gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs.to_string(),
                        status: "False".to_owned(),
                        type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                    },
                    Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: None,
                        reason: gateway_api::apis::standard::constants::ListenerConditionReason::Programmed.to_string(),
                        status: "False".to_owned(),
                        type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                    },
                ]
            }
        };
        self.update_route_parents(rejected_route, gateway_id, ("Rejected", conditions))
    }

    fn update_route_parents(&self, route: &Route, gateway_id: &ResourceKey, (new_condition_name, mut new_conditions): (&'static str, Vec<Condition>)) -> Option<HTTPRoute> {
        let routes = self.state.get_http_routes_attached_to_gateway(gateway_id);

        if let Some(routes) = routes {
            let route = routes
                .iter()
                .find(|f| f.metadata.name == Some(route.name().to_owned()) && f.metadata.namespace == Some(route.namespace().clone()));

            if let Some(mut route) = route.map(|r| (***r).clone()) {
                new_conditions.iter_mut().for_each(|f| f.observed_generation = route.meta().generation);

                let mut status = if let Some(status) = route.status { status } else { HTTPRouteStatus { parents: vec![] } };

                status
                    .parents
                    .retain(|p| !(p.controller_name == self.controller_name && p.parent_ref.namespace == self.gateway.meta().namespace && self.gateway.meta().name == Some(p.parent_ref.name.clone())));

                let route_parents = HTTPRouteStatusParents {
                    conditions: Some(new_conditions),
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

    fn update_gateway_status_addresses(gateway: &mut Gateway, attached_addresses: &[String]) {
        let mut status = gateway.status.clone().unwrap_or_default();
        status.addresses = Some(attached_addresses.iter().map(|a| GatewayStatusAddresses { r#type: None, value: a.clone() }).collect::<Vec<_>>());
        gateway.status = Some(status);
    }

    fn generate_processed_listeners_conditions(generation: Option<i64>, deployed_gateway_status: &DeployedGatewayStatus) -> Vec<GatewayStatusListeners> {
        deployed_gateway_status
            .listeners
            .iter()
            .map(|status| match status {
                ListenerStatus::Accepted((name, routes)) => GatewayStatusListeners {
                    attached_routes: *routes,
                    conditions: vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: generation,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: generation,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Programmed.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                        },
                    ],
                    name: name.clone(),
                    supported_kinds: vec![GatewayStatusListenersSupportedKinds {
                        group: None,
                        kind: "HTTPRoute".to_owned(),
                    }],
                },
                ListenerStatus::Conflicted(name) => GatewayStatusListeners {
                    attached_routes: 1,
                    conditions: vec![Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: generation,
                        reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                        status: "False".to_owned(),
                        type_: gateway_api::apis::standard::constants::ListenerConditionType::Conflicted.to_string(),
                    }],
                    name: name.clone(),
                    supported_kinds: vec![GatewayStatusListenersSupportedKinds {
                        group: None,
                        kind: "HTTPRoute".to_owned(),
                    }],
                },
            })
            .collect()
    }
}
