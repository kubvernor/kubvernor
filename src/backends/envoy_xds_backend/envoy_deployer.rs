use std::{
    collections::{btree_map::Values, BTreeMap, BTreeSet, HashMap},
    sync::LazyLock,
};

use envoy_api_rs::{
    envoy::{
        config::{
            cluster::v3::{
                cluster::{ClusterDiscoveryType, DiscoveryType, LbPolicy},
                Cluster as EnvoyCluster,
            },
            core::v3::{
                address, header_value_option::HeaderAppendAction, socket_address::PortSpecifier, Address, HeaderValue, HeaderValueOption, Http2ProtocolOptions, SocketAddress, TransportSocket,
            },
            endpoint::v3::{lb_endpoint::HostIdentifier, ClusterLoadAssignment, Endpoint, LbEndpoint, LocalityLbEndpoints},
            listener::v3::{Filter, FilterChain, Listener as EnvoyListener, ListenerFilter},
            route::v3::{
                header_matcher::HeaderMatchSpecifier,
                redirect_action,
                route::Action,
                route_action::{self, ClusterSpecifier},
                route_match::PathSpecifier,
                weighted_cluster::ClusterWeight,
                HeaderMatcher, RedirectAction, Route as EnvoyRoute, RouteAction, RouteConfiguration, RouteMatch, VirtualHost, WeightedCluster,
            },
        },
        extensions::{
            filters::{
                http::router::v3::Router,
                listener::tls_inspector::v3::TlsInspector,
                network::http_connection_manager::v3::{
                    http_connection_manager::{CodecType, RouteSpecifier},
                    HttpConnectionManager, HttpFilter,
                },
            },
            transport_sockets::tls::v3::{CommonTlsContext, DownstreamTlsContext, SdsSecretConfig},
            upstreams::http::v3::http_protocol_options::{explicit_http_config::ProtocolConfig, ExplicitHttpConfig, UpstreamProtocolOptions},
        },
        r#type::matcher::v3::{string_matcher::MatchPattern, RegexMatcher, StringMatcher},
        service::discovery::v3::Resource as EnvoyDiscoveryResource,
    },
    google::protobuf::{BoolValue, Duration, UInt32Value},
};
use futures::FutureExt;
use gateway_api::{common_types, httproutes};
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
use tera::Tera;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time,
};
use tracing::{debug, info, span, warn, Instrument, Level, Span};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use super::{
    converters,
    server::{start_aggregate_server, AckVersions, ServerAction},
};
use crate::{
    backends::{self, common::ResourceGenerator, envoy_xds_backend::resources},
    common::{
        self, Backend, BackendGatewayEvent, BackendGatewayResponse, BackendServiceConfig, Certificate, ChangedContext, ControlPlaneConfig, EffectiveRoutingRule, GRPCEffectiveRoutingRule, Gateway,
        GatewayAddress, Listener, ProtocolType, ResourceKey, TlsType,
    },
    Error,
};

pub static TEMPLATES: LazyLock<Tera> = LazyLock::new(|| match Tera::new("templates/**.tera") {
    Ok(t) => t,
    Err(e) => {
        warn!("Parsing error(s): {}", e);
        ::std::process::exit(1);
    }
});

#[derive(TypedBuilder)]
pub struct EnvoyDeployerChannelHandlerService {
    control_plane_config: ControlPlaneConfig,
    client: Client,
    backend_deploy_request_channel_receiver: Receiver<BackendGatewayEvent>,
    backend_response_channel_sender: Sender<BackendGatewayResponse>,
}

