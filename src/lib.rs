use std::{fmt::Debug, sync::Arc, time::Duration};

use clap::Parser;
use futures::FutureExt;
use gateway_deployer::GatewayDeployerService;
use kube::Client;
use patchers::{GatewayClassPatcherService, GatewayPatcherService, HttpRoutePatcherService, Patcher};
use reference_resolver::ReferenceResolverService;
use state::State;
use tokio::{
    sync::{
        mpsc::{self},
        Mutex,
    },
    time::sleep,
};
use tracing::info;

pub mod backends;
mod common;
mod controllers;
mod gateway_deployer;
mod patchers;
mod reference_resolver;
mod state;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

use backends::envoy_deployer::EnvoyDeployerChannelHandlerService;
use controllers::{
    gateway::{self, GatewayController},
    gateway_class::GatewayClassController,
    http_route::HttpRouteController,
};
use typed_builder::TypedBuilder;

const STARTUP_DURATION: Duration = Duration::from_secs(10);

/// Simple program to greet a person
#[derive(Parser, Debug, TypedBuilder)]
pub struct Args {
    controller_name: String,
}

pub async fn start(args: Args) -> Result<()> {
    info!("Kubvernor started");
    let state = Arc::new(Mutex::new(State::new()));
    let client = Client::try_default().await?;
    //let (gateway_channel_sender, mut gateway_deployer_channel_handler) = GatewayDeployerChannelHandler::new();

    let (gateway_deployer_channel_sender, gateway_deployer_channel_receiver) = mpsc::channel(1024);
    let (resolve_references_channel_sender, resolve_referemces_channel_receiver) = mpsc::channel(1024);
    let (gateway_patcher_channel_sender, gateway_patcher_channel_receiver) = mpsc::channel(1024);
    let (gateway_class_patcher_channel_sender, gateway_class_patcher_channel_receiver) = mpsc::channel(1024);
    let (http_route_patcher_channel_sender, http_route_patcher_channel_receiver) = mpsc::channel(1024);
    let (backend_deployer_channel_sender, backend_deployer_channel_receiver) = mpsc::channel(1024);

    let gateway_deployer_service = GatewayDeployerService::builder()
        .client(client.clone())
        .gateway_deployer_channel_receiver(gateway_deployer_channel_receiver)
        .backend_deployer_channel_sender(backend_deployer_channel_sender.clone())
        .state(Arc::clone(&state))
        .gateway_patcher_channel_sender(gateway_patcher_channel_sender.clone())
        .gateway_class_patcher_channel_sender(gateway_class_patcher_channel_sender.clone())
        .http_route_patcher_channel_sender(http_route_patcher_channel_sender.clone())
        .controller_name(args.controller_name.clone())
        .build();

    let resolver_service = ReferenceResolverService::builder()
        .client(client.clone())
        .resolve_channel_receiver(resolve_referemces_channel_receiver)
        .gateway_deployer_channel_sender(gateway_deployer_channel_sender.clone())
        .state(Arc::clone(&state))
        .build();

    let mut gateway_patcher_service = GatewayPatcherService::builder().client(client.clone()).receiver(gateway_patcher_channel_receiver).build();
    let mut gateway_class_patcher_service = GatewayClassPatcherService::builder().client(client.clone()).receiver(gateway_class_patcher_channel_receiver).build();
    let mut http_route_patcher_service = HttpRoutePatcherService::builder().client(client.clone()).receiver(http_route_patcher_channel_receiver).build();

    let mut envoy_deployer_service = EnvoyDeployerChannelHandlerService::builder()
        .client(client.clone())
        .controller_name(args.controller_name.clone())
        .receiver(backend_deployer_channel_receiver)
        .build();

    let gateway_class_controller = GatewayClassController::new(args.controller_name.clone(), &client, Arc::clone(&state), gateway_class_patcher_channel_sender.clone());
    let gateway_controller = GatewayController::builder()
        .ctx(Arc::new(
            gateway::GatewayControllerContext::builder()
                .client(client.clone())
                .controller_name(args.controller_name.clone())
                .gateway_channel_sender(backend_deployer_channel_sender.clone())
                .gateway_class_patcher(gateway_class_patcher_channel_sender)
                .state(Arc::clone(&state))
                .gateway_patcher(gateway_patcher_channel_sender.clone())
                .http_route_patcher(http_route_patcher_channel_sender.clone())
                .gateway_deployer_channel_sender(gateway_deployer_channel_sender.clone())
                .resolve_references_channel_sender(resolve_references_channel_sender)
                .build(),
        ))
        .build();

    let http_route_controller = HttpRouteController::new(
        args.controller_name.clone(),
        client,
        backend_deployer_channel_sender,
        state,
        http_route_patcher_channel_sender,
        gateway_patcher_channel_sender,
    );

    let resolver_service = resolver_service.start().boxed();
    let gateway_deployer_service = gateway_deployer_service.start().boxed();
    let gateway_patcher_service = gateway_patcher_service.start().boxed();
    let gateway_class_patcher_service = gateway_class_patcher_service.start().boxed();
    let http_route_patcher_service = http_route_patcher_service.start().boxed();
    let envoy_deployer_service = envoy_deployer_service.start().boxed();
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
        gateway_deployer_service,
        resolver_service,
        gateway_class_patcher_service,
        gateway_patcher_service,
        http_route_patcher_service,
        envoy_deployer_service,
        gateway_class_controller_task.boxed(),
        gateway_controller_task.boxed(),
        http_route_controller_task.boxed(),
    ])
    .await;
    info!("Kubvernor stopped");
    Ok(())
}
