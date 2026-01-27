use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Values},
    net::IpAddr,
    sync::LazyLock,
};

use envoy_api_rs::{
    envoy::{
        config::{
            core::v3::{
                GrpcService, TransportSocket,
                grpc_service::{GoogleGrpc, TargetSpecifier},
            },
            listener::v3::{Filter, FilterChain, Listener as EnvoyListener, ListenerFilter},
            route::v3::{RouteConfiguration, VirtualHost},
        },
        extensions::{
            filters::{
                http::{
                    ext_proc::v3::{ExternalProcessor, ProcessingMode, processing_mode::HeaderSendMode},
                    router::v3::Router,
                },
                listener::tls_inspector::v3::TlsInspector,
                network::http_connection_manager::v3::{
                    HttpConnectionManager, HttpFilter,
                    http_connection_manager::{CodecType, RouteSpecifier},
                    http_filter::ConfigType,
                },
            },
            transport_sockets::tls::v3::{CommonTlsContext, DownstreamTlsContext, SdsSecretConfig},
        },
        service::discovery::v3::Resource as EnvoyDiscoveryResource,
    },
    google::protobuf::BoolValue,
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
use kubvernor_common::{GatewayImplementationType, ResourceKey};
use tera::Tera;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time,
};
use tracing::{debug, info, warn};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::server::{AckVersions, ServerAction, start_aggregate_server};
use crate::{
    Error,
    backends::envoy::{
        common::{
            DurationConverter, INFERENCE_EXT_PROC_FILTER_NAME, SocketAddressFactory, converters,
            resource_generator::{EnvoyVirtualHost, ResourceGenerator},
        },
        envoy_xds_backend::resources,
    },
    common::{
        self, BackendGatewayEvent, BackendGatewayResponse, Certificate, ChangedContext, ControlPlaneConfig, Gateway, GatewayAddress,
        Listener, TlsType,
    },
};

pub static TEMPLATES: LazyLock<Tera> = LazyLock::new(|| match Tera::new("templates/**.tera") {
    Ok(t) => t,
    Err(e) => {
        warn!("Parsing error(s): {}", e);
        ::std::process::exit(1);
    },
});

#[derive(TypedBuilder)]
pub struct OrionDeployerChannelHandlerService {
    control_plane_config: ControlPlaneConfig,
    client: Client,
    backend_deploy_request_channel_receiver: Receiver<BackendGatewayEvent>,
    backend_response_channel_sender: Sender<BackendGatewayResponse>,
}

