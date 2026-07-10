// Cosmic Camera — a Wayland-native camera app for the COSMIC desktop.
// Copyright (C) 2026 Ronald Craig
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, version 3.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use ashpd::desktop::camera::Camera;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use image::{ImageBuffer, Rgba};
use tokio::sync::watch;

pub fn xdg_dir(env_var: &str, fallback_subdir: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env_var) {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(fallback_subdir)
}

pub fn pictures_dir() -> PathBuf {
    xdg_dir("XDG_PICTURES_DIR", "Pictures")
}

pub fn videos_dir() -> PathBuf {
    xdg_dir("XDG_VIDEOS_DIR", "Videos")
}

pub fn timestamp() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// A decoded RGBA video frame, cheap to clone (shared buffer) for passing
/// through the UI's message queue.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

impl Frame {
    /// Placeholder used only to seed the watch channel before the first
    /// real frame arrives; width 0 tells the UI not to render it.
    fn empty() -> Self {
        Frame {
            width: 0,
            height: 0,
            rgba: Arc::from(Vec::new().into_boxed_slice()),
        }
    }
}

/// Shared slot holding the latest full-resolution *sample*, updated by the
/// photo-branch callback and read only when a photo is actually taken. We
/// store the refcounted `gst::Sample` (cheap) rather than a decoded `Frame`,
/// so the capture thread doesn't copy a multi-megabyte full-res buffer on
/// every frame — that per-frame copy was enough to make the preview stutter.
/// Keeping this separate from the (downscaled) preview stream lets photos
/// honor the selected resolution while the preview stays smooth.
pub type PhotoSlot = std::sync::Arc<std::sync::Mutex<Option<gst::Sample>>>;

/// Largest width the live preview is rendered at. The preview card is only
/// ~720px wide, and re-uploading a full 1080p/1440p RGBA texture every frame
/// overruns the renderer (the image visibly blinks in and out). The pipeline
/// downscales the preview branch to this; the photo branch stays full-res.
const PREVIEW_MAX_WIDTH: u32 = 800;

/// Which capture backend is actually feeding the pipeline. Portal is the
/// correct Wayland-native path (sandboxable, works under Flatpak); V4l2 is
/// the direct-hardware fallback used when the portal path fails.
pub enum Setup {
    Portal {
        camera: Camera<'static>,
        // Must outlive the preview pipeline: dropping it closes the fd, and
        // it's unclear whether pipewiresrc dups the fd or takes ownership of
        // it, so we hold it until the pipeline itself is torn down.
        _preview_fd: OwnedFd,
    },
    V4l2 {
        device: String,
    },
}

impl Setup {
    pub fn describe(&self) -> String {
        match self {
            Setup::Portal { .. } => "Portal · PipeWire".to_string(),
            Setup::V4l2 { device } => format!("Direct V4L2 · {device}"),
        }
    }

    pub fn is_v4l2(&self) -> bool {
        matches!(self, Setup::V4l2 { .. })
    }
}

/// Identifies a capture source the user can pick in the settings drawer.
/// `Portal` is the auto/default path (the portal negotiates the device and
/// resolution itself); `V4l2` targets one specific `/dev/video*` node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CameraId {
    Portal,
    V4l2(String),
}

/// One selectable capture mode (a resolution + pixel format, and the best
/// framerate the device advertises for it). Framerate is optional: if the
/// device reports it as a range or an unreadable type we leave it out of the
/// caps and let GStreamer negotiate the highest available.
#[derive(Clone, Debug)]
pub struct Mode {
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub framerate: Option<(i32, i32)>,
}

impl Mode {
    fn fmt_label(&self) -> &str {
        match self.media_type.as_str() {
            "image/jpeg" => "MJPEG",
            "video/x-raw" => "Raw",
            other => other,
        }
    }

    pub fn label(&self) -> String {
        match self.framerate {
            Some((n, d)) if d != 0 => format!(
                "{}×{} · {} · {}fps",
                self.width,
                self.height,
                self.fmt_label(),
                (n as f64 / d as f64).round() as i64
            ),
            _ => format!("{}×{} · {}", self.width, self.height, self.fmt_label()),
        }
    }

