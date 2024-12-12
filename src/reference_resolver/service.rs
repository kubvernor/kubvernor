use std::{collections::BTreeMap, sync::Arc};

use gateway_api::apis::standard::gateways::Gateway as KubeGateway;
use kube::Client;
use tokio::sync::Mutex;
use tracing::{field, instrument, span, warn, Instrument, Level, Span};
use typed_builder::TypedBuilder;

use crate::{
    common::{Gateway, ReferenceResolveRequest, RequestContext, ResourceKey},
    controllers::{ListenerTlsConfigValidator, RoutesResolver},
    state::State,
};

enum ReferenceStatus {
    Resolved,
    NotFound,
}

#[derive(TypedBuilder)]
pub struct ReferenceResolverService {
    client: Client,
    state: Arc<Mutex<State>>,
    #[builder(default)]
    referenecs: BTreeMap<ResourceKey, ReferenceStatus>,
    resolve_channel_receiver: tokio::sync::mpsc::Receiver<ReferenceResolveRequest>,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<(Gateway, Arc<KubeGateway>, String)>,
}

impl ReferenceResolverService {
    pub async fn start(self) {
        let mut resolve_receiver = self.resolve_channel_receiver;
        loop {
            tokio::select! {
                Some(resolve_event) = resolve_receiver.recv() => {
                    match resolve_event{
                        ReferenceResolveRequest::New(RequestContext{ gateway, kube_gateway, gateway_class_name }) => {
                            let span = span!(Level::INFO, "ReferenceResolverService", id = %gateway.key());
                            let backend_gateway = ListenerTlsConfigValidator::new(gateway, self.client.clone(), "ReferenceResolverService").validate().instrument(span.clone()).await;
                            let state = self.state.lock().await;
                            let backend_gateway = RoutesResolver::new(backend_gateway, self.client.clone(), "ReferenceResolverService", &state, &kube_gateway).validate().instrument(span.clone()).await;
                            let _ = self.gateway_deployer_channel_sender.send((backend_gateway, kube_gateway, gateway_class_name)).await;


                        },
                        ReferenceResolveRequest::Remove(gateway) => {

                        },
                    }
                },

                else => {
                    warn!("All listener manager channels are closed...exiting");
                }
            }
        }
    }
}
