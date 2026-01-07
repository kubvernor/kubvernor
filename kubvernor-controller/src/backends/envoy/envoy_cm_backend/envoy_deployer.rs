use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Values},
    sync::LazyLock,
};

use futures::FutureExt;
use itertools::Itertools;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            ConfigMap, ConfigMapVolumeSource, ContainerPort, KeyToPath, PodSpec, PodTemplateSpec, ProjectedVolumeSource, SecretProjection,
            Service, ServiceAccount, ServicePort, ServiceSpec, Volume, VolumeProjection,
        },
    },
    apimachinery::pkg::{apis::meta::v1::LabelSelector, util::intstr::IntOrString},
};
use kube::{
    Api, Client,
    api::{DeleteParams, Patch, PatchParams},
};
use kube_core::ObjectMeta;
use kubvernor_common::ResourceKey;
use tera::Tera;
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time,
};
use tracing::{debug, info, warn};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::xds_generator::{self, RdsData};
use crate::common::{BackendGatewayEvent, BackendGatewayResponse, Certificate, ChangedContext, Gateway, GatewayAddress, Listener, TlsType};

pub static TEMPLATES: LazyLock<Tera> = LazyLock::new(|| match Tera::new("templates/**.tera") {
    Ok(t) => t,
    Err(e) => {
        warn!("Parsing error(s): {}", e);
        ::std::process::exit(1);
    },
});

#[derive(TypedBuilder)]
pub struct EnvoyDeployerChannelHandlerService {
    controller_name: String,
    client: Client,
    backend_deploy_request_channel_receiver: Receiver<BackendGatewayEvent>,
    backend_response_channel_sender: Sender<BackendGatewayResponse>,
}

