use std::{
    cmp,
    collections::{btree_map, BTreeMap, BTreeSet},
    fmt::Display,
};

use gateway_api::apis::standard::gateways::Gateway as KubeGateway;
use thiserror::Error;
use tracing::Span;
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::{EffectiveRoutingRule, GatewayAddress, Listener, ResourceKey, Route, VerifiyItems};

#[derive(Clone, Debug)]
pub struct Gateway {
    id: Uuid,
    resource_key: ResourceKey,
    addresses: BTreeSet<GatewayAddress>,
    listeners: BTreeMap<String, Listener>,
    orphaned_routes: BTreeSet<Route>,
}

impl PartialEq for Gateway {
    fn eq(&self, other: &Self) -> bool {
        self.resource_key == other.resource_key
    }
}

impl PartialOrd for Gateway {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.resource_key.partial_cmp(&other.resource_key)
    }
}

impl Gateway {
    pub fn name(&self) -> &str {
        &self.resource_key.name
    }
    pub fn namespace(&self) -> &str {
        &self.resource_key.namespace
    }
    pub fn key(&self) -> &ResourceKey {
        &self.resource_key
    }

    pub fn id(&self) -> &Uuid {
        &self.id
    }
    pub fn listeners(&self) -> btree_map::Values<'_, String, Listener> {
        self.listeners.values()
    }

    pub fn listeners_mut(&mut self) -> btree_map::ValuesMut<'_, String, Listener> {
        self.listeners.values_mut()
    }

    pub fn listener(&self, name: &str) -> Option<&Listener> {
        self.listeners.get(name)
    }

    pub fn listener_mut(&mut self, name: &str) -> Option<&mut Listener> {
        self.listeners.get_mut(name)
    }

    pub fn addresses_mut(&mut self) -> &mut BTreeSet<GatewayAddress> {
        &mut self.addresses
    }

    pub fn addresses(&self) -> &BTreeSet<GatewayAddress> {
        &self.addresses
    }

    pub fn routes(&self) -> (BTreeSet<&Route>, BTreeSet<&Route>) {
        let mut resolved_routes = BTreeSet::new();
        let mut unresolved_routes = BTreeSet::new();

        for l in self.listeners.values() {
            let (resolved, unresolved) = l.routes();
            resolved_routes.append(&mut BTreeSet::from_iter(resolved));
            unresolved_routes.append(&mut BTreeSet::from_iter(unresolved));
        }
        (resolved_routes, unresolved_routes)
    }

    pub fn effective_matching_rules(&self) -> Vec<&EffectiveRoutingRule> {
        let (resolved_routes, unresolved) = self.routes();
        let mut matching_rules: Vec<_> = resolved_routes.iter().chain(unresolved.iter()).flat_map(|r| r.effective_routing_rules()).collect();
        matching_rules.sort_by(|this, other| this.partial_cmp(other).unwrap_or(cmp::Ordering::Less));
        //matching_rules.reverse();
        matching_rules
    }

    pub fn orphaned_routes_mut(&mut self) -> &mut BTreeSet<Route> {
        &mut self.orphaned_routes
    }

    pub fn orphaned_routes(&self) -> &BTreeSet<Route> {
        &self.orphaned_routes
    }
}

impl TryFrom<&KubeGateway> for Gateway {
    type Error = GatewayError;

    fn try_from(gateway: &KubeGateway) -> std::result::Result<Self, Self::Error> {
        let id = Uuid::parse_str(&gateway.metadata.uid.clone().unwrap_or_default()).map_err(|_| GatewayError::ConversionProblem("Can't parse uuid".to_owned()))?;
        let resource_key = ResourceKey::from(gateway);

        let (listeners, listener_validation_errrors): (Vec<_>, Vec<_>) = VerifiyItems::verify(gateway.spec.listeners.iter().map(Listener::try_from));
        if !listener_validation_errrors.is_empty() {
            return Err(GatewayError::ConversionProblem("Misconfigured listeners".to_owned()));
        }

        Ok(Self {
            id,
            resource_key,
            addresses: BTreeSet::new(),
            listeners: listeners.into_iter().map(|l| (l.name().to_owned(), l)).collect::<BTreeMap<String, Listener>>(),
            orphaned_routes: BTreeSet::new(),
        })
    }
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum GatewayError {
    #[error("Conversion problem")]
    ConversionProblem(String),
}

#[derive(Debug, TypedBuilder)]
pub struct ChangedContext {
    pub span: Span,
    pub gateway: Gateway,
    pub kube_gateway: KubeGateway,
    pub gateway_class_name: String,
}

impl Display for ChangedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gateway {}.{} listeners = {} ", self.gateway.name(), self.gateway.namespace(), self.gateway.listeners.len(),)
    }
}
