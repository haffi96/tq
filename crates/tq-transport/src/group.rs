//! Sending and receiving group streams.

use std::{collections::VecDeque, time::Duration};

use bytes::{Bytes, BytesMut};
use tq_wire::{code, now_us, FrameHeader, GroupHeader, MediaFrame, GROUP_HEADER_FIXED};
use web_transport_quinn::{RecvStream, SendStream, Session};

/// How a sender trades completeness for latency.
#[derive(Debug, Clone, Copy)]
pub struct SendPolicy {
    /// Older groups allowed to keep delivering after a new group starts. 0
    /// resets the previous group's stream the moment a keyframe arrives
    /// (realtime subscribers, owner uplinks); smooth subscribers can afford to let one or two
    /// finish.
    pub keep_groups: usize,
    /// A frame older than this (since it arrived on this node) is not sent;
    /// the rest of its group is dropped and the receiver resumes at the next
    /// keyframe.
    pub max_lag: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// Not sendable (waiting for a keyframe, or not the next frame).
    Skipped,
    /// Fell behind; the group was abandoned. Worth a keyframe request.
    Late,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SenderStats {
    pub frames: u64,
    pub bytes: u64,
    pub groups: u64,
    pub skipped: u64,
    pub late_groups: u64,
}

/// Writes one track's frames to a session, one stream per group. Streams
/// are ordered by the track's priority, then newest group first.
pub struct GroupSender {
    session: Session,
    policy: SendPolicy,
    track_priority: u8,
    current: Option<(u64, SendStream)>,
    next_index: u32,
    previous: VecDeque<SendStream>,
    waiting_key: bool,
    pub stats: SenderStats,
}

/// Stream priority of a group: the track's priority in the top bits, then
/// newer groups first. The group part wraps after 2^22 groups; a sender
/// keeps at most a few groups of a track open, so a wrap only briefly
/// reorders those. Below [`crate::events::PRIORITY_URGENT`] always.
pub fn priority(track_priority: u8, group: u64) -> i32 {
    ((track_priority as i32) << 22) | (group & 0x3f_ffff) as i32
}

impl GroupSender {
    pub fn new(session: Session, policy: SendPolicy, track_priority: u8) -> Self {
        Self {
            session,
            policy,
            track_priority,
            current: None,
            next_index: 0,
            previous: VecDeque::new(),
            waiting_key: true,
            stats: SenderStats::default(),
        }
    }

    /// Drops the rest of the current group and waits for the next keyframe
    /// (the subscriber lagged behind its source).
    pub fn abandon(&mut self) {
        if let Some((_, mut s)) = self.current.take() {
            let _ = s.reset(code::GROUP_LATE);
            self.stats.late_groups += 1;
        }
        self.waiting_key = true;
    }

    /// Sends `frame`, which arrived on this node at `arrival_us`.
    pub async fn send(&mut self, frame: &MediaFrame, arrival_us: u64) -> anyhow::Result<Outcome> {
        let age = Duration::from_micros(now_us().saturating_sub(arrival_us));
        if frame.is_key() && frame.header.index == 0 {
            self.start_group(frame).await?;
            // A keyframe is the freshest picture there is; give it the full
            // budget whatever its age.
            return self.write(frame, self.policy.max_lag).await;
        }
        let in_group = matches!(self.current, Some((g, _)) if g == frame.group);
        if self.waiting_key || !in_group || frame.header.index != self.next_index {
            self.stats.skipped += 1;
            return Ok(Outcome::Skipped);
        }
        if age >= self.policy.max_lag {
            self.abandon();
            self.stats.skipped += 1;
            return Ok(Outcome::Late);
        }
        self.write(frame, self.policy.max_lag - age).await
    }

    async fn start_group(&mut self, frame: &MediaFrame) -> anyhow::Result<()> {
        if let Some((_, mut old)) = self.current.take() {
            if self.policy.keep_groups == 0 {
                let _ = old.reset(code::GROUP_SUPERSEDED);
            } else {
                let _ = old.finish();
                self.previous.push_back(old);
            }
        }
        while self.previous.len() > self.policy.keep_groups {
            if let Some(mut s) = self.previous.pop_front() {
                let _ = s.reset(code::GROUP_SUPERSEDED);
            }
        }
        let stream = self.session.open_uni().await?;
        let _ = stream.set_priority(priority(self.track_priority, frame.group));
        let mut stream = stream;
        let mut buf = BytesMut::with_capacity(64);
        frame.group_header().encode(&mut buf);
        stream.write_all(&buf).await?;
        self.current = Some((frame.group, stream));
        self.next_index = 0;
        self.waiting_key = false;
        self.stats.groups += 1;
        Ok(())
    }

    async fn write(&mut self, frame: &MediaFrame, budget: Duration) -> anyhow::Result<Outcome> {
        let Some((_, stream)) = self.current.as_mut() else {
            return Ok(Outcome::Skipped);
        };
        let mut header = BytesMut::with_capacity(tq_wire::FRAME_HEADER_LEN + 4);
        frame.header.encode(frame.payload.len(), &mut header);
        let mut chunks = [header.freeze(), frame.payload.clone()];
        match tokio::time::timeout(budget, stream.write_all_chunks(&mut chunks)).await {
            Ok(Ok(())) => {
                self.next_index = frame.header.index + 1;
                self.stats.frames += 1;
                self.stats.bytes += frame.payload.len() as u64;
                Ok(Outcome::Sent)
            }
            Ok(Err(e)) => {
                // The receiver stopped this group (it moved on); not fatal.
                if matches!(e, web_transport_quinn::WriteError::Stopped(_)) {
                    self.current = None;
                    self.waiting_key = true;
                    return Ok(Outcome::Skipped);
                }
                Err(e.into())
            }
            Err(_) => {
                self.abandon();
                Ok(Outcome::Late)
            }
        }
    }
}

/// Reads a group stream: the header, then frames until the stream ends.
pub struct GroupReader {
    stream: RecvStream,
    pub header: GroupHeader,
}

impl GroupReader {
    pub async fn open(mut stream: RecvStream) -> anyhow::Result<Self> {
        let mut fixed = [0u8; GROUP_HEADER_FIXED];
        stream.read_exact(&mut fixed).await?;
        Self::open_fixed(stream, fixed).await
    }

    /// Continues after the fixed part of the header (see
    /// [`crate::events::Incoming`]).
    pub async fn open_fixed(mut stream: RecvStream, fixed: [u8; GROUP_HEADER_FIXED]) -> anyhow::Result<Self> {
        let (mut header, codec_len) = GroupHeader::decode_fixed(&fixed)?;
        let mut codec = vec![0u8; codec_len];
        stream.read_exact(&mut codec).await?;
        header.codec = String::from_utf8(codec).map_err(|_| tq_wire::WireError::Codec)?;
        Ok(Self { stream, header })
    }

    /// The next frame, or `None` once the sender finished the group. A reset
    /// stream surfaces as an error.
    pub async fn next(&mut self) -> anyhow::Result<Option<(FrameHeader, Bytes)>> {
        let mut len = [0u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(()) => {}
            Err(web_transport_quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = FrameHeader::check_len(u32::from_be_bytes(len))?;
        let mut buf = BytesMut::zeroed(len);
        self.stream.read_exact(&mut buf).await?;
        let header = FrameHeader::decode(&buf)?;
        let payload = buf.split_off(tq_wire::FRAME_HEADER_LEN).freeze();
        Ok(Some((header, payload)))
    }

    /// Tells the sender to stop sending this group.
    pub fn stop(&mut self) {
        let _ = self.stream.stop(code::GROUP_SUPERSEDED);
    }
}
