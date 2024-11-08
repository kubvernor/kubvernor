use std::{fmt::Debug, sync::Arc, time::Duration};

use clap::Parser;
use futures::FutureExt;
use kube::Client;
use patchers::{GatewayClassPatcher, GatewayPatcher, HttpRoutePatcher, Patcher};
use state::State;
use tokio::{sync::Mutex, time::sleep};
use tracing::info;

pub mod backends;
mod common;
mod controllers;
mod patchers;
mod state;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

use backends::envoy_deployer::EnvoyDeployerChannelHandler;
use controllers::{gateway::GatewayController, gateway_class::GatewayClassController, http_route::HttpRouteController};

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
    //let (gateway_channel_sender, mut gateway_deployer_channel_handler) = GatewayDeployerChannelHandler::new();
    let (envoy_gateway_channel_sender, mut envoy_deployer_channel_handler) = EnvoyDeployerChannelHandler::new(&args.controller_name, client.clone());

    let (mut gateway_patcher, gateway_patcher_channel) = GatewayPatcher::new(client.clone());
    let (mut gateway_class_patcher, gateway_class_patcher_channel) = GatewayClassPatcher::new(client.clone());
    let (mut http_route_patcher, http_route_patcher_channel) = HttpRoutePatcher::new(client.clone());

    let gateway_class_controller = GatewayClassController::new(args.controller_name.clone(), &client, Arc::clone(&state), gateway_class_patcher_channel.clone());
    let gateway_controller = GatewayController::new(
        args.controller_name.clone(),
        envoy_gateway_channel_sender.clone(),
        client.clone(),
        Arc::clone(&state),
        gateway_patcher_channel.clone(),
        gateway_class_patcher_channel,
        http_route_patcher_channel.clone(),
    );

    let http_route_controller = HttpRouteController::new(
        args.controller_name.clone(),
        client,
        envoy_gateway_channel_sender,
        state,
        http_route_patcher_channel,
        gateway_patcher_channel,
    );

    let gateway_patcher = gateway_patcher.start().boxed();
    let gateway_class_patcher = gateway_class_patcher.start().boxed();
    let http_route_patcher = http_route_patcher.start().boxed();
    let gateway_deployer_channel_handler = envoy_deployer_channel_handler.start().boxed();
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
        http_route_controller.get_controller().await;
        info!("Route controller...stopped");
    };
    futures::future::join_all(vec![
        gateway_class_patcher,
        gateway_patcher,
        http_route_patcher,
        gateway_deployer_channel_handler,
        gateway_class_controller_task.boxed(),
        gateway_controller_task.boxed(),
        http_route_controller_task.boxed(),
    ])
    .await;
    info!("Kubvernor stopped");
    Ok(())
}
