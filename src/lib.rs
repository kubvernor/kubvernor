use std::{collections::HashMap, fmt::Debug, net::SocketAddr, sync::Arc, time::Duration};

use common::{BackendReferenceResolver, ControlPlaneConfig, ReferenceGrantsResolver, SecretsResolver};
use futures::FutureExt;
use kube::Client;
use serde::Deserialize;
use services::{
    GatewayClassPatcherService, GatewayDeployerService, GatewayPatcherService, HttpRoutePatcherService, Patcher, ReferenceValidatorService,
    patchers::GRPCRoutePatcherService,
};
use state::State;
use thiserror::Error;
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

cfg_if::cfg_if! {
    if #[cfg(feature="envoy_xds")] {
        use backends::envoy::envoy_xds_backend::EnvoyDeployerChannelHandlerService;
    } else if #[cfg(feature = "envoy_cm")] {
        use backends::envoy_cm_backend::EnvoyDeployerChannelHandlerService;
    } else {

    }
}
#[cfg(feature = "agentgateway")]
use backends::agentgateway::AgentgatewayDeployerChannelHandlerService;
use controllers::{
    gateway::{self, GatewayController},
    gateway_class::GatewayClassController,
    inference_pool::InferencePoolController,
    route::{
        grpc_route::{self, GRPCRouteController},
        http_route::{self, HttpRouteController},
    },
};
use typed_builder::TypedBuilder;

use crate::{common::GatewayImplementationType, controllers::inference_pool, services::patchers::InferencePoolPatcherService};

const STARTUP_DURATION: Duration = Duration::from_secs(10);

#[derive(Debug, TypedBuilder, Deserialize)]
pub struct EnvoyGatewayControlPlaneConfiguration {
    control_plane_socket: SocketAddr,
}

#[derive(Debug, TypedBuilder, Deserialize)]
pub struct AgentgatewayGatewayControlPlaneConfiguration {
    control_plane_socket: SocketAddr,
}

#[derive(Debug, TypedBuilder, Deserialize)]
pub struct Configuration {
    pub controller_name: String,
    pub enable_open_telemetry: Option<bool>,
    envoy_gateway_control_plane: Option<EnvoyGatewayControlPlaneConfiguration>,
    agentgateway_gateway_control_plane: Option<AgentgatewayGatewayControlPlaneConfiguration>,
}

#[derive(Error, Debug)]
enum ConfigurationError {
    #[error("controller name must be not empty")]
    ControllerName,
    #[error("one control plane must be configured")]
    ControlPlane,
}

