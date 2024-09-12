use std::time::Duration;
pub mod gateway;
pub mod gateway_class;
pub mod http_route;
mod resource_handler;
mod utils;

#[allow(dead_code)]
#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    PatchFailed,
    AlreadyAdded,
    InvalidPayload(String),
    InvalidRecipent,
    FinalizerPatchFailed(String),
    BackendError,
    UnknownResource,
    UnknownGatewayClass(String),
    ResourceInWrongState,
}

const RECONCILE_LONG_WAIT: Duration = Duration::from_secs(3600);
const RECONCILE_ERROR_WAIT: Duration = Duration::from_secs(100);

const GATEWAY_CLASS_FINALIZER_NAME: &str = "gateway-exists-finalizer.gateway.networking.k8s.io";

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
