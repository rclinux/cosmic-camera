mod app;
mod capture;

fn main() -> anyhow::Result<()> {
    let context = capture::bootstrap()?;

    let settings = cosmic::app::Settings::default().size_limits(
        cosmic::iced::Limits::NONE
            .min_width(480.0)
            .min_height(420.0),
    );

    let flags = app::Flags {
        pipeline: context.pipeline,
        setup: context.setup,
        frame_rx: context.frame_rx,
        frame_tx: context.frame_tx,
        photo_frame: context.photo_frame,
    };

    cosmic::app::run::<app::AppModel>(settings, flags).map_err(|e| anyhow::anyhow!("{e}"))
}
