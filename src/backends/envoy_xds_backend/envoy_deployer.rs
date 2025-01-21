use std::collections::{btree_map::Values, BTreeMap, BTreeSet};

use envoy_api::{
    envoy::{
        config::{
            cluster::v3::{
                cluster::{ClusterDiscoveryType, DiscoveryType, LbPolicy},
                Cluster as EnvoyCluster,
            },
            core::v3::{address, header_value_option::HeaderAppendAction, socket_address::PortSpecifier, Address, HeaderValue, HeaderValueOption, SocketAddress, TransportSocket},
            endpoint::v3::{lb_endpoint::HostIdentifier, ClusterLoadAssignment, Endpoint, LbEndpoint, LocalityLbEndpoints},
            listener::v3::{Filter, FilterChain, Listener as EnvoyListener},
            route::v3::{
                header_matcher::HeaderMatchSpecifier, route::Action, route_action::ClusterSpecifier, route_match::PathSpecifier, weighted_cluster::ClusterWeight, HeaderMatcher, RedirectAction,
                Route as EnvoyRoute, RouteAction, RouteConfiguration, RouteMatch, VirtualHost, WeightedCluster,
            },
        },
        extensions::{
            filters::network::http_connection_manager::v3::{
                http_connection_manager::{CodecType, RouteSpecifier},
                HttpConnectionManager,
            },
            transport_sockets::tls::v3::{CommonTlsContext, DownstreamTlsContext, SdsSecretConfig},
        },
        r#type::matcher::v3::RegexMatcher,
        service::discovery::v3::Resource as EnvoyDiscoveryResource,
    },
    google::protobuf::{BoolValue, Duration, UInt32Value},
};
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
    api::{Patch, PatchParams},
    Api, Client,
};
use kube_core::ObjectMeta;
use lazy_static::lazy_static;
use tera::Tera;
use tokio::sync::mpsc::{self, Receiver, Sender};

use tracing::{debug, info, span, warn, Instrument, Level};
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::{
    backends::envoy_xds_backend::resources,
    common::{
        self, Backend, BackendGatewayEvent, BackendGatewayResponse, BackendServiceConfig, Certificate, DeletedContext, EffectiveRoutingRule, Gateway, Listener, ProtocolType, ResourceKey, Route,
        TlsType, DEFAULT_ROUTE_HOSTNAME,
    },
    controllers::HostnameMatchFilter,
    Error,
};

use super::{
    converters,
    server::{start_aggregate_server, ServerAction},
};

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

#[derive(TypedBuilder)]
pub struct EnvoyDeployerChannelHandlerService {
    controller_name: String,
    client: Client,
    backend_deploy_request_channel_receiver: Receiver<BackendGatewayEvent>,
    backend_response_channel_sender: Sender<BackendGatewayResponse>,
}

