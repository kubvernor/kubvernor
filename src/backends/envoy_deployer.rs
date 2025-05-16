use std::collections::{btree_map::Values, BTreeMap, BTreeSet};

use futures::FutureExt;
use itertools::Itertools;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            ConfigMap, ConfigMapVolumeSource, ContainerPort, KeyToPath, PodSpec, PodTemplateSpec, ProjectedVolumeSource, SecretProjection, Service, ServiceAccount, ServicePort, ServiceSpec, Volume,
            VolumeProjection,
        },
    },
    apimachinery::pkg::{apis::meta::v1::LabelSelector, util::intstr::IntOrString},
};
use kube::{
    api::{DeleteParams, Patch, PatchParams},
    Api, Client,
};
use kube_core::ObjectMeta;
use std::sync::LazyLock;
//use lazy_static::lazy_static;


use tera::Tera;
use tokio::sync::mpsc::{self, Receiver};
use tracing::{debug, info, warn};

use super::xds_generator::{self, RdsData};
use crate::{
    backends::xds_generator::XdsData,
    common::{ChangedContext, DeletedContext, Gateway, GatewayAddress, GatewayEvent, GatewayResponse, Listener, ResourceKey, TlsType},
};


pub static TEMPLATES: LazyLock<Tera> = LazyLock::new(||{
    match Tera::new("templates/**.tera") {
        Ok(t) => t,
        Err(e) => {
            warn!("Parsing error(s): {}", e);
            ::std::process::exit(1);
        }
    }
}
);


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
                            GatewayEvent::GatewayChanged(ChangedContext{ response_sender, mut gateway }) => {
                                let maybe_service = self.deploy_envoy(&gateway).await;
                                if let Ok(service) = maybe_service{
                                    Self::update_addresses(&mut gateway, &service);
                                    let _res = response_sender.send(GatewayResponse::GatewayProcessed(gateway));
                                }else{
                                    warn!("Problem {maybe_service:?}");
                                    let _res = response_sender.send(GatewayResponse::GatewayProcessingError);
                                }
                            }

                            GatewayEvent::GatewayDeleted(DeletedContext{ response_sender, gateway }) => {
                                self.delete_envoy(&gateway).await;
                                let _res = response_sender.send(GatewayResponse::GatewayDeleted(vec![]));
                            }

                            GatewayEvent::RouteChanged(ChangedContext{ response_sender, mut gateway }) => {
                                let maybe_service = self.deploy_envoy(&gateway).await;
                                if let Ok(service) = maybe_service{
                                    Self::update_addresses(&mut gateway, &service);
                                    let _res = response_sender.send(GatewayResponse::GatewayProcessed(gateway));
                                }else{
                                    warn!("Problem {maybe_service:?}");
                                    let _res = response_sender.send(GatewayResponse::GatewayProcessingError);
                                }
                            }

                        }
                    }
                    else => {
                        break;
                    }
            }
        }
    }

    async fn deploy_envoy(&self, gateway: &Gateway) -> std::result::Result<Service, kube::Error> {
        debug!("Deploying Envoy {gateway:?}");

        let service_api: Api<Service> = Api::namespaced(self.client.clone(), gateway.namespace());
        let service_account_api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), gateway.namespace());
        let deployment_api: Api<Deployment> = Api::namespaced(self.client.clone(), gateway.namespace());
        let config_map_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), gateway.namespace());
        let (xds_cm, bootstrap_cm) = config_map_names(gateway);

        let deployment = Self::create_deployment(gateway);
        let service = Self::create_service(gateway);
        let service_account = Self::create_service_account(gateway);

        let pp = PatchParams {
            field_manager: Some(self.controller_name.clone()),
            ..Default::default()
        };

        debug!("Created bootstrap config map for {}-{}", gateway.name(), gateway.namespace());

        let maybe_templates = xds_generator::EnvoyXDSGenerator::new(gateway).generate_xds();

        if let Ok(XdsData {
            bootstrap_content,
            lds_content,
            rds_content,
            cds_content,
        }) = maybe_templates
        {
            let envoy_xds_config_map = Self::create_envoy_xds_config_map(&xds_cm, gateway, lds_content, rds_content, cds_content);
            let _envoy_xds_config_map = config_map_api.patch(&xds_cm, &pp, &Patch::Apply(&envoy_xds_config_map)).await?;
            let envoy_boostrap_config_map = Self::create_envoy_bootstrap_config_map(&bootstrap_cm, gateway, bootstrap_content);
            let _res = config_map_api.patch(&bootstrap_cm, &pp, &Patch::Apply(&envoy_boostrap_config_map)).await?;
            debug!("Created xds config map for {}-{}", gateway.name(), gateway.namespace());
        } else {
            warn!("Problem when processing templates {maybe_templates:?}");
        }

        let _deployment = deployment_api.patch(gateway.name(), &pp, &Patch::Apply(&deployment)).await?;
        debug!("Created deployment {}-{}", gateway.name(), gateway.namespace());

        let service = service_api.patch(gateway.name(), &pp, &Patch::Apply(&service)).await?;

        debug!("Service status {:?}", service.status);
        debug!("Created service {}-{}", gateway.name(), gateway.namespace());

        let service_account = service_account_api.patch(gateway.name(), &pp, &Patch::Apply(&service_account)).await?;
        debug!("Service account status {:?}", service_account);

        Ok(service)
    }

    async fn delete_envoy(&self, gateway: &Gateway) {
        debug!("Deleting Envoy {gateway:?}");
        let service_api: Api<Service> = Api::namespaced(self.client.clone(), gateway.namespace());
        let deployment_api: Api<Deployment> = Api::namespaced(self.client.clone(), gateway.namespace());
        let config_map_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), gateway.namespace());
        let service_account_api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), gateway.namespace());
        let dp = DeleteParams { ..Default::default() };
        let (xds_cm, bootstrap_cm) = config_map_names(gateway);
        let futures = [
            service_account_api
                .delete(gateway.name(), &dp)
                .map(|f| {
                    if f.is_err() {
                        warn!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), f.err());
                    }
                })
                .boxed(),
            service_api
                .delete(gateway.name(), &dp)
                .map(|f| {
                    if f.is_err() {
                        warn!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), f.err());
                    }
                })
                .boxed(),
            deployment_api
                .delete(gateway.name(), &dp)
                .map(|f| {
                    if f.is_err() {
                        warn!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), f.err());
                    }
                })
                .boxed(),
            config_map_api
                .delete(&xds_cm, &dp)
                .map(|f| {
                    if f.is_err() {
                        warn!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), f.err());
                    }
                })
                .boxed(),
            config_map_api
                .delete(&bootstrap_cm, &dp)
                .map(|f| {
                    if f.is_err() {
                        warn!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), f.err());
                    }
                })
                .boxed(),
        ];
        let _res = futures::future::join_all(futures).await;
    }

    fn find_gateway_addresses(service: &Service) -> Vec<String> {
        let mut ips = vec![];
        if let Some(status) = &service.status {
            if let Some(load_balancer) = &status.load_balancer {
                if let Some(ingress) = &load_balancer.ingress {
                    for i in ingress {
                        ips.push(i.ip.clone());
                    }
                }
            }
        }
        ips.into_iter().flatten().collect::<Vec<_>>()
    }

    fn create_envoy_xds_config_map(name: &str, gateway: &Gateway, lds: String, routes: Vec<RdsData>, cds: String) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("cds.yaml".to_owned(), cds);
        map.insert("lds.yaml".to_owned(), lds);

        for RdsData { route_name, rds_content } in routes {
            map.insert(format!("{route_name}.yaml"), rds_content);
        }

        ConfigMap {
            binary_data: None,
            data: Some(map),
            immutable: None,
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                ..Default::default()
            },
        }
    }

    fn create_envoy_bootstrap_config_map(name: &str, gateway: &Gateway, boostrap_content: String) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("envoy-bootstrap.yaml".to_owned(), boostrap_content);
        ConfigMap {
            binary_data: None,
            data: Some(map),
            immutable: None,
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                ..Default::default()
            },
        }
    }

    fn create_deployment(gateway: &Gateway) -> Deployment {
        let mut labels = Self::create_labels(gateway);
        let ports = gateway
            .listeners()
            .map(|l| ContainerPort {
                name: None, //Some(name(gateway.name(), l.port(), &l.protocol().to_string())),
                container_port: l.port(),
                protocol: Some("TCP".to_owned()),
                ..Default::default()
            })
            .dedup_by(|x, y| x.container_port == y.container_port && x.protocol == y.protocol)
            .collect();
        labels.insert("app".to_owned(), gateway.name().to_owned());

        let mut pod_spec: PodSpec = serde_json::from_str(ENVOY_POD_SPEC).unwrap_or(PodSpec { ..Default::default() });
        if let Some(container) = pod_spec.containers.first_mut() {
            container.ports = Some(ports);
        }
        let (xds_cm, boostrap_cm) = config_map_names(gateway);
        let mut default_volumes = vec![
            Volume {
                name: "envoy-config".to_owned(),
                config_map: Some(ConfigMapVolumeSource {
                    name: boostrap_cm,
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
                config_map: Some(ConfigMapVolumeSource { name: xds_cm, ..Default::default() }),
                ..Default::default()
            },
        ];
        let mut secret_volumes = Self::create_secret_volumes(gateway.listeners());
        default_volumes.append(&mut secret_volumes);
        pod_spec.volumes = Some(default_volumes);

        Deployment {
            metadata: ObjectMeta {
                //annotations: Some(annotations.clone()),
                name: Some(gateway.name().to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
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
                        //annotations: Some(annotations),
                        ..Default::default()
                    }),
                    spec: Some(pod_spec),
                },
                ..Default::default()
            }),

            ..Default::default()
        }
    }

    fn create_labels(_gateway: &Gateway) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn create_service_account(gateway: &Gateway) -> ServiceAccount {
        ServiceAccount {
            metadata: ObjectMeta {
                name: Some(gateway.name().to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                labels: Some(Self::create_labels(gateway)),

                ..Default::default()
            },

            automount_service_account_token: None,
            image_pull_secrets: None,
            secrets: None,
        }
    }

    fn create_service(gateway: &Gateway) -> Service {
        let ports = gateway
            .listeners()
            .map(|l| ServicePort {
                app_protocol: None,
                name: Some(name(gateway.name(), l.port(), &l.protocol().to_string())),
                node_port: None,
                port: l.port(),
                protocol: Some("TCP".to_owned()),
                target_port: Some(IntOrString::Int(l.port())),
            })
            .dedup_by(|x, y| x.port == y.port && x.protocol == y.protocol)
            .collect();
        let mut selector = BTreeMap::new();
        selector.insert("app".to_owned(), gateway.name().to_owned());
        Service {
            metadata: ObjectMeta {
                name: Some(gateway.name().to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                labels: Some(Self::create_labels(gateway)),
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

    fn update_addresses(gateway: &mut Gateway, service: &Service) {
        let attached_addresses = Self::find_gateway_addresses(service);
        gateway.addresses_mut().append(
            &mut attached_addresses
                .into_iter()
                .filter_map(|a| if let Ok(addr) = a.parse() { Some(GatewayAddress::IPAddress(addr)) } else { None })
                .collect(),
        );
    }

    fn create_secret_volumes(listeners: Values<String, Listener>) -> Vec<Volume> {
        let mut all_certificates = BTreeSet::new();
        for listener in listeners {
            if let Listener::Https(listener_data) = listener {
                if let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type {
                    for certificate in certificates {
                        all_certificates.insert(certificate.clone());
                    }
                }
            }
        }

        let secrets = all_certificates
            .into_iter()
            .map(|resource_key| VolumeProjection {
                secret: Some(SecretProjection {
                    name: resource_key.name.clone(),
                    items: Some(vec![
                        KeyToPath {
                            key: "tls.crt".to_owned(),
                            path: create_certificate_name(&resource_key),
                            ..Default::default()
                        },
                        KeyToPath {
                            key: "tls.key".to_owned(),
                            path: create_key_name(&resource_key),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();

        vec![Volume {
            name: "envoy-secrets".to_owned(),
            projected: Some(ProjectedVolumeSource {
                sources: Some(secrets),
                ..Default::default()
            }),
            ..Default::default()
        }]
    }
}

pub fn create_secret_name(resource_key: &ResourceKey) -> String {
    resource_key.name.clone() + "_" + &resource_key.namespace
}

pub fn create_certificate_name(resource_key: &ResourceKey) -> String {
    resource_key.name.clone() + "_" + &resource_key.namespace + "_cert.pem"
}

pub fn create_key_name(resource_key: &ResourceKey) -> String {
    resource_key.name.clone() + "_" + &resource_key.namespace + "_key.pem"
}

fn config_map_names(gateway: &Gateway) -> (String, String) {
    (format!("{}-xds-cm", gateway.name()), format!("{}-bootstrap-cm", gateway.name()))
}

fn name(gw: &str, port: i32, proto: &str) -> String {
    format! {"{gw}-{port}-{}",proto.to_lowercase()}
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
                },
                {
                    "name": "envoy-secrets",
                    "mountPath": "/envoy-secrets",
                    "readOnly": true
                }

            ]
        }
    ]
}
"#;
