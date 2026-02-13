use std::{
    collections::HashMap,
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
use kubvernor_common::ResourceKey;
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

use crate::{backends::envoy::envoy_xds_backend::model::TypeUrl, common::create_id};

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

struct Delta<T> {
    to_add: Vec<T>,
    to_remove: Vec<String>,
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
    sender: mpsc::Sender<Result<DeltaDiscoveryResponse, Status>>,
    ack_versions: AckVersions,
    client_id: SocketAddr,
    gateway_id: Option<String>,
    listeners: Vec<Resource>,
    clusters: Vec<Resource>,
}

impl AdsClient {
    fn new(client_id: SocketAddr, sender: mpsc::Sender<Result<DeltaDiscoveryResponse, Status>>) -> Self {
        Self { sender, client_id, gateway_id: None, ack_versions: AckVersions::default(), listeners: vec![], clusters: vec![] }
    }

    fn update_from(&mut self, value: &AdsClient) {
        self.ack_versions = value.ack_versions.clone();
        self.gateway_id.clone_from(&value.gateway_id);
        self.listeners.clone_from(&value.listeners);
        self.clusters.clone_from(&value.clusters);
    }
    fn versions(&self) -> &AckVersions {
        &self.ack_versions
    }

    fn set_gateway_id(&mut self, gateway_id: &str) {
        self.gateway_id = Some(gateway_id.to_owned());
    }

    fn cache_listeners_and_calculate_delta(&mut self, new_listeners: Vec<Resource>) -> Delta<Resource> {
        let cached_resources = &self.listeners;

        let to_add = difference(&new_listeners, cached_resources);
        let to_remove = difference(
            &cached_resources.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            &new_listeners.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        );
        debug!(
            "cache_listeners_and_calculate_delta cached resources {} {} {} {}",
            cached_resources.len(),
            new_listeners.len(),
            to_add.len(),
            to_remove.len()
        );

        //let to_remove = to_remove.into_iter().map(|r| r.name).collect();

        let delta = Delta { to_add, to_remove: to_remove.into_iter().map(std::borrow::ToOwned::to_owned).collect() };
        self.listeners = new_listeners;
        delta
    }

    fn cache_clusters_and_calculate_delta(&mut self, new_clusters: Vec<Resource>) -> Delta<Resource> {
        let cached_workloads = &self.clusters;
        let to_add = difference(&new_clusters, cached_workloads);
        let to_remove = difference(cached_workloads, &new_clusters);

        let to_remove = to_remove.into_iter().map(|r| r.name).collect();

        self.clusters = new_clusters;
        Delta { to_add, to_remove }
    }
}

#[derive(Debug, Clone, Default)]
struct ManagedResources {
    all_listeners: Vec<Resource>,
    all_clusters: Vec<Resource>,
}

#[derive(Debug, Clone, Default)]
struct AdsClients {
    ads_clients: Arc<Mutex<Vec<AdsClient>>>,
    managed_resources: Arc<Mutex<HashMap<String, ManagedResources>>>,
}

impl AdsClients {
    fn new() -> Self {
        AdsClients::default()
    }

    fn get_clients_by_gateway_id(&self, gateway_id: &str) -> Vec<AdsClient> {
        debug!("get_clients_by_gateway_id {gateway_id}");
        let clients = self.ads_clients.lock().expect("We expect the lock to work");
        clients.iter().filter(|client| client.gateway_id == Some(gateway_id.to_owned())).cloned().collect()
    }

    fn get_client_by_client_id(&self, client_id: SocketAddr) -> Option<AdsClient> {
        debug!("get_client_by_client_id {client_id}");
        let clients = self.ads_clients.lock().expect("We expect the lock to work");
        clients.iter().find(|client| client.client_id == client_id).cloned()
    }

    fn update_client(&self, client: &mut AdsClient, gateway_id: &str) {
        debug!("update_client {:?} {gateway_id}", client.client_id);
        if client.gateway_id.is_none() {
            info!("update_client {:?} {gateway_id}  Initial connection - Updating all resources", client.client_id);
            client.set_gateway_id(gateway_id);

            {
                if let Some(resources) = self.managed_resources.lock().expect("We expect the lock to work").get(gateway_id) {
                    debug!("update_client {:?} {gateway_id} - Updating resources {:?}", client.client_id, resources);
                    client.listeners.clone_from(&resources.all_listeners);
                    client.clusters.clone_from(&resources.all_clusters);
                } else {
                    debug!("update_client {:?} {gateway_id} - No resources", client.client_id);
                }
            }
        }

        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            local_client.update_from(client);
        } else {
            debug!("update_client No client {:?} {gateway_id}", client.client_id);
        }
    }

    fn update_managed_listeners(&self, gateway_id: &str, listeners: &[Resource]) {
        debug!("update_managed_listeners {gateway_id}");
        let mut managed_resources = self.managed_resources.lock().expect("We expect the lock to work");
        managed_resources
            .entry(gateway_id.to_owned())
            .and_modify(|e| e.all_listeners.clone_from(&listeners.to_vec()))
            .or_insert(ManagedResources { all_listeners: listeners.to_vec(), all_clusters: vec![] });
    }

    fn update_managed_clusters(&self, gateway_id: &str, clusters: &[Resource]) {
        debug!("update_managed_clusters {gateway_id}");
        let mut managed_resources = self.managed_resources.lock().expect("We expect the lock to work");
        managed_resources
            .entry(gateway_id.to_owned())
            .and_modify(|e| e.all_clusters.clone_from(&clusters.to_vec()))
            .or_insert(ManagedResources { all_listeners: vec![], all_clusters: clusters.to_vec() });
    }

    fn update_client_and_listeners(&self, client: &AdsClient, listeners: &[Resource]) {
        debug!("update_client_and_listeners {:?} {:?}", client.client_id, client.gateway_id);
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            local_client.update_from(client);
        } else {
            debug!("update_client_and_listeners {:?} {:?} - No client", client.client_id, client.gateway_id);
        }

        if let Some(gateway_id) = client.gateway_id.as_ref() {
            self.update_managed_listeners(gateway_id, listeners);
        }
    }

    fn update_client_and_clusters(&self, client: &AdsClient, clusters: &[Resource]) {
        debug!("update_client_and_clusters {:?} {:?}", client.client_id, client.gateway_id);
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            local_client.update_from(client);
        } else {
            debug!("update_client_and_clusters {:?} {:?} - No client", client.client_id, client.gateway_id);
        }
        if let Some(gateway_id) = client.gateway_id.as_ref() {
            self.update_managed_clusters(gateway_id, clusters);
        }
    }

    fn add_or_replace_client(&self, mut client: AdsClient) {
        debug!("add_or_replace_client {:?} {:?}", client.client_id, client.gateway_id);
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");

        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            let versions = local_client.versions().clone();
            client.ack_versions = versions;
            debug!("add_or_replace_client Updated client client {:?} {:?}", client.client_id, client.gateway_id);
            *local_client = client;
        } else {
            debug!("add_or_replace_client Added client client {:?} {:?}", client.client_id, client.gateway_id);
            clients.push(client);
        }
    }

    fn remove_client(&self, client_id: SocketAddr) {
        debug!("remove_client {:?}", client_id);
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        // if let Some(client) = clients.iter().find(|client| client.client_id == client_id).as_ref()
        //     && let Some(gateway_id) = client.gateway_id.as_ref()
        // {
        //     //debug!("remove_client : Managed resources {:?} {:?}", client_id, gateway_id);
        //     //self.managed_resources.lock().expect("We expect the lock to work").remove(gateway_id);
        // }

        clients.retain(|f| f.client_id != client_id);
    }
}

