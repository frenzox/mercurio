use bytes::*;
use crate::control_packet::*;
use crate::properties::*;
use crate::endec::*;
use std::mem;

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum QoS {
    AtMostOnce = 0,
    AtLeastOnce = 1,
    ExactlyOnce = 2
}

pub struct ConnectFlags {
    pub user_name: bool,
    pub password: bool,
    pub will_retain: bool,
    pub will_qos: QoS,
    pub will_flag: bool,
    pub clean_start: bool,
}

impl Encoder for ConnectFlags {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut flags: u8 = 0;

        if self.user_name {
            flags = 0b1000_0000;
        }

        if self.password {
            flags |= 0b0100_0000;
        }

        if self.will_retain {
            flags |= 0b0010_0000;
        }

        if self.will_flag {
            flags |= 0b0000_0100;
            flags |= (self.will_qos as u8) << 3;
        }

        if self.clean_start {
            flags |= 0b0000_0010;
        }

        buffer.put_u8(flags);
    }

    fn get_encoded_size(&self) -> usize {
        mem::size_of::<u8>()
    }
}

#[derive(Default)]
pub struct ConnectProperties {
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    pub receive_maximum: Option<ReceiveMaximum>,
    pub maximum_packet_size: Option<MaximumPacketSize>,
    pub topic_alias_maximum: Option<TopicAliasMaximum>,
    pub request_response_information: Option<RequestResponseInformation>,
    pub request_problem_information: Option<RequestProblemInformation>,
    pub user_property: Option<Vec<UserProperty>>,
    pub authentication_method: Option<AuthenticationMethod>,
    pub authentication_data: Option<AuthenticationData>
}

impl Encoder for ConnectProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.receive_maximum.encode(buffer);
        self.maximum_packet_size.encode(buffer);
        self.topic_alias_maximum.encode(buffer);
        self.request_response_information.encode(buffer);
        self.request_problem_information.encode(buffer);

        if let Some(props) = &self.user_property {
            for property in props {
                property.encode(buffer);
            }
        }

        self.authentication_method.encode(buffer);
        self.authentication_data.encode(buffer);
    }

    fn get_encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.session_expiry_interval.get_encoded_size();
        len += self.receive_maximum.get_encoded_size();
        len += self.maximum_packet_size.get_encoded_size();
        len += self.topic_alias_maximum.get_encoded_size();
        len += self.request_response_information.get_encoded_size();
        len += self.request_problem_information.get_encoded_size();

        if let Some(props) = &self.user_property {
            for property in props {
                len += property.get_encoded_size();
            }
        }

        len += self.authentication_method.get_encoded_size();
        len += self.authentication_data.get_encoded_size();

        len
    }
}

#[derive(Default)]
pub struct WillProperties {
    pub will_delay_interval: Option<WillDelayInterval>,
    pub payload_format_indicator: Option<PayloadFormatIndicator>,
    pub message_expiry_interval: Option<MessageExpiryInterval>,
    pub content_type: Option<ContentType>,
    pub response_topic: Option<ResponseTopic>,
    pub correlation_data: Option<CorrelationData>,
    pub user_property: Option<Vec<UserProperty>>
}

impl Encoder for WillProperties {
    fn encode(&self, buffer: &mut BytesMut) {

        self.will_delay_interval.encode(buffer);
        self.payload_format_indicator.encode(buffer);
        self.message_expiry_interval.encode(buffer);
        self.content_type.encode(buffer);
        self.response_topic.encode(buffer);
        self.correlation_data.encode(buffer);

        if let Some(props) = &self.user_property {
            for property in props {
                property.encode(buffer);
            }
        }
    }

    fn get_encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.will_delay_interval.get_encoded_size();
        len += self.payload_format_indicator.get_encoded_size();
        len += self.message_expiry_interval.get_encoded_size();
        len += self.content_type.get_encoded_size();
        len += self.response_topic.get_encoded_size();
        len += self.correlation_data.get_encoded_size();
        if let Some(props) = &self.user_property {
            for property in props {
                len += property.get_encoded_size();
            }
        }

        len
    }
}

#[derive(Default)]
pub struct ConnectPayload {
    pub client_id: String,
    pub will_properties: Option<WillProperties>,
    pub will_topic: Option<String>,
    pub will_payload: Option<Bytes>,
    pub user_name: Option<String>,
    pub password: Option<Bytes>,
}

impl Encoder for ConnectPayload {
    fn encode(&self, buffer: &mut BytesMut) {
        self.client_id.encode(buffer);

        if self.will_properties.get_encoded_size() > 0 {
            VariableByteInteger(self.will_properties.get_encoded_size() as u32).encode(buffer);
            self.will_properties.encode(buffer);
        }

        self.will_topic.encode(buffer);
        self.will_payload.encode(buffer);
        self.user_name.encode(buffer);
        self.password.encode(buffer);
    }

