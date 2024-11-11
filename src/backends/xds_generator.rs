use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use tracing::warn;

use super::envoy_deployer::TEMPLATES;
use crate::common::{self, Backend, ProtocolType, Route};
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
    pub lds_content: String,
    pub rds_content: Vec<RdsData>,
    pub cds_content: String,
}

impl XdsData {
    pub fn new(lds_content: String, rds_content: Vec<RdsData>, cds_content: String) -> Self {
        Self {
            lds_content,
            rds_content,
            cds_content,
        }
    }
}

type ListenerNameToHostname = (String, Option<String>);

#[derive(Debug, Clone)]
struct EnvoyVirutalHost {
    name: String,
    hostname: Option<String>,
    route: Route,
}

impl Ord for EnvoyVirutalHost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl Eq for EnvoyVirutalHost {}

impl PartialOrd for EnvoyVirutalHost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.name.cmp(&other.name))
    }
}

impl PartialEq for EnvoyVirutalHost {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.hostname == other.hostname && self.route == other.route
    }
}

#[derive(Debug, Clone)]
pub struct EnvoyListener {
    name: String,
    port: i32,
    http_listener_map: BTreeSet<EnvoyVirutalHost>,
    tcp_listener_map: BTreeSet<ListenerNameToHostname>,
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
        let collapsed_listeners = self.collapse_listeners_and_routes();

        warn!("Collapsed listeners {collapsed_listeners:#?}");
        let listeners = &collapsed_listeners.values().cloned().collect::<Vec<_>>();
        let lds = Self::generate_lds(listeners)?;
        let rds = listeners
            .iter()
            .filter_map(|listener| {
                if let Ok(rds_content) = Self::genereate_rds(listener) {
                    Some(rds_content)
                } else {
                    warn!("Can't generate RDS data for {}-{}", listener.name, listener.port);
                    None
                }
            })
            .collect();

