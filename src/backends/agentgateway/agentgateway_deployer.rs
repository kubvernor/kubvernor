use std::sync::LazyLock;

use futures::FutureExt;
use kube::Client;
use tera::Tera;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::{info, warn};
use typed_builder::TypedBuilder;

use crate::{
    backends::agentgateway::{
        resource_generator::ResourceGenerator,
        server::{ServerAction, start_aggregate_server},
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
                                    let bindings = bindings_and_listeners.keys().cloned().map(|b|Resource{kind: Some(Kind::Bind(Bind{ key: b.key, port: b.port }))}).collect::<Vec<_>>();
                                    let listeners = bindings_and_listeners.values().cloned().flat_map(| listeners | listeners.into_iter()
                                    .map(|listener|
                                        Resource{kind: Some(Kind::Listener(listener))}
                                    )).collect::<Vec<_>>();
                                    let routes = resource_generator.generate_routes().into_iter().map(|r| Resource{kind: Some(Kind::Route(r))}).collect::<Vec<_>>();

                                    let _ = stream_resource_sender.send(ServerAction::UpdateBindings{gateway_id: gateway.key().clone(), resources: bindings,ack_version: 1}).await;
                                    let _ = stream_resource_sender.send(ServerAction::UpdateListeners { gateway_id: gateway.key().clone(), resources: listeners, ack_version: 1}).await;
                                    let _ = stream_resource_sender.send(ServerAction::UpdateRoutes { gateway_id: gateway.key().clone(), resources: routes, ack_version: 1}).await;
                                    let _res = self.backend_response_channel_sender.send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                }

                                BackendGatewayEvent::Deleted(ctx) => {
                                    let response_sender = ctx.response_sender;
                                    let gateway  = &ctx.gateway;
                                    info!("AgentgatewayDeployerChannelHandlerService GatewayDeleted {}",gateway.key());

                                    let resource_generator = ResourceGenerator::new(gateway);
                                    let bindings_and_listeners = resource_generator.generate_bindings_and_listeners();
                                    let bindings = bindings_and_listeners.keys().cloned().map(|b|Resource{kind: Some(Kind::Bind(Bind{ key: b.key, port: b.port }))}).collect::<Vec<_>>();
                                    let listeners = bindings_and_listeners.values().cloned().flat_map(| listeners | listeners.into_iter()
                                    .map(|listener|
                                        Resource{kind: Some(Kind::Listener(listener))}
                                    )).collect::<Vec<_>>();
                                    let routes = resource_generator.generate_routes().into_iter().map(|r| Resource{kind: Some(Kind::Route(r))}).collect::<Vec<_>>();

                                    let _ = stream_resource_sender.send(ServerAction::DeleteRoutes{gateway_id: gateway.key().clone(), resources: routes,ack_version: 1}).await;
                                    let _ = stream_resource_sender.send(ServerAction::DeleteListeners { gateway_id: gateway.key().clone(), resources: listeners, ack_version: 1}).await;
                                    let _ = stream_resource_sender.send(ServerAction::DeleteBindings { gateway_id: gateway.key().clone(), resources: bindings, ack_version: 1}).await;
                                    let _res = self.backend_response_channel_sender.send(BackendGatewayResponse::Processed(Box::new(gateway.clone()))).await;
                                    let _ = response_sender.send(BackendGatewayResponse::Deleted(vec![]));
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

use agentgateway_api_rs::{
    agentgateway::dev::resource::{Bind, Resource, resource::Kind},
    envoy::service::discovery::v3::Resource as AgentgatewayResource,
};

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
