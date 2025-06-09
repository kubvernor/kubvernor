use std::{collections::BTreeSet, sync::Arc};

use gateway_api::{
    common_types::{ParentsRouteStatus, RouteRef},
    gateways::Gateway,
};
use k8s_openapi::{
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    chrono::Utc,
};
use kube_core::ObjectMeta;
use tracing::Span;

use crate::{
    common::{Backend, ResourceKey, Route},
    services::patchers::{FinalizerContext, Operation},
};

const CONDITION_MESSAGE: &str = "Route updated by controller";

pub fn generate_status_for_unknown_gateways(controller_name: &str, gateways: &[(&RouteRef, Option<Arc<Gateway>>)], generation: Option<i64>) -> Vec<ParentsRouteStatus> {
    gateways
        .iter()
        .map(|(gateway, _)| ParentsRouteStatus {
            conditions: Some(vec![Condition {
                last_transition_time: Time(Utc::now()),
                message: CONDITION_MESSAGE.to_owned(),
                observed_generation: generation,
                reason: "BackendNotFound".to_owned(),
                status: "False".to_owned(),
                type_: "ResolvedRefs".to_owned(),
            }]),
            controller_name: controller_name.to_owned(),
            parent_ref: RouteRef {
                group: gateway.group.clone(),
                kind: gateway.kind.clone(),
                name: gateway.name.clone(),
                namespace: gateway.namespace.clone(),
                port: gateway.port,
                section_name: gateway.section_name.clone(),
            },
        })
        .collect()
}

pub fn extract_references(route: &Route) -> BTreeSet<ResourceKey> {
    let mut backend_reference_keys = BTreeSet::new();

    for backend in &route.backends() {
        if let Backend::Maybe(backend_service_config) = backend {
            backend_reference_keys.insert(backend_service_config.resource_key.clone());
        }
    }

    backend_reference_keys
}

pub fn needs_finalizer<T: serde::Serialize>(resource_key: &ResourceKey, controller_name: &String, resource_meta: &ObjectMeta) -> Option<Operation<T>> {
    let has_finalizer = if let Some(finalizers) = resource_meta.finalizers.as_ref() {
        finalizers.contains(controller_name)
    } else {
        false
    };

    if has_finalizer {
        None
    } else {
        Some(Operation::PatchFinalizer(FinalizerContext {
            resource_key: resource_key.clone(),
            controller_name: controller_name.clone(),
            finalizer_name: controller_name.clone(),
            span: Span::current().clone(),
        }))
    }
}
