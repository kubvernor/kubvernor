use kube::{Api, Client};
use tokio::sync::mpsc;
use typed_builder::TypedBuilder;

use super::patcher::{Operation, Patcher};
use crate::common::gateway_api::httproutes::HTTPRoute;

#[derive(TypedBuilder)]
pub struct HttpRoutePatcherService {
    client: Client,
    receiver: mpsc::Receiver<Operation<HTTPRoute>>,
}

impl Patcher<HTTPRoute> for HttpRoutePatcherService {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<HTTPRoute>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<HTTPRoute> {
        Api::namespaced(self.client.clone(), namespace)
    }
}
