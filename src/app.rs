use crate::capture::{self, Frame, Recording, Setup};
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
}

#[derive(Clone, Debug)]
pub enum Message {
    Frame(Frame),
    TakePhoto,
    ToggleRecording,
    RecordingStarted(Result<String, String>),
    RecordingStopped(Result<String, String>),
}

pub struct AppModel {
    core: cosmic::Core,
    pipeline: gst::Pipeline,
    setup: Arc<Setup>,
    is_v4l2: bool,
    last_frame: Option<Frame>,
    preview: Option<widget::image::Handle>,
    recording_slot: Arc<Mutex<Option<Recording>>>,
    is_recording: bool,
    busy: bool,
    status: String,
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

    fn init(core: cosmic::Core, flags: Self::Flags) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let is_v4l2 = flags.setup.is_v4l2();
        let status = flags.setup.describe();
        *FRAME_RX.lock().expect("FRAME_RX mutex poisoned") = Some(flags.frame_rx);
        let model = AppModel {
            core,
            pipeline: flags.pipeline,
            setup: Arc::new(flags.setup),
            is_v4l2,
            last_frame: None,
            preview: None,
            recording_slot: Arc::new(Mutex::new(None)),
            is_recording: false,
            busy: false,
            status,
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
            cosmic::iced::stream::channel(16, |mut output: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
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
                    self.last_frame = Some(frame);
                }
                Task::none()
            }

            Message::TakePhoto => {
                if let Some(frame) = &self.last_frame {
                    match capture::save_photo(frame) {
                        Ok(path) => self.status = format!("Saved {}", path.display()),
                        Err(e) => self.status = format!("Photo failed: {e}"),
                    }
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
        }
    }
}