impl EnvoyDeployerChannelHandlerService {
    pub async fn start(&mut self) {
        let (stream_resource_sender, stream_resource_receiver) = mpsc::channel::<ServerAction>(1024);
        let controller_name = self.controller_name.clone();
        let client = self.client.clone();
        let envoy_deployer_service = async move {
            info!("Gateways handler started");
            loop {
                tokio::select! {
                        Some(event) = self.backend_deploy_request_channel_receiver.recv() => {
                            info!("Backend got event {event:#}");
                            match event{
                                BackendGatewayEvent::Changed(ctx) => {
                                    let gateway = &ctx.gateway;
                                    let span = span!(parent: &ctx.span, Level::INFO, "EnvoyDeployerService", event="GatewayChanged", id = %gateway.key());
                                    let _entered = span.enter();
                                    let Resources{listeners, clusters, secrets} = create_resources(gateway);

                                    let maybe_service = deploy_envoy(&controller_name, client.clone(), gateway, &secrets).instrument(span.clone()).await;
                                    if let Ok(service) = maybe_service{
                                        warn!("Service deployed {service:?} listeners {} clusters {}", listeners.len(), clusters.len());

                                        // let cluster1 = resources::create_cluster_with_endpoints("some_cluster1", "127.0.0.1:9080".parse().expect("should work"), 1, false);
                                        // let cluster2 = resources::create_cluster_with_endpoints("some_cluster2", "127.0.0.1:9080".parse().expect("should work"), 1, false);
                                        // let cluster_resource1 = resources::create_cluster_resource(&cluster1);
                                        // let cluster_resource2 = resources::create_cluster_resource(&cluster2);
                                        let _ = stream_resource_sender.send(ServerAction::UpdateClusters{gateway_id: gateway.key().clone(), resources: clusters}).await;


                                        // let listener1 = resources::create_listener("some_listener1", "0.0.0.0:10000".parse().expect("should work"),CodecType::Auto, vec!["*".to_owned()], vec![("some_cluster1".to_owned(), 100)]);
                                        // let listener2 = resources::create_listener("some_listener2", "0.0.0.0:10001".parse().expect("should work"),CodecType::Auto, vec!["*".to_owned()], vec![("some_cluster2".to_owned(),100)]);
                                        // let listener_resource1 = resources::create_listener_resource(listener1);
                                        // let listener_resource2 = resources::create_listener_resource(listener2);


                                        let _ = stream_resource_sender.send(ServerAction::UpdateListeners{gateway_id: gateway.key().clone(), resources: listeners}).await;

                                        //self.update_address_with_polling(&service, ctx, &span).await;
                                    }else{
                                        warn!("Problem {maybe_service:?}");
                                        let _res = self.backend_response_channel_sender.send(BackendGatewayResponse::Processed(gateway.clone())).await;
                                    }
                                }

                                BackendGatewayEvent::Deleted(DeletedContext{ response_sender, gateway, span }) => {
                                    let span = span!(parent: &span, Level::INFO, "EnvoyDeployerService", event="GatewayDeleted", id = %gateway.key());
                                    let _entered = span.enter();
                                    //self.delete_envoy(&gateway).instrument(span.clone()).await;
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
        }
        .boxed();

        let server_socket = "192.168.1.10:50051".parse().expect("Expect this to work");
        let xds_service = start_aggregate_server(server_socket, stream_resource_receiver).boxed();
        futures::future::join_all(vec![envoy_deployer_service, xds_service]).await;
    }
}

async fn deploy_envoy(controller_name: &str, client: Client, gateway: &Gateway, secrets: &[ResourceKey]) -> std::result::Result<Service, kube::Error> {
    debug!("Deploying Envoy {gateway:?}");

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
    let bootstrap_content = if let Ok(bootstrap_content) = dynamic_bootstrap_content(secrets) {
        bootstrap_content
    } else {
        bootstrap_content().to_owned()
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
    let mut cluster_resources = vec![];
    let mut secret_resources = vec![];

    for listener in gateway.listeners() {
        match listener {
            Listener::Http(listener_data) | Listener::Https(listener_data) => {
                let listener_name = &listener_data.config.name;

                let virtual_hosts = generate_virtual_hosts(listener);
                let http_connection_manager = HttpConnectionManager {
                    stat_prefix: listener_name.to_owned(),
                    codec_type: CodecType::Auto.into(),
                    route_specifier: Some(RouteSpecifier::RouteConfig(RouteConfiguration {
                        name: format!("{listener_name}-route"),
                        virtual_hosts,
                        ..Default::default()
                    })),
                    ..Default::default()
                };

                let http_connection_manager_any = converters::AnyTypeConverter::from((
                    "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager".to_owned(),
                    &http_connection_manager,
                ));

                let http_connection_manager_filter = Filter {
                    name: "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager".to_owned(),
                    config_type: Some(envoy_api::envoy::config::listener::v3::filter::ConfigType::TypedConfig(http_connection_manager_any)),
                };

                let (transport_socket, mut secrets) = if let Some(TlsType::Terminate(secrets)) = listener_data.config.tls_type.as_ref() {
                    let secrets: Vec<ResourceKey> = secrets
                        .iter()
                        .cloned()
                        .filter_map(|cert| match cert {
                            common::Certificate::Resolved(resource_key) => Some(resource_key),
                            common::Certificate::NotResolved(_) | common::Certificate::Invalid(_) => None,
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

                    let downstream_context_any = converters::AnyTypeConverter::from((
                        "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager".to_owned(),
                        &downstream_context,
                    ));

                    (
                        Some(TransportSocket {
                            config_type: Some(envoy_api::envoy::config::core::v3::transport_socket::ConfigType::TypedConfig(downstream_context_any)),
                            name: "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager".to_owned(),
                        }),
                        secrets,
                    )
                } else {
                    (None, vec![])
                };

                let envoy_listener = EnvoyListener {
                    name: listener_name.to_owned(),
                    address: Some(SocketAddressFactory::from(listener)),
                    filter_chains: vec![FilterChain {
                        transport_socket,
                        filters: vec![http_connection_manager_filter],
                        ..Default::default()
                    }],

                    ..Default::default()
                };
                listener_resources.push(resources::create_listener_resource(&envoy_listener));

                cluster_resources = generate_clusters(listener).iter().map(resources::create_cluster_resource).collect();
                secret_resources.append(&mut secrets);
            }
            _ => warn!("Not implemenented for other listeners"),
        }
    }
    secret_resources.dedup_by_key(|k| k.name.clone());
    Resources {
        listeners: listener_resources,
        clusters: cluster_resources,
        secrets: secret_resources,
    }
}

enum SocketAddressFactory {}
impl SocketAddressFactory {
    fn from(listener: &Listener) -> envoy_api::envoy::config::core::v3::Address {
        Address {
            address: Some(address::Address::SocketAddress(SocketAddress {
                protocol: i32::from(listener.protocol()),
                address: "0.0.0.0".to_owned(),
                port_specifier: Some(PortSpecifier::PortValue(listener.port().try_into().expect("For time being we expect this to work"))),
                resolver_name: String::new(),
                ipv4_compat: false,
            })),
        }
    }

    fn from_backend(backend: &BackendServiceConfig) -> envoy_api::envoy::config::core::v3::Address {
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
            nanos: val.as_secs().try_into().expect("At the moment we expect this to work"),
            seconds: val.subsec_nanos().into(),
        }
    }
}

fn dynamic_bootstrap_content(secrets: &[ResourceKey]) -> Result<String, Error> {
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

    Ok(TEMPLATES.render("envoy-bootstrap.yaml.tera", &tera_context)?)
}

fn bootstrap_content() -> &'static str {
    r#"
admin:
  address:
    socket_address:
      address: 0.0.0.0
      port_value: 9901
      
dynamic_resources:
  ads_config:
    api_type: GRPC
    grpc_services:
      - envoy_grpc:
          cluster_name: xds_cluster
  cds_config:
    ads: {}
  lds_config:
    ads: {}              

static_resources:
  clusters:
  - name: xds_cluster
    connect_timeout: 0.25s
    type: STATIC
    lb_policy: ROUND_ROBIN
    typed_extension_protocol_options:
      envoy.extensions.upstreams.http.v3.HttpProtocolOptions:
        "@type": type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions
        explicit_http_config:
          http2_protocol_options:
            # Configure an HTTP/2 keep-alive to detect connection issues and reconnect
            # to the admin server if the connection is no longer responsive.
            connection_keepalive:
              interval: 30s
              timeout: 5s
    load_assignment:
      cluster_name: xds_cluster
      endpoints:
      - lb_endpoints:
        - endpoint:
            address:
              socket_address:
                address: 192.168.1.10
                port_value: 50051
    "#
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

fn generate_virtual_hosts(listener: &Listener) -> Vec<VirtualHost> {
    let (resolved_routes, _) = listener.routes();
    let resolved_routes: Vec<_> = resolved_routes
        .into_iter()
        .filter(|r| match r {
            Route::Http(_) => true,
            Route::Grpc(_) => false,
        })
        .collect();

    let potential_hostnames = calculate_potential_hostnames(&resolved_routes, listener.hostname().cloned());
    debug!("generate_virtual_hosts Potential hostnames {potential_hostnames:?}");
    let mut virtual_hosts = vec![];
    for potential_hostname in potential_hostnames {
        let effective_matching_rules = listener
            .effective_matching_rules()
            .into_iter()
            .filter(|&em| {
                let filtered = HostnameMatchFilter::new(&potential_hostname, &em.hostnames).filter();
                debug!("generate_virtual_hosts {filtered} -> {potential_hostname} {:?}", em.hostnames);
                filtered
            })
            .cloned()
            .collect::<Vec<_>>();

        virtual_hosts.push(VirtualHost {
            name: listener.name().to_owned() + "-" + &potential_hostname,
            domains: calculate_effective_hostnames(&resolved_routes, Some(potential_hostname)),
            routes: effective_matching_rules.into_iter().map(EnvoyRoute::from).collect(),
            ..Default::default()
        });
    }
    virtual_hosts
}

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

fn generate_clusters(listener: &Listener) -> Vec<EnvoyCluster> {
    let (resolved_routes, unresolved_routes) = listener.routes();
    let all_routes = resolved_routes.iter().chain(unresolved_routes.iter()).filter(|r| match r {
        Route::Http(_) => true,
        Route::Grpc(_) => false,
    });

    all_routes
        .flat_map(|route| {
            route.routing_rules().iter().flat_map(|rr| {
                rr.backends
                    .iter()
                    .filter(|b| b.weight() > 0)
                    .filter_map(|b| match b {
                        Backend::Resolved(backend_service_config) => Some(backend_service_config),
                        _ => None,
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

                            ..Default::default()
                        },
                    })
            })
        })
        .map(|h| h.cluster)
        .collect()
}

fn calculate_potential_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
    let routes_hostnames = routes.iter().fold(BTreeSet::new(), |mut acc, r| {
        acc.append(&mut r.hostnames().iter().cloned().collect::<BTreeSet<_>>());
        acc
    });

    match (listener_hostname.is_none(), routes_hostnames.is_empty()) {
        (true, false) => Vec::from_iter(routes_hostnames),
        (..) => listener_hostname.map_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()], |hostname| vec![hostname]),
    }
}

fn calculate_effective_hostnames(routes: &[&Route], listener_hostname: Option<String>) -> Vec<String> {
    let routes_hostnames = routes.iter().fold(BTreeSet::new(), |mut acc, r| {
        acc.append(&mut r.hostnames().iter().cloned().collect::<BTreeSet<_>>());
        acc
    });

    match (listener_hostname.is_none(), routes_hostnames.is_empty()) {
        (true, false) => Vec::from_iter(routes_hostnames),
        (..) => listener_hostname.map_or(vec![DEFAULT_ROUTE_HOSTNAME.to_owned()], |hostname| vec![format!("{hostname}:*"), hostname]),
    }
}

impl From<EffectiveRoutingRule> for EnvoyRoute {
    fn from(effective_routing_rule: EffectiveRoutingRule) -> Self {
        let path_specifier = effective_routing_rule.route_matcher.path.clone().and_then(|matcher| {
            let value = matcher.value.clone().map_or("/".to_owned(), |v| if v.len() > 1 { v.trim_end_matches('/').to_owned() } else { v });
            matcher.r#type.map(|t| match t {
                gateway_api::apis::standard::httproutes::HTTPRouteRulesMatchesPathType::Exact => PathSpecifier::Path(value),
                gateway_api::apis::standard::httproutes::HTTPRouteRulesMatchesPathType::PathPrefix => {
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
                gateway_api::apis::standard::httproutes::HTTPRouteRulesMatchesPathType::RegularExpression => PathSpecifier::SafeRegex(RegexMatcher { regex: value, ..Default::default() }),
            })
        });

        let headers = effective_routing_rule.route_matcher.headers.map_or(vec![], |headers| {
            headers
                .iter()
                .map(|header| HeaderMatcher {
                    name: header.name.clone(),
                    header_match_specifier: Some(HeaderMatchSpecifier::ExactMatch(header.value.clone())),
                    ..Default::default()
                })
                .collect()
        });

        let route_match = RouteMatch {
            path_specifier,
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

        let mut cluster_names: Vec<_> = effective_routing_rule
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

        let cluster_action = if cluster_names.len() == 1 {
            let cluster_name = cluster_names.remove(0).name;
            RouteAction {
                cluster_specifier: Some(ClusterSpecifier::Cluster(cluster_name)),
                ..Default::default()
            }
        } else {
            let clusters: Vec<_> = cluster_names;

            RouteAction {
                cluster_specifier: Some(ClusterSpecifier::WeightedClusters(WeightedCluster { clusters, ..Default::default() })),
                ..Default::default()
            }
        };

        let action: Action = if let Some(redirect_action) = effective_routing_rule.redirect_filter.clone().map(|f| RedirectAction {
            host_redirect: f.hostname.unwrap_or_default(),

            response_code: match f.status_code {
                Some(302) => 302,
                Some(303) => 303,
                Some(307) => 307,
                Some(308) => 308,
                Some(301 | _) | None => 301,
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
                "--log-level debug"
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
            Certificate::Resolved(resource_key) => Some(resource_key),
            Certificate::NotResolved(_) | Certificate::Invalid(_) => None,
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
