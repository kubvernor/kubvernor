mod envoy_deployer;
mod xds_generator;

pub use envoy_deployer::EnvoyDeployerChannelHandlerService as EnvoyCMDeployerChannelHandlerService;

const TARGET: &str = "kubvernor::backend::envoy_cm";
