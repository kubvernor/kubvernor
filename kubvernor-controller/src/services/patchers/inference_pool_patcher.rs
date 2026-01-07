use gateway_api_inference_extension::inferencepools::InferencePool;
use kube::{Api, Client};
use tokio::sync::mpsc;
use typed_builder::TypedBuilder;

use super::patcher::{Operation, Patcher};

#[derive(TypedBuilder)]
pub struct InferencePoolPatcherService {
    client: Client,
    receiver: mpsc::Receiver<Operation<InferencePool>>,
}

impl Patcher<InferencePool> for InferencePoolPatcherService {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<InferencePool>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<InferencePool> {
        Api::namespaced(self.client.clone(), namespace)
    }
}
