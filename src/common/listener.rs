use std::{collections::BTreeSet, fmt::Display};

use gateway_api::{
    constants,
    gateways::{self, GatewayListeners},
};
use thiserror::Error;

use super::{Certificate, NotResolvedReason, ResolutionStatus, ResolvedRefs, ResourceKey, Route};
use crate::controllers::ControllerError;

#[derive(Debug, Clone, PartialEq, PartialOrd, Hash, Eq)]
pub enum ProtocolType {
    Http,
    Https,
    Tcp,
    Tls,
    Udp,
}

impl TryFrom<&String> for ProtocolType {
    type Error = ControllerError;

    fn try_from(value: &String) -> Result<Self, Self::Error> {
        Ok(match value.to_uppercase().as_str() {
            "HTTP" => Self::Http,
            "HTTPS" => Self::Https,
            "TCP" => Self::Tcp,
            "TLS" => Self::Tls,
            "UDP" => Self::Udp,
            _ => {
                return Err(ControllerError::InvalidPayload("Wrong protocol".to_owned()));
            },
        })
    }
}
impl Display for ProtocolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut e = format! {"{self:?}"};
        e.make_ascii_uppercase();
        write!(f, "{e}")
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ListenerConfig {
    pub name: String,
    pub port: i32,
    pub hostname: Option<String>,
    pub tls_type: Option<TlsType>,
}

impl PartialOrd for ListenerConfig {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.name.partial_cmp(&other.name) {
            Some(core::cmp::Ordering::Equal) => {},
            ord => return ord,
        }
        match self.port.partial_cmp(&other.port) {
            Some(core::cmp::Ordering::Equal) => {},
            ord => return ord,
        }
        self.hostname.partial_cmp(&other.hostname)
    }
}

impl ListenerConfig {
    pub fn new(name: String, port: i32, hostname: Option<String>) -> Self {
        Self { name, port, hostname, tls_type: None }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Listener {
    Http(ListenerData),
    Https(ListenerData),
    Tcp(ListenerData),
    Tls(ListenerData),
    Udp(ListenerData),
}

impl Listener {
    pub fn name(&self) -> &str {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data.config.name.as_str(),
        }
    }

    pub fn port(&self) -> i32 {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data.config.port,
        }
    }

