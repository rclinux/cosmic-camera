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

use crate::capture::{self, CameraId, CameraInfo, Frame, PhotoSlot, Recording, Setup};
use cosmic::app::context_drawer;
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget;
use cosmic::Task;
use gstreamer as gst;
use gstreamer::prelude::ElementExt;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// `Subscription::run` only accepts a capture-free `fn` pointer, so the
/// frame receiver (moved out of `Flags` once, in `init`) lives here instead
/// of on `AppModel` where a closure could capture it.
static FRAME_RX: Mutex<Option<watch::Receiver<Frame>>> = Mutex::new(None);

pub struct Flags {
    pub pipeline: gst::Pipeline,
    pub setup: Setup,
    pub frame_rx: watch::Receiver<Frame>,
    pub frame_tx: watch::Sender<Frame>,
    pub photo_frame: PhotoSlot,
}

#[derive(Clone, Debug)]
pub enum Message {
    Frame(Frame),
    TakePhoto,
    ToggleRecording,
    RecordingStarted(Result<String, String>),
    RecordingStopped(Result<String, String>),
    ToggleSettings,
    SelectCamera(usize),
    SelectMode(usize),
    ApplySettings,
}

pub struct AppModel {
    core: cosmic::Core,
    pipeline: gst::Pipeline,
    setup: Arc<Setup>,
    is_v4l2: bool,
    /// Latest full-resolution sample, shared with the capture callback; photos
    /// are decoded from here on demand so they keep the selected resolution
    /// even though the on-screen preview is downscaled.
    photo_frame: PhotoSlot,
    preview: Option<widget::image::Handle>,
    recording_slot: Arc<Mutex<Option<Recording>>>,
    is_recording: bool,
    busy: bool,
    status: String,
    // --- settings drawer: camera / resolution selection ---
    frame_tx: watch::Sender<Frame>,
    cameras: Vec<CameraInfo>,
    camera_labels: Vec<String>,
    selected_camera: Option<usize>,
    selected_mode: Option<usize>,
}

impl AppModel {
    /// Modes advertised by the currently selected camera (empty for the
    /// portal/auto entry, which negotiates its own resolution).
    fn current_modes(&self) -> &[capture::Mode] {
        self.selected_camera
            .and_then(|i| self.cameras.get(i))
            .map(|c| c.modes.as_slice())
            .unwrap_or(&[])
    }
}

