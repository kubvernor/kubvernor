use gateway_api::apis::standard::gateways::Gateway;
use kube::{Api, Client};
use tokio::sync::mpsc;
use typed_builder::TypedBuilder;

use super::patcher::{Operation, Patcher};
use crate::{common::ResourceKey, controllers::LogContext};

#[derive(TypedBuilder)]
pub struct GatewayPatcherService {
    client: Client,
    receiver: mpsc::Receiver<Operation<Gateway>>,
}

impl Patcher<Gateway> for GatewayPatcherService {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<Gateway>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<Gateway> {
        Api::namespaced(self.client.clone(), namespace)
    }
}
