use gateway_api::apis::standard::gateways::Gateway;
use kube::{Api, Client};

use tokio::sync::mpsc;

use crate::{controllers::LogContext, state::ResourceKey};

use super::patcher::{Operation, Patcher};

pub struct GatewayPatcher {
    client: Client,
    receiver: mpsc::Receiver<Operation<Gateway>>,
}

impl Patcher<Gateway> for GatewayPatcher {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<Gateway>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<Gateway> {
        Api::namespaced(self.client.clone(), namespace)
    }
    fn log_context<'a>(
        &'a self,
        resource_key: &'a ResourceKey,
        controller_name: &'a str,
        version: Option<String>,
    ) -> impl std::fmt::Display + Send {
        LogContext::<Gateway>::new(controller_name, resource_key, version.clone())
    }
}

impl GatewayPatcher {
    pub fn new(client: Client) -> (Self, mpsc::Sender<Operation<Gateway>>) {
        let (sender, receiver) = mpsc::channel(1024);
        (Self { client, receiver }, sender)
    }
}
