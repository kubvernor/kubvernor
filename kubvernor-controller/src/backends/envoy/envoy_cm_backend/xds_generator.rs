use std::collections::{BTreeMap, BTreeSet};

use gateway_api::{common::HTTPHeader, httproutes};
use log::{debug, info, warn};
use serde::Serialize;

use super::{
    super::common::{HTTPEffectiveRoutingRule, resource_generator::calculate_hostnames_common},
    envoy_deployer::{TEMPLATES, create_certificate_name, create_key_name, create_secret_name},
};
use crate::{
    common::{self, Backend, BackendTypeConfig, Listener, ProtocolType, Route, RouteType, TlsType},
    controllers::HostnameMatchFilter,
};

const TARGET: &str = super::TARGET;

#[derive(Debug)]
pub struct RdsData {
    pub route_name: String,
    pub rds_content: String,
}
impl RdsData {
    pub fn new(route_name: String, rds_content: String) -> Self {
        Self { route_name, rds_content }
    }
}

#[allow(clippy::struct_field_names)]
#[derive(Debug)]
pub struct XdsData {
    pub bootstrap_content: String,
    pub lds_content: String,
    pub rds_content: Vec<RdsData>,
    pub cds_content: String,
}

impl XdsData {
    pub fn new(bootstrap_content: String, lds_content: String, rds_content: Vec<RdsData>, cds_content: String) -> Self {
        Self { bootstrap_content, lds_content, rds_content, cds_content }
    }
}

type ListenerNameToHostname = (String, Option<String>);

#[derive(Debug, Clone)]
struct EnvoyVirutalHost {
    name: String,
    hostname: Option<String>,
    effective_hostnames: Vec<String>,
    resolved_routes: Vec<Route>,
    unresolved_routes: Vec<Route>,
    effective_matching_rules: Vec<HTTPEffectiveRoutingRule>,
}

impl Ord for EnvoyVirutalHost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl Eq for EnvoyVirutalHost {}

impl PartialOrd for EnvoyVirutalHost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EnvoyVirutalHost {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

#[derive(Debug, Clone)]
pub struct EnvoyListener {
    name: String,
    port: i32,
    http_listener_map: BTreeSet<EnvoyVirutalHost>,
    tcp_listener_map: BTreeSet<ListenerNameToHostname>,
    tls_type: Option<TlsType>,
}

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub struct EnvoyXDSGenerator<'a> {
    effective_gateway: &'a common::Gateway,
}

impl<'a> EnvoyXDSGenerator<'a> {
    pub fn new(effective_gateway: &'a common::Gateway) -> Self {
        Self { effective_gateway }
    }
    pub fn generate_xds(&self) -> Result<XdsData, Error> {
        let envoy_listeners = self.generate_envoy_representation();

        info!(target: TARGET,"Collapsed envoy listeners {envoy_listeners:#?}");
        let listeners = &envoy_listeners.values().cloned().collect::<Vec<_>>();
        let lds = Self::generate_lds(listeners)?;
        let boostrap = Self::generate_bootstrap(listeners)?;
        let rds = listeners
            .iter()
            .filter_map(|listener| {
                let maybe_content = Self::generate_rds(listener);
                if let Ok(rds_content) = maybe_content {
                    Some(rds_content)
                } else {
                    warn!(target: TARGET,"Can't generate RDS data for {}-{}  {:?}", listener.name, listener.port, maybe_content);
                    None
                }
            })
            .collect();

        let cds = Self::genereate_cds(listeners)?;
        Ok(XdsData::new(boostrap, lds, rds, cds))
    }

