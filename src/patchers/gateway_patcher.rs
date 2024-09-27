use std::{marker::PhantomData, sync::Arc};

use gateway_api::apis::standard::gateways::Gateway;
use kube::{
    api::{Patch, PatchParams},
    Api, Client,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{controllers::LogContext, state::ResourceKey};

pub enum Operation<T>
where
    T: Serialize,
{
    PatchStatus(PatchContext<T>),
}

pub struct PatchContext<T>
where
    T: Serialize,
{
    pub resource_key: ResourceKey,
    pub resource: T,
    pub controller_name: String,
    pub version: Option<String>,
}

pub struct GatewayPatcher {
    client: Client,
    receiver: mpsc::Receiver<Operation<Gateway>>,
}

impl GatewayPatcher {
    pub fn new(client: Client) -> (Self, mpsc::Sender<Operation<Gateway>>) {
        let (sender, receiver) = mpsc::channel(1024);
        (Self { client, receiver }, sender)
    }

    pub async fn start(mut self) {
        while let Some(event) = self.receiver.recv().await {
            match event {
                Operation::PatchStatus(PatchContext {
                    resource_key,
                    resource,
                    controller_name,
                    version,
                }) => {
                    let api =
                        Api::<Gateway>::namespaced(self.client.clone(), &resource_key.namespace);
                    let patch_params = PatchParams::apply(&controller_name);
                    let log_context = LogContext::new(&controller_name, &resource_key, version);
                    let res = api
                        .patch_status(&resource_key.name, &patch_params, &Patch::Apply(resource))
                        .await;
                    match res {
                        Ok(_new_gateway) => {
                            info!("{log_context} patch status result ok");
                        }
                        Err(e) => {
                            warn!("{log_context} patch status failed {e:?}");
                        }
                    }
                }
            }
        }
    }
}