pub type ResourceAction = ServerAction;

pub struct AggregateServer {
    kube_client: kube::Client,
    ads_clients: AdsClients,
    nonces: Arc<Mutex<lru_time_cache::LruCache<Uuid, Uuid>>>,
}

#[derive(Debug)]
pub struct AggregateServerService {
    ads_clients: AdsClients,
    stream_resources_rx: Receiver<ServerAction>,
}

impl AggregateServer {
    fn new(kube_client: kube::Client, ads_clients: AdsClients) -> Self {
        Self {
            kube_client,
            ads_clients,
            nonces: Arc::new(Mutex::new(lru_time_cache::LruCache::<Uuid, Uuid>::with_expiry_duration_and_capacity(
                std::time::Duration::from_secs(30),
                1000,
            ))),
        }
    }
}
impl AggregateServerService {
    fn new(stream_resources_rx: Receiver<ServerAction>, ads_clients: AdsClients) -> Self {
        Self { ads_clients, stream_resources_rx }
    }

    pub async fn start(self) {
        let mut stream_resources_rx = self.stream_resources_rx;
        let ads_clients = self.ads_clients;
        loop {
            tokio::select! {
                    Some(event) = stream_resources_rx.recv() => {
                        info!("{event}");
                        match event{
                            ServerAction::UpdateClusters{ gateway_id: gateway_key, resources, ack_version: _ } => {
                                let gateway_id = create_gateway_id(&gateway_key);
                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending cluster resources DELTA discovery response {gateway_id} clients {}", clients.len());
                                ads_clients.update_managed_clusters(&gateway_id, &resources);

                                for client in &mut clients{
                                    let Delta{to_add, to_remove} = client.cache_clusters_and_calculate_delta(resources.clone());
                                    debug!("Sending resources DELTA clusters discovery response for client {} {to_add:?} {to_remove:?}", client.client_id);
                                    let response = DeltaDiscoveryResponse {
                                        type_url: TypeUrl::Cluster.to_string(),
                                        resources: to_add.clone(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        removed_resources: to_remove,
                                        ..Default::default()
                                    };
                                    ads_clients.update_client_and_clusters(client, &resources);
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                }
                            },
                            ServerAction::UpdateListeners{ gateway_id: gateway_key, resources, ack_version: _ } => {
                                let gateway_id = create_gateway_id(&gateway_key);
                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending listener resources DELTA discovery response {gateway_id} clients {}", clients.len());
                                ads_clients.update_managed_listeners(&gateway_id, &resources);

                                for client in &mut clients{
                                    let Delta{to_add, to_remove} = client.cache_listeners_and_calculate_delta(resources.clone());
                                    debug!("Sending resources DELTA listeners discovery response for client {} {to_add:?} {to_remove:?}", client.client_id);
                                    let response = DeltaDiscoveryResponse {
                                        type_url: TypeUrl::Listener.to_string(),
                                        resources: to_add.clone(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        removed_resources: to_remove,
                                        ..Default::default()
                                    };
                                    ads_clients.update_client_and_listeners(client, &resources);
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
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

    #[allow(clippy::too_many_lines)]
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
        let ads_clients = self.ads_clients.clone();

        let mut incoming_stream = req.into_streaming_request().into_inner();

        self.ads_clients.add_or_replace_client(AdsClient::new(client_ip, tx.clone()));
        tokio::spawn(async move {
            while let Some(item) = incoming_stream.next().await {
                match item {
                    Ok(discovery_request) => {
                        info!("AggregateServer::delta_aggregated_resources {discovery_request:?}");

                        let maybe_nonce = if discovery_request.response_nonce.is_empty() {
                            None
                        } else {
                            let uuid = Uuid::parse_str(&discovery_request.response_nonce);
                            Some(uuid)
                        };

                        match maybe_nonce {
                            Some(Err(_)) => {
                                info!("Nonce set but we can't parse it");
                            },
                            Some(Ok(nonce)) => {
                                debug!(
                                    "AggregateServer::delta_aggregated_resources Got ack/nack for {nonce} {:?}",
                                    discovery_request.error_detail
                                );
                                if let Some(status) = discovery_request.error_detail {
                                    warn!("Got error... skipping  {status:?}");
                                }
                            },
                            None => {
                                if let Some(status) = discovery_request.error_detail {
                                    warn!("got unsolicited error/no nonce  {status:?}");
                                    continue;
                                }
                                let Some(node) = discovery_request.node.as_ref() else {
                                    warn!("Node is empty");
                                    continue;
                                };

                                let Some(mut ads_client) = ads_clients.get_client_by_client_id(client_ip) else {
                                    warn!("Can't find any clients for this ip {:?}", node.id);
                                    continue;
                                };

                                let maybe_gateway_id = fetch_gateway_id_by_node_id(kube_client.clone(), node).await;
                                let Ok(gateway_id) = maybe_gateway_id else {
                                    warn!("Node id is invalid {:?} {maybe_gateway_id:?}", node.id);
                                    continue;
                                };

                                debug!("Updating client {client_ip} {gateway_id}");
                                ads_clients.update_client(&mut ads_client, &gateway_id);

                                let nonce = uuid::Uuid::new_v4();
                                nonces.lock().expect("We do expect this to work").insert(nonce, nonce);

                                match TypeUrl::try_from(discovery_request.type_url.as_str()) {
                                    Ok(TypeUrl::Listener) => {
                                        info!(
                                            "Sending listeners INITIAL discovery response {gateway_id} client {} {} ",
                                            ads_client.client_id,
                                            ads_client.listeners.len(),
                                        );
                                        let response = DeltaDiscoveryResponse {
                                            type_url: TypeUrl::Listener.to_string(),
                                            resources: ads_client.listeners.clone(),
                                            nonce: uuid::Uuid::new_v4().to_string(),

                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    },

                                    Ok(TypeUrl::Cluster) => {
                                        info!(
                                            "Sending clusters INITIAL discovery response {gateway_id} client {} {}",
                                            ads_client.client_id,
                                            ads_client.clusters.len()
                                        );
                                        let response = DeltaDiscoveryResponse {
                                            type_url: TypeUrl::Cluster.to_string(),
                                            resources: ads_client.clusters.clone(),
                                            nonce: uuid::Uuid::new_v4().to_string(),
                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    },
                                    _ => {
                                        warn!("Unknown resource type {}", discovery_request.type_url);
                                        let response = DeltaDiscoveryResponse {
                                            type_url: discovery_request.type_url.clone(),
                                            resources: vec![],
                                            nonce: uuid::Uuid::new_v4().to_string(),
                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    },
                                }
                            },
                        }
                    },

                    Err(e) => {
                        warn!("AggregateServer::delta_aggregated_resources Discovery request error {:?}", e);
                    },
                }
            }
            info!("AggregateServer::delta_aggregated_resources Server side closed... removing client");
            ads_clients.remove_client(client_ip);
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream) as Self::DeltaAggregatedResourcesStream))
    }
}

pub async fn start_aggregate_server(
    kube_client: kube::Client,
    server_address: crate::Address,
    stream_resources_rx: Receiver<ResourceAction>,
) -> crate::Result<()> {
    let stream = TcpListenerStream::new(TcpListener::bind(server_address.to_ips().as_slice()).await?);
    let ads_clients = AdsClients::new();
    let service = AggregateServerService::new(stream_resources_rx, ads_clients.clone());
    let server = AggregateServer::new(kube_client, ads_clients);
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

async fn fetch_gateway_id_by_node_id(client: kube::Client, node: &EnvoyNode) -> crate::Result<String> {
    let id = &node.id;
    let namespace = &node.cluster;
    let api_client: kube::api::Api<Pod> = Api::namespaced(client, namespace);
    if let Ok(pod) = api_client.get(id).await
        && let Some(labels) = pod.metadata.labels
        && let Some(gateway_id) = labels.get("app")
    {
        debug!("create_gateway_id_from_node_id:: Node id {id} {gateway_id:?}");
        return Ok(create_id(gateway_id, namespace));
    }
    Err("Invalid node id format".into())
}

fn create_gateway_id(gateway_id: &ResourceKey) -> String {
    create_id(&gateway_id.name, &gateway_id.namespace)
}

/// Visits the values representing the difference, i.e., the values that are in self but not in other.
fn difference<T>(this: &[T], other: &[T]) -> Vec<T>
where
    T: PartialEq + Clone,
{
    let mut out = vec![];
    for t in this {
        if !other.contains(t) {
            out.push(t.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::difference;

    #[test]
    fn test_the_difference() {
        let this = vec![1, 2, 3, 4, 5, 6];
        let other = vec![];
        let diff: Vec<i32> = difference(&this, &other);
        assert_eq!(diff, this);

        let this = vec![1, 2, 3, 4];
        let other = vec![1, 2, 3, 4, 5, 6];

        let diff: Vec<i32> = difference(&this, &other);
        assert_eq!(diff, Vec::<i32>::new());

        let this = vec![1, 2, 3, 4, 5, 6];
        let other = vec![1, 2, 3, 4];

        let diff: Vec<i32> = difference(&this, &other);
        assert_eq!(diff, vec![5, 6]);

        let this = vec![1, 2, 3, 4, 5, 6];
        let other = vec![3, 4];

        let diff: Vec<i32> = difference(&this, &other);
        assert_eq!(diff, vec![1, 2, 5, 6]);
    }

    #[test]
    fn test_caching_difference() {
        let this = vec![1];
        let other = vec![1];

        let diff: Vec<i32> = difference(&this, &other);
        assert_eq!(diff, Vec::<i32>::new());
    }
}
