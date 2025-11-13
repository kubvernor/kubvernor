use std::{
    collections::BTreeMap,
    fmt::Display,
    net::SocketAddr,
    ops::AddAssign,
    pin::Pin,
    sync::{Arc, Mutex},
};

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
use tokio::{
    net::TcpListener,
    sync::mpsc::{self, Receiver},
};
use tokio_stream::{
    Stream, StreamExt,
    wrappers::{ReceiverStream, TcpListenerStream},
};
use tracing::{debug, info, warn};

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
struct ResourcesMapping {
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

    fn add_or_replace_client(&self, mut client: AdsClient) {
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
}

#[derive(Debug)]
pub struct AggregateServerService {
    ads_channels: Arc<Mutex<ResourcesMapping>>,
    ads_clients: AdsClients,
    stream_resources_rx: Receiver<ServerAction>,
}

impl AggregateServer {
    fn new(kube_client: kube::Client, ads_channels: Arc<Mutex<ResourcesMapping>>, ads_clients: AdsClients) -> Self {
        Self { kube_client, ads_channels, ads_clients }
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
    type StreamAggregatedResourcesStream = Pin<Box<dyn Stream<Item = std::result::Result<DiscoveryResponse, Status>> + Send>>;

    async fn stream_aggregated_resources(
        &self,
        req: envoy_api_rs::tonic::Request<envoy_api_rs::tonic::Streaming<DiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::StreamAggregatedResourcesStream> {
        info!("AggregateServer::stream_aggregated_resources client connected from: {:?}", req);

        let Some(client_ip) = req.remote_addr() else {
            return Err(Status::aborted("Invalid remote IP address"));
        };

        let (tx, rx) = mpsc::channel(128);
        let kube_client = self.kube_client.clone();
        let ads_channels = Arc::clone(&self.ads_channels);
        self.ads_clients.add_or_replace_client(AdsClient::new(client_ip, tx.clone()));

        let ads_clients = self.ads_clients.clone();
        let mut incoming_stream = req.into_streaming_request().into_inner();

        tokio::spawn(async move {
            let tx = tx.clone();
            while let Some(item) = incoming_stream.next().await {
                if let Ok(request) = item {
                    if let Some(status) = request.error_detail {
                        warn!("Got error... skipping  {status:?}");
                        continue;
                    }
                    let Some(node) = request.node.as_ref() else {
                        warn!("Node is empty");
                        continue;
                    };

                    let Some(gateway_id) = fetch_gateway_id_by_node_id(kube_client.clone(), node).await else {
                        warn!("Node id is invalid {:?}", node.id);
                        continue;
                    };

                    let first_connect = request.version_info.is_empty();

                    let incoming_version = if first_connect {
                        0
                    } else {
                        let Ok(incoming_version): std::result::Result<u32, _> = request.version_info.parse() else {
                            warn!("Version is invalid {}", request.version_info);
                            continue;
                        };
                        incoming_version
                    };

                    let Some(mut ads_client) = ads_clients.get_client_by_client_id(client_ip) else {
                        warn!("Can't find any clients for this ip  {:?}", node.id);
                        continue;
                    };
                    ads_client.set_gateway_id(&gateway_id);
                    ads_clients.update_client(&ads_client);
                    info!(
                        "Server : Got discovery request {:?} incoming version {} current_versions {:?} first connect {} from gateway id {gateway_id} {}",
                        client_ip, incoming_version, ads_client.ack_versions, first_connect, request.type_url,
                    );

                    match TypeUrl::try_from(request.type_url.as_str()) {
                        Ok(TypeUrl::Cluster) => {
                            let ack_version = ads_client.versions().cluster;
                            if !first_connect && incoming_version == ack_version {
                                debug!("Versions match... ignoring");
                            } else {
                                let resources = {
                                    let resource_mappings = ads_channels.lock().expect("We expect the lock to work");
                                    resource_mappings.cluster.get(&gateway_id).cloned()
                                };

                                info!(
                                    "Adding tx for cluster gateway id {gateway_id} ack version {ack_version} resources {:?}",
                                    resources.as_ref().map(std::vec::Vec::len)
                                );

                                if let Some(resources) = resources {
                                    if !resources.is_empty() {
                                        ads_clients.update_client(&ads_client);
                                    }
                                    let ack_version = ads_client.ack_versions.cluster;
                                    let response = DiscoveryResponse {
                                        type_url: TypeUrl::Cluster.to_string(),
                                        resources: resources
                                            .iter()
                                            .map(|resource| resource.resource.clone().expect("We would expect this to work"))
                                            .collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        version_info: ack_version.to_string(),

                                        ..Default::default()
                                    };
                                    let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                }
                            }
                        },
                        Ok(TypeUrl::Listener) => {
                            let ack_version = ads_client.versions().listener;
                            if !first_connect && incoming_version == ack_version {
                                debug!("Versions match... ignoring");
                            } else {
                                let resources = {
                                    let channels = ads_channels.lock().expect("We expect the lock to work");
                                    channels.listener.get(&gateway_id).cloned()
                                };

                                info!(
                                    "Adding tx for listener gateway id {gateway_id} ack version {ack_version} resources {:?}",
                                    resources.as_ref().map(std::vec::Vec::len)
                                );

                                if let Some(resources) = &resources {
                                    for v in resources {
                                        debug!("Updating resource {} {:?}", v.name, v.resource_name);
                                    }
                                }

                                if let Some(resources) = resources {
                                    if !resources.is_empty() {
                                        ads_clients.update_client(&ads_client);
                                    }
                                    let ack_version = ads_client.ack_versions.listener;

                                    let response = DiscoveryResponse {
                                        type_url: TypeUrl::Listener.to_string(),
                                        resources: resources
                                            .iter()
                                            .map(|resource| resource.resource.clone().expect("We expect this to work"))
                                            .collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        version_info: ack_version.to_string(),

                                        ..Default::default()
                                    };
                                    let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                }
                            }
                        },
                        _ => {
                            warn!("Unknown resource type");
                        },
                    }
                } else {
                    warn!("Discovery request error {:?}", item.err());
                }
            }

            info!("Server side closed... removing client {client_ip}");
            ads_clients.remove_client(client_ip);
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream) as Self::StreamAggregatedResourcesStream))
    }

    type DeltaAggregatedResourcesStream = Pin<Box<dyn Stream<Item = std::result::Result<DeltaDiscoveryResponse, Status>> + Send>>;

    async fn delta_aggregated_resources(
        &self,
        req: envoy_api_rs::tonic::Request<envoy_api_rs::tonic::Streaming<DeltaDiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::DeltaAggregatedResourcesStream> {
        info!("AggregateServer::delta_aggregated_resources");
        info!("client connected from: {:?}", req.remote_addr());

        Err(Status::unimplemented("Delta stream not implemented at the moment"))
    }
}

pub async fn start_aggregate_server(
    kube_client: kube::Client,
    server_address: crate::Address,
    stream_resources_rx: Receiver<ResourceAction>,
) -> crate::Result<()> {
    let stream = TcpListenerStream::new(TcpListener::bind(server_address.to_ips().as_slice()).await?);
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
