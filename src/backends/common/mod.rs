mod resource_generator;
pub use resource_generator::{calculate_hostnames_common, EnvoyListener, EnvoyVirtualHost, ResourceGenerator};

pub const INFERENCE_EXT_PROC_FILTER_NAME: &str = "inference.filters.http.ext_proc";