impl Configuration {
    pub fn validate(&self) -> Result<()> {
        if self.controller_name.is_empty() {
            return Err(ConfigurationError::ControllerName.into());
        }
        match (&self.agentgateway_gateway_control_plane, &self.envoy_gateway_control_plane) {
            (None, None) => Err(ConfigurationError::ControlPlane.into()),
            _ => Ok(()),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn start(configuration: Configuration) -> Result<()> {
    info!("Kubvernor started");
    let state = State::new();
    let client = Client::try_default().await?;

    let (gateway_deployer_channel_sender, gateway_deployer_channel_receiver) = mpsc::channel(1024);
    let (reference_validate_channel_sender, reference_validate_channel_receiver) = mpsc::channel(1024);
    let (gateway_patcher_channel_sender, gateway_patcher_channel_receiver) = mpsc::channel(1024);
    let (gateway_class_patcher_channel_sender, gateway_class_patcher_channel_receiver) = mpsc::channel(1024);
    let (http_route_patcher_channel_sender, http_route_patcher_channel_receiver) = mpsc::channel(1024);
    let (grpc_route_patcher_channel_sender, grpc_route_patcher_channel_receiver) = mpsc::channel(1024);
    let (inference_pool_patcher_channel_sender, inference_pool_patcher_channel_receiver) = mpsc::channel(1024);
    let (envoy_backend_deployer_channel_sender, envoy_backend_deployer_channel_receiver) = mpsc::channel(1024);
    let (backend_response_channel_sender, backend_response_channel_receiver) = mpsc::channel(1024);
    let mut backend_deployer_channel_senders = HashMap::new();
    backend_deployer_channel_senders.insert(GatewayImplementationType::Envoy, envoy_backend_deployer_channel_sender);

    #[cfg(feature = "agentgateway")]
    let (agentgateway_backend_deployer_channel_sender, agentgateway_backend_deployer_channel_receiver) = mpsc::channel(1024);
    #[cfg(feature = "agentgateway")]
    backend_deployer_channel_senders.insert(GatewayImplementationType::Agentgateway, agentgateway_backend_deployer_channel_sender);

    let secrets_resolver = SecretsResolver::builder().reference_resolver(client.clone(), reference_validate_channel_sender.clone()).build();

    let backend_references_resolver = BackendReferenceResolver::builder()
        .state(state.clone())
        .reference_resolver(client.clone(), reference_validate_channel_sender.clone())
        .inference_pool_reference_resolver(client.clone(), reference_validate_channel_sender.clone())
        .build();

    let reference_grants_resolver = ReferenceGrantsResolver::builder()
        .client(client.clone())
        .state(state.clone())
        .reference_validate_channel_sender(reference_validate_channel_sender.clone())
        .build();

    let gateway_deployer_service = GatewayDeployerService::builder()
        .gateway_deployer_channel_receiver(gateway_deployer_channel_receiver)
        .backend_deployer_channel_senders(backend_deployer_channel_senders.clone())
        .backend_response_channel_receiver(backend_response_channel_receiver)
        .state(state.clone())
        .gateway_patcher_channel_sender(gateway_patcher_channel_sender.clone())
        .gateway_class_patcher_channel_sender(gateway_class_patcher_channel_sender.clone())
        .http_route_patcher_channel_sender(http_route_patcher_channel_sender.clone())
        .grpc_route_patcher_channel_sender(grpc_route_patcher_channel_sender.clone())
        .controller_name(configuration.controller_name.clone())
        .build();

    let resolver_service = ReferenceValidatorService::builder()
        .controller_name(configuration.controller_name.clone())
        .client(client.clone())
        .reference_validate_channel_receiver(reference_validate_channel_receiver)
        .gateway_deployer_channel_sender(gateway_deployer_channel_sender.clone())
        .state(state.clone())
        .secrets_resolver(secrets_resolver.clone())
        .backend_references_resolver(backend_references_resolver.clone())
        .reference_grants_resolver(reference_grants_resolver.clone())
        .inference_pool_patcher_sender(inference_pool_patcher_channel_sender.clone())
        .build();

    let mut gateway_patcher_service =
        GatewayPatcherService::builder().client(client.clone()).receiver(gateway_patcher_channel_receiver).build();
    let mut gateway_class_patcher_service =
        GatewayClassPatcherService::builder().client(client.clone()).receiver(gateway_class_patcher_channel_receiver).build();
    let mut http_route_patcher_service =
        HttpRoutePatcherService::builder().client(client.clone()).receiver(http_route_patcher_channel_receiver).build();
    let mut grpc_route_patcher_service =
        GRPCRoutePatcherService::builder().client(client.clone()).receiver(grpc_route_patcher_channel_receiver).build();
    let mut inference_pool_patcher_service =
        InferencePoolPatcherService::builder().client(client.clone()).receiver(inference_pool_patcher_channel_receiver).build();

    let gateway_class_controller = GatewayClassController::new(
        configuration.controller_name.clone(),
        &client,
        state.clone(),
        gateway_class_patcher_channel_sender.clone(),
    );
    let gateway_controller = GatewayController::builder()
        .ctx(Arc::new(
            gateway::GatewayControllerContext::builder()
                .client(client.clone())
                .controller_name(configuration.controller_name.clone())
                .gateway_channel_senders(backend_deployer_channel_senders.clone())
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
                .controller_name(configuration.controller_name.clone())
                .state(state.clone())
                .http_route_patcher(http_route_patcher_channel_sender.clone())
                .validate_references_channel_sender(reference_validate_channel_sender.clone())
                .inference_pool_patcher_channel_sender(inference_pool_patcher_channel_sender.clone())
                .build(),
        ))
        .build();

    let grpc_route_controller = GRPCRouteController::builder()
        .ctx(Arc::new(
            grpc_route::GRPCRouteControllerContext::builder()
                .client(client.clone())
                .controller_name(configuration.controller_name.clone())
                .state(state.clone())
                .grpc_route_patcher(grpc_route_patcher_channel_sender.clone())
                .validate_references_channel_sender(reference_validate_channel_sender.clone())
                .build(),
        ))
        .build();

    let inference_pool_controller = InferencePoolController::builder()
        .ctx(Arc::new(
            inference_pool::InferencePoolControllerContext::builder()
                .client(client.clone())
                .controller_name(configuration.controller_name.clone())
                .state(state.clone())
                .validate_references_channel_sender(reference_validate_channel_sender)
                .inference_pool_patcher_sender(inference_pool_patcher_channel_sender)
                .build(),
        ))
        .build();

    let resolver_service = resolver_service.start().boxed();
    let gateway_deployer_service = gateway_deployer_service.start().boxed();
    let gateway_patcher_service = gateway_patcher_service.start().boxed();
    let gateway_class_patcher_service = gateway_class_patcher_service.start().boxed();
    let http_route_patcher_service = http_route_patcher_service.start().boxed();
    let grpc_route_patcher_service = grpc_route_patcher_service.start().boxed();
    let inference_pool_patcher_service = inference_pool_patcher_service.start().boxed();

    let gateway_class_controller_task = async move {
        info!("Gateway Class controller...started");
        gateway_class_controller.get_controller().await;
        info!("Gateway Class controller...stopped");
        crate::Result::<()>::Ok(())
    };
    let gateway_controller_task = async move {
        sleep(STARTUP_DURATION).await;
        info!("Gateway controller...started");
        gateway_controller.get_controller().await;
        info!("Gateway controller...stopped");
        crate::Result::<()>::Ok(())
    };

    let http_route_controller_task = async move {
        sleep(2 * STARTUP_DURATION).await;
        info!("HTTP Route controller...started");
        http_route_controller.get_controller().await;
        info!("HTTP Route controller...stopped");
        crate::Result::<()>::Ok(())
    };

    let grpc_route_controller_task = async move {
        sleep(2 * STARTUP_DURATION).await;
        info!("GRPC Route controller...started");
        grpc_route_controller.get_controller().await;
        info!("GRPC Route controller...stopped");
        crate::Result::<()>::Ok(())
    };

    let inference_pool_controller_task = async move {
        sleep(2 * STARTUP_DURATION).await;
        info!("Inference Pool controller...started");
        inference_pool_controller.get_controller().await;
        info!("Inference Pool controller...stopped");
        crate::Result::<()>::Ok(())
    };

    let secret_resolver_service = async move { secrets_resolver.resolve().await }.boxed();
    let backend_references_resolver_service = async move { backend_references_resolver.resolve().await }.boxed();
    let reference_grants_resolver_service = async move { reference_grants_resolver.resolve().await }.boxed();
    let mut services = vec![
        reference_grants_resolver_service,
        backend_references_resolver_service,
        secret_resolver_service,
        gateway_deployer_service,
        resolver_service,
        gateway_class_patcher_service,
        gateway_patcher_service,
        http_route_patcher_service,
        grpc_route_patcher_service,
        inference_pool_patcher_service,
        gateway_class_controller_task.boxed(),
        gateway_controller_task.boxed(),
        http_route_controller_task.boxed(),
        grpc_route_controller_task.boxed(),
        inference_pool_controller_task.boxed(),
    ];

    if let Some(control_plane_config) = configuration.envoy_gateway_control_plane {
        let control_plane_config = ControlPlaneConfig {
            listening_socket: control_plane_config.control_plane_socket,
            controller_name: configuration.controller_name.clone(),
        };

        let service = EnvoyDeployerChannelHandlerService::builder()
            .client(client.clone())
            .control_plane_config(control_plane_config.clone())
            .backend_deploy_request_channel_receiver(envoy_backend_deployer_channel_receiver)
            .backend_response_channel_sender(backend_response_channel_sender.clone())
            .build();

        services.push(service.start().boxed());
    }

    #[cfg(feature = "agentgateway")]
    if let Some(control_plane_config) = configuration.agentgateway_gateway_control_plane {
        let control_plane_config = ControlPlaneConfig {
            listening_socket: control_plane_config.control_plane_socket,
            controller_name: configuration.controller_name.clone(),
        };

        let service = AgentgatewayDeployerChannelHandlerService::builder()
            .client(client.clone())
            .control_plane_config(control_plane_config)
            .backend_deploy_request_channel_receiver(agentgateway_backend_deployer_channel_receiver)
            .backend_response_channel_sender(backend_response_channel_sender)
            .build();
        services.push(service.start().boxed());
    }

    futures::future::join_all(services).await;
    info!("Kubvernor stopped");
    Ok(())
}
