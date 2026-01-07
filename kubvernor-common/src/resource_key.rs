use std::fmt::Display;

pub const DEFAULT_GROUP_NAME: &str = "gateway.networking.k8s.io";
pub const DEFAULT_NAMESPACE_NAME: &str = "default";
pub const DEFAULT_KIND_NAME: &str = "Gateway";

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ResourceKey {
    pub group: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
}

#[allow(dead_code)]
impl ResourceKey {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_owned(), ..Default::default() }
    }

    pub fn namespaced(name: &str, namespace: &str) -> Self {
        Self { name: name.to_owned(), namespace: namespace.to_owned(), ..Default::default() }
    }
}

impl Default for ResourceKey {
    fn default() -> Self {
        Self {
            group: DEFAULT_GROUP_NAME.to_owned(),
            namespace: DEFAULT_NAMESPACE_NAME.to_owned(),
            name: String::default(),
            kind: DEFAULT_KIND_NAME.to_owned(),
        }
    }
}

impl Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", create_id(&self.name, &self.namespace))
    }
}

fn create_id(name: &str, namespace: &str) -> String {
    namespace.to_owned() + "." + name
}