impl OrionDeployerChannelHandlerService {
    pub async fn start(mut self) -> crate::Result<()> {
        let (stream_resource_sender, stream_resource_receiver) = mpsc::channel::<ServerAction>(1024);
        let control_plane_config = self.control_plane_config.clone();
        let client = self.client.clone();
        let kube_client = self.client.clone();
        let mut ack_versions = BTreeMap::<ResourceKey, AckVersions>::new();
        let server_socket = control_plane_config.addr();
        let orion_deployer_service = async move {
            info!("Gateways handler started");
            loop {
                tokio::select! {
                        Some(event) = self.backend_deploy_request_channel_receiver.recv() => {
                            info!("Backend got event {event}");
                            match event{
                                BackendGatewayEvent::Changed(ctx) => {
                                    let gateway = &ctx.gateway;
                                    info!("OrionDeployerChannelHandlerService GatewayChanged {}",gateway.key());

                                    let Resources{listeners, clusters, secrets} = create_resources(gateway);

                                    let ack_version = ack_versions.entry(gateway.key()
                                        .clone())
                                        .and_modify(super::server::AckVersions::inc)
                                        .or_default();

                                    let maybe_service = deploy_orion(&control_plane_config, client.clone(), gateway, &secrets).await;
                                    if let Ok(service) = maybe_service{
                                        info!("Service deployed {:?} listeners {} clusters {}", service.metadata.name, listeners.len(), clusters.len());
                                        let _ = stream_resource_sender.send(ServerAction::UpdateClusters{
                                            gateway_id: gateway.key().clone(),
                                            resources: clusters,
                                            ack_version: ack_version.cluster()
                                        }).await;
                                        let _ = stream_resource_sender.send(ServerAction::UpdateListeners{
                                            gateway_id: gateway.key().clone(),
                                            resources: listeners,
                                            ack_version: ack_version.listener()
                                        }).await;

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
                                    let _ = ack_versions.remove(gateway.key());
                                    info!("OrionDeployerChannelHandlerService GatewayDeleted {}",gateway.key());
                                    self.delete_orion(&gateway).await;
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
        .boxed();

        let xds_service = start_aggregate_server(kube_client, server_socket, stream_resource_receiver).boxed();
        futures::future::join_all(vec![orion_deployer_service, xds_service]).await;
        Ok(())
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

    async fn delete_orion(&self, gateway: &Gateway) {
        debug!("Deleting Orion {gateway:?}");
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

async fn deploy_orion(
    control_plane_config: &ControlPlaneConfig,
    client: Client,
    gateway: &Gateway,
    secrets: &[ResourceKey],
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

    let bootstrap_content = if secrets.is_empty() {
        bootstrap_content(control_plane_config).map_err(kube::Error::Service)?
    } else {
        bootstrap_content_with_secrets(control_plane_config, secrets).map_err(kube::Error::Service)?
    };

    let envoy_boostrap_config_map = create_orion_bootstrap_config_map(
        &bootstrap_cm,
        gateway,
        config_content(control_plane_config).map_err(kube::Error::Service)?,
        bootstrap_content,
    );
    let _res = config_map_api.patch(&bootstrap_cm, &pp, &Patch::Apply(&envoy_boostrap_config_map)).await?;

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

#[derive(Debug, Default)]
struct Resources {
    listeners: Vec<EnvoyDiscoveryResource>,
    clusters: Vec<EnvoyDiscoveryResource>,
    secrets: Vec<ResourceKey>,
}

fn calculate_effective_hostnames(hostname: &str) -> Vec<String> {
    vec![hostname.to_owned()]
}

fn create_resources(gateway: &Gateway) -> Resources {
    let mut listener_resources = vec![];
    let mut secret_resources = vec![];
    let mut resource_generator = ResourceGenerator::new(gateway, GatewayImplementationType::Orion, Box::new(calculate_effective_hostnames));

    let listeners = resource_generator.generate_envoy_listeners().values();

    for listener in listeners {
        let router = Router { ..Default::default() };
        let ext_processor = ExternalProcessor {
            processing_mode: Some(ProcessingMode {
                request_header_mode: HeaderSendMode::Skip.into(),
                response_header_mode: HeaderSendMode::Skip.into(),
                ..Default::default()
            }),

            grpc_service: Some(GrpcService {
                target_specifier: Some(TargetSpecifier::GoogleGrpc(GoogleGrpc {
                    target_uri: "127.0.0.1:1000".to_owned(),
                    stat_prefix: listener.name.clone() + "ext_filter_stats",
                    ..Default::default()
                })),
                ..Default::default()
            }),
            send_body_without_waiting_for_header_response: true,
            message_timeout: Some(DurationConverter::from(std::time::Duration::from_secs(2))),
            failure_mode_allow: true,
            allow_mode_override: true,
            ..Default::default()
        };
        let listener_name = listener.name.clone();
        let http_connection_manager_router_filter_any =
            converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.filters.http.router.v3.Router".to_owned(), &router));
        let http_connection_manager_ext_processor_any = converters::AnyTypeConverter::from((
            "type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor".to_owned(),
            &ext_processor,
        ));

        let router_filter = HttpFilter {
            name: format!("{listener_name}-http-connection-manager-route-filter"),
            is_optional: false,
            disabled: false,
            config_type: Some(ConfigType::TypedConfig(http_connection_manager_router_filter_any)),
        };

        let external_processor_filter = HttpFilter {
            name: INFERENCE_EXT_PROC_FILTER_NAME.to_owned(),
            is_optional: false,
            disabled: false,
            config_type: Some(ConfigType::TypedConfig(http_connection_manager_ext_processor_any)),
        };

        let virtual_hosts = generate_virtual_hosts_from_xds(&listener.http_listener_map);
        let http_connection_manager = HttpConnectionManager {
            stat_prefix: listener_name.clone(),
            codec_type: CodecType::Auto.into(),
            http_filters: vec![external_processor_filter, router_filter],
            route_specifier: Some(RouteSpecifier::RouteConfig(RouteConfiguration {
                name: format!("{listener_name}-route"),
                virtual_hosts,
                validate_clusters: Some(BoolValue { value: false }),
                ..Default::default()
            })),
            ..Default::default()
        };

        let http_connection_manager_any = converters::AnyTypeConverter::from((
            "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager".to_owned(),
            &http_connection_manager,
        ));

        let http_connection_manager_filter = Filter {
            name: format!("{listener_name}-http-connection-manager"),
            config_type: Some(envoy_api_rs::envoy::config::listener::v3::filter::ConfigType::TypedConfig(http_connection_manager_any)),
        };

        let (transport_socket, mut secrets) = if let Some(TlsType::Terminate(secrets)) = listener.tls_type.as_ref() {
            let secrets: Vec<ResourceKey> = secrets
                .iter()
                .cloned()
                .filter_map(|cert| match cert {
                    common::Certificate::ResolvedSameSpace(resource_key) => Some(resource_key),
                    common::Certificate::NotResolved(_) | common::Certificate::Invalid(_) | common::Certificate::ResolvedCrossSpace(_) => {
                        None
                    },
                })
                .collect();

            let downstream_context = DownstreamTlsContext {
                require_client_certificate: Some(BoolValue { value: false }),
                full_scan_certs_on_sni_mismatch: Some(BoolValue { value: true }),
                common_tls_context: Some(CommonTlsContext {
                    tls_certificate_sds_secret_configs: secrets
                        .iter()
                        .map(|s| SdsSecretConfig { name: create_secret_name(s), ..Default::default() })
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let downstream_context_any = converters::AnyTypeConverter::from((
                "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext".to_owned(),
                &downstream_context,
            ));

            (
                Some(TransportSocket {
                    config_type: Some(envoy_api_rs::envoy::config::core::v3::transport_socket::ConfigType::TypedConfig(
                        downstream_context_any,
                    )),
                    name: format!("{listener_name}-downstream-tls-context"),
                }),
                secrets,
            )
        } else {
            (None, vec![])
        };
        let tls_inspector = TlsInspector::default();

        let tls_inspector_listener_filter = ListenerFilter {
            name: format!("{listener_name}-tls-inspector"),
            config_type: Some(envoy_api_rs::envoy::config::listener::v3::listener_filter::ConfigType::TypedConfig(
                converters::AnyTypeConverter::from((
                    "type.googleapis.com/envoy.extensions.filters.listener.tls_inspector.v3.TlsInspector".to_owned(),
                    &tls_inspector,
                )),
            )),
            ..Default::default()
        };

        let envoy_listener = EnvoyListener {
            name: listener_name.clone(),
            address: Some(SocketAddressFactory::from(listener)),
            listener_filters: vec![tls_inspector_listener_filter],
            filter_chains: vec![FilterChain { transport_socket, filters: vec![http_connection_manager_filter], ..Default::default() }],

            ..Default::default()
        };

        listener_resources.push(resources::create_listener_resource(&envoy_listener));
        secret_resources.append(&mut secrets);
    }

    let cluster_resources = resource_generator.generate_envoy_clusters().iter().map(resources::create_cluster_resource).collect();
    secret_resources.dedup_by_key(|k| k.name.clone());
    Resources { listeners: listener_resources, clusters: cluster_resources, secrets: secret_resources }
}

fn bootstrap_content_with_secrets(control_plane_config: &ControlPlaneConfig, secrets: &[ResourceKey]) -> Result<String, Error> {
    use serde::Serialize;
    #[derive(Serialize, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct TeraSecret {
        name: String,
        certificate: String,
        key: String,
    }
    let tera_secrets: BTreeSet<TeraSecret> = secrets
        .iter()
        .map(|resource_key| TeraSecret {
            name: create_secret_name(resource_key),
            certificate: create_certificate_name(resource_key),
            key: create_key_name(resource_key),
        })
        .collect();
    let mut tera_context = tera::Context::new();
    if !tera_secrets.is_empty() {
        tera_context.insert("secrets", &tera_secrets);
    }
    tera_context.insert("control_plane_host", &control_plane_config.listening_socket.hostname);
    tera_context.insert("control_plane_port", &control_plane_config.listening_socket.port);
    tera_context.insert(
        "resolver_type",
        if control_plane_config.listening_socket.hostname.parse::<IpAddr>().is_err() { "STRICT_DNS" } else { "STATIC" },
    );

    Ok(TEMPLATES.render("orion-bootstrap-dynamic-with-secrets.yaml.tera", &tera_context)?)
}

fn bootstrap_content(control_plane_config: &ControlPlaneConfig) -> Result<String, Error> {
    let mut tera_context = tera::Context::new();
    tera_context.insert("control_plane_host", &control_plane_config.listening_socket.hostname);
    tera_context.insert("control_plane_port", &control_plane_config.listening_socket.port);
    tera_context.insert(
        "resolver_type",
        if control_plane_config.listening_socket.hostname.parse::<IpAddr>().is_err() { "STRICT_DNS" } else { "STATIC" },
    );

    Ok(TEMPLATES.render("orion-bootstrap-dynamic.yaml.tera", &tera_context)?)
}

fn config_content(_: &ControlPlaneConfig) -> Result<String, Error> {
    let tera_context = tera::Context::new();
    Ok(TEMPLATES.render("orion-config.yaml.tera", &tera_context)?)
}

fn config_map_names(gateway: &Gateway) -> (String, String) {
    (format!("{}-xds-cm", gateway.name()), format!("{}-bootstrap-cm", gateway.name()))
}

fn create_deployment(gateway: &Gateway) -> Deployment {
    let mut labels = create_labels(gateway);
    let ports = gateway
        .listeners()
        .map(|l| ContainerPort { name: None, container_port: l.port(), protocol: Some("TCP".to_owned()), ..Default::default() })
        .dedup_by(|x, y| x.container_port == y.container_port && x.protocol == y.protocol)
        .collect();
    labels.insert("app".to_owned(), gateway.name().to_owned());

    let mut pod_spec: PodSpec = serde_json::from_str(ORION_POD_SPEC).unwrap_or(PodSpec { ..Default::default() });
    if let Some(container) = pod_spec.containers.first_mut() {
        container.ports = Some(ports);
    }
    let (_, boostrap_cm) = config_map_names(gateway);
    let mut default_volumes = vec![Volume {
        name: "orion-config".to_owned(),
        config_map: Some(ConfigMapVolumeSource {
            name: boostrap_cm,
            items: Some(vec![
                KeyToPath { key: "orion-config.yaml".to_owned(), path: "orion-config.yaml".to_owned(), ..Default::default() },
                KeyToPath { key: "envoy-bootstrap.yaml".to_owned(), path: "envoy-bootstrap.yaml".to_owned(), ..Default::default() },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut secret_volumes = create_secret_volumes(gateway.listeners());
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

fn create_orion_bootstrap_config_map(name: &str, gateway: &Gateway, orion_config: String, envoy_boostrap_content: String) -> ConfigMap {
    let mut map = BTreeMap::new();
    map.insert("envoy-bootstrap.yaml".to_owned(), envoy_boostrap_content);
    map.insert("orion-config.yaml".to_owned(), orion_config);
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

fn generate_virtual_hosts_from_xds(virtual_hosts: &BTreeSet<EnvoyVirtualHost>) -> Vec<VirtualHost> {
    virtual_hosts
        .iter()
        .map(|vh| VirtualHost {
            name: vh.name.clone(),
            domains: vh.effective_hostnames.clone(),
            routes: vh.http_routes.clone().into_iter().chain(vh.grpc_routes.clone()).collect(),
            ..Default::default()
        })
        .collect()
}

const ORION_POD_SPEC: &str = r#"
{
    "volumes": [
        {
            "name": "orion-config",
            "configMap": {
                "name": "orion-cm",
                "items": [
                    {
                        "key": "orion-config.yaml",
                        "path": "orion-config.yaml"
                    },
                    {
                        "key": "envoy-bootstrap.yaml",
                        "path": "envoy-bootstrap.yaml"
                    }
                ]
            }
        }
    ],
    "containers": [
        {
            "args": [
                "--config",
                "/orion-config/orion-config.yaml",
                "--with-envoy-bootstrap",
                "/orion-config/envoy-bootstrap.yaml"
            ],
            "command": [
                "/orion"
            ],
            "image": "ghcr.io/kmesh-net/orion:0.1.0",
            "imagePullPolicy": "IfNotPresent",
            "name": "orion",
            "env": [
                {
                    "name": "POD_NAMESPACE",
                    "valueFrom": {
                        "fieldRef": {
                            "apiVersion": "v1",
                            "fieldPath": "metadata.namespace"
                        }
                    }
                },
                {
                    "name": "POD_NAME",
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
                    "name": "orion-config",
                    "mountPath": "/orion-config",
                    "readOnly": true
                },
                {
                    "name": "orion-secrets",
                    "mountPath": "/orion-secrets",
                    "readOnly": true
                }
            ]
        }
    ]
}
"#;

pub fn create_secret_name(resource_key: &ResourceKey) -> String {
    resource_key.name.clone() + "_" + &resource_key.namespace
}

pub fn create_certificate_name(resource_key: &ResourceKey) -> String {
    resource_key.name.clone() + "_" + &resource_key.namespace + "_cert.pem"
}

pub fn create_key_name(resource_key: &ResourceKey) -> String {
    resource_key.name.clone() + "_" + &resource_key.namespace + "_key.pem"
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
        name: "orion-secrets".to_owned(),
        projected: Some(ProjectedVolumeSource { sources: Some(secrets), ..Default::default() }),
        ..Default::default()
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_effective_hostnames() {
        let routes = vec![];
        let hostname = Some("*".to_owned());
        let hostnames = crate::backends::envoy::common::resource_generator::calculate_hostnames_common(
            &routes,
            hostname,
            calculate_effective_hostnames,
        );
        assert_eq!(hostnames, vec!["*".to_owned()]);
        let hostname = Some("host.blah".to_owned());
        let hostnames: BTreeSet<String> = crate::backends::envoy::common::resource_generator::calculate_hostnames_common(
            &routes,
            hostname,
            calculate_effective_hostnames,
        )
        .into_iter()
        .collect();
        assert_eq!(hostnames, vec!["host.blah".to_owned(), "host.blah:*".to_owned()].into_iter().collect::<BTreeSet<_>>());
        let hostname = Some("host.blah".to_owned());
        let hostnames = crate::backends::envoy::common::resource_generator::calculate_hostnames_common(&routes, hostname, |h| {
            vec![format!("{h}:*"), h.to_owned()]
        })
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(hostnames, vec!["host.blah".to_owned(), "host.blah:*".to_owned()].into_iter().collect::<BTreeSet<_>>());
    }
}