impl EnvoyDeployerChannelHandlerService {
    pub async fn start(&mut self) -> crate::Result<()> {
        info!("Gateways handler started");
        loop {
            tokio::select! {
                    Some(event) = self.backend_deploy_request_channel_receiver.recv() => {
                        info!("Backend got event {event:#}");
                        match event{
                            BackendGatewayEvent::Changed(ctx) => {
                                let gateway = &ctx.gateway;
                                info!("EnvoyDeployerService GatewayChanged {}",gateway.key());
                                let maybe_service = self.deploy_envoy(gateway).await;
                                if let Ok(service) = maybe_service{
                                    self.update_address_with_polling(&service, *ctx).await;
                                }else{
                                    warn!("Problem {maybe_service:?}");
                                    let _res = self.backend_response_channel_sender
                                        .send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                }
                            }

                            BackendGatewayEvent::Deleted(boxed) => {
                                let response_sender = boxed.response_sender;
                                let gateway  = boxed.gateway;
                                info!("EnvoyDeployerService GatewayDeleted {}",gateway.key());
                                self.delete_envoy(&gateway).await;
                                let _res = self.backend_response_channel_sender.send(BackendGatewayResponse::Deleted(vec![]));
                                let _res = response_sender.send(BackendGatewayResponse::Deleted(vec![]));
                            }

                        }
                    }
                    else => {
                        break;
                    }
            }
        }
        crate::Result::<()>::Ok(())
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

        let pp = PatchParams { field_manager: Some(self.controller_name.clone()), ..Default::default() };

        debug!("Created bootstrap config map for {}-{}", gateway.name(), gateway.namespace());

        let maybe_templates = xds_generator::EnvoyXDSGenerator::new(gateway).generate_xds();

        if let Ok(xds_generator::XdsData { bootstrap_content, lds_content, rds_content, cds_content }) = maybe_templates {
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

    fn print_delete_error<S>(f: Result<itertools::Either<S, kube_core::Status>, kube::Error>, gateway: &Gateway) {
        match f {
            Ok(_) => (),
            Err(kube::Error::Api(e)) if e.code != 404 => {
                debug!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), e);
            },
            Err(e) => {
                warn!("Could not delete {}-{} {:?}", gateway.name(), gateway.namespace(), e);
            },
        }
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
            service_account_api.delete(gateway.name(), &dp).map(|e| Self::print_delete_error(e, gateway)).boxed(),
            service_api.delete(gateway.name(), &dp).map(|e| Self::print_delete_error(e, gateway)).boxed(),
            deployment_api.delete(gateway.name(), &dp).map(|e| Self::print_delete_error(e, gateway)).boxed(),
            config_map_api.delete(&xds_cm, &dp).map(|e| Self::print_delete_error(e, gateway)).boxed(),
            config_map_api.delete(&bootstrap_cm, &dp).map(|e| Self::print_delete_error(e, gateway)).boxed(),
        ];
        let _res = futures::future::join_all(futures).await;
    }

    fn find_gateway_addresses(service: &Service) -> Option<Vec<String>> {
        let mut ips = vec![];
        if let Some(status) = &service.status
            && let Some(load_balancer) = &status.load_balancer
            && let Some(ingress) = &load_balancer.ingress
        {
            for i in ingress {
                ips.push(i.ip.clone());
            }
        }
        let ips = ips.into_iter().flatten().collect::<Vec<_>>();
        if ips.is_empty() { None } else { Some(ips) }
    }

    fn create_envoy_xds_config_map(name: &str, gateway: &Gateway, lds: String, routes: Vec<RdsData>, cds: String) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("cds.yaml".to_owned(), cds);
        map.insert("lds.yaml".to_owned(), lds);

        for RdsData { route_name, rds_content } in routes {
            map.insert(format!("{route_name}.yaml"), rds_content);
        }

        let mut annotations = BTreeMap::new();
        annotations.insert("gateway-version".to_owned(), Uuid::new_v4().to_string());

        ConfigMap {
            binary_data: None,
            data: Some(map),
            immutable: None,
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                annotations: Some(annotations),
                ..Default::default()
            },
        }
    }

    fn create_envoy_bootstrap_config_map(name: &str, gateway: &Gateway, boostrap_content: String) -> ConfigMap {
        let mut map = BTreeMap::new();
        map.insert("envoy-bootstrap.yaml".to_owned(), boostrap_content);
        let mut annotations = BTreeMap::new();
        annotations.insert("gateway-version".to_owned(), Uuid::new_v4().to_string());
        ConfigMap {
            binary_data: None,
            data: Some(map),
            immutable: None,
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                annotations: Some(annotations),
                ..Default::default()
            },
        }
    }

    fn create_deployment(gateway: &Gateway) -> Deployment {
        let mut labels = Self::create_labels(gateway);
        let ports = gateway
            .listeners()
            .map(|l| ContainerPort { name: None, container_port: l.port(), protocol: Some("TCP".to_owned()), ..Default::default() })
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

        let mut annotations = BTreeMap::new();
        annotations.insert("gateway-version".to_owned(), Uuid::new_v4().to_string());
        Deployment {
            metadata: ObjectMeta {
                annotations: Some(annotations.clone()),
                name: Some(gateway.name().to_owned()),
                namespace: Some(gateway.namespace().to_owned()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector { match_expressions: None, match_labels: Some(labels.clone()) },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta { labels: Some(labels.clone()), ..Default::default() }),
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

    fn update_addresses(gateway: &mut Gateway, service: &Service) -> bool {
        if let Some(attached_addresses) = Self::find_gateway_addresses(service) {
            gateway.addresses_mut().append(
                &mut attached_addresses
                    .into_iter()
                    .filter_map(|a| if let Ok(addr) = a.parse() { Some(GatewayAddress::IPAddress(addr)) } else { None })
                    .collect(),
            );
            true
        } else {
            false
        }
    }

    async fn update_address_with_polling(&self, service: &Service, ctx: ChangedContext) {
        if let Some(attached_addresses) = Self::find_gateway_addresses(service) {
            debug!("Got address address {attached_addresses:?}");
            let ChangedContext { mut gateway, kube_gateway, gateway_class_name } = ctx;
            gateway.addresses_mut().append(
                &mut attached_addresses
                    .into_iter()
                    .filter_map(|a| if let Ok(addr) = a.parse() { Some(GatewayAddress::IPAddress(addr)) } else { None })
                    .collect(),
            );
            let _res = self
                .backend_response_channel_sender
                .send(BackendGatewayResponse::ProcessedWithContext {
                    gateway: Box::new(gateway.clone()),
                    kube_gateway: Box::new(kube_gateway),
                    gateway_class_name,
                })
                .await;
        } else {
            let client = self.client.clone();
            let resource_key = ResourceKey::from(service);

            let backend_response_channel_sender = self.backend_response_channel_sender.clone();

            let _handle = tokio::spawn(async move {
                let api: Api<Service> = Api::namespaced(client, &resource_key.namespace);
                let ChangedContext { mut gateway, kube_gateway, gateway_class_name } = ctx;
                let mut interval = time::interval(time::Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    debug!("Resolving address for Service {}", resource_key);
                    let maybe_service = api.get(&resource_key.name).await;
                    if let Ok(service) = maybe_service {
                        if Self::update_addresses(&mut gateway, &service) {
                            let _res = backend_response_channel_sender
                                .send(BackendGatewayResponse::ProcessedWithContext {
                                    gateway: Box::new(gateway.clone()),
                                    kube_gateway: Box::new(kube_gateway.clone()),
                                    gateway_class_name: gateway_class_name.clone(),
                                })
                                .await;
                            break;
                        }
                    } else {
                        warn!("Problem {maybe_service:?}");
                        let _res = backend_response_channel_sender.send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                    }
                }
                debug!("Task completed for gateway {} service {}", gateway.key(), resource_key);
            });
        }
    }

    fn create_secret_volumes(listeners: Values<String, Listener>) -> Vec<Volume> {
        let mut all_certificates = BTreeSet::new();
        for listener in listeners {
            if let Listener::Https(listener_data) = listener
                && let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type
            {
                for certificate in certificates {
                    all_certificates.insert(certificate.clone());
                }
            }
        }

        let secrets = all_certificates
            .into_iter()
            .filter_map(|c| match c {
                Certificate::ResolvedSameSpace(resource_key) => Some(resource_key),
                Certificate::NotResolved(_) | Certificate::Invalid(_) | Certificate::ResolvedCrossSpace(_) => None,
            })
            .map(|resource_key| VolumeProjection {
                secret: Some(SecretProjection {
                    name: resource_key.name.clone(),
                    items: Some(vec![
                        KeyToPath { key: "tls.crt".to_owned(), path: create_certificate_name(&resource_key), ..Default::default() },
                        KeyToPath { key: "tls.key".to_owned(), path: create_key_name(&resource_key), ..Default::default() },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();

        vec![Volume {
            name: "envoy-secrets".to_owned(),
            projected: Some(ProjectedVolumeSource { sources: Some(secrets), ..Default::default() }),
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
                "--log-level debug"
            ],
            "command": [
                "envoy"
            ],
            "image": "docker.io/envoyproxy/envoy:v1.36.4",
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
                "periodSeconds": 20
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