    fn generate_envoy_representation(&self) -> BTreeMap<i32, EnvoyListener> {
        let gateway = self.effective_gateway;
        gateway.listeners().fold(BTreeMap::<i32, EnvoyListener>::new(), |mut acc, listener| {
            let port = listener.port();
            let listener_name = listener.name().to_owned();
            let listener_hostname = listener.hostname().cloned();
            let gateway_name = gateway.name().to_owned();
            let protocol_type = listener.protocol();
            let maybe_added = acc.get_mut(&port);

            if let Some(envoy_listener) = maybe_added {
                match protocol_type {
                    ProtocolType::Http | ProtocolType::Https => {
                        let mut new_listener = Self::generate_virtual_hosts(gateway_name, listener);
                        envoy_listener.http_listener_map.append(&mut new_listener.http_listener_map);
                    },
                    ProtocolType::Tcp => {
                        envoy_listener.tcp_listener_map.insert((listener_name, listener_hostname));
                    },
                    _ => (),
                }
            } else {
                match protocol_type {
                    ProtocolType::Http | ProtocolType::Https => {
                        let envoy_listener = Self::generate_virtual_hosts(gateway_name, listener);
                        acc.insert(port, envoy_listener);
                    },
                    ProtocolType::Tcp => {
                        let mut listener_map = BTreeSet::new();
                        listener_map.insert((listener_name, listener_hostname));
                        acc.insert(
                            port,
                            EnvoyListener {
                                name: gateway_name,
                                port,
                                http_listener_map: BTreeSet::new(),
                                tcp_listener_map: listener_map,
                                tls_type: None,
                            },
                        );
                    },
                    _ => (),
                }
            }

            acc
        })
    }

    fn generate_virtual_hosts(gateway_name: String, listener: &Listener) -> EnvoyListener {
        let (resolved, unresolved) = listener.routes();
        let resolved: Vec<_> = resolved
            .into_iter()
            .filter(|r| match &r.config.route_type {
                RouteType::Http(_) => true,
                RouteType::Grpc(_) => false,
            })
            .collect();

        let mut listener_map = BTreeSet::new();
        let potential_hostnames = Self::calculate_potential_hostnames(&resolved, listener.hostname().cloned());
        debug!(target: TARGET,"generate_virtual_hosts Potential hostnames {potential_hostnames:?}");
        for potential_hostname in potential_hostnames {
            let effective_matching_rules = listener
                .http_matching_rules()
                .into_iter()
                .filter(|em| {
                    let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                    debug!(target: TARGET,"generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                    filtered
                })
                .collect::<Vec<_>>();

            listener_map.insert(EnvoyVirutalHost {
                effective_matching_rules,
                name: listener.name().to_owned() + "-" + &potential_hostname,
                hostname: listener.hostname().cloned(),
                effective_hostnames: Self::calculate_effective_hostnames(&resolved, Some(potential_hostname)),
                resolved_routes: resolved.iter().map(|r| (**r).clone()).collect(),
                unresolved_routes: unresolved.iter().map(|r| (**r).clone()).collect(),
            });
        }

        EnvoyListener {
            name: gateway_name,
            port: listener.port(),
            tls_type: listener.config().tls_type.clone(),
            http_listener_map: listener_map,
            tcp_listener_map: BTreeSet::new(),
        }
    }

    fn calculate_potential_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
        calculate_hostnames_common(routes, listener_hostname, |h| vec![h.to_owned()])
    }