    fn get_encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.client_id.get_encoded_size();
        if self.will_properties.get_encoded_size() > 0 {
            len += VariableByteInteger(self.will_properties.get_encoded_size() as u32).get_encoded_size();
            len += self.will_properties.get_encoded_size();
        }

        len += self.will_topic.get_encoded_size();
        len += self.will_payload.get_encoded_size();
        len += self.user_name.get_encoded_size();
        len += self.password.get_encoded_size();

        len
    }
}

pub struct ConnectPacket {
    flags: ConnectFlags,
    keepalive: u16,
    properties: ConnectProperties,
    payload: ConnectPayload
}

impl ConnectPacket {
    const PROTOCOL_NAME: &'static str = "MQTT";
    const PROTOCOL_VERSION: u8 = 5;

    pub fn new(flags: ConnectFlags,
               keepalive: u16,
               properties: ConnectProperties,
               payload: ConnectPayload) -> Self {
        ConnectPacket {
            flags,
            keepalive,
            properties,
            payload
        }
    }
}

impl ControlPacket for ConnectPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Connect;
}

impl Encoder for ConnectPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        // fixed header
        buffer.put_u8((Self::PACKET_TYPE as u8) << 4 | 0b0000_0000);
        remaining_len += Self::PROTOCOL_NAME.get_encoded_size();
        remaining_len += Self::PROTOCOL_VERSION.get_encoded_size();
        remaining_len += self.flags.get_encoded_size();
        remaining_len += self.keepalive.get_encoded_size();
        if self.properties.get_encoded_size() > 0 {
            remaining_len += VariableByteInteger(self.properties.get_encoded_size() as u32).get_encoded_size();
        }
        remaining_len += self.properties.get_encoded_size();
        remaining_len += self.payload.get_encoded_size();

        VariableByteInteger(remaining_len as u32).encode(buffer);

        // variable header
        Self::PROTOCOL_NAME.encode(buffer);
        Self::PROTOCOL_VERSION.encode(buffer);
        self.flags.encode(buffer);
        self.keepalive.encode(buffer);

        VariableByteInteger(self.properties.get_encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);

        // payload
        self.payload.encode(buffer);
    }
}

#[cfg(test)]
mod tests {
    use bytes::*;
    // use crate::properties::*;
    use crate::packets::connect::*;

    #[test]
    fn test_connect_packet_encoding() {
        let expected = vec![0x10, 0x10, 0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, 0x05, 0x02, 0x00, 0x3c, 0x03, 0x21, 0x00, 0x14, 0x00, 0x00];

        let flags = ConnectFlags {
            user_name: false,
            password: false,
            will_retain: false,
            will_qos: QoS::AtMostOnce,
            will_flag: false,
            clean_start: true,
        };

        let mut connect_properties = ConnectProperties::default();
        connect_properties.receive_maximum = ReceiveMaximum::new(20).into();

        let payload = ConnectPayload::default();
        let packet = ConnectPacket::new(flags, 60, connect_properties, payload);
        let mut encoded = BytesMut::new();

        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);
    }

    #[test]
    fn test_connect_packet_encoding2() {
        let expected = vec![0x10, 0x52, 0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, 0x05, 0xee, 0x00, 0x3c, 0x08, 0x11,
                            0x00, 0x00, 0x00, 0x1e, 0x21, 0x00, 0x14, 0x00, 0x00, 0x0d, 0x08, 0x00, 0x0a, 0x74,
                            0x65, 0x73, 0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63, 0x00, 0x0a, 0x74, 0x65, 0x73,
                            0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63, 0x00, 0x0c, 0x74, 0x65, 0x73, 0x74, 0x5f,
                            0x70, 0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, 0x00, 0x09, 0x74, 0x65, 0x73, 0x74, 0x5f,
                            0x75, 0x73, 0x65, 0x72, 0x00, 0x08, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x70, 0x77, 0x64];

        let flags = ConnectFlags {
            user_name: true,
            password: true,
            will_retain: true,
            will_qos: QoS::AtLeastOnce,
            will_flag: true,
            clean_start: true,
        };

        let mut connect_properties = ConnectProperties::default();
        connect_properties.session_expiry_interval = SessionExpiryInterval::new(30).into();
        connect_properties.receive_maximum = ReceiveMaximum::new(20).into();

        let mut payload = ConnectPayload::default();

        let mut will_properties = WillProperties::default();
        will_properties.response_topic = ResponseTopic::new(String::from("test_topic")).into();

        payload.will_properties = will_properties.into();
        payload.will_topic = String::from("test_topic").into();
        payload.will_payload = Bytes::from("test_payload").into();
        payload.user_name = String::from("test_user").into();
        payload.password = Bytes::from("test_pwd").into();

        let packet = ConnectPacket::new(flags, 60, connect_properties, payload);
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);
    }
}
