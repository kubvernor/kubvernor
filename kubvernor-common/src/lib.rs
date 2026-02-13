pub mod configuration;
mod resource_key;
pub use resource_key::ResourceKey;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub enum GatewayImplementationType {
    Envoy,
    Agentgateway,
    Orion,
}

impl TryFrom<Option<&String>> for GatewayImplementationType {
    type Error = crate::Error;

    fn try_from(value: Option<&String>) -> std::result::Result<Self, Self::Error> {
        match value.map(std::string::String::as_str) {
            Some("agentgateway") => Ok(GatewayImplementationType::Agentgateway),
            Some("orion") => Ok(GatewayImplementationType::Orion),
            Some("envoy") | None => Ok(GatewayImplementationType::Envoy),
            Some(_) => Err("Invalid backend type ".into()),
        }
    }
}

pub fn format_resource<R>() -> &'static str {
    std::any::type_name::<R>().split("::").last().unwrap_or_default()
}
