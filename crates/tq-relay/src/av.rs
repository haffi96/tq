//! Per-AV state on the relay.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::watch;
use tq_proto::{
    BuildInfo, CameraInfo, CameraStatus, Control, PublisherStatus, QuicStats, RelayState, Role, TeleopStatus,
};
use tq_transport::{control::ControlTx, track::Track, Session};

pub struct PublisherLink {
    pub conn_id: u64,
    pub publisher_id: String,
    pub build: Option<BuildInfo>,
    pub session: Session,
    pub ctl: ControlTx,
    pub cameras: Vec<CameraInfo>,
    pub camera_status: Vec<CameraStatus>,
    pub teleop: TeleopStatus,
    pub rtt_ms: Option<f64>,
    pub clock: tq_proto::ClockFilter,
    pub quic: Option<QuicStats>,
}

pub struct ClientLink {
    pub role: Role,
    pub sub: String,
    /// From the client's `hello`; `None` until then (or ever, before 1.1).
    pub build: Option<BuildInfo>,
    pub session: Session,
    pub ctl: ControlTx,
}

#[derive(Default)]
pub struct AvInner {
    pub publisher: Option<PublisherLink>,
    /// Tracks by camera id; they outlive publisher reconnects so viewers
    /// resume without resubscribing.
    pub tracks: HashMap<String, Arc<Track>>,
    /// Camera list from the last announce (index = group stream camera).
    pub cameras: Vec<CameraInfo>,
    pub operator: Option<String>,
    pub clients: HashMap<String, ClientLink>,
    pub keyframe_requests: HashMap<String, Instant>,
}

pub struct Av {
    pub id: String,
    pub inner: Mutex<AvInner>,
    /// Bumped on every change a browser should see.
    pub version: watch::Sender<u64>,
    /// Bumped when the camera list changes (subscribers restart).
    pub cameras_version: watch::Sender<u64>,
}

const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

impl Av {
    pub fn new(id: String) -> Arc<Self> {
        Arc::new(Self {
            id,
            inner: Mutex::new(AvInner::default()),
            version: watch::channel(0).0,
            cameras_version: watch::channel(0).0,
        })
    }

    pub fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    pub fn state(&self) -> RelayState {
        let inner = self.inner.lock().unwrap();
        RelayState {
            av_id: self.id.clone(),
            publisher: inner.publisher.as_ref().map(|p| PublisherStatus {
                publisher_id: p.publisher_id.clone(),
                build: p.build.clone(),
                cameras: p.cameras.clone(),
                camera_status: p.camera_status.clone(),
                teleop: p.teleop.clone(),
                rtt_ms: p.rtt_ms,
                clock_offset_us: p.clock.estimate().map(|(o, _)| o),
                clock_error_us: p.clock.estimate().map(|(_, e)| e),
                quic: p.quic.clone(),
            }),
            operator: inner.operator.clone(),
            viewers: inner.clients.values().filter(|c| c.role == Role::Viewer).count(),
            operators: inner.clients.values().filter(|c| c.role == Role::Operator).count(),
        }
    }

    /// Cameras and their tracks, in announce order.
    pub fn tracks(&self) -> Vec<(u16, CameraInfo, Arc<Track>)> {
        let inner = self.inner.lock().unwrap();
        inner
            .cameras
            .iter()
            .enumerate()
            .filter_map(|(i, c)| inner.tracks.get(&c.id).map(|t| (i as u16, c.clone(), t.clone())))
            .collect()
    }

    /// Tells the publisher which session holds the operator claim.
    pub fn notify_operator(&self) {
        let (ctl, session) = {
            let inner = self.inner.lock().unwrap();
            (inner.publisher.as_ref().map(|p| p.ctl.clone()), inner.operator.clone())
        };
        if let Some(ctl) = ctl {
            tokio::spawn(async move {
                let _ = ctl.send(&Control::Operator { session }).await;
            });
        }
    }

    /// Forwards a keyframe request to the publisher, at most one per camera
    /// per 500 ms.
    pub fn request_keyframe(&self, camera: &str, reason: &str) {
        let ctl = {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            if inner
                .keyframe_requests
                .get(camera)
                .is_some_and(|t| now - *t < KEYFRAME_REQUEST_INTERVAL)
            {
                return;
            }
            inner.keyframe_requests.insert(camera.to_string(), now);
            inner.publisher.as_ref().map(|p| p.ctl.clone())
        };
        if let Some(ctl) = ctl {
            let msg = Control::KeyframeRequest {
                camera: camera.to_string(),
                reason: reason.to_string(),
            };
            tokio::spawn(async move {
                let _ = ctl.send(&msg).await;
            });
        }
    }
}
