use std::{collections::BTreeMap, sync::LazyLock};

use agentgateway_api_rs::agentgateway::dev::resource::{Bind, Resource, resource::Kind};
use futures::FutureExt;
use itertools::Itertools;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            ConfigMap, ConfigMapVolumeSource, ContainerPort, KeyToPath, PodSpec, PodTemplateSpec, Service, ServiceAccount, ServicePort,
            ServiceSpec, Volume,
        },
    },
    apimachinery::pkg::{apis::meta::v1::LabelSelector, util::intstr::IntOrString},
};
use kube::{
    Api, Client,
    api::{Patch, PatchParams},
};
use kube_core::ObjectMeta;
use tera::Tera;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time,
};
use tracing::{debug, error, info, warn};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::{
    Error,
    backends::agentgateway::{
        resource_generator::ResourceGenerator,
        server::{ServerAction, start_aggregate_server},
    },
    common::{BackendGatewayEvent, BackendGatewayResponse, ChangedContext, ControlPlaneConfig, Gateway, GatewayAddress, ResourceKey},
};

pub static TEMPLATES: LazyLock<Tera> = LazyLock::new(|| match Tera::new("templates/**.tera") {
    Ok(t) => t,
    Err(e) => {
        warn!("Parsing error(s): {}", e);
        ::std::process::exit(1);
    },
});

#[derive(TypedBuilder)]
pub struct AgentgatewayDeployerChannelHandlerService {
    control_plane_config: ControlPlaneConfig,
    client: Client,
    backend_deploy_request_channel_receiver: Receiver<BackendGatewayEvent>,
    backend_response_channel_sender: Sender<BackendGatewayResponse>,
}