    pub fn protocol(&self) -> ProtocolType {
        match self {
            Listener::Http(_) => ProtocolType::Http,
            Listener::Https(_) => ProtocolType::Https,
            Listener::Tcp(_) => ProtocolType::Tcp,
            Listener::Tls(_) => ProtocolType::Tls,
            Listener::Udp(_) => ProtocolType::Udp,
        }
    }
    pub fn hostname(&self) -> Option<&String> {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data.config.hostname.as_ref(),
        }
    }

    pub fn config(&self) -> &ListenerConfig {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => &listener_data.config,
        }
    }

    pub fn conditions(&self) -> impl Iterator<Item = &ListenerCondition> {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data.conditions.iter(),
        }
    }

    pub fn conditions_mut(&mut self) -> &mut ListenerConditions {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => &mut listener_data.conditions,
        }
    }

    pub fn data_mut(&mut self) -> &mut ListenerData {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data,
        }
    }

    pub fn data(&self) -> &ListenerData {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data,
        }
    }

    pub fn routes(&self) -> (Vec<&Route>, Vec<&Route>) {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => {
                (Vec::from_iter(&listener_data.resolved_routes), Vec::from_iter(&listener_data.unresolved_routes))
            },
        }
    }

    pub fn update_routes(&mut self, resolved_routes: BTreeSet<Route>, unresolved_routes: BTreeSet<Route>) {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => {
                if !unresolved_routes.is_empty() {
                    if let Some(route) = unresolved_routes.first() {
                        if *route.resolution_status()
                            == ResolutionStatus::NotResolved(NotResolvedReason::RefNotPermitted)
                        {
                            listener_data.conditions.replace(ListenerCondition::ResolvedRefs(
                                ResolvedRefs::InvalidBackend(APPROVED_ROUTES.iter().map(|s| (*s).to_owned()).collect()),
                            ));
                        }
                    } else {
                        listener_data.conditions.replace(ListenerCondition::UnresolvedRouteRefs);
                    }
                }

                listener_data.attached_routes = unresolved_routes.len() + resolved_routes.len();
                listener_data.resolved_routes = resolved_routes;
                listener_data.unresolved_routes = unresolved_routes;
            },
        }
    }

    pub fn attached_routes(&self) -> usize {
        match self {
            Listener::Http(listener_data)
            | Listener::Https(listener_data)
            | Listener::Tcp(listener_data)
            | Listener::Tls(listener_data)
            | Listener::Udp(listener_data) => listener_data.attached_routes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum TlsType {
    Terminate(Vec<Certificate>),
    Passthrough,
}

impl TryFrom<&GatewayListeners> for Listener {
    type Error = ListenerError;

    fn try_from(gateway_listener: &GatewayListeners) -> std::result::Result<Self, Self::Error> {
        let mut config = ListenerConfig::new(
            gateway_listener.name.clone(),
            gateway_listener.port,
            gateway_listener.hostname.clone(),
        );

        let condition = validate_allowed_routes(gateway_listener);

        let mut listener_conditions = ListenerConditions::new();
        _ = listener_conditions.replace(condition);

        let maybe_tls_config = gateway_listener
            .tls
            .as_ref()
            .map(|tls| match tls.mode {
                Some(gateways::GatewayListenersTlsMode::Passthrough) => Ok(TlsType::Passthrough),
                Some(gateways::GatewayListenersTlsMode::Terminate) => {
                    let secrets = tls
                        .certificate_refs
                        .as_ref()
                        .map(|refs| {
                            refs.iter()
                                .map(|r| {
                                    Certificate::NotResolved(ResourceKey::from((
                                        r.group.clone(),
                                        r.namespace.clone(),
                                        r.name.clone(),
                                        r.kind.clone(),
                                    )))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Ok(TlsType::Terminate(secrets))
                },
                None => Err(ListenerError::UnknownTlsMode),
            })
            .transpose();

        match maybe_tls_config {
            Ok(tls) => config.tls_type = tls,
            Err(e) => return Err(e),
        }

        let listener_data = ListenerData {
            config,
            conditions: listener_conditions,
            resolved_routes: BTreeSet::new(),
            unresolved_routes: BTreeSet::new(),
            attached_routes: 0,
        };

        match gateway_listener.protocol.as_str() {
            "HTTP" => Ok(Self::Http(listener_data)),
            "HTTPS" => Ok(Self::Https(listener_data)),
            "TCP" => Ok(Self::Tcp(listener_data)),
            "TLS" => Ok(Self::Tls(listener_data)),
            "UDP" => Ok(Self::Udp(listener_data)),
            _ => Err(ListenerError::UnknownProtocol(gateway_listener.protocol.clone())),
        }
    }
}

#[derive(Error, Debug, PartialEq, PartialOrd)]
pub enum ListenerError {
    #[error("Unknown protocol")]
    UnknownProtocol(String),
    #[error("Lisetner is not distinct")]
    NotDistinct(String, i32, ProtocolType, Option<String>),
    #[error("Unknown tls mode")]
    UnknownTlsMode,
}
pub type ListenerConditions = BTreeSet<ListenerCondition>;

#[derive(Clone, Default, Debug, PartialEq, PartialOrd)]
pub struct ListenerData {
    pub config: ListenerConfig,
    pub conditions: ListenerConditions,
    pub resolved_routes: BTreeSet<Route>,
    pub unresolved_routes: BTreeSet<Route>,
    pub attached_routes: usize,
}

#[derive(Clone, Debug)]
pub enum ListenerCondition {
    ResolvedRefs(ResolvedRefs),
    UnresolvedRouteRefs,
    Accepted,
    NotAccepted,
    Programmed,
    NotProgrammed,
}

impl ListenerCondition {
    pub fn discriminant(&self) -> u8 {
        match self {
            ListenerCondition::ResolvedRefs(_) => 0,
            ListenerCondition::Accepted | ListenerCondition::NotAccepted => 1,
            ListenerCondition::Programmed | ListenerCondition::NotProgrammed => 2,
            ListenerCondition::UnresolvedRouteRefs => 3,
        }
    }
}

impl PartialOrd for ListenerCondition {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ListenerCondition {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let self_disc = self.discriminant();
        let other_disc = other.discriminant();
        self_disc.cmp(&other_disc)
    }
}

impl Eq for ListenerCondition {}

impl PartialEq for ListenerCondition {
    fn eq(&self, other: &Self) -> bool {
        core::mem::discriminant(self) == core::mem::discriminant(other)
    }
}

impl ListenerCondition {
    pub fn resolved_type(
        &self,
    ) -> (&'static str, constants::ListenerConditionType, constants::ListenerConditionReason) {
        match self {
            ListenerCondition::ResolvedRefs(
                ResolvedRefs::InvalidAllowedRoutes | ResolvedRefs::ResolvedWithNotAllowedRoutes(_),
            ) => (
                "False",
                constants::ListenerConditionType::ResolvedRefs,
                constants::ListenerConditionReason::InvalidRouteKinds,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidBackend(_)) => (
                "True",
                constants::ListenerConditionType::ResolvedRefs,
                constants::ListenerConditionReason::InvalidRouteKinds,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(_)) => (
                "True",
                constants::ListenerConditionType::ResolvedRefs,
                constants::ListenerConditionReason::ResolvedRefs,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidCertificates(_)) => (
                "False",
                constants::ListenerConditionType::ResolvedRefs,
                constants::ListenerConditionReason::InvalidCertificateRef,
            ),

            ListenerCondition::ResolvedRefs(ResolvedRefs::RefNotPermitted(_)) => (
                "False",
                constants::ListenerConditionType::ResolvedRefs,
                constants::ListenerConditionReason::RefNotPermitted,
            ),

            ListenerCondition::UnresolvedRouteRefs => (
                "False",
                constants::ListenerConditionType::ResolvedRefs,
                constants::ListenerConditionReason::ResolvedRefs,
            ),

            ListenerCondition::Accepted => {
                ("True", constants::ListenerConditionType::Accepted, constants::ListenerConditionReason::Accepted)
            },
            ListenerCondition::NotAccepted => {
                ("False", constants::ListenerConditionType::Accepted, constants::ListenerConditionReason::Accepted)
            },
            ListenerCondition::Programmed => {
                ("True", constants::ListenerConditionType::Programmed, constants::ListenerConditionReason::Programmed)
            },

            ListenerCondition::NotProgrammed => {
                ("False", constants::ListenerConditionType::Programmed, constants::ListenerConditionReason::Programmed)
            },
        }
    }
    pub fn supported_routes(&self) -> Vec<String> {
        match self {
            ListenerCondition::ResolvedRefs(
                ResolvedRefs::Resolved(supported_routes)
                | ResolvedRefs::ResolvedWithNotAllowedRoutes(supported_routes)
                | ResolvedRefs::InvalidCertificates(supported_routes),
            ) => supported_routes.clone(),
            _ => APPROVED_ROUTES.iter().map(|r| (*r).to_owned()).collect(),
        }
    }
}

const APPROVED_ROUTES: [&str; 3] = ["GRPCRoute", "HTTPRoute", "TCPRoute"];

fn validate_allowed_routes(gateway_listeners: &GatewayListeners) -> ListenerCondition {
    if let Some(ar) = gateway_listeners.allowed_routes.as_ref() {
        if let Some(kinds) = ar.kinds.as_ref() {
            let cloned_kinds = kinds.clone().into_iter().map(|k| k.kind);
            let (supported, invalid): (Vec<_>, Vec<_>) =
                cloned_kinds.partition(|f| APPROVED_ROUTES.contains(&f.as_str()));

            match (supported.is_empty(), invalid.is_empty()) {
                (_, true) => ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(supported)),
                (true, false) => ListenerCondition::ResolvedRefs(ResolvedRefs::InvalidAllowedRoutes),
                (false, false) => {
                    ListenerCondition::ResolvedRefs(ResolvedRefs::ResolvedWithNotAllowedRoutes(supported))
                },
            }
        } else if gateway_listeners.protocol == "HTTP" || gateway_listeners.protocol == "HTTPS" {
            ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![
                "HTTPRoute".to_owned(),
                "GRPCRoute".to_owned(),
            ]))
        } else {
            ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![]))
        }
    } else if gateway_listeners.protocol == "HTTP" || gateway_listeners.protocol == "HTTPS" {
        ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec!["HTTPRoute".to_owned(), "GRPCRoute".to_owned()]))
    } else {
        ListenerCondition::ResolvedRefs(ResolvedRefs::Resolved(vec![]))
    }
}
