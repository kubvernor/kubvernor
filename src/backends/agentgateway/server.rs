use std::{
    collections::HashMap,
    fmt::Display,
    net::SocketAddr,
    ops::AddAssign,
    pin::Pin,
    sync::{Arc, Mutex},
};

use agentgateway_api_rs::{
    agentgateway::dev::{
        resource::{Resource, resource::Kind},
        workload::{self, Address},
    },
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
    UpdateResources { gateway_id: ResourceKey, resources: Vec<Resource> },
    UpdateWorkloads { gateway_id: ResourceKey, workloads: Vec<workload::Address> },
}

fn print_resource(res: &Resource) -> String {
    match res.kind.as_ref() {
        Some(kind) => match kind {
            Kind::Bind(bind) => format!("Resource: Bind key={}", bind.key),
            Kind::Listener(listener) => format!("Resource: Listener bind_key={} listener_key={} ", listener.bind_key, listener.key),
            Kind::Route(route) => format!("Resource: Route listener_key={} route_key={} {route:?}", route.listener_key, route.key),
            Kind::Backend(backend) => format!("Resource: Backend backend_name={} {backend:?}", backend.name),
            Kind::Policy(policy) => format!("Resource: Policy policy_name={}", policy.name),
            Kind::TcpRoute(tcp_route) => {
                format!("Resource: TCPRoute listener_name={} tcp_route_key={}", tcp_route.listener_key, tcp_route.key)
            },
        },
        None => "Unknown".to_owned(),
    }
}

impl Display for ServerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerAction::UpdateResources { gateway_id, resources } => {
                write!(
                    f,
                    "ServerAction::UpdateResources {{gateway_id: {gateway_id}, resources: {:?} }}",
                    resources.iter().map(print_resource).collect::<Vec<_>>(),
                )
            },

            ServerAction::UpdateWorkloads { gateway_id, workloads } => {
                write!(f, "ServerAction::UpdateAddresses {{gateway_id: {gateway_id}, workloads: {} }}", workloads.len(),)
            },
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
    resources: Vec<Resource>,
    workloads: Vec<workload::Address>,
}

struct Delta<T> {
    to_add: Vec<T>,
    to_remove: Vec<String>,
}

impl<T> From<(Vec<T>, Vec<String>)> for Delta<T> {
    fn from((to_add, to_remove): (Vec<T>, Vec<String>)) -> Self {
        Delta { to_add, to_remove }
    }
}

impl AdsClient {
    fn new(client_id: SocketAddr, sender: mpsc::Sender<Result<DeltaDiscoveryResponse, Status>>) -> Self {
        Self { sender, client_id, gateway_id: None, ack_versions: AckVersions::default(), resources: vec![], workloads: vec![] }
    }

    fn update_from(&mut self, value: &AdsClient) {
        self.ack_versions = value.ack_versions.clone();
        self.gateway_id.clone_from(&value.gateway_id);
        self.resources.clone_from(&value.resources);
        self.workloads.clone_from(&value.workloads);
    }
    fn versions(&self) -> &AckVersions {
        &self.ack_versions
    }

    fn set_gateway_id(&mut self, gateway_id: &str) {
        self.gateway_id = Some(gateway_id.to_owned());
    }

    fn cache_resources_and_calculate_delta(&mut self, new_resources: Vec<Resource>) -> Delta<Resource> {
        let cached_resources = &self.resources;
        let to_add = difference(&new_resources, cached_resources);
        let to_remove = difference(cached_resources, &new_resources);

        let to_remove = to_remove
            .into_iter()
            .filter_map(|r| {
                r.kind.map(|kind| match kind {
                    Kind::Bind(bind) => "bind/".to_owned() + &bind.key,
                    Kind::Listener(listener) => "listener/".to_owned() + &listener.key,
                    Kind::Route(route) => "route/".to_owned() + &route.key,
                    Kind::Backend(backend) => "backend/".to_owned() + &backend.name,
                    Kind::Policy(policy) => "policy/".to_owned() + &policy.name,
                    Kind::TcpRoute(tcp_route) => "tcproute/".to_owned() + &tcp_route.key,
                })
            })
            .collect();

        self.resources = new_resources;
        Delta { to_add, to_remove }
    }

    fn cache_workloads_and_calculate_delta(&mut self, new_workloads: Vec<Address>) -> Delta<Address> {
        let cached_workloads = &self.workloads;
        let to_add = difference(&new_workloads, cached_workloads);
        let to_remove = difference(cached_workloads, &new_workloads);

        let to_remove = to_remove
            .into_iter()
            .filter_map(|r| {
                r.r#type.map(|workloads_type| match workloads_type {
                    agentgateway_api_rs::agentgateway::dev::workload::address::Type::Workload(workload) => {
                        "address/".to_owned() + &workload.uid
                    },
                    agentgateway_api_rs::agentgateway::dev::workload::address::Type::Service(service) => {
                        format!("address/{}/{}", service.name, service.namespace)
                    },
                })
            })
            .collect();

        self.workloads = new_workloads;
        Delta { to_add, to_remove }
    }
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

