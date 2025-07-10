use envoy_api_rs::{
    envoy::{
        config::{cluster::v3::Cluster, listener::v3::Listener},
        service::discovery::v3::Resource,
    },
    prost::{self, Message},
};

use crate::backends::common::converters;

use super::model::TypeUrl;

pub fn create_cluster_resource(cluster: &Cluster) -> Resource {
    let any = converters::AnyTypeConverter::from((TypeUrl::Cluster.to_string(), cluster));

    let mut cluster_resource = Resource { ..Default::default() };
    cluster_resource.name.clone_from(&cluster.name);
    cluster_resource.resource = Some(any);
    cluster_resource
}

pub fn create_listener_resource(listener: &Listener) -> Resource {
    let mut buf: Vec<u8> = vec![];
    listener.encode(&mut buf).expect("We expect this to work");
    let any = prost::bytes::Bytes::from(buf);
    let any = envoy_api_rs::google::protobuf::Any {
        type_url: TypeUrl::Listener.to_string(),
        value: any.to_vec(),
    };

    let mut listener_resource = Resource { ..Default::default() };
    listener_resource.name.clone_from(&listener.name);
    listener_resource.resource = Some(any);
    listener_resource
}