/// Finds the camera list index matching the currently active capture setup,
/// so the dropdown opens on the right entry.
fn index_for_setup(cameras: &[CameraInfo], setup: &Setup) -> Option<usize> {
    let target = match setup {
        Setup::Portal { .. } => CameraId::Portal,
        Setup::V4l2 { device } => CameraId::V4l2(device.clone()),
    };
    cameras.iter().position(|c| c.id == target)
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = "io.github.rclinux.CosmicCamera";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        vec![
            widget::button::icon(widget::icon::from_name("emblem-system-symbolic").size(18))
                .padding(8)
                .on_press(Message::ToggleSettings)
                .into(),
        ]
    }

    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }
        let spacing = cosmic::theme::spacing();

        let camera_dd = widget::dropdown(
            self.camera_labels.clone(),
            self.selected_camera,
            Message::SelectCamera,
        );

        let mode_labels: Vec<String> = self.current_modes().iter().map(|m| m.label()).collect();
        let has_modes = !mode_labels.is_empty();
        let mode_dd = widget::dropdown(mode_labels, self.selected_mode, Message::SelectMode);

        let resolution_note = if has_modes {
            widget::text::caption("Pick a resolution, or leave unset for automatic.")
        } else {
            widget::text::caption(
                "This source negotiates its own resolution — no manual modes to pick.",
            )
        };

        let apply = widget::button::text("Apply")
            .class(cosmic::style::Button::Suggested)
            .on_press_maybe((!self.is_recording).then_some(Message::ApplySettings));

        let recording_warn = self
            .is_recording
            .then(|| widget::text::caption("Stop the recording before switching cameras."));

        let content = widget::column::with_capacity(7)
            .push(widget::text::heading("Camera"))
            .push(camera_dd)
            .push(widget::text::heading("Resolution"))
            .push(mode_dd)
            .push(resolution_note)
            .push(apply)
            .push_maybe(recording_warn)
            .spacing(spacing.space_s);

        Some(
            context_drawer::context_drawer(content, Message::ToggleSettings)
                .title("Camera settings"),
        )
    }

    fn init(core: cosmic::Core, flags: Self::Flags) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let is_v4l2 = flags.setup.is_v4l2();
        let status = flags.setup.describe();
        *FRAME_RX.lock().expect("FRAME_RX mutex poisoned") = Some(flags.frame_rx);

        let cameras = capture::enumerate_cameras();
        let camera_labels = cameras.iter().map(|c| c.label.clone()).collect();
        let selected_camera = index_for_setup(&cameras, &flags.setup);

        let model = AppModel {
            core,
            pipeline: flags.pipeline,
            setup: Arc::new(flags.setup),
            is_v4l2,
            photo_frame: flags.photo_frame,
            preview: None,
            recording_slot: Arc::new(Mutex::new(None)),
            is_recording: false,
            busy: false,
            status,
            frame_tx: flags.frame_tx,
            cameras,
            camera_labels,
            selected_camera,
            selected_mode: None,
        };
        (model, Task::none())
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let spacing = cosmic::theme::spacing();

        let preview: Element<_> = if let Some(handle) = &self.preview {
            widget::image(handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(cosmic::iced::ContentFit::Contain)
                .into()
        } else {
            widget::container(widget::text::body("Waiting for camera…"))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .into()
        };

        let preview_card = widget::container(preview)
            .width(Length::Fixed(720.0))
            .height(Length::Fixed(480.0))
            .class(cosmic::style::Container::Card);

        let photo_button = widget::button::icon(widget::icon::from_name("camera-photo-symbolic").size(22))
            .padding(16)
            .class(cosmic::style::Button::Suggested)
            .on_press(Message::TakePhoto);

        let (record_icon, record_class) = if self.is_recording {
            ("media-playback-stop-symbolic", cosmic::style::Button::Destructive)
        } else {
            ("media-record-symbolic", cosmic::style::Button::Standard)
        };
        let record_button = widget::button::icon(widget::icon::from_name(record_icon).size(22))
            .padding(16)
            .class(record_class)
            .on_press_maybe((!self.busy).then_some(Message::ToggleRecording));

        let controls = widget::row::with_capacity(2)
            .push(photo_button)
            .push(record_button)
            .spacing(spacing.space_m)
            .align_y(Alignment::Center);

        let status_text = if self.is_recording {
            format!("● Recording — {}", self.status)
        } else {
            self.status.clone()
        };
        let status = widget::text::caption(status_text);

        let content = widget::column::with_capacity(3)
            .push(preview_card)
            .push(controls)
            .push(status)
            .spacing(spacing.space_m)
            .align_x(Alignment::Center);

        widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .padding(spacing.space_l)
            .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::run(|| {
            cosmic::iced::stream::channel(1, |mut output: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                use cosmic::iced::futures::SinkExt;
                let taken = FRAME_RX.lock().expect("FRAME_RX mutex poisoned").take();
                if let Some(mut rx) = taken {
                    // `changed()` only resolves when a *newer* frame has
                    // landed, so if the UI falls behind for a moment it
                    // naturally skips the stale ones in between instead of
                    // queueing them up.
                    while rx.changed().await.is_ok() {
                        let frame = rx.borrow_and_update().clone();
                        if output.send(Message::Frame(frame)).await.is_err() {
                            break;
                        }
                    }
                } else {
                    // Another (discarded) instance of this closure already
                    // owns the receiver; park so we don't busy-loop.
                    std::future::pending::<()>().await;
                }
            })
        })
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::Frame(frame) => {
                if frame.width > 0 && frame.height > 0 {
                    self.preview = Some(widget::image::Handle::from_rgba(
                        frame.width,
                        frame.height,
                        frame.rgba.to_vec(),
                    ));
                }
                Task::none()
            }

            Message::TakePhoto => {
                // Decode the latest full-resolution sample on demand, so the
                // photo keeps the selected resolution (the preview is smaller).
                let latest = self.photo_frame.lock().ok().and_then(|g| g.clone());
                match latest.as_ref().and_then(capture::frame_from_sample) {
                    Some(frame) => match capture::save_photo(&frame) {
                        Ok(path) => self.status = format!("Saved {}", path.display()),
                        Err(e) => self.status = format!("Photo failed: {e}"),
                    },
                    None => self.status = "No frame captured yet".to_string(),
                }
                Task::none()
            }

            Message::ToggleRecording => {
                if self.busy {
                    return Task::none();
                }
                self.busy = true;

                if self.is_recording {
                    let recording_slot = self.recording_slot.clone();
                    let pipeline = self.pipeline.clone();
                    let is_v4l2 = self.is_v4l2;
                    Task::perform(
                        async move {
                            let rec = recording_slot
                                .lock()
                                .expect("recording_slot mutex poisoned")
                                .take();
                            let result = match rec {
                                Some(rec) => tokio::task::spawn_blocking(move || capture::stop_recording(rec))
                                    .await
                                    .map_err(|e| e.to_string())
                                    .and_then(|r| r.map_err(|e| e.to_string()))
                                    .map(|p| p.display().to_string()),
                                None => Err("not currently recording".to_string()),
                            };
                            if is_v4l2 {
                                let _ = pipeline.set_state(gst::State::Playing);
                            }
                            result
                        },
                        |result| cosmic::Action::App(Message::RecordingStopped(result)),
                    )
                } else {
                    if self.is_v4l2 {
                        // Release the device so the recording pipeline can open it.
                        let _ = self.pipeline.set_state(gst::State::Null);
                    }
                    let setup = self.setup.clone();
                    let recording_slot = self.recording_slot.clone();
                    Task::perform(
                        async move {
                            match capture::start_recording(&setup).await {
                                Ok(rec) => {
                                    let path = rec.path.display().to_string();
                                    *recording_slot
                                        .lock()
                                        .expect("recording_slot mutex poisoned") = Some(rec);
                                    Ok(path)
                                }
                                Err(e) => Err(e.to_string()),
                            }
                        },
                        |result| cosmic::Action::App(Message::RecordingStarted(result)),
                    )
                }
            }

            Message::RecordingStarted(result) => {
                self.busy = false;
                match result {
                    Ok(path) => {
                        self.is_recording = true;
                        self.status = format!("Recording to {path}");
                    }
                    Err(e) => {
                        self.status = format!("Recording failed: {e}");
                        if self.is_v4l2 {
                            let _ = self.pipeline.set_state(gst::State::Playing);
                        }
                    }
                }
                Task::none()
            }

            Message::RecordingStopped(result) => {
                self.busy = false;
                self.is_recording = false;
                match result {
                    Ok(path) => self.status = format!("Saved {path}"),
                    Err(e) => self.status = format!("Recording error: {e}"),
                }
                Task::none()
            }

            Message::ToggleSettings => {
                self.set_show_context(!self.core.window.show_context);
                Task::none()
            }

            Message::SelectCamera(i) => {
                self.selected_camera = Some(i);
                // Different camera → its old mode index is meaningless.
                self.selected_mode = None;
                Task::none()
            }

            Message::SelectMode(i) => {
                self.selected_mode = Some(i);
                Task::none()
            }

            Message::ApplySettings => {
                if self.is_recording {
                    self.status = "Stop the recording before switching cameras.".to_string();
                    return Task::none();
                }
                let Some(cam_idx) = self.selected_camera else {
                    return Task::none();
                };
                let Some(cam) = self.cameras.get(cam_idx) else {
                    return Task::none();
                };
                let id = cam.id.clone();
                let mode = self
                    .selected_mode
                    .and_then(|m| cam.modes.get(m))
                    .cloned();

                // Tear down the live preview so the device is free for the
                // replacement pipeline to open.
                let _ = self.pipeline.set_state(gst::State::Null);

                // Drop stale imagery so a photo can't capture the old camera's
                // last frame before the new one arrives.
                self.preview = None;
                if let Ok(mut slot) = self.photo_frame.lock() {
                    *slot = None;
                }

                match capture::build_selected(
                    &id,
                    mode.as_ref(),
                    self.frame_tx.clone(),
                    self.photo_frame.clone(),
                ) {
                    Ok((pipeline, setup)) => {
                        self.is_v4l2 = setup.is_v4l2();
                        self.status = setup.describe();
                        self.setup = Arc::new(setup);
                        self.pipeline = pipeline;
                        self.set_show_context(false);
                    }
                    Err(e) => {
                        self.status = format!("Couldn't switch camera: {e}");
                        // Best effort: bring the previous pipeline back to life.
                        let _ = self.pipeline.set_state(gst::State::Playing);
                    }
                }
                Task::none()
            }
        }
    }
}
