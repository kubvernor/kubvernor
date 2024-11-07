use std::sync::Arc;

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
use tracing::{debug, error, warn};

use crate::{
    common::{self, ChangedContext, GatewayEvent, GatewayResponse, ResolvedRefs},
    controllers::{
        gateway_processed_handler::GatewayProcessedHandler,
        utils::{ListenerTlsConfigValidator, RoutesResolver},
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
    pub kube_gateway: &'a Arc<Gateway>,
    pub state: &'a State,

    pub http_route_patcher: mpsc::Sender<Operation<HTTPRoute>>,
    pub controller_name: &'a str,
}
impl<'a> GatewayDeployer<'a> {
    pub async fn deploy_gateway(&mut self) -> Result<Gateway> {
        let log_context = self.log_context;
        let mut updated_kube_gateway = (**self.kube_gateway).clone();

        let maybe_gateway = common::Gateway::try_from(&updated_kube_gateway);
        let Ok(backend_gateway) = maybe_gateway else {
            warn!("{log_context} Misconfigured  gateway {maybe_gateway:?}");
            return Err(ControllerError::InvalidPayload("Misconfigured gateway".to_owned()));
        };

        let backend_gateway = ListenerTlsConfigValidator::new(backend_gateway, self.client.clone(), log_context).validate().await;
        let mut backend_gateway = RoutesResolver::new(backend_gateway, self.client.clone(), log_context, self.state, self.kube_gateway).validate().await;
        Self::adjust_statuses(&mut backend_gateway);
        self.resolve_listeners_statuses(&backend_gateway, &mut updated_kube_gateway);
        debug!("Effective gateway {:#?}", backend_gateway);

        let (response_sender, response_receiver) = oneshot::channel();
        let listener_event = GatewayEvent::GatewayChanged(ChangedContext::new(response_sender, backend_gateway));
        let _ = self.sender.send(listener_event).await;
        let response = response_receiver.await;

        if let Ok(GatewayResponse::GatewayProcessed(gateway_processed)) = response {
            let gateway_event_handler = GatewayProcessedHandler {
                gateway_processed_payload: gateway_processed,
                gateway: updated_kube_gateway,
                state: self.state,
                log_context,
                route_patcher: self.http_route_patcher.clone(),
                controller_name: self.controller_name.to_owned(),
            };
            gateway_event_handler.deploy_gateway().await
        } else {
            warn!("{log_context} {response:?} ... Problem {response:?}");
            Err(ControllerError::BackendError)
        }
    }

    fn adjust_statuses(gateway: &mut common::Gateway) {
        gateway.listeners_mut().for_each(|l| {
            let name = l.name().to_owned();
            let (resolved, unresolved) = l.routes();
            let resolved_count = resolved.len();
            let unresolved_count = unresolved.len();
            if l.attached_routes() != resolved_count + unresolved_count {
                error!("We have a problem here... the route accounting is off ");
            }
            let conditions = l.conditions_mut();

            debug!("Adjusting conditions {} conditions {:#?}", name, conditions);
            if let Some(common::ListenerCondition::ResolvedRefs(resolved_refs)) = conditions.get(&common::ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes)) {
                match resolved_refs {
                    ResolvedRefs::Resolved(_) | ResolvedRefs::ResolvedWithNotAllowedRoutes(_) => {
                        conditions.replace(common::ListenerCondition::Accepted);
                        conditions.replace(common::ListenerCondition::Programmed);
                    }

                    ResolvedRefs::InvalidAllowedRoutes => {
                        conditions.insert(common::ListenerCondition::NotProgrammed);
                        conditions.replace(common::ListenerCondition::NotAccepted);
                    }
                    ResolvedRefs::InvalidCertificates(_) => {
                        conditions.replace(common::ListenerCondition::NotAccepted);
                    }
                }
            };

            if conditions.contains(&common::ListenerCondition::UnresolvedRouteRefs) {
                if resolved_count == 0 {
                    conditions.replace(common::ListenerCondition::NotProgrammed);
                    conditions.replace(common::ListenerCondition::NotAccepted);
                }
                conditions.remove(&common::ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes));
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
            debug!("{log_context} Processing listener {name} {} {:#?}", l.attached_routes(), l.conditions().collect::<Vec<_>>());

            let mut listener_status = GatewayStatusListeners { name, ..Default::default() };

            let listener_conditions = &mut listener_status.conditions;
            listener_status.attached_routes = i32::try_from(l.attached_routes()).unwrap_or_default();

            for condition in l.conditions() {
                let (status, type_, reason) = condition.resolved_type();
                let status = status.to_owned();
                let type_ = type_.to_string();
                let reason = reason.to_string();
                listener_conditions.retain(|c| c.type_ != type_);
                if let common::ListenerCondition::ResolvedRefs(
                    ResolvedRefs::Resolved(allowed_routes) | ResolvedRefs::ResolvedWithNotAllowedRoutes(allowed_routes) | ResolvedRefs::InvalidCertificates(allowed_routes),
                ) = condition
                {
                    listener_conditions.push(Condition {
                        last_transition_time: Time(Utc::now()),
                        message: "Updated by controller".to_owned(),
                        observed_generation: generation,
                        reason,
                        status,
                        type_,
                    });
                    listener_status.supported_kinds = allowed_routes.iter().map(|r| GatewayStatusListenersSupportedKinds { group: None, kind: r.clone() }).collect();
                } else {
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
            listeners_statuses.push(listener_status);
        });

        status.listeners = Some(listeners_statuses);
        kube_gateway.status = Some(status);
    }
}
