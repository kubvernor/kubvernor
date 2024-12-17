use gateway_api::apis::standard::{
    gateways::{Gateway, GatewayStatusAddresses},
    httproutes::{HTTPRoute, HTTPRouteStatus, HTTPRouteStatusParents, HTTPRouteStatusParentsParentRef},
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube::Resource;
use tokio::sync::{mpsc::Sender, oneshot};
use tracing::{debug, info, warn, Instrument, Span};

use crate::{
    common::{self, GatewayAddress, NotResolvedReason, ResolutionStatus, ResourceKey, Route},
    controllers::ControllerError,
    patchers::{Operation, PatchContext},
    state::State,
};

type Result<T, E = ControllerError> = std::result::Result<T, E>;

const GATEWAY_CONDITION_MESSAGE: &str = "Gateway status updated by controller";
const ROUTE_CONDITION_MESSAGE: &str = "Route status updated by controller";
pub struct GatewayProcessedHandler<'a> {
    pub effective_gateway: common::Gateway,
    pub gateway: Gateway,
    pub state: &'a State,
    pub route_patcher: Sender<Operation<HTTPRoute>>,
    pub controller_name: String,
}

impl<'a> GatewayProcessedHandler<'a> {
    pub async fn deploy_gateway(mut self) -> Result<Gateway> {
        self.update_gateway_resource();
        self.update_routes().instrument(Span::current().clone()).await;
        Ok(self.gateway)
    }

    fn update_gateway_resource(&mut self) {
        self.update_gateway_status_addresses();
        self.update_gateway_status_conditions();
        self.clear_meta();
    }

    fn update_gateway_status_conditions(&mut self) {
        let observed_generation = self.gateway.metadata.generation;
        let mut status = self.gateway.status.clone().unwrap_or_default();
        let mut conditions = status.conditions.unwrap_or_default();

        conditions.retain(|f| f.type_ != gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string());
        for f in &mut conditions {
            f.last_transition_time = Time(Utc::now());
            f.observed_generation = observed_generation;
            f.status = String::from("True");
            f.reason = gateway_api::apis::standard::constants::GatewayConditionReason::Ready.to_string();
        }

        let new_condition = Condition {
            last_transition_time: Time(Utc::now()),
            message: GATEWAY_CONDITION_MESSAGE.to_owned(),
            observed_generation,
            reason: gateway_api::apis::standard::constants::GatewayConditionReason::Ready.to_string(),
            status: String::from("True"),
            type_: gateway_api::apis::standard::constants::GatewayConditionType::Ready.to_string(),
        };
        conditions.push(new_condition);
        status.conditions = Some(conditions);
        self.gateway.status = Some(status);
    }

    fn clear_meta(&mut self) {
        self.gateway.metadata.managed_fields = None;
    }

