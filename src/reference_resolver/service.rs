use std::collections::BTreeMap;

use kube::Client;
use tracing::{span, warn, Instrument, Level};
use typed_builder::TypedBuilder;

use crate::{
    common::{GatewayDeployRequest, ReferenceResolveRequest, RequestContext, ResourceKey},
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
    state: State,
    #[builder(default)]
    referenecs: BTreeMap<ResourceKey, ReferenceStatus>,
    resolve_channel_receiver: tokio::sync::mpsc::Receiver<ReferenceResolveRequest>,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
}

struct ReferenceResolverHandler {
    client: Client,
    state: State,
    referenecs: BTreeMap<ResourceKey, ReferenceStatus>,
    gateway_deployer_channel_sender: tokio::sync::mpsc::Sender<GatewayDeployRequest>,
}

impl ReferenceResolverService {
    pub async fn start(self) {
        let mut resolve_receiver = self.resolve_channel_receiver;
        let handler = ReferenceResolverHandler {
            client: self.client,
            state: self.state,
            referenecs: self.referenecs,
            gateway_deployer_channel_sender: self.gateway_deployer_channel_sender,
        };

        loop {
            tokio::select! {
                Some(resolve_event) = resolve_receiver.recv() => {
                    handler.handle(resolve_event).await;
                },

                else => {
                    warn!("All listener manager channels are closed...exiting");
                }
            }
        }
    }
}
impl ReferenceResolverHandler {
    async fn handle(&self, resolve_event: ReferenceResolveRequest) {
        match resolve_event {
            ReferenceResolveRequest::New(RequestContext {
                gateway,
                kube_gateway,
                gateway_class_name,
                span,
            }) => {
                let span = span!(parent: &span, Level::INFO, "ReferenceResolverService", id = %gateway.key());
                let _entered = span.enter();
                let backend_gateway = ListenerTlsConfigValidator::new(gateway, self.client.clone(), "ReferenceResolverService")
                    .validate()
                    .instrument(span.clone())
                    .await;

                let backend_gateway = RoutesResolver::new(backend_gateway, self.client.clone(), &self.state, &kube_gateway)
                    .validate()
                    .instrument(span.clone())
                    .await;
                let _ = self
                    .gateway_deployer_channel_sender
                    .send(GatewayDeployRequest::Deploy(
                        RequestContext::builder()
                            .gateway(backend_gateway)
                            .kube_gateway(kube_gateway)
                            .gateway_class_name(gateway_class_name)
                            .span(span.clone())
                            .build(),
                    ))
                    .await;
            }
            ReferenceResolveRequest::Remove(_gateway) => {}
        }
    }
}
