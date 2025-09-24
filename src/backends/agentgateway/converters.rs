use agentgateway_api_rs::google;

pub enum AnyTypeConverter {}

impl AnyTypeConverter {
    pub fn from<Msg: agentgateway_api_rs::prost::Message>((type_url, msg): (String, &Msg)) -> google::protobuf::Any {
        google::protobuf::Any { type_url, value: msg.encode_to_vec() }
    }
}
