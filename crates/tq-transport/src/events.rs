//! Events streams ([`tq_proto::room`]) and telling them apart from group
//! streams.

use bytes::Bytes;
use tq_proto::{RoomMessage, WireError, EVENTS_VERSION, GROUP_HEADER_FIXED, STREAM_TYPE_EVENTS, STREAM_TYPE_GROUP};
use web_transport_quinn::{RecvStream, SendStream, Session};

use crate::group::GroupReader;

/// Send priority of the control and events streams: above every video
/// group, so that they never queue behind video. They carry little data,
/// so they cannot starve it.
pub const PRIORITY_URGENT: i32 = i32::MAX;

/// Writer of an events stream.
pub struct EventsTx {
    stream: SendStream,
}

impl EventsTx {
    pub async fn open(session: &Session) -> anyhow::Result<Self> {
        let mut stream = session.open_uni().await?;
        let _ = stream.set_priority(PRIORITY_URGENT);
        stream.write_all(&tq_proto::events_stream_header()).await?;
        Ok(Self { stream })
    }

    pub async fn send(&mut self, msg: &RoomMessage) -> anyhow::Result<()> {
        self.send_encoded(&msg.encode()).await
    }

    /// Sends a message already encoded with [`RoomMessage::encode`] (encode
    /// once, send to every participant).
    pub async fn send_encoded(&mut self, bytes: &Bytes) -> anyhow::Result<()> {
        self.stream.write_all(bytes).await?;
        Ok(())
    }
}

/// Reader of an events stream, after its type byte.
pub struct EventsRx {
    stream: RecvStream,
}

impl EventsRx {
    /// Next message; `None` when the peer finished the stream.
    pub async fn recv(&mut self) -> anyhow::Result<Option<RoomMessage>> {
        let mut len = [0u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(()) => {}
            Err(web_transport_quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = RoomMessage::check_len(u32::from_be_bytes(len))?;
        let mut json = vec![0u8; len];
        self.stream.read_exact(&mut json).await?;
        Ok(Some(RoomMessage::decode(&json)?))
    }
}

/// An incoming unidirectional stream, by its type byte.
pub enum Incoming {
    Group(GroupReader),
    Events(EventsRx),
}

impl Incoming {
    pub async fn open(mut stream: RecvStream) -> anyhow::Result<Self> {
        let mut ty = [0u8; 1];
        stream.read_exact(&mut ty).await?;
        match ty[0] {
            STREAM_TYPE_GROUP => {
                let mut fixed = [0u8; GROUP_HEADER_FIXED];
                fixed[0] = ty[0];
                stream.read_exact(&mut fixed[1..]).await?;
                Ok(Incoming::Group(GroupReader::open_fixed(stream, fixed).await?))
            }
            STREAM_TYPE_EVENTS => {
                let mut version = [0u8; 1];
                stream.read_exact(&mut version).await?;
                if version[0] != EVENTS_VERSION {
                    return Err(WireError::Version(version[0]).into());
                }
                Ok(Incoming::Events(EventsRx { stream }))
            }
            other => Err(WireError::StreamType(other).into()),
        }
    }
}
