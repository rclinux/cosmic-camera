use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use ashpd::desktop::camera::Camera;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
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
            Setup::V4l2 { device } => format!("Direct V4L2 fallback · {device}"),
        }
    }

    pub fn is_v4l2(&self) -> bool {
        matches!(self, Setup::V4l2 { .. })
    }
}

fn build_preview_pipeline(
    source: &str,
    frame_tx: watch::Sender<Frame>,
) -> anyhow::Result<(gst::Pipeline, gst_app::AppSink)> {
    let pipeline_str = format!(
        "{source} ! videoconvert ! video/x-raw,format=RGBA ! \
         appsink name=video_sink sync=false max-buffers=1 drop=true"
    );
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("expected gst::parse::launch to return a Pipeline"))?;
    let appsink = pipeline
        .by_name("video_sink")
        .ok_or_else(|| anyhow!("video_sink element not found in pipeline"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("video_sink is not an AppSink"))?;

    // Diagnostic only: log the negotiated caps once, then measured (actual
    // delivery) FPS every 5 seconds, since negotiated framerate and actual
    // throughput can differ.
    static LOG_CAPS_ONCE: std::sync::Once = std::sync::Once::new();
    static FRAME_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static WINDOW_START: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let s = caps.structure(0).ok_or(gst::FlowError::Error)?;
                let width: i32 = s.get("width").map_err(|_| gst::FlowError::Error)?;
                let height: i32 = s.get("height").map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                LOG_CAPS_ONCE.call_once(|| {
                    println!("Negotiated preview caps: {}", caps.to_string());
                });
                {
                    use std::sync::atomic::Ordering;
                    let mut start = WINDOW_START.lock().expect("WINDOW_START mutex poisoned");
                    let now = std::time::Instant::now();
                    let started_at = *start.get_or_insert(now);
                    let count = FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    let elapsed = now.duration_since(started_at).as_secs_f64();
                    if elapsed >= 5.0 {
                        println!("Measured preview delivery rate: {:.1} fps over {:.1}s", count as f64 / elapsed, elapsed);
                        FRAME_COUNT.store(0, Ordering::Relaxed);
                        *start = Some(now);
                    }
                }

                let frame = Frame {
                    width: width as u32,
                    height: height as u32,
                    rgba: Arc::from(map.as_slice()),
                };
                // Ignore send errors: they just mean the UI side went away.
                let _ = frame_tx.send(frame);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    Ok((pipeline, appsink))
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

async fn try_portal_setup(frame_tx: watch::Sender<Frame>) -> anyhow::Result<(gst::Pipeline, Setup)> {
    let camera = Camera::new().await?;

    if !camera.is_present().await? {
        return Err(anyhow!("portal reports no camera present"));
    }

    camera.request_access().await?;
    let remote_fd = camera.open_pipe_wire_remote().await?;
    let fd = remote_fd.as_raw_fd();

    let (pipeline, _appsink) = build_preview_pipeline(&format!("pipewiresrc fd={fd}"), frame_tx)?;
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

fn try_v4l2_setup(frame_tx: watch::Sender<Frame>) -> anyhow::Result<(gst::Pipeline, Setup)> {
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
        let attempt = build_preview_pipeline(&format!("v4l2src device={device}"), frame_tx.clone())
            .and_then(|(pipeline, appsink)| match confirm_playing(&pipeline) {
                Ok(()) => Ok((pipeline, appsink)),
                Err(e) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    Err(e)
                }
            });

        match attempt {
            Ok((pipeline, _appsink)) => {
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

    let rt = tokio::runtime::Runtime::new()?;
    println!("Trying portal capture path (org.freedesktop.portal.Camera)...");
    let (pipeline, setup) = match rt.block_on(try_portal_setup(frame_tx.clone())) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Portal path failed: {e:#}");
            eprintln!("Falling back to direct V4L2 access:");
            try_v4l2_setup(frame_tx).context("both the portal and V4L2 capture paths failed")?
        }
    };
    println!("Capture path in use: {}", setup.describe());

    Ok(CaptureContext {
        pipeline,
        setup,
        frame_rx,
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
