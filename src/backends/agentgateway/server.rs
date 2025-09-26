use std::{
    collections::BTreeMap,
    fmt::Display,
    net::SocketAddr,
    ops::AddAssign,
    pin::Pin,
    sync::{Arc, Mutex},
};

use agentgateway_api_rs::{
    agentgateway::dev::resource::{Resource, resource::Kind},
    envoy::service::discovery::v3::{
        DeltaDiscoveryRequest, DeltaDiscoveryResponse, DiscoveryRequest, DiscoveryResponse, Node,
        aggregated_discovery_service_server::{AggregatedDiscoveryService, AggregatedDiscoveryServiceServer},
    },
};
use envoy_api_rs::tonic::{IntoStreamingRequest, Response, Status, transport::Server};
use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use lru_time_cache;
use tokio::{
    net::TcpListener,
    sync::mpsc::{self, Receiver},
};
use tokio_stream::{
    Stream, StreamExt,
    wrappers::{ReceiverStream, TcpListenerStream},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    backends::agentgateway::converters::AnyTypeConverter,
    common::{ResourceKey, create_id},
};

pub enum ServerAction {
    UpdateBindings { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
    UpdateListeners { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
    UpdateRoutes { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },

    DeleteBindings { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
    DeleteListeners { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
    DeleteRoutes { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
}

impl Display for ServerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerAction::UpdateBindings { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::UpdateBindings {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),
            ServerAction::UpdateListeners { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::UpdateListeners {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),
            ServerAction::UpdateRoutes { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::UpdateRoutes {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),

            ServerAction::DeleteBindings { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::DeleteBindings {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),
            ServerAction::DeleteListeners { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::DeleteListeners {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),
            ServerAction::DeleteRoutes { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::DeleteRoutes {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AckVersions {
    cluster: u32,
    listener: u32,
}

impl AddAssign<u32> for AckVersions {
    fn add_assign(&mut self, rhs: u32) {
        self.cluster += rhs;
        self.listener += rhs;
    }
}

#[derive(Debug, Clone)]
struct AdsClient {
    sender: mpsc::Sender<Result<DeltaDiscoveryResponse, Status>>,
    ack_versions: AckVersions,
    client_id: SocketAddr,
    gateway_id: Option<String>,
}

impl AdsClient {
    fn new(client_id: SocketAddr, sender: mpsc::Sender<Result<DeltaDiscoveryResponse, Status>>) -> Self {
        Self { sender, client_id, gateway_id: None, ack_versions: AckVersions::default() }
    }

    fn update_from(&mut self, value: &AdsClient) {
        self.ack_versions = value.ack_versions.clone();
        self.gateway_id.clone_from(&value.gateway_id);
    }
    fn versions(&self) -> &AckVersions {
        &self.ack_versions
    }

    fn versions_mut(&mut self) -> &mut AckVersions {
        &mut self.ack_versions
    }

    fn set_gateway_id(&mut self, gateway_id: &str) {
        self.gateway_id = Some(gateway_id.to_owned());
    }
}

#[derive(Debug, Default)]
pub struct ResourcesMapping {
    bindings: BTreeMap<String, Vec<Resource>>,
    listeners: BTreeMap<String, Vec<Resource>>,
    routes: BTreeMap<String, Vec<Resource>>,
}

#[derive(Debug, Clone, Default)]
struct AdsClients {
    ads_clients: Arc<Mutex<Vec<AdsClient>>>,
}

impl AdsClients {
    fn new() -> Self {
        AdsClients::default()
    }

    fn get_clients_by_gateway_id(&self, gateway_id: &str) -> Vec<AdsClient> {
        let clients = self.ads_clients.lock().expect("We expect the lock to work");
        let clients = clients.iter().filter(|client| client.gateway_id == Some(gateway_id.to_owned())).cloned().collect();
        clients
    }

    fn get_client_by_client_id(&self, client_id: SocketAddr) -> Option<AdsClient> {
        let clients = self.ads_clients.lock().expect("We expect the lock to work");
        clients.iter().find(|client| client.client_id == client_id).cloned()
    }

    fn update_client(&self, client: &AdsClient) {
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            local_client.update_from(client);
        } else {
            info!("No client");
        }
    }

    fn replace_client(&self, mut client: AdsClient) {
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            let versions = local_client.versions().clone();
            client.ack_versions = versions;
            info!("Updated client client {client:?}");
            *local_client = client;
        } else {
            info!("Adding client {client:?}");
            clients.push(client);
        }
    }

    fn remove_client(&self, client_id: SocketAddr) {
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        clients.retain(|f| f.client_id != client_id);
    }
}

pub type ResourceAction = ServerAction;

pub struct AggregateServer {
    kube_client: kube::Client,
    ads_channels: Arc<Mutex<ResourcesMapping>>,
    ads_clients: AdsClients,
    nonces: Arc<Mutex<lru_time_cache::LruCache<Uuid, Uuid>>>,
}

#[derive(Debug)]
pub struct AggregateServerService {
    ads_channels: Arc<Mutex<ResourcesMapping>>,
    ads_clients: AdsClients,
    stream_resources_rx: Receiver<ServerAction>,
}

impl AggregateServer {
    fn new(kube_client: kube::Client, ads_channels: Arc<Mutex<ResourcesMapping>>, ads_clients: AdsClients) -> Self {
        Self {
            kube_client,
            ads_channels,
            ads_clients,
            nonces: Arc::new(Mutex::new(lru_time_cache::LruCache::<Uuid, Uuid>::with_expiry_duration_and_capacity(
                std::time::Duration::from_secs(30),
                1000,
            ))),
        }
    }
}
impl AggregateServerService {
    fn new(stream_resources_rx: Receiver<ServerAction>, ads_channels: Arc<Mutex<ResourcesMapping>>, ads_clients: AdsClients) -> Self {
        Self { ads_channels, ads_clients, stream_resources_rx }
    }

    pub async fn start(self) {
        let mut stream_resources_rx = self.stream_resources_rx;
        let ads_channels = self.ads_channels;
        let ads_clients = self.ads_clients;
        loop {
            tokio::select! {
                    Some(event) = stream_resources_rx.recv() => {
                        info!("{event}");
                        match event{
                            ServerAction::UpdateBindings{ gateway_id: gateway_key, resources, ack_version } => {
                                let gateway_id = create_gateway_id(&gateway_key);


                                {
                                    let mut channels = ads_channels.lock().expect("We expect lock to work");
                                    channels.bindings.insert(gateway_id.clone(), resources.clone());
                                };

                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending Bindings discovery response {gateway_id} clients {}", clients.len());
                                for client in &mut clients{
                                    let response = DeltaDiscoveryResponse {
                                        type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                        resources: resources.iter().map(|resource|
                                            agentgateway_api_rs::envoy::service::discovery::v3::Resource{
                                                name:"type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                resource:Some(AnyTypeConverter::from(("type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),resource))),
                                                ..Default::default() }
                                            ).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().cluster = ack_version;
                                    ads_clients.update_client(client);
                                }
                            },
                            ServerAction::UpdateListeners{ gateway_id: gateway_key, resources, ack_version } => {
                                let gateway_id = create_gateway_id(&gateway_key);


                                {
                                    let mut channels = ads_channels.lock().expect("We expect lock to work");
                                    channels.listeners.insert(gateway_id.clone(), resources.clone());

                                };

                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending Listeners discovery response {gateway_id} clients {}", clients.len());
                                for client in &mut clients{
                                    let response = DeltaDiscoveryResponse {
                                        type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                        resources: resources.iter().map(|resource|
                                            agentgateway_api_rs::envoy::service::discovery::v3::Resource{
                                                name:"type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                resource:Some(AnyTypeConverter::from(("type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),resource))),
                                                ..Default::default() }
                                            ).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().listener = ack_version;
                                    ads_clients.update_client(client);
                                }
                            },
                            ServerAction::UpdateRoutes{ gateway_id: gateway_key, resources, ack_version } => {
                                let gateway_id = create_gateway_id(&gateway_key);

                                {
                                    let mut channels = ads_channels.lock().expect("We expect lock to work");
                                    channels.routes.insert(gateway_id.clone(), resources.clone());
                                };

                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending Routes discovery response {gateway_id} clients {}", clients.len());
                                for client in &mut clients{
                                    let response = DeltaDiscoveryResponse {
                                        type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                        resources: resources.iter().map(|resource|
                                            agentgateway_api_rs::envoy::service::discovery::v3::Resource{
                                                name:"type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                resource:Some(AnyTypeConverter::from(("type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),resource))),
                                                ..Default::default() }
                                            ).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().listener = ack_version;
                                    ads_clients.update_client(client);
                                }
                            },
                            ServerAction::DeleteRoutes{ gateway_id: gateway_key, resources, ack_version } |
                            ServerAction::DeleteListeners{ gateway_id: gateway_key, resources, ack_version }|
                            ServerAction::DeleteBindings{ gateway_id: gateway_key, resources, ack_version }
                            => {
                                let gateway_id = create_gateway_id(&gateway_key);

                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Deleting discovery response {gateway_id} clients {}", clients.len());
                                for client in &mut clients{
                                    let response = DeltaDiscoveryResponse {
                                        type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                        removed_resources: resources.iter().filter_map(|resource| match &resource.kind{
                                            Some(kind) => match kind{
                                                Kind::Bind(bind) => Some(bind.key.clone()),
                                                Kind::Listener(listener) => Some(listener.key.clone()),
                                                Kind::Route(route) => Some(route.key.clone()),
                                                Kind::TcpRoute(_) |
                                                Kind::Policy(_) |
                                                Kind::Backend(_) => None,
                                            },
                                            None => None,
                                        }).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),

                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().listener = ack_version;
                                    ads_clients.update_client(client);
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
}

type AggregatedDiscoveryServiceResult<T> = std::result::Result<Response<T>, Status>;

#[agentgateway_api_rs::tonic::async_trait]
impl AggregatedDiscoveryService for AggregateServer {
    type StreamAggregatedResourcesStream = Pin<Box<dyn Stream<Item = std::result::Result<DiscoveryResponse, Status>> + Send>>;

    async fn stream_aggregated_resources(
        &self,
        req: envoy_api_rs::tonic::Request<envoy_api_rs::tonic::Streaming<DiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::StreamAggregatedResourcesStream> {
        info!("AggregateServer::stream_aggregated_resources client connected from: {:?}", req);

        return Err(Status::aborted("AggregateServer::stream_aggregated_resources not supported"));
    }

    type DeltaAggregatedResourcesStream = Pin<Box<dyn Stream<Item = std::result::Result<DeltaDiscoveryResponse, Status>> + Send>>;

    async fn delta_aggregated_resources(
        &self,
        req: envoy_api_rs::tonic::Request<envoy_api_rs::tonic::Streaming<DeltaDiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::DeltaAggregatedResourcesStream> {
        info!("AggregateServer::delta_aggregated_resources client connected from: {:?}", req.remote_addr());
        let Some(client_ip) = req.remote_addr() else {
            return Err(Status::aborted("Invalid remote IP address"));
        };

        let (tx, rx) = mpsc::channel(128);
        let nonces = Arc::clone(&self.nonces);
        let kube_client = self.kube_client.clone();
        let ads_channels = Arc::clone(&self.ads_channels);
        let ads_clients = self.ads_clients.clone();

        let mut incoming_stream = req.into_streaming_request().into_inner();

        self.ads_clients.replace_client(AdsClient::new(client_ip, tx.clone()));
        tokio::spawn(async move {
            while let Some(item) = incoming_stream.next().await {
                match item {
                    Ok(discovery_request) => {
                        if discovery_request.type_url == "type.googleapis.com/agentgateway.dev.resource.Resource" {
                            info!("AggregateServer::delta_aggregated_resources {discovery_request:?}");
                            if let Some(status) = discovery_request.error_detail {
                                warn!("Got error... skipping  {status:?}");
                                continue;
                            }
                            let Some(node) = discovery_request.node.as_ref() else {
                                warn!("Node is empty");
                                continue;
                            };

                            let Ok(gateway_id) = fetch_gateway_id_by_node_id(kube_client.clone(), node).await else {
                                warn!("Node id is invalid {:?}", node.id);
                                continue;
                            };
                            let maybe_nonce = if discovery_request.response_nonce.is_empty() {
                                None
                            } else {
                                let uuid = Uuid::parse_str(&discovery_request.response_nonce);
                                Some(uuid)
                            };

                            let Some(mut ads_client) = ads_clients.get_client_by_client_id(client_ip) else {
                                warn!("Can't find any clients for this ip  {:?}", node.id);
                                continue;
                            };
                            ads_client.set_gateway_id(&gateway_id);
                            ads_clients.update_client(&ads_client);

                            match maybe_nonce {
                                Some(Err(_)) => {
                                    info!("Nonce set but we can't parse it");
                                },
                                Some(Ok(nonce)) => {
                                    debug!(
                                        "AggregateServer::delta_aggregated_resources Got ack/nack for {nonce} {:?}",
                                        discovery_request.error_detail
                                    );
                                },
                                None => {
                                    let nonce = uuid::Uuid::new_v4();
                                    nonces.lock().expect("We do expect this to work").insert(nonce, nonce);
                                    let resources = {
                                        let resource_mappings = ads_channels.lock().expect("We expect the lock to work");
                                        let bindings = resource_mappings.bindings.get(&gateway_id).cloned().unwrap_or_default();
                                        let listeners = resource_mappings.listeners.get(&gateway_id).cloned().unwrap_or_default();
                                        let routes = resource_mappings.listeners.get(&gateway_id).cloned().unwrap_or_default();
                                        bindings.into_iter().chain(listeners.into_iter()).chain(routes.into_iter()).collect::<Vec<_>>()
                                    };

                                    if resources.is_empty() {
                                        let response = DeltaDiscoveryResponse {
                                            type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                            resources: resources
                                                .iter()
                                                .map(|resource| agentgateway_api_rs::envoy::service::discovery::v3::Resource {
                                                    name: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                    resource: Some(AnyTypeConverter::from((
                                                        "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                        resource,
                                                    ))),
                                                    ..Default::default()
                                                })
                                                .collect(),
                                            nonce: uuid::Uuid::new_v4().to_string(),

                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    } else {
                                        ads_clients.update_client(&ads_client);
                                    }
                                },
                            }
                        }
                    },
                    Err(e) => {
                        warn!("AggregateServer::delta_aggregated_resources Discovery request error {:?}", e);
                    },
                }
            }
            info!("AggregateServer::delta_aggregated_resources Server side closed... removing client");
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream) as Self::DeltaAggregatedResourcesStream))
    }
}

pub async fn start_aggregate_server(
    kube_client: kube::Client,
    server_address: SocketAddr,
    stream_resources_rx: Receiver<ResourceAction>,
) -> crate::Result<()> {
    let stream = TcpListenerStream::new(TcpListener::bind(server_address).await?);
    let channels = Arc::new(Mutex::new(ResourcesMapping::default()));
    let ads_clients = AdsClients::new();
    let service = AggregateServerService::new(stream_resources_rx, Arc::clone(&channels), ads_clients.clone());
    let server = AggregateServer::new(kube_client, channels, ads_clients);
    let aggregate_server = AggregatedDiscoveryServiceServer::new(server);
    let server = Server::builder().concurrency_limit_per_connection(256).add_service(aggregate_server).serve_with_incoming(stream).boxed();

    let service = async move {
        service.start().await;
    }
    .boxed();

    let server = async move {
        let _ = server.await;
    }
    .boxed();
    futures::future::join_all(vec![server, service]).await;
    Ok(())
}

fn parse_agengateway_id(id: &str) -> crate::Result<(&str, &str)> {
    //agentgateway~1.1.1.1~agentgateway-one-6776fc578f-n2mms.default~default.svc.cluster.local
    //
    //
    let mut tokens = id.split('~');
    _ = tokens.next();
    let node_id = tokens.next();
    if let Some(node_id) = node_id {
        tokens = node_id.split('.');
        let id = tokens.next().ok_or("No id provided")?;
        let namespace = tokens.next().ok_or("No namespace provided")?;
        Ok((id, namespace))
    } else {
        Err("Invalid node id format".into())
    }
}
async fn fetch_gateway_id_by_node_id(client: kube::Client, node: &Node) -> crate::Result<String> {
    let (id, namespace) = parse_agengateway_id(&node.id)?;
    let api_client: kube::api::Api<Pod> = Api::namespaced(client, namespace);
    if let Ok(pod) = api_client.get(id).await {
        if let Some(labels) = pod.metadata.labels {
            if let Some(gateway_id) = labels.get("app") {
                debug!("create_gateway_id_from_node_id:: Node id {id} {gateway_id:?}");
                return Ok(create_id(gateway_id, namespace));
            }
        }
    }
    Err("Can't get pod with id".into())
}

fn create_gateway_id(gateway_id: &ResourceKey) -> String {
    create_id(&gateway_id.name, &gateway_id.namespace)
}
