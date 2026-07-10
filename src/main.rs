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