#[derive(Debug, Clone, Default)]
struct ManagedResources {
    all_resources: Vec<Resource>,
    all_workloads: Vec<Address>,
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
            info!("update_client {:?} {gateway_id}  Initial connection - Uupdating all resources", client.client_id);
            client.set_gateway_id(gateway_id);

            {
                if let Some(resources) = self.managed_resources.lock().expect("We expect the lock to work").get(gateway_id) {
                    debug!("update_client {:?} {gateway_id} - Updating resources {:?}", client.client_id, resources);
                    client.resources.clone_from(&resources.all_resources);
                    client.workloads.clone_from(&resources.all_workloads);
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

    fn update_managed_resources(&self, gateway_id: &str, resources: &[Resource]) {
        debug!("update_managed_resources {gateway_id}");
        let mut managed_resources = self.managed_resources.lock().expect("We expect the lock to work");
        managed_resources
            .entry(gateway_id.to_owned())
            .and_modify(|e| e.all_resources.clone_from_slice(resources))
            .or_insert(ManagedResources { all_resources: resources.to_vec(), all_workloads: vec![] });
    }

    fn update_managed_workloads(&self, gateway_id: &str, workloads: &[Address]) {
        debug!("update_managed_workloads {gateway_id}");
        let mut managed_resources = self.managed_resources.lock().expect("We expect the lock to work");
        managed_resources
            .entry(gateway_id.to_owned())
            .and_modify(|e| e.all_workloads.clone_from_slice(workloads))
            .or_insert(ManagedResources { all_resources: vec![], all_workloads: workloads.to_vec() });
    }

    fn update_client_and_resources(&self, client: &AdsClient, resources: &[Resource]) {
        debug!("update_client_and_resources {:?} {:?}", client.client_id, client.gateway_id);
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            local_client.update_from(client);
        } else {
            debug!("update_client_and_resources {:?} {:?} - No client", client.client_id, client.gateway_id);
        }
        let mut managed_resources = self.managed_resources.lock().expect("We expect the lock to work");
        if let Some(gateway_id) = client.gateway_id.as_ref() {
            managed_resources
                .entry(gateway_id.clone())
                .and_modify(|m| m.all_resources.clone_from_slice(resources))
                .or_insert(ManagedResources { all_resources: resources.to_vec(), all_workloads: vec![] });
        }
    }

    fn update_client_and_workloads(&self, client: &AdsClient, workloads: &[Address]) {
        debug!("update_client_and_workloads {:?} {:?}", client.client_id, client.gateway_id);
        let mut clients = self.ads_clients.lock().expect("We expect the lock to work");
        if let Some(local_client) = clients.iter_mut().find(|c| c.client_id == client.client_id) {
            local_client.update_from(client);
        } else {
            debug!("update_client_and_workloads {:?} {:?} - No client", client.client_id, client.gateway_id);
        }
        let mut managed_resources = self.managed_resources.lock().expect("We expect the lock to work");
        if let Some(gateway_id) = client.gateway_id.as_ref() {
            managed_resources
                .entry(gateway_id.clone())
                .and_modify(|m| m.all_workloads.clone_from_slice(workloads))
                .or_insert(ManagedResources { all_resources: vec![], all_workloads: workloads.to_vec() });
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
        if let Some(client) = clients.iter().find(|client| client.client_id == client_id).as_ref()
            && let Some(gateway_id) = client.gateway_id.as_ref()
        {
            debug!("remove_client : Managed resources {:?} {:?}", client_id, gateway_id);
            self.managed_resources.lock().expect("We expect the lock to work").remove(gateway_id);
        }

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

    #[allow(clippy::too_many_lines)]
    pub async fn start(self) {
        let mut stream_resources_rx = self.stream_resources_rx;
        let ads_clients = self.ads_clients;
        loop {
            tokio::select! {
                    Some(event) = stream_resources_rx.recv() => {
                        info!("AggregateServerService :: {event}");
                        match event{
                            ServerAction::UpdateResources{ gateway_id: gateway_key, resources } => {
                                let gateway_id = create_gateway_id(&gateway_key);
                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending resources DELTA discovery response {gateway_id} clients {}", clients.len());
                                ads_clients.update_managed_resources(&gateway_id, &resources);

                                for client in &mut clients{
                                    let Delta{to_add, to_remove} = client.cache_resources_and_calculate_delta(resources.clone());
                                    debug!("Sending resources DELTA discovery response for client {} {to_add:?} {to_remove:?}", client.client_id);
                                    let response = DeltaDiscoveryResponse {
                                        type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                        resources: to_add.into_iter().map(|resource|
                                            agentgateway_api_rs::envoy::service::discovery::v3::Resource{
                                                name:"type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                resource:Some(AnyTypeConverter::from(("type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),resource))),
                                                ..Default::default() }
                                            ).collect(),
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        removed_resources: to_remove,
                                        ..Default::default()
                                    };
                                    ads_clients.update_client_and_resources(client, &resources);
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;
                                }
                            },

                            ServerAction::UpdateWorkloads{ gateway_id: gateway_key, workloads } => {
                                let gateway_id = create_gateway_id(&gateway_key);
                                let clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                info!("Sending workloads DELTA discovery response {gateway_id} clients {}", clients.len());

                                let mut clients = ads_clients.get_clients_by_gateway_id(&gateway_id);
                                ads_clients.update_managed_workloads(&gateway_id, &workloads);

                                for client in &mut clients{
                                    let Delta{to_add: to_add_workloads, to_remove: to_remove_workloads} =
                                        client.cache_workloads_and_calculate_delta(workloads.clone());

                                    debug!("Sending workloads DELTA Addresses discovery response for client {} {to_add_workloads:?} {to_remove_workloads:?}", client.client_id);

                                    let resources: Vec<_> = to_add_workloads.into_iter().map(|address|
                                        agentgateway_api_rs::envoy::service::discovery::v3::Resource{
                                            name:"type.googleapis.com/agentgateway.dev.workload.Address".to_owned(),
                                            resource:Some(AnyTypeConverter::from(("type.googleapis.com/agentgateway.dev.workload.Address".to_owned(),
                                            address
                                        ))),..Default::default()})
                                    .collect();

                                    let removed_resources = to_remove_workloads;

                                    let response = DeltaDiscoveryResponse {
                                        type_url: "type.googleapis.com/agentgateway.dev.workload.Address".to_owned(),
                                        resources,
                                        nonce: uuid::Uuid::new_v4().to_string(),
                                        removed_resources,
                                        ..Default::default()
                                    };
                                    let _  = client.sender.send(std::result::Result::<_, Status>::Ok(response)).await;

                                    ads_clients.update_client_and_workloads(client,&workloads);
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
                        if let Some(status) = discovery_request.error_detail {
                            warn!("Got error... skipping  {status:?}");
                            continue;
                        }

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
                            },
                            None => {
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

                                // if ads_client.resources.is_empty() {
                                //     info!(
                                //         "Initial connection from {gateway_id} {} {} - updating all resources",
                                //         ads_client.client_id, client_ip
                                //     );
                                //     ads_clients.update_client_resources(&mut ads_client);
                                // }

                                info!(
                                    "Sending resources INITIAL discovery response {gateway_id} client {} {} {} ",
                                    ads_client.client_id,
                                    ads_client.resources.len(),
                                    ads_client.workloads.len()
                                );

                                match discovery_request.type_url.as_str() {
                                    "type.googleapis.com/agentgateway.dev.resource.Resource" => {
                                        let response = DeltaDiscoveryResponse {
                                            type_url: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                            resources: ads_client
                                                .resources
                                                .iter()
                                                .map(|resource| agentgateway_api_rs::envoy::service::discovery::v3::Resource {
                                                    name: "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                    resource: Some(AnyTypeConverter::from((
                                                        "type.googleapis.com/agentgateway.dev.resource.Resource".to_owned(),
                                                        resource.clone(),
                                                    ))),
                                                    ..Default::default()
                                                })
                                                .collect(),
                                            nonce: uuid::Uuid::new_v4().to_string(),

                                            ..Default::default()
                                        };
                                        let _ = tx.send(std::result::Result::<_, Status>::Ok(response)).await;
                                    },

                                    "type.googleapis.com/agentgateway.dev.workload.Address" => {
                                        info!("Sending workloads INITIAL discovery response {gateway_id} client {}", ads_client.client_id);
                                        let response = DeltaDiscoveryResponse {
                                            type_url: "type.googleapis.com/agentgateway.dev.workload.Address".to_owned(),
                                            resources: ads_client
                                                .workloads
                                                .iter()
                                                .map(|address| agentgateway_api_rs::envoy::service::discovery::v3::Resource {
                                                    name: "type.googleapis.com/agentgateway.dev.workload.Address".to_owned(),
                                                    resource: Some(AnyTypeConverter::from((
                                                        "type.googleapis.com/agentgateway.dev.workload.Address".to_owned(),
                                                        address.clone(),
                                                    ))),
                                                    ..Default::default()
                                                })
                                                .collect(),
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
    let server = Server::builder()
        .concurrency_limit_per_connection(256)
        .accept_http1(true)
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

fn parse_agengateway_id(id: &str) -> crate::Result<(&str, &str)> {
    //agentgateway~1.1.1.1~agentgateway-one-6776fc578f-n2mms.default~default.svc.cluster.local
    //
    //
    let mut tokens = id.split('~');
    _ = tokens.next();
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
    debug!("fetch_gateway_id_by_node_id:: Node id {id} {namespace}");
    let api_client: kube::api::Api<Pod> = Api::namespaced(client, namespace);
    if let Ok(pod) = api_client.get(id).await
        && let Some(labels) = pod.metadata.labels
        && let Some(gateway_id) = labels.get("app")
    {
        return Ok(create_id(gateway_id, namespace));
    }
    Err("Can't get pod with id".into())
}

fn create_gateway_id(gateway_id: &ResourceKey) -> String {
    create_id(&gateway_id.name, &gateway_id.namespace)
}
