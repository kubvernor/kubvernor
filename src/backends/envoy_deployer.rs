use std::collections::BTreeMap;

use futures::FutureExt;
use itertools::Itertools;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{ConfigMap, ConfigMapVolumeSource, ContainerPort, KeyToPath, PodSpec, PodTemplateSpec, Service, ServicePort, ServiceSpec, Volume},
    },
    apimachinery::pkg::{apis::meta::v1::LabelSelector, util::intstr::IntOrString},
};
use kube::{
    api::{DeleteParams, Patch, PatchParams},
    Api, Client,
};
use kube_core::ObjectMeta;
use lazy_static::lazy_static;
use serde::Serialize;
use tera::Tera;
use tokio::sync::mpsc::{self, Receiver};
use tracing::{debug, info, warn};

use crate::common::{
    ChangedContext, DeletedContext, Gateway, GatewayEvent, GatewayProcessedPayload, GatewayResponse, GatewayStatus, Listener, ListenerStatus, Route, RouteProcessedPayload, RouteStatus,
    RouteToListenersMapping,
};

use super::compactor;

lazy_static! {
    pub static ref TEMPLATES: Tera = {
        match Tera::new("templates/**.tera") {
            Ok(t) => t,
            Err(e) => {
                warn!("Parsing error(s): {}", e);
                ::std::process::exit(1);
            }
        }
    };
}

pub struct EnvoyDeployerChannelHandler {
    controller_name: String,
    client: Client,
    event_receiver: Receiver<GatewayEvent>,
}

