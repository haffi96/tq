//! TCP data source: messages from a TCP server -> a data track. One message
//! per line (`lines`) or per `len:u32` big-endian prefix
//! (`length_prefixed`). A message's capture time is when it arrived here.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use tokio::{io::AsyncReadExt, net::TcpStream};
use tq::Publication;

use super::{SourceConfig, Stats};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// The largest message a data track carries, as a frame.
const MAX_MESSAGE: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Framing {
    Lines,
    LengthPrefixed,
}

impl Framing {
    pub fn from_config(c: &SourceConfig) -> anyhow::Result<Self> {
        let SourceConfig::Tcp {
            framing,
            codec,
            complete_on_vcl,
            ..
        } = c;
        anyhow::ensure!(
            codec.is_none() && !complete_on_vcl,
            "`codec` and `complete_on_vcl` are for video tracks"
        );
        Ok(match framing.as_deref().unwrap_or("lines") {
            "lines" => Framing::Lines,
            "length_prefixed" => Framing::LengthPrefixed,
            other => anyhow::bail!("unknown data framing '{other}' (lines, length_prefixed)"),
        })
    }
}

/// Reads messages from `addr` forever, reconnecting when it drops.
pub async fn run(id: String, addr: String, framing: Framing, encoding: String, p: Arc<Publication>, stats: Arc<Stats>) {
    let out = p.data(&id).expect("advertised from this config");
    let mut logged_down = false;
    loop {
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                logged_down = false;
                let _ = stream.set_nodelay(true);
                tracing::info!(track = %id, source = %addr, "source connected");
                p.set_online(&id, true, "");
                let reason = match read(stream, framing, &encoding, &id, &out, &stats).await {
                    Ok(()) => "source closed".to_string(),
                    Err(e) => format!("source error: {e}"),
                };
                tracing::warn!(track = %id, %reason);
                p.set_online(&id, false, &reason);
            }
            Err(e) => {
                if !logged_down {
                    tracing::warn!(track = %id, source = %addr, error = %e, "source unavailable, retrying every second");
                    logged_down = true;
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn read(
    mut stream: TcpStream,
    framing: Framing,
    encoding: &str,
    id: &str,
    out: &tq::DataWriter,
    stats: &Stats,
) -> anyhow::Result<()> {
    let mut buf = BytesMut::with_capacity(64 << 10);
    loop {
        // Every complete message in the buffer first.
        while let Some(msg) = next_message(&mut buf, framing)? {
            if msg.is_empty() {
                continue;
            }
            if encoding == "json" && serde_json::from_slice::<serde::de::IgnoredAny>(&msg).is_err() {
                tracing::warn!(track = %id, "not JSON, dropped: {:?}", String::from_utf8_lossy(&msg[..msg.len().min(80)]));
                continue;
            }
            stats.frames.fetch_add(1, Relaxed);
            out.send(msg);
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Ok(());
        }
    }
}

/// Takes the next whole message off the front of `buf`.
fn next_message(buf: &mut BytesMut, framing: Framing) -> anyhow::Result<Option<Bytes>> {
    match framing {
        Framing::Lines => {
            let Some(end) = buf.iter().position(|b| *b == b'\n') else {
                anyhow::ensure!(buf.len() <= MAX_MESSAGE, "line longer than {MAX_MESSAGE} bytes");
                return Ok(None);
            };
            let mut line = buf.split_to(end + 1);
            line.truncate(end);
            if line.last() == Some(&b'\r') {
                line.truncate(end - 1);
            }
            Ok(Some(line.freeze()))
        }
        Framing::LengthPrefixed => {
            if buf.len() < 4 {
                return Ok(None);
            }
            let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
            anyhow::ensure!(len <= MAX_MESSAGE, "message of {len} bytes is over {MAX_MESSAGE}");
            if buf.len() < 4 + len {
                return Ok(None);
            }
            let _ = buf.split_to(4);
            Ok(Some(buf.split_to(len).freeze()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_and_length_prefixed_messages() {
        let mut buf = BytesMut::from(&b"{\"a\":1}\r\n{\"b\":2}\n{\"c\""[..]);
        assert_eq!(next_message(&mut buf, Framing::Lines).unwrap().unwrap(), &b"{\"a\":1}"[..]);
        assert_eq!(next_message(&mut buf, Framing::Lines).unwrap().unwrap(), &b"{\"b\":2}"[..]);
        assert!(next_message(&mut buf, Framing::Lines).unwrap().is_none());
        assert_eq!(&buf[..], b"{\"c\"");

        let mut buf = BytesMut::from(&[0, 0, 0, 2, b'h', b'i', 0, 0, 0, 3, b'x'][..]);
        assert_eq!(next_message(&mut buf, Framing::LengthPrefixed).unwrap().unwrap(), &b"hi"[..]);
        assert!(next_message(&mut buf, Framing::LengthPrefixed).unwrap().is_none());
        let mut buf = BytesMut::from(&[0xff, 0xff, 0xff, 0xff][..]);
        assert!(next_message(&mut buf, Framing::LengthPrefixed).is_err());
    }
}