    async fn update_routes(&self) {
        let (attached_routes, unresolved_routes) = self.effective_gateway.routes();

        let routes_with_no_hostnames = self.effective_gateway.orphaned_routes();
        debug!("Updating attached routes {attached_routes:?}");
        let gateway_id = &self.effective_gateway.key();
        for attached_route in attached_routes {
            let updated_route = self.update_attached_route_parents(attached_route, gateway_id);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let (sender, receiver) = oneshot::channel();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key.clone(),
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        response_sender: sender,
                        span: Span::current().clone(),
                    }))
                    .await;
                let patched_route = receiver.await;
                if let Ok(maybe_patched) = patched_route {
                    match maybe_patched {
                        Ok(_patched_route) => {}
                        Err(e) => {
                            warn!("Error while patching {e}");
                        }
                    }
                }
            }
        }
        debug!("Updating unresolved routes  {unresolved_routes:?}");
        for unresolve_route in unresolved_routes {
            let updated_route = self.update_unresolved_route_parents(unresolve_route, gateway_id);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let (sender, receiver) = oneshot::channel();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key.clone(),
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        response_sender: sender,
                        span: Span::current().clone(),
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
                            warn!("Error while patching {e}");
                        }
                    }
                }
            }
        }
        debug!("Updating routes with no hostnames  {routes_with_no_hostnames:?}");
        for route_with_no_hostname in self.effective_gateway.orphaned_routes() {
            let updated_route = self.update_non_attached_route_parents(route_with_no_hostname, gateway_id);
            if let Some(route) = updated_route {
                let route_resource_key = ResourceKey::from(route.meta());
                let (sender, receiver) = oneshot::channel();
                let _res = self
                    .route_patcher
                    .send(Operation::PatchStatus(PatchContext {
                        resource_key: route_resource_key.clone(),
                        resource: route,
                        controller_name: self.controller_name.clone(),
                        response_sender: sender,
                        span: Span::current().clone(),
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
                            warn!("Error while patching {e}");
                        }
                    }
                }
            }
        }
    }

    fn update_attached_route_parents(&self, attached_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        self.update_route_parents(
            attached_route,
            gateway_id,
            (
                "Accepted",
                vec![
                    Condition {
                        last_transition_time: Time(Utc::now()),
                        message: ROUTE_CONDITION_MESSAGE.to_owned(),
                        observed_generation: None,
                        reason: "Accepted".to_owned(),
                        status: "True".to_owned(),
                        type_: "Accepted".to_owned(),
                    },
                    Condition {
                        last_transition_time: Time(Utc::now()),
                        message: ROUTE_CONDITION_MESSAGE.to_owned(),
                        observed_generation: None,
                        reason: "ResolvedRefs".to_owned(),
                        status: "True".to_owned(),
                        type_: "ResolvedRefs".to_owned(),
                    },
                ],
            ),
        )
    }

    fn update_unresolved_route_parents(&self, rejected_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        let key = rejected_route.resource_key();
        info!("Unresolved route resolution status  {key:?}  {:?}", rejected_route.resolution_status());
        let conditions = match rejected_route.resolution_status() {
            ResolutionStatus::Resolved => vec![Condition {
                last_transition_time: Time(Utc::now()),
                message: ROUTE_CONDITION_MESSAGE.to_owned(),
                observed_generation: None,
                reason: gateway_api::apis::standard::constants::ListenerConditionReason::Invalid.to_string(),
                status: "False".to_owned(),
                type_: gateway_api::apis::standard::constants::ListenerConditionType::Conflicted.to_string(),
            }],

            ResolutionStatus::NotResolved(resolution_reason) => match resolution_reason {
                NotResolvedReason::InvalidBackend => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "InvalidKind".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Programmed.to_string(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                    ]
                }
                NotResolvedReason::BackendNotFound => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "BackendNotFound".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Programmed.to_string(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                    ]
                }
                NotResolvedReason::RefNotPermitted => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "RefNotPermitted".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Accepted.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                    ]
                }
                NotResolvedReason::NoMatchingParent => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "NoMatchingParent".to_owned(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "NoMatchingParent".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                    ]
                }
                _ => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs.to_string(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Programmed.to_string(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                        },
                    ]
                }
            },
        };
        self.update_route_parents(rejected_route, gateway_id, ("Rejected", conditions))
    }

    fn update_non_attached_route_parents(&self, non_attached_route: &Route, gateway_id: &ResourceKey) -> Option<HTTPRoute> {
        let key = non_attached_route.resource_key();
        info!("Non attached route resolution status  {key:?}  {:?}", non_attached_route.resolution_status());
        let conditions = match non_attached_route.resolution_status() {
            ResolutionStatus::Resolved => vec![
                Condition {
                    last_transition_time: Time(Utc::now()),
                    message: ROUTE_CONDITION_MESSAGE.to_owned(),
                    observed_generation: None,
                    reason: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                    status: "True".to_owned(),
                    type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                },
                Condition {
                    last_transition_time: Time(Utc::now()),
                    message: ROUTE_CONDITION_MESSAGE.to_owned(),
                    observed_generation: None,
                    reason: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                    status: "True".to_owned(),
                    type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                },
            ],

            ResolutionStatus::NotResolved(resolution_reason) => match resolution_reason {
                NotResolvedReason::Unknown => vec![Condition {
                    last_transition_time: Time(Utc::now()),
                    message: ROUTE_CONDITION_MESSAGE.to_owned(),
                    observed_generation: None,
                    reason: "Uknown reason".to_owned(),
                    status: "False".to_owned(),
                    type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                }],

                NotResolvedReason::NotAllowedByListeners => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "NotAllowedByListeners".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                    ]
                }

                NotResolvedReason::RefNotPermitted => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "RefNotPermitted".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "RefNotPermitted".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                    ]
                }

                NotResolvedReason::NoMatchingListenerHostname => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "NoMatchingListenerHostname".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                    ]
                }

                NotResolvedReason::NoMatchingParent => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "NoMatchingParent".to_owned(),
                            status: "True".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: "NoMatchingParent".to_owned(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Accepted.to_string(),
                        },
                    ]
                }

                NotResolvedReason::InvalidBackend | NotResolvedReason::BackendNotFound => {
                    vec![
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::ResolvedRefs.to_string(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::ResolvedRefs.to_string(),
                        },
                        Condition {
                            last_transition_time: Time(Utc::now()),
                            message: ROUTE_CONDITION_MESSAGE.to_owned(),
                            observed_generation: None,
                            reason: gateway_api::apis::standard::constants::ListenerConditionReason::Programmed.to_string(),
                            status: "False".to_owned(),
                            type_: gateway_api::apis::standard::constants::ListenerConditionType::Programmed.to_string(),
                        },
                    ]
                }
            },
        };
        self.update_route_parents(non_attached_route, gateway_id, ("Rejected", conditions))
    }

    fn update_route_parents(&self, route: &Route, gateway_id: &ResourceKey, (new_condition_name, mut new_conditions): (&'static str, Vec<Condition>)) -> Option<HTTPRoute> {
        let kube_routes = self.state.get_http_routes_attached_to_gateway(gateway_id).expect("We expect the lock to work");

        if let Some(kube_routes) = kube_routes {
            let kube_route = kube_routes
                .iter()
                .find(|f| f.metadata.name == Some(route.name().to_owned()) && f.metadata.namespace == Some(route.namespace().clone()));

            if let Some(mut kube_route) = kube_route.map(|r| (**r).clone()) {
                new_conditions.iter_mut().for_each(|f| f.observed_generation = kube_route.meta().generation);

                let mut status = if let Some(status) = kube_route.status { status } else { HTTPRouteStatus { parents: vec![] } };

                status
                    .parents
                    .retain(|p| !(p.controller_name == self.controller_name && p.parent_ref.namespace == self.gateway.meta().namespace && self.gateway.meta().name == Some(p.parent_ref.name.clone())));

                for kube_parent in kube_route.spec.parent_refs.clone().unwrap_or_default() {
                    let route_parents = HTTPRouteStatusParents {
                        conditions: Some(new_conditions.clone()),
                        controller_name: self.controller_name.clone(),
                        parent_ref: HTTPRouteStatusParentsParentRef {
                            namespace: kube_parent.namespace.clone(),
                            name: kube_parent.name.clone(),
                            group: kube_parent.group.clone(),
                            kind: kube_parent.kind.clone(),
                            section_name: kube_parent.section_name.clone(),
                            port: kube_parent.port,
                        },
                    };
                    status.parents.push(route_parents);
                }

                kube_route.status = Some(status);
                kube_route.metadata.managed_fields = None;
                return Some(kube_route);
            }
        }
        None
    }

    fn update_gateway_status_addresses(&mut self) {
        let mut status = self.gateway.status.clone().unwrap_or_default();
        let addresses = self
            .effective_gateway
            .addresses()
            .iter()
            .map(|a| match a {
                GatewayAddress::Hostname(hostname) => GatewayStatusAddresses {
                    r#type: None,
                    value: hostname.clone(),
                },
                GatewayAddress::IPAddress(ip_addr) => GatewayStatusAddresses {
                    r#type: None,
                    value: ip_addr.to_string(),
                },
                GatewayAddress::NamedAddress(addr) => GatewayStatusAddresses { r#type: None, value: addr.clone() },
            })
            .collect::<Vec<_>>();
        status.addresses = Some(addresses);
        self.gateway.status = Some(status);
    }
}
