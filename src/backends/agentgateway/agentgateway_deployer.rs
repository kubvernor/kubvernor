use std::sync::LazyLock;

use futures::FutureExt;
use kube::Client;
use tera::Tera;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{info, warn};
use typed_builder::TypedBuilder;

use crate::common::{BackendGatewayEvent, BackendGatewayResponse, ControlPlaneConfig};

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
    pub async fn start(&mut self) -> crate::Result<()> {
        let control_plane_config = self.control_plane_config.clone();
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

        futures::future::join_all(vec![agentgateway_deployer_service]).await;
        Ok(())
    }
}