        let cds = Self::genereate_cds(listeners)?;
        Ok(XdsData::new(lds, rds, cds))
    }
    fn collapse_listeners_and_routes(&self) -> BTreeMap<i32, EnvoyListener> {
        let gateway = self.effective_gateway;
        let envoy_listeners = gateway.listeners().fold(BTreeMap::<i32, EnvoyListener>::new(), |mut acc, listener| {
            let port = listener.port();
            let listener_name = listener.name().to_owned();
            let listener_hostname = listener.hostname().cloned();
            let gateway_name = gateway.name().to_owned();
            let protocol_type = listener.protocol();
            let maybe_added = acc.get_mut(&port);

            if let Some(added) = maybe_added {
                match protocol_type {
                    ProtocolType::Http => {
                        let (resolved, _, _) = listener.routes();
                        let resolved: Vec<_> = resolved
                            .iter()
                            .filter(|r| match r {
                                Route::Http(_) => true,
                                Route::Grpc(_) => false,
                            })
                            .collect();
                        added.http_listener_map.append(
                            &mut resolved
                                .into_iter()
                                .map(|r| EnvoyVirutalHost {
                                    name: listener_name.clone(),
                                    hostname: listener_hostname.clone(),
                                    route: (*r).clone(),
                                })
                                .collect(),
                        );
                    }
                    ProtocolType::Tcp => {
                        added.tcp_listener_map.insert((listener_name, listener_hostname));
                    }
                    _ => (),
                }
            } else {
                match protocol_type {
                    ProtocolType::Http => {
                        let (resolved, _, _) = listener.routes();
                        let resolved: Vec<_> = resolved
                            .iter()
                            .filter(|r| match r {
                                Route::Http(_) => true,
                                Route::Grpc(_) => false,
                            })
                            .collect();
                        let listener_map = resolved
                            .into_iter()
                            .map(|r| EnvoyVirutalHost {
                                name: listener_name.clone(),
                                hostname: listener_hostname.clone(),
                                route: (*r).clone(),
                            })
                            .collect();

                        acc.insert(
                            port,
                            EnvoyListener {
                                name: gateway_name,
                                port,
                                http_listener_map: listener_map,
                                tcp_listener_map: BTreeSet::new(),
                            },
                        );
                    }
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
                            },
                        );
                    }
                    _ => (),
                }
            }

            acc
        });
        envoy_listeners
    }

    fn genereate_rds(listener: &EnvoyListener) -> Result<RdsData, Error> {
        #[derive(Serialize)]
        struct VeraRouteConfigs {
            pub path: Option<TeraPath>,
            pub cluster_name: String,
        }

        #[derive(Serialize)]
        struct TeraVirtualHost {
            pub hostname: String,
            pub route_configs: Vec<VeraRouteConfigs>,
        }

        #[derive(Serialize)]
        struct TeraRoute {
            pub name: String,
            pub virtual_hosts: Vec<TeraVirtualHost>,
        }

        #[derive(Serialize)]
        struct TeraPath {
            pub path: String,
            pub match_type: String,
        }

        let mut tera_context = tera::Context::new();
        let EnvoyListener {
            name,
            port,
            http_listener_map,
            tcp_listener_map: _,
        } = listener;

        let tvh: Vec<TeraVirtualHost> = http_listener_map
            .iter()
            .map(|evc| TeraVirtualHost {
                hostname: evc.hostname.clone().unwrap_or("*".to_owned()),
                route_configs: evc
                    .route
                    .routing_rules()
                    .iter()
                    .flat_map(|rr| {
                        rr.matching_rules.iter().map(|mr| VeraRouteConfigs {
                            path: mr.path.clone().map(|matcher| TeraPath {
                                path: matcher.value.unwrap_or_default(),
                                match_type: matcher.r#type.map_or(String::new(), |f| match f {
                                    gateway_api::apis::standard::httproutes::HTTPRouteRulesMatchesPathType::Exact => "path".to_owned(),
                                    gateway_api::apis::standard::httproutes::HTTPRouteRulesMatchesPathType::PathPrefix => "prefix".to_owned(),
                                    gateway_api::apis::standard::httproutes::HTTPRouteRulesMatchesPathType::RegularExpression => "safe_regex".to_owned(),
                                }),
                            }),
                            cluster_name: rr.name.clone(),
                        })
                    })
                    .collect(),
            })
            .collect();
        let route_name = format!("{}-{}-route", name.clone(), port);
        let tr = TeraRoute {
            name: route_name.clone(),
            virtual_hosts: tvh,
        };
        tera_context.insert("route", &tr);
        let rds_content = TEMPLATES.render("rds.yaml.tera", &tera_context)?;
        Ok(RdsData::new(route_name, rds_content))
    }

    fn genereate_cds(listeners: &[EnvoyListener]) -> Result<String, Error> {
        #[derive(Serialize, Debug)]
        pub struct TeraEndpoint {
            pub service: String,
            pub port: i32,
            pub weight: i32,
        }
        #[derive(Serialize, Debug)]
        pub struct TeraCluster {
            pub name: String,
            pub endpoints: Vec<TeraEndpoint>,
        }

        let mut tera_context = tera::Context::new();

        let tera_clusters: Vec<TeraCluster> = listeners
            .iter()
            .flat_map(|listener| {
                listener.http_listener_map.iter().flat_map(|evc| {
                    evc.route.routing_rules().iter().map(|rr| {
                        let endpoints = rr
                            .backends
                            .iter()
                            .map(Backend::config)
                            .map(|r| TeraEndpoint {
                                service: r.endpoint.clone(),
                                port: r.port,
                                weight: r.weight,
                            })
                            .collect();
                        TeraCluster { name: rr.name.clone(), endpoints }
                    })
                })
            })
            .collect();

        tera_context.insert("clusters", &tera_clusters);
        let cds = TEMPLATES.render("cds.yaml.tera", &tera_context)?;
        Ok(cds)
    }

    fn generate_lds(listeners: &[EnvoyListener]) -> Result<String, Error> {
        #[derive(Serialize, Debug)]
        pub struct TeraListener {
            pub name: String,
            pub port: i32,
            pub route_name: String,
            pub ip_address: String,
        }
        let tera_listeners: Vec<TeraListener> = listeners
            .iter()
            .map(|l| TeraListener {
                name: l.name.clone(),
                port: l.port,
                route_name: format!("{}-dynamic-route", l.name),
                ip_address: "0.0.0.0".to_owned(),
            })
            .collect();
        let mut tera_context = tera::Context::new();
        tera_context.insert("listeners", &tera_listeners);
        let lds = TEMPLATES.render("lds.yaml.tera", &tera_context)?;
        Ok(lds)
    }
}

#[cfg(test)]
mod tests {
    // use std::{fs::File, io::Write};

    // use gateway_api::apis::standard::{gateways::Gateway, httproutes::HTTPRoute};

    // use crate::{
    //     backends::xds_generator::{EnvoyXDSGenerator, RdsData, XdsData},
    //     common::{self, Route},
    // };

