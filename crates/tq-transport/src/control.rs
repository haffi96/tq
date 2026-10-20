//! Length-prefixed JSON control messages over a bidirectional stream.

use std::sync::Arc;

use bytes::BytesMut;
use tokio::sync::Mutex;
use tq_proto::Control;
use web_transport_quinn::{RecvStream, SendStream};

/// Cloneable writer half of a control stream.
#[derive(Clone)]
pub struct ControlTx {
    inner: Arc<Mutex<SendStream>>,
}

impl ControlTx {
    /// Takes the send half of the control stream and puts it ahead of
    /// video ([`crate::events::PRIORITY_URGENT`]).
    pub fn new(stream: SendStream) -> Self {
        let _ = stream.set_priority(crate::events::PRIORITY_URGENT);
        Self {
            inner: Arc::new(Mutex::new(stream)),
        }
    }

    pub async fn send(&self, msg: &Control) -> anyhow::Result<()> {
        let mut buf = BytesMut::new();
        msg.encode(&mut buf);
        let mut s = self.inner.lock().await;
        s.write_all(&buf).await?;
        Ok(())
    }
}

pub struct ControlRx {
    stream: RecvStream,
}

impl ControlRx {
    pub fn new(stream: RecvStream) -> Self {
        Self { stream }
    }

    /// Next message; `None` when the peer finished the stream.
    pub async fn recv(&mut self) -> anyhow::Result<Option<Control>> {
        let mut len = [0u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(()) => {}
            Err(web_transport_quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = Control::check_len(u32::from_be_bytes(len))?;
        let mut json = vec![0u8; len];
        self.stream.read_exact(&mut json).await?;
        Ok(Some(Control::decode(&json)?))
    }
}
