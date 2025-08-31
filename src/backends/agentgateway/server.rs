use envoy_api_rs::{
    envoy::{
        config::core::v3::Node as EnvoyNode,
        service::discovery::v3::{
            DeltaDiscoveryRequest, DeltaDiscoveryResponse, DiscoveryRequest, DiscoveryResponse, Resource,
            aggregated_discovery_service_server::{AggregatedDiscoveryService, AggregatedDiscoveryServiceServer},
        },
    },
    tonic::{IntoStreamingRequest, Response, Status, transport::Server},
};
use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use lru_time_cache;
use std::{
    collections::BTreeMap,
    fmt::Display,
    net::SocketAddr,
    ops::AddAssign,
    pin::Pin,
    sync::{Arc, Mutex},
};
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
    backends::envoy::envoy_xds_backend::model::TypeUrl,
    common::{ResourceKey, create_id},
};

pub enum ServerAction {
    UpdateClusters { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
    UpdateListeners { gateway_id: ResourceKey, resources: Vec<Resource>, ack_version: u32 },
}

impl Display for ServerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerAction::UpdateClusters { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::UpdateClusters {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
                resources.len()
            ),
            ServerAction::UpdateListeners { gateway_id, resources, ack_version } => write!(
                f,
                "ServerAction::UpdateListeners {{gateway_id: {gateway_id}, resources: {}, ack_version: {ack_version} }}",
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

impl AckVersions {
    pub fn inc(&mut self) {
        *self += 1;
    }
    pub fn listener(&self) -> u32 {
        self.listener
    }

    pub fn cluster(&self) -> u32 {
        self.cluster
    }
}

#[derive(Debug, Clone)]
struct AdsClient {
    sender: mpsc::Sender<Result<DiscoveryResponse, Status>>,
    ack_versions: AckVersions,
    client_id: SocketAddr,
    gateway_id: Option<String>,
}

impl AdsClient {
    fn new(client_id: SocketAddr, sender: mpsc::Sender<Result<DiscoveryResponse, Status>>) -> Self {
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
    cluster: BTreeMap<String, Vec<Resource>>,
    listener: BTreeMap<String, Vec<Resource>>,
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
        let clients =
            clients.iter().filter(|client| client.gateway_id == Some(gateway_id.to_owned())).cloned().collect();
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
    fn new(
        stream_resources_rx: Receiver<ServerAction>,
        ads_channels: Arc<Mutex<ResourcesMapping>>,
        ads_clients: AdsClients,
    ) -> Self {
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
                            ServerAction::UpdateClusters{ gateway_id: gateway_key, resources, ack_version } => {
                                let gateway_id = create_gateway_id(&gateway_key);


                                {
                                    let mut channels = ads_channels.lock().expect("We expect lock to work");
                                    channels.cluster.insert(gateway_id.clone(), resources.clone());

                                };

                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending cluster discovery response {gateway_id} clients {}", clients.len());
                                for client in &mut clients{
                                    let response = DiscoveryResponse {
                                        type_url: TypeUrl::Cluster.to_string(),
                                        resources: resources.iter().map(|resource| resource.resource.clone().expect("We expect this to work")).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        version_info: ack_version.to_string(),
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
                                    channels.listener.insert(gateway_id.clone(), resources.clone());

                                };



                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending listener discovery response {gateway_id} clients {}", clients.len());
                                for client in &mut clients{
                                    let response = DiscoveryResponse {
                                        type_url: TypeUrl::Listener.to_string(),
                                        resources: resources.iter().map(|resource| resource.resource.clone().expect("We expect this to work")).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        version_info: ack_version.to_string(),
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().listener = ack_version;
                                    ads_clients.update_client(client);
                                }
                            },

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

#[envoy_api_rs::tonic::async_trait]
impl AggregatedDiscoveryService for AggregateServer {
    type StreamAggregatedResourcesStream =
        Pin<Box<dyn Stream<Item = std::result::Result<DiscoveryResponse, Status>> + Send>>;

    async fn stream_aggregated_resources(
        &self,
        req: envoy_api_rs::tonic::Request<envoy_api_rs::tonic::Streaming<DiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::StreamAggregatedResourcesStream> {
        info!("AggregateServer::stream_aggregated_resources client connected from: {:?}", req);

        return Err(Status::aborted("AggregateServer::stream_aggregated_resources not supported"));
    }

    type DeltaAggregatedResourcesStream =
        Pin<Box<dyn Stream<Item = std::result::Result<DeltaDiscoveryResponse, Status>> + Send>>;

    async fn delta_aggregated_resources(
        &self,
        req: envoy_api_rs::tonic::Request<envoy_api_rs::tonic::Streaming<DeltaDiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::DeltaAggregatedResourcesStream> {
        info!("AggregateServer::delta_aggregated_resources client connected from: {:?}", req.remote_addr());
        let mut incoming_stream = req.into_streaming_request().into_inner();
        let (tx, rx) = mpsc::channel(128);
        let nonces = Arc::clone(&self.nonces);
        tokio::spawn(async move {
            while let Some(item) = incoming_stream.next().await {
                match item {
                    Ok(discovery_request) => {
                        info!("AggregateServer::delta_aggregated_resources {discovery_request:?}");
                        if discovery_request.type_url == "type.googleapis.com/agentgateway.dev.resource.Resource" {
                            let Ok(response_nonce) = Uuid::parse_str(&discovery_request.response_nonce) else {
                                continue;
                            };

                            if nonces.lock().expect("We do expect this to work").contains_key(&response_nonce) {
                                debug!("AggregateServer::delta_aggregated_resources Got ack/nack for {response_nonce}");
                            } else {
                                let nonce = uuid::Uuid::new_v4();
                                nonces.lock().expect("We do expect this to work").insert(nonce, nonce);
                                let response = DeltaDiscoveryResponse {
                                    resources: vec![],
                                    nonce: nonce.to_string(),
                                    type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                    ..Default::default() //, system_version_info: todo!(), removed_resources: todo!(), removed_resource_names: todo!(), control_plane: todo!(), resource_errors: todo!() }
                                };
                                let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
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
    let server = Server::builder()
        .concurrency_limit_per_connection(256)
        .add_service(aggregate_server)
        .serve_with_incoming(stream)
        .boxed();

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

async fn fetch_gateway_id_by_node_id(client: kube::Client, node: &EnvoyNode) -> Option<String> {
    let id = &node.id;
    let namespace = &node.cluster;
    let api_client: kube::api::Api<Pod> = Api::namespaced(client, namespace);
    if let Ok(pod) = api_client.get(id).await {
        if let Some(labels) = pod.metadata.labels {
            if let Some(gateway_id) = labels.get("app") {
                debug!("create_gateway_id_from_node_id:: Node id {id} {gateway_id:?}");
                return Some(create_id(gateway_id, namespace));
            }
        }
    }
    None
}

fn create_gateway_id(gateway_id: &ResourceKey) -> String {
    create_id(&gateway_id.name, &gateway_id.namespace)
}
