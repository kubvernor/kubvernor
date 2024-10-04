use std::{collections::BTreeMap, future::Future};

use futures::FutureExt;
use itertools::Itertools;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{ConfigMap, ConfigMapVolumeSource, ContainerPort, KeyToPath, PodSpec, PodTemplateSpec, Service, ServicePort, ServiceSpec, Volume},
    },
    apimachinery::pkg::{
        apis::meta::v1::{DeleteOptions, LabelSelector},
        util::intstr::IntOrString,
    },
};
use kube::{
    api::{DeleteParams, Patch, PatchParams, PostParams},
    Api, Client, Resource,
};
use kube_core::ObjectMeta;
use tokio::sync::mpsc::{self, Receiver};
use tracing::{debug, info, warn};

use crate::backends::{GatewayProcessedPayload, GatewayResponse, GatewayStatus};

use super::{common::GatewayEvent, Gateway, Route};

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
                        info!("Backend got gateway {event:#}");
                         match event{
                            GatewayEvent::GatewayChanged((response_sender, gateway, routes)) => {
                                self.deploy_envoy(&gateway, &routes).await;
                                let _res = response_sender.send(GatewayResponse::GatewayProcessed(GatewayProcessedPayload{gateway_status:GatewayStatus{ id: uuid::Uuid::new_v4(), name: gateway.name, namespace: gateway.namespace, listeners: vec![] }, attached_routes: vec![], ignored_routes: vec![]}));
                            }

                            GatewayEvent::GatewayDeleted((response_sender, gateway, routes)) => {
                                self.delete_envoy(&gateway, &routes).await;
                                let _res = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                            }

                            GatewayEvent::RouteChanged((response_sender, _route, _linked_routes, _gateway)) | GatewayEvent::RouteDeleted((response_sender, _route, _linked_routes, _gateway))=> {
                                let _res = response_sender.send(GatewayResponse::RouteDeleted);

                            }
                        }
                    }
                    else => {
                        break;
                    }
            }
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

    async fn deploy_envoy(&self, gateway: &Gateway, _routes: &[Route]) {
        debug!("Deploying Envoy {gateway:?}");
        let service_api: Api<Service> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let deployment_api: Api<Deployment> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let config_map_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &gateway.namespace);
        let (xds_cm, bootstrap_cm) = config_map_names(gateway);
        let envoy_boostrap_config_map = Self::create_envoy_bootstrap_config_map(&bootstrap_cm, gateway);
        let envoy_xds_config_map = Self::create_envoy_xds_config_map(&xds_cm, gateway);
        let service = Self::create_service(gateway);
        let deployment = Self::create_deployment(gateway);

        let pp = PatchParams {
            field_manager: Some(self.controller_name.clone()),
            ..Default::default()
        };

        let res = config_map_api.patch(&bootstrap_cm, &pp, &Patch::Apply(&envoy_boostrap_config_map)).await;
        if res.is_ok() {
            debug!("Created config map for  {}-{}", gateway.name, gateway.namespace);
        } else {
            warn!("Could not create config map for {}-{} {:?}", gateway.name, gateway.namespace, res.err());
        }

        let res = config_map_api.patch(&xds_cm, &pp, &Patch::Apply(&envoy_xds_config_map)).await;
        if res.is_ok() {
            debug!("Created config map for {}-{}", gateway.name, gateway.namespace);
        } else {
            warn!("Could not create config map for {}-{} {:?}", gateway.name, gateway.namespace, res.err());
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

    fn create_envoy_xds_config_map(name: &str, gateway: &Gateway) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("cds.yaml".to_owned(), String::new());
        map.insert("lds.yaml".to_owned(), String::new());
        map.insert("rds.yaml".to_owned(), String::new());
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
        labels.insert("app".to_owned(), "envoy".to_owned());

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
                    items: Some(vec![
                        KeyToPath {
                            key: "lds.yaml".to_owned(),
                            path: "lds.yaml".to_owned(),
                            ..Default::default()
                        },
                        KeyToPath {
                            key: "cds.yaml".to_owned(),
                            path: "cds.yaml".to_owned(),
                            ..Default::default()
                        },
                        KeyToPath {
                            key: "rds.yaml".to_owned(),
                            path: "rds.yaml".to_owned(),
                            ..Default::default()
                        },
                    ]),
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
        Service {
            metadata: ObjectMeta {
                name: Some(gateway.name.clone()),
                namespace: Some(gateway.namespace.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(ports),
                type_: Some("LoadBalancer".to_owned()),
                ..Default::default()
            }),
            status: None,
        }
    }
}

fn config_map_names(gateway: &Gateway) -> (String, String) {
    (format!("{}-bootstrap-cm", gateway.name), format!("{}-xds-cm", gateway.name))
}

fn name(gw: &str, port: i32, proto: &str) -> String {
    format! {"{gw}-{port}-{}",proto.to_lowercase()}
}

const ENVOY_POD_SPEC: &str = r#"
{
    "affinity": {
        "podAntiAffinity": {
            "preferredDuringSchedulingIgnoredDuringExecution": [
                {
                    "podAffinityTerm": {
                        "labelSelector": {
                            "matchLabels": {
                                "app": "envoy"
                            }
                        },
                        "topologyKey": "kubernetes.io/hostname"
                    },
                    "weight": 100
                }
            ]
        }
    },
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
