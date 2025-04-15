use std::sync::Arc;

use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use tokio::sync::mpsc::{self, Sender};
use tracing::{debug, error, info, Instrument, Span};
use typed_builder::TypedBuilder;

use crate::{
    common::{
        self,
        gateway_api::{
            gatewayclasses::GatewayClass,
            gateways::{GatewayStatusListeners, GatewayStatusListenersSupportedKinds},
        },
        BackendGatewayEvent, ChangedContext, KubeGateway, ListenerCondition, ResolvedRefs, ResourceKey,
    },
    controllers::ControllerError,
    services::patchers::Operation,
    state::State,
};

#[derive(TypedBuilder)]
pub struct GatewayDeployerServiceInternal<'a> {
    state: &'a State,
    gateway_patcher_channel_sender: mpsc::Sender<Operation<KubeGateway>>,
    gateway_class_patcher_channel_sender: mpsc::Sender<Operation<GatewayClass>>,
    controller_name: String,
}

impl GatewayDeployerServiceInternal<'_> {
    pub async fn on_version_not_changed(&mut self, gateway_id: &ResourceKey, gateway_class_name: &str, resource: &Arc<KubeGateway>) {
        let controller_name = &self.controller_name;
        if let Some(status) = &resource.status {
            if let Some(conditions) = &status.conditions {
                if conditions.iter().any(|c| c.type_ == crate::common::gateway_api::constants::GatewayConditionType::Ready.to_string()) {
                    self.state.save_gateway(gateway_id.clone(), resource).expect("We expect the lock to work");
                    common::add_finalizer_to_gateway_class(&self.gateway_class_patcher_channel_sender, gateway_class_name, controller_name)
                        .instrument(Span::current().clone())
                        .await;
                    let has_finalizer = if let Some(finalizers) = &resource.metadata.finalizers {
                        finalizers.iter().any(|f| f == controller_name)
                    } else {
                        false
                    };

                    if !has_finalizer {
                        let () = common::add_finalizer(&self.gateway_patcher_channel_sender, gateway_id, controller_name)
                            .instrument(Span::current().clone())
                            .await;
                    };
                }
            }
        }
    }
}

type Result<T, E = ControllerError> = std::result::Result<T, E>;

const CONDITION_MESSAGE: &str = "Gateway updated by controller";

#[derive(TypedBuilder)]
pub struct GatewayDeployer {
    sender: Sender<BackendGatewayEvent>,
    gateway: common::Gateway,
    kube_gateway: KubeGateway,
    gateway_class_name: String,
}
impl GatewayDeployer {
    pub async fn deploy_gateway(self) -> Result<()> {
        let mut updated_kube_gateway = self.kube_gateway;
        let mut backend_gateway = self.gateway.clone();

        Self::adjust_statuses(&mut backend_gateway);
        Self::resolve_listeners_statuses(&backend_gateway, &mut updated_kube_gateway);
        info!("Effective gateway {}-{} {:#?}", backend_gateway.name(), backend_gateway.namespace(), backend_gateway);

        let listener_event = BackendGatewayEvent::Changed(
            ChangedContext::builder()
                .kube_gateway(updated_kube_gateway.clone())
                .gateway(backend_gateway)
                .gateway_class_name(self.gateway_class_name)
                .span(Span::current().clone())
                .build(),
        );
        let _ = self.sender.send(listener_event).await;
        Ok(())
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
            if let Some(ListenerCondition::ResolvedRefs(resolved_refs)) = conditions.get(&ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes)) {
                match resolved_refs {
                    ResolvedRefs::Resolved(_) | ResolvedRefs::ResolvedWithNotAllowedRoutes(_) | ResolvedRefs::InvalidBackend(_) => {
                        conditions.replace(ListenerCondition::Accepted);
                        conditions.replace(ListenerCondition::Programmed);
                    }

                    ResolvedRefs::InvalidAllowedRoutes | ResolvedRefs::RefNotPermitted(_) => {
                        conditions.insert(ListenerCondition::NotProgrammed);
                        conditions.replace(ListenerCondition::NotAccepted);
                    }

                    ResolvedRefs::InvalidCertificates(_) => {
                        conditions.remove(&ListenerCondition::Accepted);
                        conditions.replace(ListenerCondition::NotProgrammed);
                    }
                }
            };

            if conditions.contains(&ListenerCondition::UnresolvedRouteRefs) {
                if resolved_count == 0 {
                    conditions.replace(ListenerCondition::NotProgrammed);
                    conditions.replace(ListenerCondition::NotAccepted);
                }
                conditions.remove(&ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes));
            };

            debug!("Adjusted  conditions {conditions:#?}");
        });
    }

    fn resolve_listeners_statuses(gateway: &common::Gateway, kube_gateway: &mut KubeGateway) {
        let mut status = kube_gateway.status.clone().unwrap_or_default();
        let mut listeners_statuses = vec![];
        let generation = kube_gateway.metadata.generation;

        gateway.listeners().for_each(|l| {
            let name = l.name().to_owned();
            debug!("Processing listener {name} {} {:#?}", l.attached_routes(), l.conditions().collect::<Vec<_>>());

            let mut listener_status = GatewayStatusListeners { name, ..Default::default() };

            let listener_conditions = &mut listener_status.conditions;
            listener_status.attached_routes = i32::try_from(l.attached_routes()).unwrap_or_default();

            for condition in l.conditions() {
                let (status, type_, reason) = condition.resolved_type();
                let status = status.to_owned();
                let type_ = type_.to_string();
                let reason = reason.to_string();
                listener_conditions.retain(|c| c.type_ != type_);
                match condition {
                    ListenerCondition::ResolvedRefs(
                        ResolvedRefs::Resolved(_)
                        | ResolvedRefs::InvalidBackend(_)
                        | ResolvedRefs::ResolvedWithNotAllowedRoutes(_)
                        | ResolvedRefs::InvalidCertificates(_)
                        | ResolvedRefs::RefNotPermitted(_),
                    ) => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: CONDITION_MESSAGE.to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                        listener_status.supported_kinds = condition
                            .supported_routes()
                            .iter()
                            .map(|r| GatewayStatusListenersSupportedKinds { group: None, kind: r.clone() })
                            .collect();
                    }
                    ListenerCondition::UnresolvedRouteRefs => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: CONDITION_MESSAGE.to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                        listener_status.supported_kinds = condition
                            .supported_routes()
                            .iter()
                            .map(|r| GatewayStatusListenersSupportedKinds { group: None, kind: r.clone() })
                            .collect();
                    }
                    _ => {
                        listener_conditions.push(Condition {
                            last_transition_time: Time(Utc::now()),
                            message: CONDITION_MESSAGE.to_owned(),
                            observed_generation: generation,
                            reason,
                            status,
                            type_,
                        });
                    }
                }
            }
            listeners_statuses.push(listener_status);
        });

        status.listeners = Some(listeners_statuses);
        kube_gateway.status = Some(status);
    }
}