    fn calculate_effective_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
        calculate_hostnames_common(routes, listener_hostname, |h| vec![format!("{h}:*"), h.to_owned()])
    }

    #[allow(clippy::too_many_lines)]
    fn generate_rds(listener: &EnvoyListener) -> Result<RdsData, Error> {
        #[derive(Serialize)]
        struct TeraClusterName {
            pub name: String,
            pub weight: i32,
        }

        #[derive(Serialize)]
        struct TeraMatchHeader {
            pub name: String,
            pub value: String,
            pub match_type: String,
        }

        #[derive(Serialize)]
        struct VeraRouteConfigs {
            pub path: Option<TeraPath>,
            pub headers: Option<Vec<TeraMatchHeader>>,
            pub cluster_name: String,
            pub cluster_names: Vec<TeraClusterName>,
            pub request_headers_to_add_or_set: Vec<TeraFilterHeader>,
            pub request_headers_to_remove: Vec<String>,
            pub response_headers_to_add_or_set: Vec<TeraFilterHeader>,
            pub response_headers_to_remove: Vec<String>,
            pub redirect_filter: Option<TeraFilterRedirect>,
        }

        #[derive(Serialize, Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
        #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
        enum FilterHeaderAction {
            AppendIfExistsOrAdd,
            OverwriteIfExistsOrAdd,
        }

        #[derive(Serialize, Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
        pub struct TeraFilterHeader {
            name: String,
            value: String,
            action: FilterHeaderAction,
        }
        impl From<HTTPHeader> for TeraFilterHeader {
            fn from(header: HTTPHeader) -> Self {
                Self { name: header.name, value: header.value, action: FilterHeaderAction::AppendIfExistsOrAdd }
            }
        }

        #[derive(Serialize)]
        struct TeraVirtualHost {
            pub hostname: String,
            pub hostnames: Vec<String>,
            pub route_configs: Vec<VeraRouteConfigs>,
        }

        #[derive(Serialize)]
        struct TeraRoute {
            pub name: String,
            pub virtual_hosts: Vec<TeraVirtualHost>,
        }

        #[derive(Serialize)]
        struct TeraFilterRedirect {
            pub hostname: Option<String>,
            pub status_code: Option<String>,
        }

        #[derive(Serialize)]
        struct TeraPath {
            pub path: String,
            pub match_type: String,
        }

        let mut tera_context = tera::Context::new();
        let EnvoyListener { name, port, http_listener_map, tcp_listener_map: _, tls_type: _ } = listener;

        let tvh: Vec<TeraVirtualHost> = http_listener_map
            .iter()
            .map(|evc| {
                let route_configs = evc
                    .effective_matching_rules
                    .iter()
                    .map(|er| VeraRouteConfigs {
                        path: er.route_matcher.path.clone().map(|matcher| TeraPath {
                            path: matcher
                                .value
                                .clone()
                                .map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v }),
                            match_type: matcher.r#type.map_or(String::new(), |f| match f {
                                httproutes::HttpRouteRulesMatchesPathType::Exact => "path".to_owned(),
                                httproutes::HttpRouteRulesMatchesPathType::PathPrefix => {
                                    if let Some(val) = matcher.value {
                                        if val == "/" { "prefix".to_owned() } else { "path_separated_prefix".to_owned() }
                                    } else {
                                        "prefix".to_owned()
                                    }
                                },
                                httproutes::HttpRouteRulesMatchesPathType::RegularExpression => "safe_regex".to_owned(),
                            }),
                        }),
                        headers: er.route_matcher.headers.clone().map(|headers| {
                            headers
                                .iter()
                                .map(|header| TeraMatchHeader {
                                    name: header.name.clone(),
                                    value: header.value.clone(),
                                    match_type: "exact".to_owned(),
                                })
                                .collect::<Vec<TeraMatchHeader>>()
                        }),
                        cluster_name: er.name.clone(),
                        cluster_names: er
                            .backends
                            .iter()
                            .filter_map(|b| match b.backend_type() {
                                common::BackendType::Service(service_type_config) => Some(service_type_config),
                                _ => None,
                            })
                            .filter(|b| b.weight() > 0)
                            .map(|b| TeraClusterName { name: b.cluster_name(), weight: b.weight() })
                            .collect(),
                        request_headers_to_add_or_set: er
                            .request_headers
                            .add
                            .clone()
                            .into_iter()
                            .map(TeraFilterHeader::from)
                            .chain(er.request_headers.set.clone().into_iter().map(TeraFilterHeader::from).map(|mut th| {
                                th.action = FilterHeaderAction::OverwriteIfExistsOrAdd;
                                th
                            }))
                            .collect(),
                        request_headers_to_remove: er.request_headers.remove.clone(),
                        response_headers_to_add_or_set: vec![],
                        response_headers_to_remove: vec![],
                        redirect_filter: er.redirect_filter.clone().map(|f| TeraFilterRedirect {
                            hostname: f.hostname,
                            status_code: match f.status_code {
                                Some(302) => Some("FOUND".to_owned()),
                                Some(303) => Some("SEE_OTHER".to_owned()),
                                Some(307) => Some("TEMPORARY_REDIRECT".to_owned()),
                                Some(308) => Some("PERMANENT_REDIRECT".to_owned()),
                                Some(301 | _) => Some("MOVED_PERMANENTLY".to_owned()),
                                None => None,
                            },
                        }),
                    })
                    .collect();

                TeraVirtualHost {
                    hostname: evc.hostname.clone().map_or("default-accept-all".to_owned(), |hostname| hostname.clone()),
                    hostnames: evc.effective_hostnames.clone(),
                    route_configs,
                }
            })
            .collect();
        let route_name = format!("{}-{}-route", name.clone(), port);
        let tr = TeraRoute { name: route_name.clone(), virtual_hosts: tvh };
        tera_context.insert("route", &tr);
        let rds_content = TEMPLATES.render("rds.yaml.tera", &tera_context)?;
        Ok(RdsData::new(route_name, rds_content))
    }

    fn genereate_cds(listeners: &[EnvoyListener]) -> Result<String, Error> {
        #[derive(Serialize, Debug, PartialEq, PartialOrd, Ord, Eq)]
        pub struct TeraEndpoint {
            pub service: String,
            pub port: i32,
            pub weight: i32,
        }
        #[derive(Serialize, Debug, PartialEq, PartialOrd, Ord, Eq)]
        pub struct TeraCluster {
            pub name: String,
            pub endpoints: Vec<TeraEndpoint>,
        }

        let mut tera_context = tera::Context::new();

        let tera_clusters: BTreeSet<TeraCluster> = listeners
            .iter()
            .flat_map(|listener| {
                listener.http_listener_map.iter().flat_map(|evc| {
                    evc.resolved_routes.iter().chain(evc.unresolved_routes.iter()).flat_map(|r| {
                        r.backends()
                            .iter()
                            .filter_map(|b| match b {
                                Backend::Resolved(common::BackendType::Service(config)) => Some(config),
                                _ => None,
                            })
                            .filter(|b| b.weight() > 0)
                            .map(|r| TeraCluster {
                                name: r.cluster_name(),
                                endpoints: vec![TeraEndpoint { service: r.endpoint.clone(), port: r.effective_port, weight: r.weight }],
                            })
                            .collect::<Vec<_>>()
                    })
                })
            })
            .collect();

        tera_context.insert("clusters", &tera_clusters);
        let cds = TEMPLATES.render("cds.yaml.tera", &tera_context)?;
        Ok(cds)
    }

    fn generate_bootstrap(listeners: &[EnvoyListener]) -> Result<String, Error> {
        #[derive(Serialize, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub struct TeraSecret {
            name: String,
            certificate: String,
            key: String,
        }
        let tera_secrets: BTreeSet<TeraSecret> = listeners
            .iter()
            .filter_map(|l| {
                if let Some(TlsType::Terminate(certificates)) = &l.tls_type {
                    Some(
                        certificates
                            .iter()
                            .filter_map(|cert| match cert {
                                common::Certificate::ResolvedSameSpace(resource_key, _) => Some(resource_key),
                                common::Certificate::NotResolved(_)
                                | common::Certificate::Invalid(_)
                                | common::Certificate::ResolvedCrossSpace(..) => None,
                            })
                            .map(|certificate_key| TeraSecret {
                                name: create_secret_name(certificate_key),
                                certificate: create_certificate_name(certificate_key),
                                key: create_key_name(certificate_key),
                            }),
                    )
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        let mut tera_context = tera::Context::new();
        if !tera_secrets.is_empty() {
            tera_context.insert("secrets", &tera_secrets);
        }

        Ok(TEMPLATES.render("envoy-bootstrap.yaml.tera", &tera_context)?)
    }

    fn generate_lds(listeners: &[EnvoyListener]) -> Result<String, Error> {
        #[derive(Serialize, Debug)]
        pub struct TeraSecret {
            name: String,
        }
        #[derive(Serialize, Debug)]
        pub struct TeraListener {
            pub name: String,
            pub port: i32,
            pub route_name: String,
            pub ip_address: String,
            pub secrets: Option<Vec<TeraSecret>>,
        }
        let tera_listeners: Vec<TeraListener> = listeners
            .iter()
            .map(|l| TeraListener {
                name: l.name.clone(),
                port: l.port,
                route_name: format!("{}-dynamic-route", l.name),
                ip_address: "0.0.0.0".to_owned(),
                secrets: if let Some(TlsType::Terminate(certificates)) = &l.tls_type {
                    Some(
                        certificates
                            .iter()
                            .filter_map(|cert| match cert {
                                common::Certificate::ResolvedSameSpace(resource_key, _) => Some(resource_key),
                                common::Certificate::NotResolved(_)
                                | common::Certificate::Invalid(_)
                                | common::Certificate::ResolvedCrossSpace(..) => None,
                            })
                            .map(|certificate_key| TeraSecret { name: create_secret_name(certificate_key) })
                            .collect(),
                    )
                } else {
                    None
                },
            })
            .collect();
        let mut tera_context = tera::Context::new();
        tera_context.insert("listeners", &tera_listeners);
        let lds = TEMPLATES.render("lds.yaml.tera", &tera_context)?;
        Ok(lds)
    }
}
