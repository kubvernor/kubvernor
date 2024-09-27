use std::{fmt::Debug, sync::Arc, time::Duration};

use clap::Parser;
use futures::FutureExt;
use kube::Client;
use patchers::GatewayPatcher;
use state::State;
use tokio::{sync::Mutex, time::sleep};
use tracing::info;

pub mod backends;
mod controllers;
mod patchers;
pub mod route;
mod state;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

use backends::gateway_deployer::GatewayDeployerChannelHandler;
use controllers::{
    gateway::GatewayController, gateway_class::GatewayClassController,
    http_route::HttpRouteController,
};

const STARTUP_DURATION: Duration = Duration::from_secs(10);

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(short, long)]
    controller_name: String,
}

pub async fn start(args: Args) -> Result<()> {
    info!("Kubvernor started");
    let state = Arc::new(Mutex::new(State::new()));
    let client = Client::try_default().await?;
    let (gateway_channel_sender, mut gateway_deployer_channel_handler) =
        GatewayDeployerChannelHandler::new();

    let (gateway_patcher, sender) = GatewayPatcher::new(client.clone());

    let gateway_class_controller = GatewayClassController::new(
        args.controller_name.clone(),
        client.clone(),
        Arc::clone(&state),
    );
    let gateway_controller = GatewayController::new(
        args.controller_name.clone(),
        gateway_channel_sender.clone(),
        client.clone(),
        Arc::clone(&state),
        sender,
    );

    let http_roue_controller = HttpRouteController::new(
        args.controller_name.clone(),
        client,
        gateway_channel_sender,
        state,
    );
    let gateway_patcher = gateway_patcher.start().boxed();
    let gateway_deployer_channel_handler = gateway_deployer_channel_handler.start().boxed();
    let gateway_class_controller_task = async move {
        info!("Gateway Class controller...started");
        gateway_class_controller.get_controller().await;
        info!("Gateway Class controller...stopped");
    };
    let gateway_controller_task = async move {
        sleep(STARTUP_DURATION).await;
        info!("Gateway controller...started");
        gateway_controller.get_controller().await;
        info!("Gateway controller...stopped");
    };

    let http_route_controller_task = async move {
        sleep(2 * STARTUP_DURATION).await;
        info!("Route controller...started");
        http_roue_controller.get_controller().await;
        info!("Route controller...stopped");
    };
    futures::future::join_all(vec![
        gateway_patcher,
        gateway_deployer_channel_handler,
        gateway_class_controller_task.boxed(),
        gateway_controller_task.boxed(),
        http_route_controller_task.boxed(),
    ])
    .await;
    info!("Kubvernor stopped");
    Ok(())
}
