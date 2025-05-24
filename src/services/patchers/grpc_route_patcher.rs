use gateway_api::grpcroutes::GRPCRoute;
use kube::{Api, Client};
use tokio::sync::mpsc;
use typed_builder::TypedBuilder;

use super::patcher::{Operation, Patcher};

#[derive(TypedBuilder)]
pub struct GRPCRoutePatcherService {
    client: Client,
    receiver: mpsc::Receiver<Operation<GRPCRoute>>,
}

impl Patcher<GRPCRoute> for GRPCRoutePatcherService {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<GRPCRoute>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<GRPCRoute> {
        Api::namespaced(self.client.clone(), namespace)
    }
}
