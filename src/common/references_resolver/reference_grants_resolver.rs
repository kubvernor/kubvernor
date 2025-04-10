use crate::common::gateway_api::referencegrants::ReferenceGrant;
use std::{collections::BTreeSet, fmt, future::Future, pin::Pin, sync::Arc};

use gateway_api::apis::standard::referencegrants::{ReferenceGrantFrom, ReferenceGrantTo};
use kube::{api::ListParams, Api, Client};
use kube_core::ObjectList;
use tokio::{sync::Mutex, time};
use tracing::{span, warn, Level};
use typed_builder::TypedBuilder;

use super::ReferencesResolver;
use crate::{
    common::{Backend, Gateway, ProtocolType, ReferenceValidateRequest, ResourceKey, TlsType},
    controllers::find_linked_routes,
    state::State,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct ReferenceGrantRef {
    from: ResourceKey,
    to: ResourceKey,
}

impl ReferenceGrantRef {
    fn new(from: ResourceKey, to: ResourceKey) -> Self {
        Self { from, to }
    }
}

#[derive(Clone, TypedBuilder)]
pub struct ReferenceGrantsResolver {
    client: Client,
    #[builder(default)]
    references: BTreeSet<ReferenceGrantRef>,
    #[builder(default)]
    resolved_grants: Arc<tokio::sync::Mutex<BTreeSet<ReferenceGrantRef>>>,
    reference_validate_channel_sender: tokio::sync::mpsc::Sender<ReferenceValidateRequest>,
    state: State,
}

type FutReturn = dyn Future<Output = Option<kube::Result<ObjectList<ReferenceGrant>>>>;

impl ReferenceGrantsResolver {
    fn search_references(&self, gateway: &Gateway) -> BTreeSet<ReferenceGrantRef> {
        let gateway_key = gateway.key();

        let linked_routes = find_linked_routes(&self.state, gateway_key);

        let mut backend_reference_keys = BTreeSet::new();

        for route in linked_routes {
            let from = route.resource_key();
            let route_config = route.config();
            for rule in &route_config.routing_rules {
                for backend in &rule.backends {
                    if let Backend::Maybe(backend_service_config) = backend {
                        backend_reference_keys.insert(ReferenceGrantRef::new(from.clone(), backend_service_config.resource_key.clone()));
                    };
                }
            }
        }

        let mut secrets_references = BTreeSet::new();
        for listener in gateway.listeners().filter(|f| f.protocol() == ProtocolType::Https || f.protocol() == ProtocolType::Tls) {
            let listener_data = listener.data();
            if let Some(TlsType::Terminate(certificates)) = &listener_data.config.tls_type {
                for certificate in certificates {
                    let certificate_key = certificate.resouce_key();
                    secrets_references.insert(ReferenceGrantRef::new(gateway_key.clone(), certificate_key.clone()));
                }
            }
        }
        backend_reference_keys.append(&mut secrets_references);
        backend_reference_keys
    }

    pub async fn add_references_by_gateway(&mut self, gateway: &Gateway) {
        let mut references = self.search_references(gateway);        
        self.references.append(&mut references);
    }

    pub async fn delete_references_by_gateway(&mut self, gateway: &Gateway) {
        let mut references = self.search_references(gateway);        
        let diff = self.references.difference(&mut references).cloned();        
        self.references  =diff.collect();                
    }

    pub async fn resolve(&self, key_namespace: String ) {
        let client = self.client.clone();
        
        let resolver_fn = move |key_namespace:String |{
            // let api: Api<ReferenceGrant> = Api::namespaced(client, &key_namespace);    
            // let lp = ListParams::default();
            // Box::pin(api.list(&lp))
            async ||{}
        };      

        //let b = resolver_fn("balls".to_owned()).await;
        self.resolve_internal(resolver_fn).await
        //self.resolve_internal(resolver_fn);
        
    }
    

    pub async fn resolve_internal<U>(&self, resolve_fn: U ) where U : Fn(String) -> Pin<Box<dyn Future<Output = Result<ObjectList<ReferenceGrant>, kube::Error>>>>{
        let mut interval = time::interval(time::Duration::from_secs(10));
        let span = span!(Level::INFO, "ReferenceGrantsResolver");
        let _entered = span.enter();
        
        loop {
            let mut allowed_grants = BTreeSet::new();     
            interval.tick().await;
            let mut configured_grants = BTreeSet::new();
            let this = self;
            let resolved_namespace_keys: BTreeSet<&ResourceKey> =  this.references.iter().map(|reference| &reference.to).collect();
            for resolved_namespace_key in resolved_namespace_keys{
                //let resolved = resolve_fn(&resolved_namespace_key.namespace.clone());
                // let api: Api<ReferenceGrant> = Api::namespaced(this.client.clone(), &resolved_namespace_key.namespace);
                // let lp = ListParams::default();
                if let Ok(reference_grants) = resolve_fn(resolved_namespace_key.namespace.clone()).await{
                    for grant in reference_grants{                        
                        for from in &grant.spec.from{
                            for to in &grant.spec.to{
                                ReferenceGrantRef::new(ResourceKey::from(from), ResourceKey::from(to));
                                configured_grants.insert(ReferenceGrantRef::new(ResourceKey::from(from), ResourceKey::from(to)));
                            }    
                        }
                        
                    }                    
                }else{
                    warn!("Unable to list ReferenceGrants for {resolved_namespace_key}");
                }
            }
            
            for reference in this.references.iter(){
                if configured_grants.contains(reference){
                    allowed_grants.insert(reference);
                }
            }                                    
            let mut resolved_grants = this.resolved_grants.lock().await;
            *resolved_grants =  allowed_grants.into_iter().cloned().collect();
        }                                        
    }

    pub async fn is_allowed(&self, from: &ResourceKey, to: &ResourceKey)->bool{        
        self.resolved_grants.lock().await.contains(&ReferenceGrantRef::new(from.clone(), to.clone()))
    }
}

impl From<&ReferenceGrantFrom> for ResourceKey{
    fn from(value: &ReferenceGrantFrom) -> Self {
        ResourceKey { group: value.group.clone(),  namespace: value.namespace.clone(), name: Default::default(), kind: value.kind.clone()}         
    }
}

impl From<&ReferenceGrantTo> for ResourceKey{
    fn from(value: &ReferenceGrantTo) -> Self {
        ResourceKey { group: value.group.clone(),  namespace: Default::default(), name: value.name.as_ref().unwrap_or(&String::new()).to_owned(), kind: value.kind.clone()}         
    }
}