    /// Caps fragment forced right after the source so the device negotiates
    /// this exact mode. Format is intentionally omitted so any raw pixel
    /// layout the device offers at this size still negotiates.
    fn caps_str(&self) -> String {
        let base = format!(
            "{},width={},height={}",
            self.media_type, self.width, self.height
        );
        match self.framerate {
            Some((n, d)) if d != 0 => format!("{base},framerate={n}/{d}"),
            _ => base,
        }
    }
}

/// A camera the user can select, with the discrete modes it advertises.
#[derive(Clone, Debug)]
pub struct CameraInfo {
    pub id: CameraId,
    pub label: String,
    pub modes: Vec<Mode>,
}

fn fps_val(fr: Option<(i32, i32)>) -> f64 {
    match fr {
        Some((n, d)) if d != 0 => n as f64 / d as f64,
        _ => 0.0,
    }
}

/// Highest framerate advertised for one caps structure. Handles the three
/// ways v4l2 reports it: a single fraction, a list of fractions, or a range.
fn best_framerate(s: &gst::StructureRef) -> Option<(i32, i32)> {
    if let Ok(f) = s.get::<gst::Fraction>("framerate") {
        return Some((f.numer(), f.denom()));
    }
    if let Ok(list) = s.get::<gst::List>("framerate") {
        let mut best = None;
        for v in list.iter() {
            if let Ok(f) = v.get::<gst::Fraction>() {
                let cand = Some((f.numer(), f.denom()));
                if fps_val(cand) > fps_val(best) {
                    best = cand;
                }
            }
        }
        return best;
    }
    if let Ok(range) = s.get::<gst::FractionRange>("framerate") {
        let f = range.max();
        return Some((f.numer(), f.denom()));
    }
    None
}

/// Pulls the `/dev/video*` path out of a monitored device's properties,
/// trying the keys different GStreamer versions use.
fn device_path(device: &gst::Device) -> Option<String> {
    let props = device.properties()?;
    for key in ["device.path", "api.v4l2.path", "object.path", "device"] {
        if let Ok(p) = props.get::<String>(key) {
            if p.starts_with("/dev/") {
                return Some(p);
            }
        }
    }
    None
}

/// Collapses a device's full caps into a deduplicated, largest-first list of
/// discrete modes (one per resolution+media-type, keeping the highest fps).
fn extract_modes(caps: Option<&gst::Caps>) -> Vec<Mode> {
    use std::collections::HashMap;
    let Some(caps) = caps else { return Vec::new() };
    let mut best: HashMap<(u32, u32, String), Option<(i32, i32)>> = HashMap::new();
    for s in caps.iter() {
        let name = s.name().to_string();
        if name != "video/x-raw" && name != "image/jpeg" {
            continue;
        }
        let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else {
            continue;
        };
        if w <= 0 || h <= 0 {
            continue;
        }
        let fr = best_framerate(s);
        let entry = best.entry((w as u32, h as u32, name)).or_insert(None);
        if fps_val(fr) > fps_val(*entry) {
            *entry = fr;
        }
    }
    let mut modes: Vec<Mode> = best
        .into_iter()
        .map(|((width, height, media_type), framerate)| Mode {
            media_type,
            width,
            height,
            framerate,
        })
        .collect();
    modes.sort_by(|a, b| {
        (b.width * b.height)
            .cmp(&(a.width * a.height))
            .then(b.media_type.cmp(&a.media_type))
    });
    modes
}

/// Enumerates selectable cameras. Entry 0 is always the auto/portal path so
/// the user can return to the default; any addressable `/dev/video*` devices
/// follow, each with its advertised modes.
pub fn enumerate_cameras() -> Vec<CameraInfo> {
    let mut cameras = vec![CameraInfo {
        id: CameraId::Portal,
        label: "Auto (portal / default)".to_string(),
        modes: Vec::new(),
    }];

    let monitor = gst::DeviceMonitor::new();
    let _ = monitor.add_filter(Some("Video/Source"), None);
    if monitor.start().is_ok() {
        for device in monitor.devices() {
            let Some(path) = device_path(&device) else {
                continue;
            };
            let label = format!("{} ({})", device.display_name(), path);
            let modes = extract_modes(device.caps().as_ref());
            cameras.push(CameraInfo {
                id: CameraId::V4l2(path),
                label,
                modes,
            });
        }
        monitor.stop();
    }
    cameras
}