impl EnvoyDeployerChannelHandler {
    pub fn new(controller_name: &str, client: Client) -> (mpsc::Sender<GatewayEvent>, Self) {
        let (sender, receiver) = mpsc::channel(1024);
        (
            sender,
            Self {
                controller_name: controller_name.to_owned(),
                client,
                event_receiver: receiver,
            },
        )
    }
    pub async fn start(&mut self) {
        info!("Gateways handler started");
        loop {
            tokio::select! {
                    Some(event) = self.event_receiver.recv() => {
                        info!("Backend got event {event:#}");
                         match event{
                            GatewayEvent::GatewayChanged(ChangedContext{ response_sender, gateway, route_to_listeners_mapping }) => {
                                self.deploy_envoy(&gateway, &route_to_listeners_mapping).await;
                                let attached_routes: Vec<_> = route_to_listeners_mapping.iter().map(|m| m.route.clone()).collect();
                                let listener_status = gateway.listeners.iter().map(|l| ListenerStatus::Accepted((l.name().to_owned(), i32::try_from(attached_routes.len()).unwrap_or_default()))).collect();
                                let gateway_status = GatewayStatus{ id: uuid::Uuid::new_v4(), name: gateway.name, namespace: gateway.namespace, listeners: listener_status };
                                let _res = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload{gateway_status, attached_routes, ignored_routes: vec![]}));
                            }

                            GatewayEvent::GatewayDeleted(DeletedContext{ response_sender, gateway, routes }) => {
                                self.delete_envoy(&gateway, &routes).await;
                                let _res = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                            }

                            GatewayEvent::RouteChanged(ChangedContext{ response_sender, gateway, route_to_listeners_mapping }) => {
                                self.deploy_envoy(&gateway, &route_to_listeners_mapping).await;
                                let _res = response_sender.send(GatewayResponse::RouteProcessed(RouteProcessedPayload{ status: RouteStatus::Attached}));
                            }

                        }
                    }
                    else => {
                        break;
                    }
            }
        }
    }

    async fn deploy_envoy(&self, gateway: &Gateway, route_to_listeners_mapping: &[RouteToListenersMapping]) {
        debug!("Deploying Envoy {gateway:?}");
        let service_api: Api<Service> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let deployment_api: Api<Deployment> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let config_map_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let (xds_cm, bootstrap_cm) = config_map_names(gateway);

        let envoy_boostrap_config_map = Self::create_envoy_bootstrap_config_map(&bootstrap_cm, gateway);

        let service = Self::create_service(gateway);

        let deployment = Self::create_deployment(gateway);

        let pp = PatchParams {
            field_manager: Some(self.controller_name.clone()),
            ..Default::default()
        };

        let res = config_map_api.patch(&bootstrap_cm, &pp, &Patch::Apply(&envoy_boostrap_config_map)).await;
        if res.is_ok() {
            debug!("Created bootstrap config map for {}-{}", gateway.name, gateway.namespace);
        } else {
            warn!("Could not create bootstrap config map for {}-{} {:?}", gateway.name, gateway.namespace, res.err());
        }

        let maybe_templates = Self::create_envoy_xds(gateway, route_to_listeners_mapping);
        if let Ok(XdsData {
            lds_content,
            rds_content,
            cds_content,
        }) = maybe_templates
        {
            let envoy_xds_config_map = Self::create_envoy_xds_config_map(&xds_cm, gateway, lds_content, rds_content, cds_content);
            let res = config_map_api.patch(&xds_cm, &pp, &Patch::Apply(&envoy_xds_config_map)).await;
            if res.is_ok() {
                debug!("Created xds config map for {}-{}", gateway.name, gateway.namespace);
            } else {
                warn!("Could not create xds config map for {}-{} {:?}", gateway.name, gateway.namespace, res.err());
            }
        } else {
            warn!("Problem when processing templates {maybe_templates:?}");
        }

        let res = service_api.patch(&gateway.name, &pp, &Patch::Apply(&service)).await;
        if res.is_ok() {
            debug!("Created service {}-{}", gateway.name, gateway.namespace);
        } else {
            warn!("Could not create service  {}-{} {:?}", gateway.name, gateway.namespace, res.err());
        }

        let res = deployment_api.patch(&gateway.name, &pp, &Patch::Apply(&deployment)).await;

        if res.is_ok() {
            debug!("Created deployment {}-{}", gateway.name, gateway.namespace);
        } else {
            warn!("Could not create deployment  {}-{} {:?}", gateway.name, gateway.namespace, res.err());
        }
    }

    async fn delete_envoy(&self, gateway: &Gateway, _routes: &[Route]) {
        debug!("Deleting Envoy {gateway:?}");
        let service_api: Api<Service> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let deployment_api: Api<Deployment> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let config_map_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let dp = DeleteParams { ..Default::default() };
        let (xds_cm, bootstrap_cm) = config_map_names(gateway);
        let futures = [
            service_api
                .delete(&gateway.name, &dp)
                .map(|f| {
                    if f.is_ok() {
                        debug!("Deleted for  {}-{}", gateway.name, gateway.namespace);
                    } else {
                        warn!("Could not delete {}-{} {:?}", gateway.name, gateway.namespace, f.err());
                    }
                })
                .boxed(),
            deployment_api
                .delete(&gateway.name, &dp)
                .map(|f| {
                    if f.is_ok() {
                        debug!("Deleted for  {}-{}", gateway.name, gateway.namespace);
                    } else {
                        warn!("Could not delete {}-{} {:?}", gateway.name, gateway.namespace, f.err());
                    }
                })
                .boxed(),
            config_map_api
                .delete(&xds_cm, &dp)
                .map(|f| {
                    if f.is_ok() {
                        debug!("Deleted for  {}-{}", gateway.name, gateway.namespace);
                    } else {
                        warn!("Could not delete {}-{} {:?}", gateway.name, gateway.namespace, f.err());
                    }
                })
                .boxed(),
            config_map_api
                .delete(&bootstrap_cm, &dp)
                .map(|f| {
                    if f.is_ok() {
                        debug!("Deleted for  {}-{}", gateway.name, gateway.namespace);
                    } else {
                        warn!("Could not delete {}-{} {:?}", gateway.name, gateway.namespace, f.err());
                    }
                })
                .boxed(),
        ];
        let _res = futures::future::join_all(futures).await;
    }

    fn create_envoy_xds_config_map(name: &str, gateway: &Gateway, lds: String, routes: Vec<RdsData>, cds: String) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("cds.yaml".to_owned(), cds);
        map.insert("lds.yaml".to_owned(), lds);

        for RdsData {
            route_file_name: route_file,
            rds_content,
        } in routes
        {
            map.insert(format!("{route_file}.yaml"), rds_content);
        }

        ConfigMap {
            binary_data: None,
            data: Some(map),
            immutable: None,
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(gateway.namespace.clone()),
                ..Default::default()
            },
        }
    }

    fn create_envoy_bootstrap_config_map(name: &str, gateway: &Gateway) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("envoy-bootstrap.yaml".to_owned(), ENVOY_BOOTSTRAP.to_owned());
        ConfigMap {
            binary_data: None,
            data: Some(map),
            immutable: None,
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(gateway.namespace.clone()),
                ..Default::default()
            },
        }
    }

    fn create_envoy_xds(_gateway: &Gateway, route_to_listeners_mapping: &[RouteToListenersMapping]) -> Result<XdsData, tera::Error> {
        #[derive(Serialize, Debug)]
        pub struct TeraListener {
            pub name: String,
            pub port: i32,
            pub route_name: String,
            pub ip_address: String,
        }

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

        #[derive(Serialize, Debug)]
        pub struct TeraRoute {
            pub name: String,
            pub cluster_name: String,
        }

        let mut tera_context = tera::Context::new();
        let tera_listeners: Vec<TeraListener> = route_to_listeners_mapping
            .iter()
            .flat_map(|r| &r.listeners)
            .dedup()
            .filter_map(|listener| {
                if let Ok(l) = Listener::try_from(listener) {
                    Some(TeraListener {
                        name: l.name().to_owned(),
                        port: l.port(),
                        route_name: format!("{}-dynamic-route", l.name()),
                        ip_address: "0.0.0.0".to_owned(),
                    })
                } else {
                    None
                }
            })
            .collect();

        let tera_clusters: Vec<_> = route_to_listeners_mapping
            .iter()
            .filter(|RouteToListenersMapping { route, listeners: _ }| match route {
                Route::Http(_) => true,
                Route::Grpc(_) => false,
            })
            .flat_map(|RouteToListenersMapping { route, listeners }| {
                route.routing_rules().iter().flat_map(|rr| {
                    listeners.iter().map(|l| {
                        let endpoints = rr
                            .backends
                            .iter()
                            .map(|r| TeraEndpoint {
                                service: r.endpoint.clone(),
                                port: r.port,
                                weight: r.weight,
                            })
                            .collect();
                        TeraCluster {
                            name: format!("cluster-{}", l.name),
                            endpoints,
                        }
                    })
                })
            })
            .collect();

        let tera_routes = route_to_listeners_mapping
            .iter()
            .filter(|RouteToListenersMapping { route, listeners: _ }| match route {
                Route::Http(_) => true,
                Route::Grpc(_) => false,
            })
            .flat_map(|RouteToListenersMapping { route: _, listeners }| {
                listeners.iter().map(|l| {
                    let route_name = format!("{}-dynamic-route", l.name);
                    let cluster_name = format!("cluster-{}", l.name);
                    (route_name.clone(), TeraRoute { name: route_name, cluster_name })
                })
            });

        let tera_routes = tera_routes
            .into_iter()
            .filter_map(|(name, route)| {
                tera_context.remove("route_name");
                tera_context.remove("cluster_name");
                tera_context.insert("route_name", &route.name);
                tera_context.insert("cluster_name", &route.cluster_name);

                let maybe_rds = TEMPLATES.render("rds.yaml.tera", &tera_context);
                match maybe_rds {
                    Ok(rds) => Some(RdsData::new(name, rds)),
                    Err(e) => {
                        warn!("Can't generate RDS for route {name} {e}");
                        None
                    }
                }
            })
            .collect();

        tera_context.insert("listeners", &tera_listeners);
        tera_context.insert("clusters", &tera_clusters);

        let lds = TEMPLATES.render("lds.yaml.tera", &tera_context)?;
        let cds = TEMPLATES.render("cds.yaml.tera", &tera_context)?;
        Ok(XdsData::new(lds, tera_routes, cds))
    }

    fn create_deployment(gateway: &Gateway) -> Deployment {
        let mut labels = BTreeMap::new();
        let ports = gateway
            .listeners
            .iter()
            .map(|l| ContainerPort {
                name: None, //Some(name(&gateway.name, l.port(), &l.protocol().to_string())),
                container_port: l.port(),
                protocol: Some("TCP".to_owned()),
                ..Default::default()
            })
            .dedup_by(|x, y| x.container_port == y.container_port && x.protocol == y.protocol)
            .collect();
        labels.insert("app".to_owned(), gateway.name.clone());

        let mut pod_spec: PodSpec = serde_json::from_str(ENVOY_POD_SPEC).unwrap_or(PodSpec { ..Default::default() });
        if let Some(container) = pod_spec.containers.first_mut() {
            container.ports = Some(ports);
        }

        let (xds_cm, boostrap_cm) = config_map_names(gateway);
        pod_spec.volumes = Some(vec![
            Volume {
                name: "envoy-config".to_owned(),
                config_map: Some(ConfigMapVolumeSource {
                    name: Some(boostrap_cm),
                    items: Some(vec![KeyToPath {
                        key: "envoy-bootstrap.yaml".to_owned(),
                        path: "envoy-bootstrap.yaml".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Volume {
                name: "envoy-xds".to_owned(),
                config_map: Some(ConfigMapVolumeSource {
                    name: Some(xds_cm),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]);

        Deployment {
            metadata: ObjectMeta {
                name: Some(gateway.name.clone()),
                namespace: Some(gateway.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(2),
                selector: LabelSelector {
                    match_expressions: None,
                    match_labels: Some(labels.clone()),
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels.clone()),
                        ..Default::default()
                    }),
                    spec: Some(pod_spec),
                },
                ..Default::default()
            }),

            ..Default::default()
        }
    }

    fn create_service(gateway: &Gateway) -> Service {
        let ports = gateway
            .listeners
            .iter()
            .map(|l| ServicePort {
                app_protocol: None,
                name: Some(name(&gateway.name, l.port(), &l.protocol().to_string())),
                node_port: None,
                port: l.port(),
                protocol: Some("TCP".to_owned()),
                target_port: Some(IntOrString::Int(l.port())),
            })
            .dedup_by(|x, y| x.port == y.port && x.protocol == y.protocol)
            .collect();
        let mut selector = BTreeMap::new();
        selector.insert("app".to_owned(), gateway.name.clone());
        Service {
            metadata: ObjectMeta {
                name: Some(gateway.name.clone()),
                namespace: Some(gateway.namespace.clone()),

                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(selector),
                ports: Some(ports),
                type_: Some("LoadBalancer".to_owned()),
                ..Default::default()
            }),
            status: None,
        }
    }
}

fn config_map_names(gateway: &Gateway) -> (String, String) {
    (format!("{}-xds-cm", gateway.name), format!("{}-bootstrap-cm", gateway.name))
}

fn name(gw: &str, port: i32, proto: &str) -> String {
    format! {"{gw}-{port}-{}",proto.to_lowercase()}
}

#[derive(Debug)]
struct RdsData {
    pub route_file_name: String,
    pub rds_content: String,
}
impl RdsData {
    fn new(route_file_name: String, rds_content: String) -> Self {
        Self { route_file_name, rds_content }
    }
}

#[allow(clippy::struct_field_names)]
#[derive(Debug)]
struct XdsData {
    pub lds_content: String,
    pub rds_content: Vec<RdsData>,
    pub cds_content: String,
}

impl XdsData {
    fn new(lds_content: String, rds_content: Vec<RdsData>, cds_content: String) -> Self {
        Self {
            lds_content,
            rds_content,
            cds_content,
        }
    }
}

const ENVOY_POD_SPEC: &str = r#"
{
    "volumes": [
        {
            "name": "envoy-config",
            "configMap": {
                "name": "envoy-cm",
                "items": [
                    {
                        "key": "envoy-bootstrap.yaml",
                        "path": "envoy-bootstrap.yaml"
                    }
                ]
            }
        },
        {
            "name": "envoy-xds",
            "configMap": {
                "name": "envoy-xds",
                "items": [
                    {
                        "key": "lds.yaml",
                        "path": "lds.yaml"
                    },
                    {
                        "key": "cds.yaml",
                        "path": "cds.yaml"
                    },
                    {
                        "key": "rds.yaml",
                        "path": "rds.yaml"
                    }
                ]
            }
        }
    ],
    "containers": [
        {
            "args": [
                "-c",
                "/envoy-config/envoy-bootstrap.yaml",
                "--service-cluster $(GATEWAY_NAMESPACE)",
                "--service-node $(ENVOY_POD_NAME)",
                "--log-level info"
            ],
            "command": [
                "envoy"
            ],
            "image": "docker.io/envoyproxy/envoy:v1.31.0",
            "imagePullPolicy": "IfNotPresent",
            "name": "envoy",
            "env": [
                {
                    "name": "GATEWAY_NAMESPACE",
                    "valueFrom": {
                        "fieldRef": {
                            "apiVersion": "v1",
                            "fieldPath": "metadata.namespace"
                        }
                    }
                },
                {
                    "name": "ENVOY_POD_NAME",
                    "valueFrom": {
                        "fieldRef": {
                            "apiVersion": "v1",
                            "fieldPath": "metadata.name"
                        }
                    }
                }
            ],
            "ports": [
                {
                    "containerPort": 8080,
                    "protocol": "TCP"
                },
                {
                    "containerPort": 8443,
                    "protocol": "TCP"
                },
                {
                    "containerPort": 8081,
                    "protocol": "TCP"
                },
                {
                    "containerPort": 8082,
                    "protocol": "TCP"
                }
            ],
            "readinessProbe": {
                "httpGet": {
                    "path": "/ready",
                    "port": 9901
                },
                "initialDelaySeconds": 3,
                "periodSeconds": 4
            },
            "volumeMounts": [
                {
                    "name": "envoy-config",
                    "mountPath": "/envoy-config",
                    "readOnly": true
                },
                {
                    "name": "envoy-xds",
                    "mountPath": "/envoy-xds",
                    "readOnly": true
                }
            ],
            "lifecycle": {
                "preStop": {
                    "httpGet": {
                        "path": "/shutdown",
                        "port": 8090,
                        "scheme": "HTTP"
                    }
                }
            }
        }
    ]
}
"#;

const ENVOY_BOOTSTRAP: &str = "
admin:
  address:
    socket_address:
      address: 0.0.0.0
      port_value: 9901
dynamic_resources:
  lds_config:
    path_config_source:
      path: ./envoy-xds/lds.yaml
      watched_directory:
        path: ./envoy-xds
  cds_config:
    path_config_source:
      path: ./envoy-xds/cds.yaml
      watched_directory:
        path: ./envoy-xds
static_resources:
  listeners: []
  clusters: []
";

#[cfg(test)]
mod tests {
    use gateway_api::apis::standard::gateways::GatewayListeners;
    use serde_yaml;

    use super::EnvoyDeployerChannelHandler;
    use crate::{
        backends::envoy_deployer::{RdsData, XdsData},
        common::{BackendServiceConfig, Gateway, Route, RouteConfig, RouteToListenersMapping, RoutingRule},
    };

    #[test]
    fn test_template() {
        let mut rc = RouteConfig::new("name".to_owned(), "namespace".to_owned(), Some(vec![]));
        rc.routing_rules = vec![
            RoutingRule {
                name: "routing-rule-1".to_owned(),
                matching_rules: vec![], 
                backends: vec![
                    BackendServiceConfig {
                        endpoint: "backend1".to_owned(),
                        port: 1991,
                        weight: 1,
                    },
                    BackendServiceConfig {
                        endpoint: "backend2".to_owned(),
                        port: 1992,
                        weight: 2,
                    },
                ],
            },
            RoutingRule {
                name: "routing-rule-2".to_owned(),
                matching_rules: vec![], 
                backends: vec![
                    BackendServiceConfig {
                        endpoint: "backend3".to_owned(),
                        port: 1993,
                        weight: 3,
                    },
                    BackendServiceConfig {
                        endpoint: "backend4".to_owned(),
                        port: 1994,
                        weight: 4,
                    },
                ],
            },
        ];

        let route = Route::Http(rc);
        let listeners = vec![GatewayListeners {
            allowed_routes: None,
            hostname: None,
            name: "name".to_owned(),
            port: 10,
            protocol: "HTTP".to_owned(),
            tls: None,
        }];
        let XdsData {
            lds_content,
            rds_content,
            cds_content,
        } = EnvoyDeployerChannelHandler::create_envoy_xds(
            &Gateway {
                id: uuid::Uuid::new_v4(),
                name: "name".to_owned(),
                namespace: "namespace".to_owned(),
                listeners: vec![],
            },
            &[RouteToListenersMapping::new(route, listeners)],
        )
        .unwrap();
        println!("{cds_content}");

        let _: serde_yaml::value::Value = serde_yaml::from_str(&lds_content).unwrap();
        let _: serde_yaml::value::Value = serde_yaml::from_str(&cds_content).unwrap();
        for RdsData { route_file_name: _, rds_content } in rds_content {
            let _: serde_yaml::value::Value = serde_yaml::from_str(&rds_content).unwrap();
        }
    }
}