impl AgentgatewayDeployerChannelHandlerService {
    pub async fn start(mut self) -> crate::Result<()> {
        let control_plane_config = self.control_plane_config.clone();
        let (stream_resource_sender, stream_resource_receiver) = mpsc::channel::<ServerAction>(1024);
        let client = self.client.clone();
        let kube_client = self.client.clone();

        let server_socket = control_plane_config.addr();
        let agentgateway_deployer_service = async move {
            info!("Agentgateway handler started");
            loop {
                tokio::select! {
                        Some(event) = self.backend_deploy_request_channel_receiver.recv() => {
                            info!("Backend got event {event}");
                            match event{
                                BackendGatewayEvent::Changed(ctx) => {
                                    let gateway = &ctx.gateway;
                                    info!("AgentgatewayDeployerChannelHandlerService GatewayChanged {}",gateway.key());


                                    let maybe_service = deploy_agentgateway(&control_plane_config, client.clone(), gateway).await;
                                    if let Ok(service) = maybe_service {
                                        let resource_generator = ResourceGenerator::new(gateway);
                                        let bindings_and_listeners = resource_generator.generate_bindings_and_listeners();
                                        let bindings = bindings_and_listeners.keys().cloned().map(|b|
                                            Resource{
                                                kind: Some(Kind::Bind(Bind{ key: b.key, port: b.port}
                                            ))}).collect::<Vec<_>>();
                                        let listeners = bindings_and_listeners.values().cloned().flat_map(| listeners | listeners.into_iter()
                                        .map(|listener|
                                            Resource{kind: Some(Kind::Listener(listener))}
                                        )).collect::<Vec<_>>();
                                        let routes = resource_generator.generate_routes().into_iter().map(|r| Resource {
                                            kind: Some(Kind::Route(r))
                                        }).collect::<Vec<_>>();

                                        let _ = stream_resource_sender.send(ServerAction::UpdateBindings {
                                            gateway_id: gateway.key().clone(),
                                            resources: bindings,ack_version: 1
                                        }).await;
                                        let _ = stream_resource_sender.send(ServerAction::UpdateListeners {
                                            gateway_id: gateway.key().clone(),
                                            resources: listeners, ack_version: 1
                                        }).await;
                                        let _ = stream_resource_sender.send(ServerAction::UpdateRoutes {
                                            gateway_id: gateway.key().clone(),
                                            resources: routes, ack_version: 1
                                        }).await;
                                        self.update_address_with_polling(&service, *ctx).await;
                                    }else{
                                        warn!("Problem {maybe_service:?}");
                                        let _res = self.backend_response_channel_sender
                                            .send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                    }


                                }

                                BackendGatewayEvent::Deleted(ctx) => {
                                    let response_sender = ctx.response_sender;
                                    let gateway  = &ctx.gateway;
                                    info!("AgentgatewayDeployerChannelHandlerService GatewayDeleted {}",gateway.key());

                                    let resource_generator = ResourceGenerator::new(gateway);
                                    let bindings_and_listeners = resource_generator.generate_bindings_and_listeners();
                                    let bindings = bindings_and_listeners.keys().cloned().map(|b|
                                            Resource{
                                                kind: Some(Kind::Bind(
                                                    Bind{
                                                        key: b.key,
                                                        port: b.port }
                                                    ))
                                                }).collect::<Vec<_>>();
                                    let listeners = bindings_and_listeners.values().cloned().flat_map(| listeners | listeners.into_iter()
                                    .map(|listener|
                                        Resource{kind: Some(Kind::Listener(listener))}
                                    )).collect::<Vec<_>>();
                                    let routes = resource_generator.generate_routes().into_iter().map(|r|
                                        Resource{
                                            kind: Some(Kind::Route(r))
                                        }).collect::<Vec<_>>();

                                    let _ = stream_resource_sender.send(ServerAction::DeleteRoutes {
                                        gateway_id: gateway.key().clone(),
                                        resources: routes,ack_version: 1
                                    }).await;
                                    let _ = stream_resource_sender.send(ServerAction::DeleteListeners {
                                        gateway_id: gateway.key().clone(),
                                        resources: listeners, ack_version: 1
                                    }).await;
                                    let _ = stream_resource_sender.send(ServerAction::DeleteBindings {
                                        gateway_id: gateway.key().clone(),
                                        resources: bindings, ack_version: 1
                                    }).await;
                                    let _res = self.backend_response_channel_sender
                                        .send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                    let _ = response_sender.send(BackendGatewayResponse::Deleted(vec![]));
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
        .boxed();

        let xds_service = start_aggregate_server(kube_client, server_socket, stream_resource_receiver).boxed();
        futures::future::join_all(vec![agentgateway_deployer_service, xds_service]).await;
        Ok(())
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
    fn find_gateway_addresses(service: &Service) -> Option<Vec<String>> {
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
        let ips = ips.into_iter().flatten().collect::<Vec<_>>();
        if ips.is_empty() { None } else { Some(ips) }
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
}

async fn deploy_agentgateway(
    control_plane_config: &ControlPlaneConfig,
    client: Client,
    gateway: &Gateway,
) -> std::result::Result<Service, kube::Error> {
    let controller_name = &control_plane_config.controller_name;
    let service_api: Api<Service> = Api::namespaced(client.clone(), gateway.namespace());
    let service_account_api: Api<ServiceAccount> = Api::namespaced(client.clone(), gateway.namespace());
    let deployment_api: Api<Deployment> = Api::namespaced(client.clone(), gateway.namespace());
    let config_map_api: Api<ConfigMap> = Api::namespaced(client.clone(), gateway.namespace());
    let (_, bootstrap_cm) = config_map_names(gateway);

    let deployment = create_deployment(gateway);
    let service = create_service(gateway);
    let service_account = create_service_account(gateway);

    let pp = PatchParams { field_manager: Some(controller_name.to_owned()), ..Default::default() };

    let bootstrap_content = bootstrap_content(control_plane_config).map_err(kube::Error::Service)?;

    let boostrap_config_map = create_bootstrap_config_map(&bootstrap_cm, gateway, bootstrap_content);
    let _res = config_map_api.patch(&bootstrap_cm, &pp, &Patch::Apply(&boostrap_config_map)).await?;

    debug!("Created bootstrap config map for {}-{}", gateway.name(), gateway.namespace());

    let _deployment = deployment_api.patch(gateway.name(), &pp, &Patch::Apply(&deployment)).await?;
    debug!("Created deployment {}-{}", gateway.name(), gateway.namespace());

    let service = service_api.patch(gateway.name(), &pp, &Patch::Apply(&service)).await?;

    debug!("Service status {:?}", service.status);
    debug!("Created service {}-{}", gateway.name(), gateway.namespace());

    let service_account = service_account_api.patch(gateway.name(), &pp, &Patch::Apply(&service_account)).await?;
    debug!("Service account status {:?}", service_account);

    Ok(service)
}

fn create_labels(_gateway: &Gateway) -> BTreeMap<String, String> {
    BTreeMap::new()
}
fn create_deployment(gateway: &Gateway) -> Deployment {
    let mut labels = create_labels(gateway);
    let ports = gateway
        .listeners()
        .map(|l| ContainerPort { name: None, container_port: l.port(), protocol: Some("TCP".to_owned()), ..Default::default() })
        .dedup_by(|x, y| x.container_port == y.container_port && x.protocol == y.protocol)
        .collect();
    labels.insert("app".to_owned(), gateway.name().to_owned());

    let maybe_pod_spec = serde_json::from_str(AGENTGATEWAY_POD_SPEC);
    if maybe_pod_spec.as_ref().is_err() {
        error!("Can't parse the pod {maybe_pod_spec:?}");
        maybe_pod_spec.as_ref().expect("Expect this to work");
    }
    let mut pod_spec: PodSpec = maybe_pod_spec.expect("Expect this to work");
    if let Some(container) = pod_spec.containers.first_mut() {
        container.ports = Some(ports);
    }
    let (_, boostrap_cm) = config_map_names(gateway);
    let default_volumes = vec![Volume {
        name: "agentgateway-config".to_owned(),
        config_map: Some(ConfigMapVolumeSource {
            name: boostrap_cm,
            items: Some(vec![KeyToPath {
                key: "agentgateway-bootstrap.yaml".to_owned(),
                path: "agentgateway-bootstrap.yaml".to_owned(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }];
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
            labels: Some(create_labels(gateway)),
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

fn create_bootstrap_config_map(name: &str, gateway: &Gateway, boostrap_content: String) -> ConfigMap {
    let mut map = BTreeMap::new();
    map.insert("agentgateway-bootstrap.yaml".to_owned(), boostrap_content);
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

fn name(gw: &str, port: i32, proto: &str) -> String {
    format! {"{gw}-{port}-{}",proto.to_lowercase()}
}

fn create_service_account(gateway: &Gateway) -> ServiceAccount {
    ServiceAccount {
        metadata: ObjectMeta {
            name: Some(gateway.name().to_owned()),
            namespace: Some(gateway.namespace().to_owned()),
            labels: Some(create_labels(gateway)),

            ..Default::default()
        },

        automount_service_account_token: None,
        image_pull_secrets: None,
        secrets: None,
    }
}

fn config_map_names(gateway: &Gateway) -> (String, String) {
    (format!("{}-xds-cm", gateway.name()), format!("{}-bootstrap-cm", gateway.name()))
}

fn bootstrap_content(control_plane_config: &ControlPlaneConfig) -> Result<String, Error> {
    let mut tera_context = tera::Context::new();
    tera_context.insert("control_plane_host", &control_plane_config.listening_socket.ip().to_string());
    tera_context.insert("control_plane_port", &control_plane_config.listening_socket.port());

    Ok(TEMPLATES.render("agent-gateway-bootstrap-dynamic.yaml.tera", &tera_context)?)
}

const AGENTGATEWAY_POD_SPEC: &str = r#"
{
    "volumes": [
        {
            "name": "agentgateway-config",
            "configMap": {
                "name": "agentgateway-cm",
                "items": [
                    {
                        "key": "agentgateway-bootstrap.yaml",
                        "path": "agentgateway-bootstrap.yaml"
                    }
                ]
            }
        }
    ],
    "containers": [
        {
            "args": [
                "--file",
                "/agentgateway-config/agentgateway-bootstrap.yaml"
            ],
            "command": [
                "/app/agentgateway"
            ],
            "image": "docker.io/agentgateway/agentgateway:local",
            "imagePullPolicy": "IfNotPresent",
            "name": "agentgateway",
            "env": [
                {
                    "name": "GRPC_TRACE",
                    "value": "tcp,http,api"
                },
                {
                    "name": "NAMESPACE",
                    "valueFrom": {
                        "fieldRef": {
                            "apiVersion": "v1",
                            "fieldPath": "metadata.namespace"
                        }
                    }
                },
                {
                    "name": "GATEWAY",
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
                "tcpSocket":{
                    "port": 19001
                },
                "initialDelaySeconds": 3,
                "periodSeconds": 20
            },
            "volumeMounts": [
                {
                    "name": "agentgateway-config",
                    "mountPath": "/agentgateway-config",
                    "readOnly": true
                }
            ]
        }
    ]
}
"#;
