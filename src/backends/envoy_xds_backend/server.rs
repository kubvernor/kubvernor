use envoy_api::envoy::config::core::v3::Node as EnvoyNode;
use futures::FutureExt;

use std::{
    fmt::Display,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
};

use envoy_api::envoy::service::discovery::v3::{
    aggregated_discovery_service_server::{AggregatedDiscoveryService, AggregatedDiscoveryServiceServer},
    DeltaDiscoveryRequest, DeltaDiscoveryResponse, DiscoveryRequest, DiscoveryResponse, Resource,
};
use envoy_api::tonic::{transport::Server, IntoStreamingRequest, Response, Status};

use multimap::MultiMap;
use tokio::{
    net::TcpListener,
    sync::mpsc::{self, Receiver},
};
use tokio_stream::{
    wrappers::{ReceiverStream, TcpListenerStream},
    Stream, StreamExt,
};
use tracing::{debug, info, warn};

use crate::{
    backends::envoy_xds_backend::model::TypeUrl,
    common::{create_id, ResourceKey},
};

pub enum ServerAction {
    UpdateClusters { gateway_id: ResourceKey, resources: Vec<Resource> },
    UpdateListeners { gateway_id: ResourceKey, resources: Vec<Resource> },
}

impl Display for ServerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerAction::UpdateClusters { gateway_id, resources } => write!(f, "ServerAction::UpdateClusters {{gateway_id: {gateway_id}, resources: {} }}", resources.len()),
            ServerAction::UpdateListeners { gateway_id, resources } => write!(f, "ServerAction::UpdateListeners {{gateway_id: {gateway_id}, resources: {} }}", resources.len()),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AckVersions {
    cluster: u32,
    listener: u32,
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
        Self {
            sender,
            client_id,
            gateway_id: None,
            ack_versions: AckVersions::default(),
        }
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
    cluster: MultiMap<String, Vec<Resource>>,
    listener: MultiMap<String, Vec<Resource>>,
    // route: MultiMap<String, Resource>,
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
#[derive(Debug)]
pub struct AggregateServer {
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
    fn new(ads_channels: Arc<Mutex<ResourcesMapping>>, ads_clients: AdsClients) -> Self {
        Self { ads_channels, ads_clients }
    }
}
impl AggregateServerService {
    fn new(stream_resources_rx: Receiver<ServerAction>, ads_channels: Arc<Mutex<ResourcesMapping>>, ads_clients: AdsClients) -> Self {
        Self {
            ads_channels,
            ads_clients,
            stream_resources_rx,
        }
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
                            ServerAction::UpdateClusters{ gateway_id: gateway_key, resources } => {
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
                                        version_info: client.versions().cluster.to_string(),
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().cluster+=1;
                                    ads_clients.update_client(client);
                                }
                            },
                            ServerAction::UpdateListeners{ gateway_id: gateway_key, resources } => {
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
                                        version_info: client.versions().listener.to_string(),
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    client.versions_mut().listener+=1;
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

#[envoy_api::tonic::async_trait]
impl AggregatedDiscoveryService for AggregateServer {
    type StreamAggregatedResourcesStream = Pin<Box<dyn Stream<Item = std::result::Result<DiscoveryResponse, Status>> + Send>>;

    async fn stream_aggregated_resources(
        &self,
        req: envoy_api::tonic::Request<envoy_api::tonic::Streaming<DiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::StreamAggregatedResourcesStream> {
        info!("AggregateServer::stream_aggregated_resources client connected from: {:?}", req);

        let Some(client_ip) = req.remote_addr() else {
            return Err(Status::aborted("Invalid remote IP address"));
        };

        let (tx, rx) = mpsc::channel(128);

        let ads_channels = Arc::clone(&self.ads_channels);
        self.ads_clients.replace_client(AdsClient::new(client_ip, tx.clone()));

        let ads_clients = self.ads_clients.clone();
        let mut incoming_stream = req.into_streaming_request().into_inner();

        tokio::spawn(async move {
            let tx = tx.clone();

            while let Some(item) = incoming_stream.next().await {
                if let Ok(request) = item {
                    if let Some(status) = request.error_detail {
                        warn!("Got error... skipping  {status:?}");
                        // if status.code == 13 {
                        //     info!("Got warning... {status:?}");
                        // } else {
                        //     warn!("Got error... skipping  {status:?}");
                        //     continue;
                        // }
                        continue;
                    }
                    let Some(node) = request.node.as_ref() else {
                        warn!("Node is empty");
                        continue;
                    };
                    let Some(gateway_id) = create_gateway_id_from_node_id(node) else {
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
                                    let channels = ads_channels.lock().expect("We expect the lock to work");
                                    channels.cluster.get_vec(&gateway_id).cloned()
                                };

                                info!(
                                    "Adding tx for cluster gateway id {gateway_id} ack version {ack_version} resources {:?}",
                                    resources.as_ref().map(std::vec::Vec::len)
                                );

                                if let Some(resources) = resources {
                                    if !resources.is_empty() {
                                        ads_client.versions_mut().cluster += 1;
                                        ads_clients.update_client(&ads_client);
                                    }
                                    let ack_version = ads_client.ack_versions.cluster;
                                    for any_channels in resources {
                                        let response = DiscoveryResponse {
                                            type_url: TypeUrl::Cluster.to_string(),
                                            resources: any_channels.iter().map(|resource| resource.resource.clone().expect("We would expect this to work")).collect(),
                                            nonce: uuid::Uuid::new_v4().to_string(),
                                            version_info: ack_version.to_string(),

                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    }
                                };
                            }
                        }
                        Ok(TypeUrl::Listener) => {
                            let ack_version = ads_client.versions().listener;
                            if !first_connect && incoming_version == ack_version {
                                debug!("Versions match... ignoring");
                            } else {
                                let resources = {
                                    let channels = ads_channels.lock().expect("We expect the lock to work");
                                    channels.listener.get_vec(&gateway_id).cloned()
                                };

                                info!(
                                    "Adding tx for listener gateway id {gateway_id} ack version {ack_version} resources {:?}",
                                    resources.as_ref().map(std::vec::Vec::len)
                                );

                                if let Some(resources) = resources {
                                    if !resources.is_empty() {
                                        ads_client.versions_mut().listener += 1;
                                        ads_clients.update_client(&ads_client);
                                    }
                                    let ack_version = ads_client.ack_versions.listener;

                                    for any_channels in resources {
                                        let response = DiscoveryResponse {
                                            type_url: TypeUrl::Listener.to_string(),
                                            resources: any_channels.iter().map(|resource| resource.resource.clone().expect("We expect this to work")).collect(),
                                            nonce: uuid::Uuid::new_v4().to_string(),
                                            version_info: ack_version.to_string(),

                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    }
                                };
                            }
                        }
                        _ => {
                            warn!("Unknown resource type");
                        }
                    }
                } else {
                    warn!("Discovery request error {:?}", item.err());
                }
            }

            info!("Server side closed... we should clean up the resources");
            ads_clients.remove_client(client_ip);
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream) as Self::StreamAggregatedResourcesStream))
    }

    type DeltaAggregatedResourcesStream = Pin<Box<dyn Stream<Item = std::result::Result<DeltaDiscoveryResponse, Status>> + Send>>;

    async fn delta_aggregated_resources(
        &self,
        req: envoy_api::tonic::Request<envoy_api::tonic::Streaming<DeltaDiscoveryRequest>>,
    ) -> AggregatedDiscoveryServiceResult<Self::DeltaAggregatedResourcesStream> {
        info!("AggregateServer::delta_aggregated_resources");
        info!("client connected from: {:?}", req.remote_addr());

        Err(Status::unimplemented("Delta stream not implemented at the moment"))
    }
}

pub async fn start_aggregate_server(server_address: SocketAddr, stream_resources_rx: Receiver<ResourceAction>) {
    let stream = TcpListenerStream::new(TcpListener::bind(server_address).await.expect("Expect this to work "));
    let channels = Arc::new(Mutex::new(ResourcesMapping::default()));
    let ads_clients = AdsClients::new();
    let service = AggregateServerService::new(stream_resources_rx, Arc::clone(&channels), ads_clients.clone());
    let server = AggregateServer::new(channels, ads_clients);
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
    //info!("Server exited {server:?}");
    futures::future::join_all(vec![server, service]).await;
}

const POSTFIX_LENGTH: usize = 17;
// for example id is gateway-one-74dd5dbc59-6jmmw
// and postfix is -74dd5dbc59-6jmmw
fn create_gateway_id_from_node_id(node: &EnvoyNode) -> Option<String> {
    let id = &node.id;
    let namespace = &node.cluster;

    (id.len() > POSTFIX_LENGTH).then(|| {
        let len = id.len() - POSTFIX_LENGTH;
        create_id(&id[..len], namespace)
    })
}

fn create_gateway_id(gateway_id: &ResourceKey) -> String {
    create_id(&gateway_id.name, &gateway_id.namespace)
}