/// Converts an appsink sample into a tightly packed, opaque RGBA `Frame`,
/// honoring the buffer's real row stride. (A raw map can pad rows out to a
/// hardware alignment; copying it verbatim would shear every row — the
/// "flashing garbage" some resolutions first produced.)
pub fn frame_from_sample(sample: &gst::Sample) -> Option<Frame> {
    let caps = sample.caps()?;
    let buffer = sample.buffer()?;
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let vframe = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let width = info.width() as usize;
    let height = info.height() as usize;
    let stride = info.stride()[0] as usize;
    let src = vframe.plane_data(0).ok()?;
    let tight = width * 4;
    if width == 0 || height == 0 || src.len() < stride * height {
        return None;
    }
    let mut rgba = Vec::with_capacity(tight * height);
    for row in 0..height {
        let start = row * stride;
        rgba.extend_from_slice(&src[start..start + tight]);
    }
    // Force fully opaque: a camera image has no meaningful alpha, and this
    // guards against any decode path that leaves alpha at 0 (transparent).
    for px in rgba.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some(Frame {
        width: width as u32,
        height: height as u32,
        rgba: Arc::from(rgba.into_boxed_slice()),
    })
}

fn appsink_by_name(pipeline: &gst::Pipeline, name: &str) -> anyhow::Result<gst_app::AppSink> {
    pipeline
        .by_name(name)
        .ok_or_else(|| anyhow!("{name} element not found in pipeline"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("{name} is not an AppSink"))
}

/// Builds a preview pipeline that splits (via `tee`) into two sinks: a
/// `videoscale`-downscaled `preview_sink` that drives the smooth on-screen
/// preview, and a full-resolution `photo_sink` whose latest frame is kept for
/// photo capture. Doing the downscale in GStreamer (not per-frame on the CPU)
/// keeps even a 1080p source smooth without the renderer choking on a huge
/// texture every frame, while photos still honor the selected resolution.
fn build_preview_pipeline(
    source: &str,
    frame_tx: watch::Sender<Frame>,
    photo_slot: PhotoSlot,
) -> anyhow::Result<gst::Pipeline> {
    let pipeline_str = format!(
        "{source} ! videoconvert ! video/x-raw,format=RGBA ! tee name=t \
         t. ! queue max-size-buffers=4 leaky=downstream ! videoscale ! \
              video/x-raw,width={PREVIEW_MAX_WIDTH},pixel-aspect-ratio=1/1 ! \
              appsink name=preview_sink sync=false max-buffers=1 drop=true \
         t. ! queue max-size-buffers=2 leaky=downstream ! \
              appsink name=photo_sink sync=false max-buffers=1 drop=true"
    );
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("expected gst::parse::launch to return a Pipeline"))?;

    // Scaled branch → the live UI preview.
    let preview_sink = appsink_by_name(&pipeline, "preview_sink")?;
    preview_sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                if let Some(frame) = frame_from_sample(&sample) {
                    // Ignore send errors: they just mean the UI went away.
                    let _ = frame_tx.send(frame);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Full-resolution branch → latest frame kept for photo capture.
    let photo_sink = appsink_by_name(&pipeline, "photo_sink")?;
    photo_sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                // Just stash the refcounted sample; decode to a Frame lazily,
                // only when a photo is captured.
                if let Ok(mut slot) = photo_slot.lock() {
                    *slot = Some(sample);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    Ok(pipeline)
}

/// Requests Playing and blocks until the pipeline actually gets there (or
/// fails), instead of trusting the immediate return value of `set_state`,
/// since negotiation failures can surface asynchronously.
fn confirm_playing(pipeline: &gst::Pipeline) -> anyhow::Result<()> {
    pipeline.set_state(gst::State::Playing)?;
    let (result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(3));
    result.map_err(|_| anyhow!("pipeline did not reach Playing within 3s"))?;
    Ok(())
}

/// How long to wait on the whole portal negotiation before giving up and
/// falling back. On desktops whose portal has no working Camera backend
/// (e.g. a plain Cinnamon/X11 box) the portal calls can block indefinitely
/// rather than erroring, which would otherwise hang the app before its
/// window ever appears.
const PORTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `try_portal_setup` with a hard timeout so a non-responsive Camera portal
/// can't wedge startup (or a settings-drawer "Auto" apply) forever.
async fn portal_setup_with_timeout(
    frame_tx: watch::Sender<Frame>,
    photo_slot: PhotoSlot,
) -> anyhow::Result<(gst::Pipeline, Setup)> {
    match tokio::time::timeout(PORTAL_TIMEOUT, try_portal_setup(frame_tx, photo_slot)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "portal negotiation timed out after {}s",
            PORTAL_TIMEOUT.as_secs()
        )),
    }
}

