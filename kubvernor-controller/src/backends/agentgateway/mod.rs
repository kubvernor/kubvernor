mod agentgateway_deployer;
mod converters;
pub(crate) mod resource_generator;
mod server;
use agentgateway_api_rs::agentgateway::dev::resource::Listener;
#[cfg(feature = "agentgateway")]
pub use agentgateway_deployer::AgentgatewayDeployerChannelHandlerService;

#[derive(Clone, PartialEq, Default)]
pub struct SecureListenerWrapper(Listener);

impl std::fmt::Debug for SecureListenerWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Listener")
            .field(&self.0.key)
            .field(&self.0.bind_key)
            .field(&self.0.name)
            .field(&self.0.hostname)
            .field(&self.0.protocol)
            .finish()
    }
}
impl From<SecureListenerWrapper> for Listener {
    fn from(value: SecureListenerWrapper) -> Self {
        value.0
    }
}

const TARGET: &str = "kubvernor::backend::agentgateway";