impl EnvoyDeployerChannelHandlerService {
    pub async fn start(&mut self) -> crate::Result<()> {
        let (stream_resource_sender, stream_resource_receiver) = mpsc::channel::<ServerAction>(1024);
        let control_plane_config = self.control_plane_config.clone();
        let client = self.client.clone();
        let kube_client = self.client.clone();
        let mut ack_versions = BTreeMap::<ResourceKey, AckVersions>::new();
        let server_socket = control_plane_config.addr();
        let envoy_deployer_service = async move {
            info!("Gateways handler started");
            loop {
                tokio::select! {
                        Some(event) = self.backend_deploy_request_channel_receiver.recv() => {
                            info!("Backend got event {event}");
                            match event{
                                BackendGatewayEvent::Changed(ctx) => {
                                    let gateway = &ctx.gateway;
                                    let span = span!(parent: &ctx.span, Level::INFO, "EnvoyDeployerService", event="GatewayChanged", id = %gateway.key());
                                    let _entered = span.enter();


                                    let Resources{listeners, clusters, secrets} = create_resources(gateway);

                                    let ack_version = ack_versions.entry(gateway.key().clone()).and_modify(super::server::AckVersions::inc).or_default();

                                    let maybe_service = deploy_envoy(&control_plane_config, client.clone(), gateway, &secrets).instrument(span.clone()).await;
                                    if let Ok(service) = maybe_service{
                                        info!("Service deployed {:?} listeners {} clusters {}", service.metadata.name, listeners.len(), clusters.len());                                        
                                        let _ = stream_resource_sender.send(ServerAction::UpdateClusters{gateway_id: gateway.key().clone(), resources: clusters, ack_version: ack_version.cluster()}).await;
                                        let _ = stream_resource_sender.send(ServerAction::UpdateListeners{gateway_id: gateway.key().clone(), resources: listeners, ack_version: ack_version.listener()}).await;

                                        self.update_address_with_polling(&service, *ctx, &span).await;
                                    }else{
                                        warn!("Problem {maybe_service:?}");
                                        let _res = self.backend_response_channel_sender.send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                    }
                                }

                                BackendGatewayEvent::Deleted(boxed) => {
                                    let span = boxed.span;
                                    let response_sender = boxed.response_sender;
                                    let gateway  = boxed.gateway;
                                    let _ = ack_versions.remove(gateway.key());
                                    let span = span!(parent: &span, Level::INFO, "EnvoyDeployerService", event="GatewayDeleted", id = %gateway.key());
                                    let _entered = span.enter();
                                    self.delete_envoy(&gateway).instrument(span.clone()).await;
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
        futures::future::join_all(vec![envoy_deployer_service, xds_service]).await;
        Ok(())
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

    async fn update_address_with_polling(&self, service: &Service, ctx: ChangedContext, parent_span: &Span) {
        if let Some(attached_addresses) = Self::find_gateway_addresses(service) {
            debug!("Got address address {attached_addresses:?}");
            let ChangedContext {
                span,
                mut gateway,
                kube_gateway,
                gateway_class_name,
            } = ctx;
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
                    span,
                    gateway_class_name,
                })
                .await;
        } else {
            let client = self.client.clone();
            let resource_key = ResourceKey::from(service);

            let backend_response_channel_sender = self.backend_response_channel_sender.clone();
            let span = span!(parent: parent_span, Level::INFO, "ServiceResolverTask");
            let _handle = tokio::spawn(
                async move {
                    let api: Api<Service> = Api::namespaced(client, &resource_key.namespace);
                    let ChangedContext {
                        span,
                        mut gateway,
                        kube_gateway,
                        gateway_class_name,
                    } = ctx;
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
                                        span: span.clone(),
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
                }
                .instrument(span),
            );
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
        if ips.is_empty() {
            None
        } else {
            Some(ips)
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
}

async fn deploy_envoy(control_plane_config: &ControlPlaneConfig, client: Client, gateway: &Gateway, secrets: &[ResourceKey]) -> std::result::Result<Service, kube::Error> {
    //debug!("Deploying Envoy {gateway:#?}");
    let controller_name = &control_plane_config.controller_name;
    let service_api: Api<Service> = Api::namespaced(client.clone(), gateway.namespace());
    let service_account_api: Api<ServiceAccount> = Api::namespaced(client.clone(), gateway.namespace());
    let deployment_api: Api<Deployment> = Api::namespaced(client.clone(), gateway.namespace());
    let config_map_api: Api<ConfigMap> = Api::namespaced(client.clone(), gateway.namespace());
    let (_, bootstrap_cm) = config_map_names(gateway);

    let deployment = create_deployment(gateway);
    let service = create_service(gateway);
    let service_account = create_service_account(gateway);

    let pp = PatchParams {
        field_manager: Some(controller_name.to_owned()),
        ..Default::default()
    };

    let bootstrap_content = if secrets.is_empty() {
        bootstrap_content(control_plane_config).map_err(kube::Error::Service)?
    } else {
        bootstrap_content_with_secrets(control_plane_config, secrets).map_err(kube::Error::Service)?
    };

    let envoy_boostrap_config_map = create_envoy_bootstrap_config_map(&bootstrap_cm, gateway, bootstrap_content);
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

fn create_resources(gateway: &Gateway) -> Resources {
    let mut listener_resources = vec![];
    let mut secret_resources = vec![];

    let resources = ResourceGenerator::new(gateway).generate_resources();
    let listeners = resources.values();

    for listener in listeners {
        let router = Router { ..Default::default() };
        let listener_name = listener.name.clone();
        let http_connection_manager_router_filter_any = converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.filters.http.router.v3.Router".to_owned(), &router));

        let router_filter = HttpFilter {
            name: format!("{listener_name}-http-connection-manager-route-filter"),
            is_optional: false,
            disabled: false,
            config_type: Some(envoy_api_rs::envoy::extensions::filters::network::http_connection_manager::v3::http_filter::ConfigType::TypedConfig(
                http_connection_manager_router_filter_any,
            )),
        };

        let virtual_hosts = generate_virtual_hosts_from_xds(&listener.http_listener_map);
        let http_connection_manager = HttpConnectionManager {
            stat_prefix: listener_name.clone(),
            codec_type: CodecType::Auto.into(),
            http_filters: vec![router_filter],
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
                    common::Certificate::NotResolved(_) | common::Certificate::Invalid(_) | common::Certificate::ResolvedCrossSpace(_) => None,
                })
                .collect();

            let downstream_context = DownstreamTlsContext {
                require_client_certificate: Some(BoolValue { value: false }),
                full_scan_certs_on_sni_mismatch: Some(BoolValue { value: true }),
                common_tls_context: Some(CommonTlsContext {
                    tls_certificate_sds_secret_configs: secrets
                        .iter()
                        .map(|s| SdsSecretConfig {
                            name: create_secret_name(s),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let downstream_context_any = converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext".to_owned(), &downstream_context));

            (
                Some(TransportSocket {
                    config_type: Some(envoy_api_rs::envoy::config::core::v3::transport_socket::ConfigType::TypedConfig(downstream_context_any)),
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
            config_type: Some(envoy_api_rs::envoy::config::listener::v3::listener_filter::ConfigType::TypedConfig(converters::AnyTypeConverter::from(
                ("type.googleapis.com/envoy.extensions.filters.listener.tls_inspector.v3.TlsInspector".to_owned(), &tls_inspector),
            ))),
            ..Default::default()
        };

        let envoy_listener = EnvoyListener {
            name: listener_name.clone(),
            address: Some(SocketAddressFactory::from(listener)),
            listener_filters: vec![tls_inspector_listener_filter],
            filter_chains: vec![FilterChain {
                transport_socket,
                filters: vec![http_connection_manager_filter],
                ..Default::default()
            }],

            ..Default::default()
        };

        listener_resources.push(resources::create_listener_resource(&envoy_listener));
        secret_resources.append(&mut secrets);
    }

    let cluster_resources = generate_clusters(resources.values()).iter().map(resources::create_cluster_resource).collect();

    secret_resources.dedup_by_key(|k| k.name.clone());
    Resources {
        listeners: listener_resources,
        clusters: cluster_resources,
        secrets: secret_resources,
    }
}

enum SocketAddressFactory {}
impl SocketAddressFactory {
    fn from(listener: &backends::common::EnvoyListener) -> envoy_api_rs::envoy::config::core::v3::Address {
        Address {
            address: Some(address::Address::SocketAddress(SocketAddress {
                address: "0.0.0.0".to_owned(),
                port_specifier: Some(PortSpecifier::PortValue(listener.port.try_into().expect("For time being we expect this to work"))),
                resolver_name: String::new(),
                ipv4_compat: false,
                ..Default::default()
            })),
        }
    }

    fn from_backend(backend: &BackendServiceConfig) -> envoy_api_rs::envoy::config::core::v3::Address {
        Address {
            address: Some(address::Address::SocketAddress(SocketAddress {
                address: backend.endpoint.clone(),
                port_specifier: Some(PortSpecifier::PortValue(backend.effective_port.try_into().expect("For time being we expect this to work"))),
                ..Default::default()
            })),
        }
    }
}

impl From<ProtocolType> for i32 {
    fn from(val: ProtocolType) -> Self {
        i32::from(val == ProtocolType::Udp)
    }
}

enum DurationConverter {}

impl DurationConverter {
    fn from(val: std::time::Duration) -> Duration {
        Duration {
            nanos: val.subsec_nanos().try_into().expect("At the moment we expect this to work"),
            seconds: val.as_secs().try_into().expect("At the moment we expect this to work"),
        }
    }
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
    tera_context.insert("control_plane_host", &control_plane_config.host);
    tera_context.insert("control_plane_port", &control_plane_config.port);

    Ok(TEMPLATES.render("envoy-bootstrap-dynamic-with-secrets.yaml.tera", &tera_context)?)
}

fn bootstrap_content(control_plane_config: &ControlPlaneConfig) -> Result<String, Error> {
    let mut tera_context = tera::Context::new();
    tera_context.insert("control_plane_host", &control_plane_config.host);
    tera_context.insert("control_plane_port", &control_plane_config.port);

    Ok(TEMPLATES.render("envoy-bootstrap-dynamic.yaml.tera", &tera_context)?)
}

fn config_map_names(gateway: &Gateway) -> (String, String) {
    (format!("{}-xds-cm", gateway.name()), format!("{}-bootstrap-cm", gateway.name()))
}

fn create_deployment(gateway: &Gateway) -> Deployment {
    let mut labels = create_labels(gateway);
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
    let (_, boostrap_cm) = config_map_names(gateway);
    let mut default_volumes = vec![Volume {
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

fn generate_virtual_hosts_from_xds(virtual_hosts: &BTreeSet<backends::common::EnvoyVirtualHost>) -> Vec<VirtualHost> {
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

#[derive(Debug)]
struct ClusterHolder {
    name: String,
    cluster: EnvoyCluster,
}
impl Eq for ClusterHolder {}

impl PartialEq for ClusterHolder {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl PartialOrd for ClusterHolder {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.name.cmp(&other.name))
    }
}
impl Ord for ClusterHolder {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

fn generate_clusters(listeners: Values<i32, backends::common::EnvoyListener>) -> Vec<EnvoyCluster> {
    let grpc_protocol_options = envoy_api_rs::envoy::extensions::upstreams::http::v3::HttpProtocolOptions {
        upstream_protocol_options: Some(UpstreamProtocolOptions::ExplicitHttpConfig(ExplicitHttpConfig {
            protocol_config: Some(ProtocolConfig::Http2ProtocolOptions(Http2ProtocolOptions {
                max_concurrent_streams: Some(UInt32Value { value: 10 }),
                ..Default::default()
            })),
        })),
        ..Default::default()
    };

    let grpc_http_configuration = converters::AnyTypeConverter::from(("type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), &grpc_protocol_options));

    let clusters: BTreeSet<ClusterHolder> = listeners
        .flat_map(|listener| {
            listener.http_listener_map.iter().flat_map(|evc| {
                evc.resolved_routes.iter().chain(evc.unresolved_routes.iter()).flat_map(|r| {
                    let route_type = r.route_type();

                    r.backends()
                        .iter()
                        .filter(|b| b.weight() > 0)
                        .filter_map(|b| {
                            if let Backend::Resolved(backend_service_config) = b {
                                Some(backend_service_config)
                            } else {
                                warn!("Filtering out backend not resolved {:?}", b);
                                None
                            }
                        })
                        .map(|r| ClusterHolder {
                            name: r.cluster_name(),
                            cluster: EnvoyCluster {
                                name: r.cluster_name(),
                                cluster_discovery_type: Some(ClusterDiscoveryType::Type(DiscoveryType::StrictDns.into())),
                                lb_policy: LbPolicy::RoundRobin.into(),
                                connect_timeout: Some(DurationConverter::from(std::time::Duration::from_millis(250))),
                                load_assignment: Some(ClusterLoadAssignment {
                                    cluster_name: r.cluster_name(),
                                    endpoints: vec![LocalityLbEndpoints {
                                        lb_endpoints: vec![LbEndpoint {
                                            host_identifier: Some(HostIdentifier::Endpoint(Endpoint {
                                                address: Some(SocketAddressFactory::from_backend(r)),
                                                ..Default::default()
                                            })),
                                            load_balancing_weight: Some(UInt32Value {
                                                value: r.weight.try_into().expect("For time being we expect this to work"),
                                            }),

                                            ..Default::default()
                                        }],
                                        ..Default::default()
                                    }],
                                    ..Default::default()
                                }),
                                typed_extension_protocol_options: match route_type {
                                    common::RouteType::Http(_) => HashMap::new(),
                                    common::RouteType::Grpc(_) => vec![("envoy.extensions.upstreams.http.v3.HttpProtocolOptions".to_owned(), grpc_http_configuration.clone())]
                                        .into_iter()
                                        .collect(),
                                },

                                ..Default::default()
                            },
                        })
                        .collect::<Vec<_>>()
                })
            })
        })
        .collect();
    warn!("Clusters produced {:?}", clusters);
    clusters.into_iter().map(|c| c.cluster).collect::<Vec<_>>()
}

impl From<EffectiveRoutingRule> for EnvoyRoute {
    fn from(effective_routing_rule: EffectiveRoutingRule) -> Self {
        let path_specifier = effective_routing_rule.route_matcher.path.clone().and_then(|matcher| {
            let value = matcher.value.clone().map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v });
            matcher.r#type.map(|t| match t {
                httproutes::HTTPRouteRulesMatchesPathType::Exact => PathSpecifier::Path(value),
                httproutes::HTTPRouteRulesMatchesPathType::PathPrefix => {
                    if let Some(val) = matcher.value {
                        if val == "/" {
                            PathSpecifier::Prefix(value)
                        } else {
                            PathSpecifier::PathSeparatedPrefix(value)
                        }
                    } else {
                        PathSpecifier::Prefix(value)
                    }
                }
                httproutes::HTTPRouteRulesMatchesPathType::RegularExpression => PathSpecifier::SafeRegex(RegexMatcher { regex: value, ..Default::default() }),
            })
        });

        let path_specifier = if path_specifier.is_none() { Some(PathSpecifier::Path("/".to_owned())) } else { path_specifier };

        warn!("Headers to match {:?}", effective_routing_rule.route_matcher.headers);
        let headers = effective_routing_rule.route_matcher.headers.map_or(vec![], |headers| {
            headers
                .iter()
                .map(|header| HeaderMatcher {
                    name: header.name.clone(),
                    header_match_specifier: Some(HeaderMatchSpecifier::StringMatch(StringMatcher {
                        match_pattern: Some(MatchPattern::Exact(header.value.clone())),
                        ..Default::default()
                    })),
                    ..Default::default()
                })
                .collect()
        });

        let route_match = RouteMatch {
            path_specifier,
            grpc: None,
            headers,
            ..Default::default()
        };

        let request_filter_headers = effective_routing_rule.request_headers;

        let request_headers_to_add = request_filter_headers
            .add
            .clone()
            .into_iter()
            .map(|h| HeaderValueOption {
                header: Some(HeaderValue {
                    key: h.name,
                    value: h.value,
                    ..Default::default()
                }),
                append_action: HeaderAppendAction::AppendIfExistsOrAdd.into(),
                ..Default::default()
            })
            .chain(request_filter_headers.set.clone().into_iter().map(|h| HeaderValueOption {
                header: Some(HeaderValue {
                    key: h.name,
                    value: h.value,
                    ..Default::default()
                }),
                append_action: HeaderAppendAction::OverwriteIfExistsOrAdd.into(),
                ..Default::default()
            }))
            .collect();

        let request_headers_to_remove = request_filter_headers.remove;

        let cluster_names: Vec<_> = effective_routing_rule
            .backends
            .iter()
            .filter(|b| b.weight() > 0)
            .map(|b| ClusterWeight {
                name: b.cluster_name(),
                weight: Some(UInt32Value {
                    value: b.weight().try_into().expect("We do expect this to work for time being"),
                }),
                ..Default::default()
            })
            .collect();
        let cluster_action = RouteAction {
            cluster_not_found_response_code: route_action::ClusterNotFoundResponseCode::InternalServerError.into(),
            cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster {
                clusters: cluster_names,
                ..Default::default()
            })),
            ..Default::default()
        };

        let action: Action = if let Some(redirect_action) = effective_routing_rule.redirect_filter.clone().map(|f| RedirectAction {
            host_redirect: f.hostname.unwrap_or_default(),

            response_code: match f.status_code {
                Some(302) => redirect_action::RedirectResponseCode::Found.into(),
                Some(303) => redirect_action::RedirectResponseCode::SeeOther.into(),
                Some(307) => redirect_action::RedirectResponseCode::TemporaryRedirect.into(),
                Some(308) => redirect_action::RedirectResponseCode::PermanentRedirect.into(),
                Some(301 | _) | None => redirect_action::RedirectResponseCode::MovedPermanently.into(),
            },
            ..Default::default()
        }) {
            Action::Redirect(redirect_action)
        } else {
            Action::Route(cluster_action)
        };

        EnvoyRoute {
            name: format!("{}-route", effective_routing_rule.name),
            r#match: Some(route_match),
            request_headers_to_add,
            request_headers_to_remove,
            action: Some(action),
            ..Default::default()
        }
    }
}

impl From<GRPCEffectiveRoutingRule> for EnvoyRoute {
    fn from(effective_routing_rule: GRPCEffectiveRoutingRule) -> Self {
        warn!("Headers to match {:?}", effective_routing_rule.route_matcher.headers);

        let headers = effective_routing_rule.route_matcher.headers.map_or(vec![], |headers| {
            headers
                .iter()
                .map(|header| HeaderMatcher {
                    name: header.name.clone(),
                    header_match_specifier: Some(HeaderMatchSpecifier::StringMatch(StringMatcher {
                        match_pattern: Some(MatchPattern::Exact(header.value.clone())),
                        ..Default::default()
                    })),
                    ..Default::default()
                })
                .collect()
        });

        let path_specifier = effective_routing_rule.route_matcher.method.clone().and_then(|matcher| {
            let service = matcher.service.clone().map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v });

            let path = if let Some(method) = matcher.method {
                "/".to_owned() + &service + "/" + &method
            } else {
                "/".to_owned() + &service
            };

            matcher.r#type.map(|t| match t {
                common_types::HeaderMatchesType::Exact => PathSpecifier::Path(path),
                common_types::HeaderMatchesType::RegularExpression => PathSpecifier::SafeRegex(RegexMatcher { regex: path, ..Default::default() }),
            })
        });

        let path_specifier = if path_specifier.is_none() { Some(PathSpecifier::Prefix("/".to_owned())) } else { path_specifier };

        //let route_match = RouteMatch { headers, path_specifier, grpc: Some(GrpcRouteMatchOptions::default()), ..Default::default() };
        let route_match = RouteMatch {
            headers,
            path_specifier,
            grpc: None,
            ..Default::default()
        };

        let request_filter_headers = effective_routing_rule.request_headers;

        let request_headers_to_add = request_filter_headers
            .add
            .clone()
            .into_iter()
            .map(|h| HeaderValueOption {
                header: Some(HeaderValue {
                    key: h.name,
                    value: h.value,
                    ..Default::default()
                }),
                append_action: HeaderAppendAction::AppendIfExistsOrAdd.into(),
                ..Default::default()
            })
            .chain(request_filter_headers.set.clone().into_iter().map(|h| HeaderValueOption {
                header: Some(HeaderValue {
                    key: h.name,
                    value: h.value,
                    ..Default::default()
                }),
                append_action: HeaderAppendAction::OverwriteIfExistsOrAdd.into(),
                ..Default::default()
            }))
            .collect();

        let request_headers_to_remove = request_filter_headers.remove;

        let cluster_names: Vec<_> = effective_routing_rule
            .backends
            .iter()
            .filter(|b| b.weight() > 0)
            .map(|b| ClusterWeight {
                name: b.cluster_name(),
                weight: Some(UInt32Value {
                    value: b.weight().try_into().expect("We do expect this to work for time being"),
                }),
                ..Default::default()
            })
            .collect();
        let cluster_action = RouteAction {
            cluster_not_found_response_code: route_action::ClusterNotFoundResponseCode::NotFound.into(),
            cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster {
                clusters: cluster_names,
                ..Default::default()
            })),
            ..Default::default()
        };

        let action: Action = Action::Route(cluster_action);

        EnvoyRoute {
            name: format!("{}-grpc-route", effective_routing_rule.name),
            r#match: Some(route_match),
            request_headers_to_add,
            request_headers_to_remove,
            action: Some(action),
            ..Default::default()
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
                    "name": "envoy-secrets",
                    "mountPath": "/envoy-secrets",
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
        .filter_map(|c| match c {
            Certificate::ResolvedSameSpace(resource_key) => Some(resource_key),
            Certificate::NotResolved(_) | Certificate::Invalid(_) | Certificate::ResolvedCrossSpace(_) => None,
        })
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