async fn try_portal_setup(
    frame_tx: watch::Sender<Frame>,
    photo_slot: PhotoSlot,
) -> anyhow::Result<(gst::Pipeline, Setup)> {
    let camera = Camera::new().await?;

    if !camera.is_present().await? {
        return Err(anyhow!("portal reports no camera present"));
    }

    camera.request_access().await?;
    let remote_fd = camera.open_pipe_wire_remote().await?;
    let fd = remote_fd.as_raw_fd();

    let pipeline = build_preview_pipeline(&format!("pipewiresrc fd={fd}"), frame_tx, photo_slot)?;
    if let Err(e) = confirm_playing(&pipeline) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(e);
    }

    Ok((
        pipeline,
        Setup::Portal {
            camera,
            _preview_fd: remote_fd,
        },
    ))
}

fn try_v4l2_setup(
    frame_tx: watch::Sender<Frame>,
    photo_slot: PhotoSlot,
) -> anyhow::Result<(gst::Pipeline, Setup)> {
    let mut devices: Vec<String> = std::fs::read_dir("/dev")
        .context("failed to list /dev for video devices")?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("video"))
        .collect();
    devices.sort();

    if devices.is_empty() {
        return Err(anyhow!("no /dev/video* devices found"));
    }

    let mut last_err = None;
    for name in devices {
        let device = format!("/dev/{name}");
        let attempt = build_preview_pipeline(
            &format!("v4l2src device={device}"),
            frame_tx.clone(),
            photo_slot.clone(),
        )
        .and_then(|pipeline| match confirm_playing(&pipeline) {
            Ok(()) => Ok(pipeline),
            Err(e) => {
                let _ = pipeline.set_state(gst::State::Null);
                Err(e)
            }
        });

        match attempt {
            Ok(pipeline) => {
                return Ok((pipeline, Setup::V4l2 { device }));
            }
            Err(e) => {
                eprintln!("  {device} not usable for capture: {e}");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no working /dev/video* device found")))
}

pub struct CaptureContext {
    pub pipeline: gst::Pipeline,
    pub setup: Setup,
    pub frame_rx: watch::Receiver<Frame>,
    /// Retained so the UI can build a *replacement* preview pipeline (on a
    /// camera/resolution change) that feeds the very same frame channel the
    /// running subscription already listens on.
    pub frame_tx: watch::Sender<Frame>,
    /// Latest full-resolution frame, for photo capture (see [`PhotoSlot`]).
    pub photo_frame: PhotoSlot,
}

/// Builds a fresh preview pipeline for a user-selected camera + mode, used
/// when applying a change from the settings drawer. Runs synchronously: the
/// V4L2 path is instant, and the portal path spins a throwaway runtime for
/// its async negotiation (same pattern as `bootstrap`). Returns the new
/// pipeline already confirmed Playing, plus the resulting `Setup`.
pub fn build_selected(
    id: &CameraId,
    mode: Option<&Mode>,
    frame_tx: watch::Sender<Frame>,
    photo_slot: PhotoSlot,
) -> anyhow::Result<(gst::Pipeline, Setup)> {
    match id {
        CameraId::V4l2(path) => {
            let source = match mode {
                // Decode only when the mode is compressed (MJPEG). Using
                // jpegdec directly (rather than decodebin) skips typefind and
                // dynamic-pad overhead, which keeps the preview steadier. Raw
                // modes need no decoder — videoconvert handles them.
                Some(m) if m.media_type == "image/jpeg" => {
                    format!("v4l2src device={path} ! {} ! jpegdec", m.caps_str())
                }
                Some(m) => format!("v4l2src device={path} ! {}", m.caps_str()),
                None => format!("v4l2src device={path}"),
            };
            let pipeline = build_preview_pipeline(&source, frame_tx, photo_slot)?;
            if let Err(e) = confirm_playing(&pipeline) {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(e);
            }
            Ok((pipeline, Setup::V4l2 { device: path.clone() }))
        }
        CameraId::Portal => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(portal_setup_with_timeout(frame_tx, photo_slot))
        }
    }
}

/// Runs the (async) portal negotiation and pipeline setup synchronously
/// before the UI starts, using a throwaway Tokio runtime dedicated to just
/// this bootstrap step. The cosmic app's own executor takes over for any
/// later async work (e.g. starting a recording).
pub fn bootstrap() -> anyhow::Result<CaptureContext> {
    gst::init()?;
    // A watch channel (not mpsc) is deliberate: the UI should always show
    // the latest frame, never a growing backlog of stale ones. mpsc's FIFO
    // queueing caused visible stutter when the UI briefly fell behind the
    // capture rate, since every queued frame still had to be drawn in order.
    let (frame_tx, frame_rx) = watch::channel(Frame::empty());
    let photo_frame: PhotoSlot = std::sync::Arc::new(std::sync::Mutex::new(None));

    let rt = tokio::runtime::Runtime::new()?;
    println!("Trying portal capture path (org.freedesktop.portal.Camera)...");
    let (pipeline, setup) =
        match rt.block_on(portal_setup_with_timeout(frame_tx.clone(), photo_frame.clone())) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("Portal path failed: {e:#}");
                eprintln!("Falling back to direct V4L2 access:");
                try_v4l2_setup(frame_tx.clone(), photo_frame.clone())
                    .context("both the portal and V4L2 capture paths failed")?
            }
        };
    println!("Capture path in use: {}", setup.describe());

    Ok(CaptureContext {
        pipeline,
        setup,
        frame_rx,
        frame_tx,
        photo_frame,
    })
}

