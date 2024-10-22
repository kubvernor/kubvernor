use std::collections::{BTreeMap, BTreeSet};

use gateway_api::apis::standard::gateways::Gateway;
use kube::ResourceExt;
use serde::Serialize;
use tracing::warn;

use super::envoy_deployer::TEMPLATES;
use crate::common::{ProtocolType, Route, RouteToListenersMapping};
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
    gateway: &'a Gateway,
    route_mapping: &'a [RouteToListenersMapping],
}

impl<'a> EnvoyXDSGenerator<'a> {
    pub fn new(gateway: &'a Gateway, route_mapping: &'a [RouteToListenersMapping]) -> Self {
        Self { gateway, route_mapping }
    }
    pub fn generate_xds(&self) -> Result<XdsData, Error> {
        let collapsed_listeners = self.collapse_listeners_and_routes();
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
        let gateway = self.gateway;
        let route_to_listeners_mapping = self.route_mapping;
        let envoy_listeners = gateway.spec.listeners.iter().fold(BTreeMap::<i32, EnvoyListener>::new(), |mut acc, listener| {
            let port = listener.port;
            let name = gateway.name_any();
            let protocol_type = ProtocolType::try_from(listener.protocol.clone()).unwrap_or(ProtocolType::Http);
            let maybe_added = acc.get_mut(&port);

            if let Some(added) = maybe_added {
                match protocol_type {
                    ProtocolType::Http => {
                        for RouteToListenersMapping { route, listeners } in route_to_listeners_mapping {
                            for route_listener in listeners {
                                if route_listener.name == listener.name {
                                    added.http_listener_map.insert(EnvoyVirutalHost {
                                        name: listener.name.clone(),
                                        hostname: listener.hostname.clone(),
                                        route: route.clone(),
                                    });
                                }
                            }
                        }
                    }
                    ProtocolType::Tcp => {
                        added.tcp_listener_map.insert((listener.name.clone(), listener.hostname.clone()));
                    }
                    _ => (),
                }
            } else {
                match protocol_type {
                    ProtocolType::Http => {
                        let mut listener_map = BTreeSet::new();
                        for RouteToListenersMapping { route, listeners } in route_to_listeners_mapping {
                            for route_listener in listeners {
                                if route_listener.name == listener.name {
                                    listener_map.insert(EnvoyVirutalHost {
                                        name: listener.name.clone(),
                                        hostname: listener.hostname.clone(),
                                        route: route.clone(),
                                    });
                                }
                            }
                        }
                        acc.insert(
                            port,
                            EnvoyListener {
                                name,
                                port,
                                http_listener_map: listener_map,
                                tcp_listener_map: BTreeSet::new(),
                            },
                        );
                    }
                    ProtocolType::Tcp => {
                        let mut listener_map = BTreeSet::new();
                        listener_map.insert((listener.name.clone(), listener.hostname.clone()));
                        acc.insert(
                            port,
                            EnvoyListener {
                                name,
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
    use std::{fs::File, io::Write};

    use gateway_api::apis::standard::{gateways::Gateway, httproutes::HTTPRoute};

    use crate::{
        backends::xds_generator::{EnvoyXDSGenerator, RdsData, XdsData},
        common::{Route, RouteToListenersMapping},
    };

    // https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/refs/heads/main/conformance/tests/gateway-http-listener-isolation.yaml
    #[allow(clippy::similar_names)]
    #[test]
    pub fn test1() {
        let gateway: Gateway = serde_yaml::from_str(GATEWAY_YAML).unwrap();
        let route1: HTTPRoute = serde_yaml::from_str(ROUTE1_YAML).unwrap();
        let route2: HTTPRoute = serde_yaml::from_str(ROUTE2_YAML).unwrap();
        let route3: HTTPRoute = serde_yaml::from_str(ROUTE3_YAML).unwrap();
        let route4: HTTPRoute = serde_yaml::from_str(ROUTE4_YAML).unwrap();

        let route11 = Route::try_from(&route1).unwrap();
        let route12 = Route::try_from(&route2).unwrap();
        let route13 = Route::try_from(&route3).unwrap();
        let route14 = Route::try_from(&route4).unwrap();

        let listener1 = gateway.spec.listeners[0].clone();
        let listener2 = gateway.spec.listeners[1].clone();
        let listener3 = gateway.spec.listeners[2].clone();
        let listener4 = gateway.spec.listeners[3].clone();
        let _listener5 = gateway.spec.listeners[4].clone();
        let _listener6 = gateway.spec.listeners[5].clone();
        let _listener7 = gateway.spec.listeners[6].clone();
        let _listener8 = gateway.spec.listeners[7].clone();
        let listener9 = gateway.spec.listeners[8].clone();
        let listener10 = gateway.spec.listeners[9].clone();
        let _listener11 = gateway.spec.listeners[10].clone();
        let _listener12 = gateway.spec.listeners[11].clone();

        let route_mapping = vec![
            RouteToListenersMapping {
                route: route11,
                listeners: vec![listener1, listener9],
            },
            RouteToListenersMapping {
                route: route12,
                listeners: vec![listener2, listener10],
            },
            RouteToListenersMapping {
                route: route13,
                listeners: vec![listener3],
            },
            RouteToListenersMapping {
                route: route14,
                listeners: vec![listener4],
            },
        ];
        let generator = EnvoyXDSGenerator {
            gateway: &gateway,
            route_mapping: &route_mapping,
        };

        let xds_data = generator.generate_xds().unwrap();

        let XdsData {
            lds_content,
            rds_content,
            cds_content,
        } = xds_data;

        let mut file = File::create("test_lds.yaml").unwrap();
        file.write_all(lds_content.as_bytes()).unwrap();
        let mut file = File::create("test_cds.yaml").unwrap();
        file.write_all(cds_content.as_bytes()).unwrap();
        for rds in &rds_content {
            let mut file = File::create(format!("{}.yaml", rds.route_name)).unwrap();
            file.write_all(rds.rds_content.as_bytes()).unwrap();
        }

        let _: serde_yaml::value::Value = serde_yaml::from_str(&lds_content).unwrap();
        let _: serde_yaml::value::Value = serde_yaml::from_str(&cds_content).unwrap();
        for RdsData { route_name: _, rds_content } in rds_content {
            let _: serde_yaml::value::Value = serde_yaml::from_str(&rds_content).unwrap();
        }
    }

    const GATEWAY_YAML: &str = r#"
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: http-listener-isolation
  namespace: gateway-conformance-infra
spec:
  gatewayClassName: my-class
  listeners:
  - name: empty-hostname
    port: 80
    protocol: HTTP
    allowedRoutes:
      namespaces:
        from: All
  - name: wildcard-example-com
    port: 80
    protocol: HTTP
    hostname: "*.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: wildcard-foo-example-com
    port: 80
    protocol: HTTP
    hostname: "*.foo.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: abc-foo-example-com
    port: 80
    protocol: HTTP
    hostname: "abc.foo.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: empty-hostname-tcp
    port: 80
    protocol: TCP
    allowedRoutes:
      namespaces:
        from: All
  - name: wildcard-example-com-tcp
    port: 80
    protocol: TCP
    hostname: "*.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: wildcard-foo-example-com-tcp
    port: 80
    protocol: TCP
    hostname: "*.foo.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: abc-foo-example-com-tcp
    port: 80
    protocol: TCP
    hostname: "abc.foo.example.com"
    allowedRoutes:
      namespaces:
        from: All        
  - name: empty-hostname-9090
    port: 9090
    protocol: HTTP
    allowedRoutes:
      namespaces:
        from: All
  - name: wildcard-example-com-9090
    port: 9090
    protocol: HTTP
    hostname: "*.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: wildcard-foo-example-com-9090
    port: 9090
    protocol: HTTP
    hostname: "*.foo.example.com"
    allowedRoutes:
      namespaces:
        from: All
  - name: abc-foo-example-com-tcp-9090
    port: 9090
    protocol: HTTP
    hostname: "abc.foo.example.com"
    allowedRoutes:
      namespaces:
        from: All                
"#;

    const ROUTE1_YAML: &str = r"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: attaches-to-empty-hostname
  namespace: gateway-conformance-infra
spec:
  parentRefs:
  - name: http-listener-isolation
    namespace: gateway-conformance-infra
    sectionName: empty-hostname
  - name: http-listener-isolation
    namespace: gateway-conformance-infra
    sectionName: empty-hostname-9090        
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /empty-hostname    
    - path:
        type: Exact
        value: /empty-hostname-2
    backendRefs:
    - name: infra-backend-v1
      port: 8080
";

    const ROUTE2_YAML: &str = r"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: attaches-to-wildcard-example-com
  namespace: gateway-conformance-infra
spec:
  parentRefs:
  - name: http-listener-isolation
    namespace: gateway-conformance-infra
    sectionName: wildcard-example-com
  - name: http-listener-isolation
    namespace: gateway-conformance-infra
    sectionName: wildcard-example-com-9090    
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /wildcard-example-com
    backendRefs:
    - name: infra-backend-v1
      port: 8080
";

    const ROUTE3_YAML: &str = r"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: attaches-to-wildcard-foo-example-com
  namespace: gateway-conformance-infra
spec:
  parentRefs:
  - name: http-listener-isolation
    namespace: gateway-conformance-infra
    sectionName: wildcard-foo-example-com
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /wildcard-foo-example-com
    backendRefs:
    - name: infra-backend-v1
      port: 8080
";

    const ROUTE4_YAML: &str = r"
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: attaches-to-abc-foo-example-com
  namespace: gateway-conformance-infra
spec:
  parentRefs:
  - name: http-listener-isolation
    namespace: gateway-conformance-infra
    sectionName: abc-foo-example-com
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /abc-foo-example-com
    backendRefs:
    - name: infra-backend-v1
      port: 8080
";
}
