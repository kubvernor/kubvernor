use std::time::Duration;
pub mod gateway;
pub mod gateway_class;
pub mod http_route;
//mod resource_handler;
mod utils;

#[allow(dead_code)]
#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    PatchFailed,
    AlreadyAdded,
    InvalidPayload(String),
    InvalidRecipent,
    FinalizerPatchFailed(String),
}

const RECONCILE_WAIT: Duration = Duration::from_secs(3600);

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
