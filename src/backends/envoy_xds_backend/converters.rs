use envoy_api::google;

pub enum AnyTypeConverter {}

impl AnyTypeConverter {
    pub fn from<Msg: envoy_api::prost::Message>((type_url, msg): (String, &Msg)) -> google::protobuf::Any {
        google::protobuf::Any { type_url, value: msg.encode_to_vec() }
    }
}
