use std::{
    net::SocketAddr,
    sync::{Arc, LazyLock, Mutex},
};

use envoy_api_rs::tonic::transport::Server;
use futures::FutureExt;
use kube::Client;
use tera::Tera;
use tokio::{
    net::TcpListener,
    sync::mpsc::{self, Receiver, Sender},
};
use tokio_stream::wrappers::TcpListenerStream;
use tracing::{info, warn};
use typed_builder::TypedBuilder;

use crate::{
    backends::agentgateway::{
        resource_generator::ResourceGenerator,
        server::{AggregateServer, AggregateServerService, ServerAction, start_aggregate_server},
    },
    common::{BackendGatewayEvent, BackendGatewayResponse, ControlPlaneConfig, Gateway, ResourceKey},
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
                                    let resource_generator = ResourceGenerator::new(gateway);
                                    let bindings_and_listeners = resource_generator.generate_bindings_and_listeners();
                                    let routes = resource_generator.generate_routes();
                                    let _res = self.backend_response_channel_sender.send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                }

                                BackendGatewayEvent::Deleted(boxed) => {
                                    let response_sender = boxed.response_sender;
                                    let gateway  = boxed.gateway;
                                    info!("AgentgatewayDeployerChannelHandlerService GatewayDeleted {}",gateway.key());
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
        futures::future::join_all(vec![agentgateway_deployer_service, xds_service]).await;
        Ok(())
    }
}

use agentgateway_api_rs::envoy::service::discovery::v3::Resource as AgentgatewayResource;

#[derive(Debug, Default)]
struct Resources {
    listeners: Vec<AgentgatewayResource>,
    clusters: Vec<AgentgatewayResource>,
    secrets: Vec<ResourceKey>,
}

fn create_resources(gateway: &Gateway) -> Resources {
    let mut listener_resources = vec![];
    let mut secret_resources = vec![];
    let mut resource_generator = super::resource_generator::ResourceGenerator::new(gateway);

    //let listeners = resource_generator.generate_listeners().values();

    Resources { listeners: listener_resources, clusters: vec![], secrets: secret_resources }
}
