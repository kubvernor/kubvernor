use gateway_api::apis::standard::gatewayclasses::GatewayClass;
use kube::{Api, Client};
use tokio::sync::mpsc;

use super::patcher::{Operation, Patcher};
use crate::{common::ResourceKey, controllers::LogContext};

pub struct GatewayClassPatcher {
    client: Client,
    receiver: mpsc::Receiver<Operation<GatewayClass>>,
}

impl Patcher<GatewayClass> for GatewayClassPatcher {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<GatewayClass>> {
        &mut self.receiver
    }

    fn api(&self, _namespace: &str) -> Api<GatewayClass> {
        Api::all(self.client.clone())
    }

    fn log_context<'a>(&'a self, resource_key: &'a ResourceKey, controller_name: &'a str, version: Option<String>) -> impl std::fmt::Display + Send {
        LogContext::<GatewayClass>::new(controller_name, resource_key, version.clone())
    }
}

impl GatewayClassPatcher {
    pub fn new(client: Client) -> (Self, mpsc::Sender<Operation<GatewayClass>>) {
        let (sender, receiver) = mpsc::channel(1024);
        (Self { client, receiver }, sender)
    }
}
