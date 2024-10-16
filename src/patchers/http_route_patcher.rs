use gateway_api::apis::standard::httproutes::HTTPRoute;
use kube::{Api, Client};
use tokio::sync::mpsc;

use super::patcher::{Operation, Patcher};
use crate::{common::ResourceKey, controllers::LogContext};

pub struct HttpRoutePatcher {
    client: Client,
    receiver: mpsc::Receiver<Operation<HTTPRoute>>,
}

impl Patcher<HTTPRoute> for HttpRoutePatcher {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Operation<HTTPRoute>> {
        &mut self.receiver
    }

    fn api(&self, namespace: &str) -> Api<HTTPRoute> {
        Api::namespaced(self.client.clone(), namespace)
    }
    fn log_context<'a>(&'a self, resource_key: &'a ResourceKey, controller_name: &'a str, version: Option<String>) -> impl std::fmt::Display + Send {
        LogContext::<HTTPRoute>::new(controller_name, resource_key, version.clone())
    }
}

impl HttpRoutePatcher {
    pub fn new(client: Client) -> (Self, mpsc::Sender<Operation<HTTPRoute>>) {
        let (sender, receiver) = mpsc::channel(1024);
        (Self { client, receiver }, sender)
    }
}
