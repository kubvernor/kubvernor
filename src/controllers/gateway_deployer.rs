use std::{collections::HashMap, sync::Arc};

use gateway_api::apis::standard::{
    gateways::{Gateway, GatewayStatusListeners, GatewayStatusListenersSupportedKinds},
    httproutes::HTTPRoute,
};

use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::Client;
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tracing::{debug, warn};

use crate::{
    common::{self, ChangedContext, GatewayEvent, GatewayResponse, ResolvedRefs},
    controllers::{
        gateway_processed_handler::GatewayProcessedHandler,
        utils::{ListenerTlsConfigValidator, RoutesValidator},
        ControllerError,
    },
    patchers::Operation,
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

pub struct GatewayDeployer<'a> {
    pub client: Client,
    pub log_context: &'a str,
    pub sender: Sender<GatewayEvent>,
    pub gateway: &'a Arc<Gateway>,
    pub state: &'a State,

    pub http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    pub controller_name: &'a str,
}
impl<'a> GatewayDeployer<'a> {
    pub async fn deploy_gateway(&mut self) -> Result<Gateway> {
        let log_context = self.log_context;
        let mut updated_gateway = (**self.gateway).clone();

        let maybe_gateway = common::Gateway::try_from(&updated_gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };

        let backend_gateway = ListenerTlsConfigValidator::new(backend_gateway, self.client.clone(), log_context).validate().await;
        let (mut backend_gateway, filtered_mapping, mut unresolved_linked_routes) = RoutesValidator::new(backend_gateway, self.client.clone(), log_context, self.state, self.gateway).validate().await;
        self.adjust_statuses(&mut backend_gateway);
        self.resolve_listeners_statuses(&backend_gateway, &mut updated_gateway);
        debug!("Effective gateway {:#?}", backend_gateway);

        let (response_sender, response_receiver) = oneshot::channel();
        let resource_key = backend_gateway.key().clone();
        let listener_event = GatewayEvent::GatewayChanged(ChangedContext::new(response_sender, backend_gateway, updated_gateway.clone(), filtered_mapping));
        let _ = self.sender.send(listener_event).await;
        let response = response_receiver.await;

        if let Ok(GatewayResponse::GatewayProcessed(mut gateway_processed)) = response {
            gateway_processed.ignored_routes.append(&mut unresolved_linked_routes);
            let gateway_event_handler = GatewayProcessedHandler {
                gateway_processed_payload: gateway_processed,
                gateway: updated_gateway,
                state: self.state,
                log_context,
                resource_key: &resource_key,
                route_patcher: self.http_route_patcher.clone(),
                controller_name: self.controller_name.to_owned(),
                per_listener_calculated_attached_routes: HashMap::new(),
            };
            gateway_event_handler.deploy_gateway().await
        } else {
            warn!("{log_context} {response:?} ... Problem {response:?}");
            Err(ControllerError::BackendError)
        }
    }

    fn adjust_statuses(&self, gateway: &mut common::Gateway) {
        gateway.listeners_mut().for_each(|l| {
            let conditions = l.conditions_mut();
            debug!("Adjusting conditions {conditions:#?}");

            if conditions.contains(&common::ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![]))) {
                conditions.replace(common::ListenerCondition::Accepted);
                conditions.replace(common::ListenerCondition::Programmed);
            };

            if conditions.contains(&common::ListenerCondition::ResolvedRefs(ResolvedRefs::ResolvedWithNotAllowedRoutes(vec![]))) {
                conditions.replace(common::ListenerCondition::Accepted);
                conditions.replace(common::ListenerCondition::Programmed);
            };

            if conditions.contains(&common::ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(vec![]))) {
                conditions.replace(common::ListenerCondition::Accepted);
            };

            if conditions.contains(&common::ListenerCondition::ResolvedRefs(ResolvedRefs::UnresolvedRoutes)) {
                conditions.replace(common::ListenerCondition::NotAccepted);
                conditions.replace(common::ListenerCondition::NotProgrammed);
            };
            debug!("Adjusted  conditions {conditions:#?}");
        });
    }
    fn resolve_listeners_statuses(&self, gateway: &common::Gateway, kube_gateway: &mut Gateway) {
        let log_context = self.log_context;
        let mut status = kube_gateway.status.clone().unwrap_or_default();
        let mut listeners_statuses = vec![];
        let generation = kube_gateway.metadata.generation;

        gateway.listeners().for_each(|l| {
            let name = l.name().to_owned();
            debug!("{log_context} Processing listener {name}");

            let mut listener_status = GatewayStatusListeners { name, ..Default::default() };

            let listener_conditions = &mut listener_status.conditions;

            for condition in l.conditions() {
                let (status, type_, reason) = condition.resolved_type();
                let status = status.to_owned();
                let type_ = type_.to_string();
                let reason = reason.to_string();
                match condition {
                    common::ListenerCondition::ResolvedRefs(
                        ResolvedRefs::Resolved(allowed_routes) | ResolvedRefs::ResolvedWithNotAllowedRoutes(allowed_routes) | ResolvedRefs::InvalidCertificates(allowed_routes),
                    ) => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                        listener_status.supported_kinds = allowed_routes.iter().map(|r| GatewayStatusListenersSupportedKinds { group: None, kind: r.clone() }).collect();
                    }

                    _ => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: "Updated by controller".to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                        listener_status.supported_kinds = vec![];
                    }
                }
            }
            listeners_statuses.push(listener_status);
        });
        status.listeners = Some(listeners_statuses);
        kube_gateway.status = Some(status);
    }
}
