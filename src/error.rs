use crate::reason::ReasonCode;

#[derive(Debug)]
pub enum Error {
    PacketIncomplete,
    Io(std::io::Error),
    MQTTReasonCode(ReasonCode),
}

impl From<ReasonCode> for Error {
    fn from(err: ReasonCode) -> Self {
        Error::MQTTReasonCode(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}
