use std::{fmt::Debug, sync::Arc, time::Duration};

use clap::Parser;
use futures::FutureExt;
use kube::Client;
use services::{GatewayClassPatcherService, GatewayDeployerService, GatewayPatcherService, HttpRoutePatcherService, Patcher, ReferenceValidatorService};
use state::State;
use tokio::{
    sync::mpsc::{self},
    time::sleep,
};
use tracing::info;

pub mod backends;
mod common;
mod controllers;

mod services;

mod state;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

use backends::envoy_backend::EnvoyDeployerChannelHandlerService;
use controllers::{
    gateway::{self, GatewayController},
    gateway_class::GatewayClassController,
    http_route::{self, HttpRouteController},
    BackendReferenceResolver, SecretsResolver,
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
    let state = State::new();
    let client = Client::try_default().await?;

    let (gateway_deployer_channel_sender, gateway_deployer_channel_receiver) = mpsc::channel(1024);
    let (reference_validate_channel_sender, reference_validate_channel_receiver) = mpsc::channel(1024);
    let (gateway_patcher_channel_sender, gateway_patcher_channel_receiver) = mpsc::channel(1024);
    let (gateway_class_patcher_channel_sender, gateway_class_patcher_channel_receiver) = mpsc::channel(1024);
    let (http_route_patcher_channel_sender, http_route_patcher_channel_receiver) = mpsc::channel(1024);
    let (backend_deployer_channel_sender, backend_deployer_channel_receiver) = mpsc::channel(1024);
    let (backend_response_channel_sender, backend_response_channel_receiver) = mpsc::channel(1024);

    let secrets_resolver = SecretsResolver::builder().reference_resolver(client.clone(), reference_validate_channel_sender.clone()).build();

    let backend_references_resolver = BackendReferenceResolver::builder()
        .state(state.clone())
        .reference_resolver(client.clone(), reference_validate_channel_sender.clone())
        .build();

    let gateway_deployer_service = GatewayDeployerService::builder()
        .gateway_deployer_channel_receiver(gateway_deployer_channel_receiver)
        .backend_deployer_channel_sender(backend_deployer_channel_sender.clone())
        .backend_response_channel_receiver(backend_response_channel_receiver)
        .state(state.clone())
        .gateway_patcher_channel_sender(gateway_patcher_channel_sender.clone())
        .gateway_class_patcher_channel_sender(gateway_class_patcher_channel_sender.clone())
        .http_route_patcher_channel_sender(http_route_patcher_channel_sender.clone())
        .controller_name(args.controller_name.clone())
        .build();

    let resolver_service = ReferenceValidatorService::builder()
        .client(client.clone())
        .reference_validate_channel_receiver(reference_validate_channel_receiver)
        .gateway_deployer_channel_sender(gateway_deployer_channel_sender.clone())
        .state(state.clone())
        .secrets_resolver(secrets_resolver.clone())
        .backend_references_resolver(backend_references_resolver.clone())
        .build();

    let mut gateway_patcher_service = GatewayPatcherService::builder().client(client.clone()).receiver(gateway_patcher_channel_receiver).build();
    let mut gateway_class_patcher_service = GatewayClassPatcherService::builder().client(client.clone()).receiver(gateway_class_patcher_channel_receiver).build();
    let mut http_route_patcher_service = HttpRoutePatcherService::builder().client(client.clone()).receiver(http_route_patcher_channel_receiver).build();

    let mut envoy_deployer_service = EnvoyDeployerChannelHandlerService::builder()
        .client(client.clone())
        .controller_name(args.controller_name.clone())
        .backend_deploy_request_channel_receiver(backend_deployer_channel_receiver)
        .backend_response_channel_sender(backend_response_channel_sender)
        .build();

    let gateway_class_controller = GatewayClassController::new(args.controller_name.clone(), &client, state.clone(), gateway_class_patcher_channel_sender.clone());
    let gateway_controller = GatewayController::builder()
        .ctx(Arc::new(
            gateway::GatewayControllerContext::builder()
                .client(client.clone())
                .controller_name(args.controller_name.clone())
                .gateway_channel_sender(backend_deployer_channel_sender.clone())
                .gateway_class_patcher(gateway_class_patcher_channel_sender)
                .state(state.clone())
                .gateway_patcher(gateway_patcher_channel_sender.clone())
                .validate_references_channel_sender(reference_validate_channel_sender.clone())
                .build(),
        ))
        .build();

    let http_route_controller = HttpRouteController::builder()
        .ctx(Arc::new(
            http_route::HttpRouteControllerContext::builder()
                .client(client.clone())
                .controller_name(args.controller_name.clone())
                .state(state.clone())
                .http_route_patcher(http_route_patcher_channel_sender.clone())
                .validate_references_channel_sender(reference_validate_channel_sender)
                .build(),
        ))
        .build();

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

    let secret_resolver_service = async move { secrets_resolver.resolve().await }.boxed();
    let backend_references_resolver_service = async move { backend_references_resolver.resolve().await }.boxed();
    futures::future::join_all(vec![
        backend_references_resolver_service,
        secret_resolver_service,
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
