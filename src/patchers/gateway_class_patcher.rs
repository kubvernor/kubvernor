use gateway_api::apis::standard::gatewayclasses::GatewayClass;
use kube::{Api, Client};
use tokio::sync::mpsc;
use typed_builder::TypedBuilder;

use super::patcher::{Operation, Patcher};
use crate::{common::ResourceKey, controllers::LogContext};

#[derive(TypedBuilder)]
pub struct GatewayClassPatcherService {
    client: Client,
    receiver: mpsc::Receiver<Operation<GatewayClass>>,
}

impl Patcher<GatewayClass> for GatewayClassPatcherService {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<GatewayClass>> {
        &mut self.receiver
    }

    fn api(&self, _namespace: &str) -> Api<GatewayClass> {
        Api::all(self.client.clone())
    }
}
