use std::time::Duration;
pub mod gateway;
pub mod gateway_class;
mod handlers;
pub mod inference_pool;
pub mod route;

mod utils;

pub use utils::{find_linked_routes, FinalizerPatcher, HostnameMatchFilter, ListenerTlsConfigValidator, ResourceFinalizer, RoutesResolver};

#[allow(dead_code)]
#[derive(thiserror::Error, Debug, PartialEq, PartialOrd)]
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
    ResourceHasWrongStatus,
}

const RECONCILE_LONG_WAIT: Duration = Duration::from_secs(3600);
const RECONCILE_ERROR_WAIT: Duration = Duration::from_secs(100);


impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
