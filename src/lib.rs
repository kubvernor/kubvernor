use std::{fmt::Debug, sync::Arc};

use clap::Parser;
use futures::FutureExt;
use kube::Client;
use log::info;
use state::State;
use tokio::sync::Mutex;

mod controllers;
pub mod listener;
pub mod route;
mod state;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

use controllers::{
    gateway::GatewayController, gateway_class::GatewayClassController,
    http_route::HttpRouteController,
};
use listener::ListenerChannelHandler;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(short, long)]
    controller_name: String,
}

pub async fn start(args: Args) -> Result<()> {
    info!("Kubevernor started");
    let state = Arc::new(Mutex::new(State::new()));
    let client = Client::try_default().await?;
    let (listener_sender, mut listeners) = ListenerChannelHandler::new();
    let gateway_class_controller = GatewayClassController::new(
        args.controller_name.clone(),
        client.clone(),
        Arc::clone(&state),
    );
    let gateway_controller = GatewayController::new(
        args.controller_name.clone(),
        listener_sender,
        client.clone(),
        Arc::clone(&state),
    );

    let http_roue_controller =
        HttpRouteController::new(args.controller_name.clone(), client, state);
    let f0 = listeners.start().boxed();
    let f1 = gateway_class_controller.get_controller();
    let f2 = gateway_controller.get_controller();
    let f3 = http_roue_controller.get_controller();
    futures::future::join_all(vec![f0, f1, f2, f3]).await;
    info!("Kubevernor ended");
    Ok(())
}