pub fn save_photo(frame: &Frame) -> anyhow::Result<PathBuf> {
    let img: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_raw(frame.width, frame.height, frame.rgba.to_vec())
            .ok_or_else(|| anyhow!("frame size doesn't match RGBA buffer length"))?;

    let dir = pictures_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("cosmic-camera-{}.jpg", timestamp()?));
    image::DynamicImage::ImageRgba8(img).to_rgb8().save(&path)?;
    Ok(path)
}

/// A recording session: its own independent pipeline (separate from the
/// preview pipeline) so start/stop lifecycle and EOS finalization of the
/// webm container don't disturb the live preview.
pub struct Recording {
    pipeline: gst::Pipeline,
    pub path: PathBuf,
    _remote_fd: Option<OwnedFd>,
}

pub async fn start_recording(setup: &Setup) -> anyhow::Result<Recording> {
    let dir = videos_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("cosmic-camera-{}.webm", timestamp()?));

    let (source, remote_fd) = match setup {
        Setup::Portal { camera, .. } => {
            let remote_fd = camera.open_pipe_wire_remote().await?;
            let fd = remote_fd.as_raw_fd();
            (format!("pipewiresrc fd={fd}"), Some(remote_fd))
        }
        Setup::V4l2 { device } => (format!("v4l2src device={device}"), None),
    };

    let pipeline_str = format!(
        "{source} ! videoconvert ! vp8enc ! webmmux ! filesink location=\"{}\"",
        path.display()
    );
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("expected gst::parse::launch to return a Pipeline"))?;
    pipeline.set_state(gst::State::Playing)?;

    Ok(Recording {
        pipeline,
        path,
        _remote_fd: remote_fd,
    })
}

pub fn stop_recording(rec: Recording) -> anyhow::Result<PathBuf> {
    let bus = rec
        .pipeline
        .bus()
        .expect("pipeline should always have a bus");
    rec.pipeline.send_event(gst::event::Eos::new());

    let deadline = gst::ClockTime::from_seconds(5);
    if let Some(msg) =
        bus.timed_pop_filtered(deadline, &[gst::MessageType::Eos, gst::MessageType::Error])
    {
        if let gst::MessageView::Error(err) = msg.view() {
            eprintln!(
                "Error while finalizing recording: {} ({:?})",
                err.error(),
                err.debug()
            );
        }
    } else {
        eprintln!("Timed out waiting for recording to finalize; file may be truncated.");
    }

    rec.pipeline.set_state(gst::State::Null)?;
    Ok(rec.path)
}