    // https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/refs/heads/main/conformance/tests/gateway-http-listener-isolation.yaml
    #[allow(clippy::similar_names)]
    #[test]
    pub fn test1() {
        todo!();
    }
}

//     const GATEWAY_YAML: &str = r#"
// apiVersion: gateway.networking.k8s.io/v1
// kind: Gateway
// metadata:
//   name: http-listener-isolation
//   namespace: gateway-conformance-infra
// spec:
//   gatewayClassName: my-class
//   listeners:
//   - name: empty-hostname
//     port: 80
//     protocol: HTTP
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: wildcard-example-com
//     port: 80
//     protocol: HTTP
//     hostname: "*.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: wildcard-foo-example-com
//     port: 80
//     protocol: HTTP
//     hostname: "*.foo.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: abc-foo-example-com
//     port: 80
//     protocol: HTTP
//     hostname: "abc.foo.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: empty-hostname-tcp
//     port: 80
//     protocol: TCP
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: wildcard-example-com-tcp
//     port: 80
//     protocol: TCP
//     hostname: "*.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: wildcard-foo-example-com-tcp
//     port: 80
//     protocol: TCP
//     hostname: "*.foo.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: abc-foo-example-com-tcp
//     port: 80
//     protocol: TCP
//     hostname: "abc.foo.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: empty-hostname-9090
//     port: 9090
//     protocol: HTTP
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: wildcard-example-com-9090
//     port: 9090
//     protocol: HTTP
//     hostname: "*.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: wildcard-foo-example-com-9090
//     port: 9090
//     protocol: HTTP
//     hostname: "*.foo.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
//   - name: abc-foo-example-com-tcp-9090
//     port: 9090
//     protocol: HTTP
//     hostname: "abc.foo.example.com"
//     allowedRoutes:
//       namespaces:
//         from: All
// "#;

//     const ROUTE1_YAML: &str = r"
// apiVersion: gateway.networking.k8s.io/v1
// kind: HTTPRoute
// metadata:
//   name: attaches-to-empty-hostname
//   namespace: gateway-conformance-infra
// spec:
//   parentRefs:
//   - name: http-listener-isolation
//     namespace: gateway-conformance-infra
//     sectionName: empty-hostname
//   - name: http-listener-isolation
//     namespace: gateway-conformance-infra
//     sectionName: empty-hostname-9090
//   rules:
//   - matches:
//     - path:
//         type: PathPrefix
//         value: /empty-hostname
//     - path:
//         type: Exact
//         value: /empty-hostname-2
//     backendRefs:
//     - name: infra-backend-v1
//       port: 8080
// ";

//     const ROUTE2_YAML: &str = r"
// apiVersion: gateway.networking.k8s.io/v1
// kind: HTTPRoute
// metadata:
//   name: attaches-to-wildcard-example-com
//   namespace: gateway-conformance-infra
// spec:
//   parentRefs:
//   - name: http-listener-isolation
//     namespace: gateway-conformance-infra
//     sectionName: wildcard-example-com
//   - name: http-listener-isolation
//     namespace: gateway-conformance-infra
//     sectionName: wildcard-example-com-9090
//   rules:
//   - matches:
//     - path:
//         type: PathPrefix
//         value: /wildcard-example-com
//     backendRefs:
//     - name: infra-backend-v1
//       port: 8080
// ";

//     const ROUTE3_YAML: &str = r"
// apiVersion: gateway.networking.k8s.io/v1
// kind: HTTPRoute
// metadata:
//   name: attaches-to-wildcard-foo-example-com
//   namespace: gateway-conformance-infra
// spec:
//   parentRefs:
//   - name: http-listener-isolation
//     namespace: gateway-conformance-infra
//     sectionName: wildcard-foo-example-com
//   rules:
//   - matches:
//     - path:
//         type: PathPrefix
//         value: /wildcard-foo-example-com
//     backendRefs:
//     - name: infra-backend-v1
//       port: 8080
// ";

//     const ROUTE4_YAML: &str = r"
// apiVersion: gateway.networking.k8s.io/v1
// kind: HTTPRoute
// metadata:
//   name: attaches-to-abc-foo-example-com
//   namespace: gateway-conformance-infra
// spec:
//   parentRefs:
//   - name: http-listener-isolation
//     namespace: gateway-conformance-infra
//     sectionName: abc-foo-example-com
//   rules:
//   - matches:
//     - path:
//         type: PathPrefix
//         value: /abc-foo-example-com
//     backendRefs:
//     - name: infra-backend-v1
//       port: 8080
// ";
// }
