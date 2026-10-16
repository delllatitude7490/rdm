use iced::{Color, Theme};

pub const MUTED: Color = Color::from_rgb(0.62, 0.66, 0.72);
pub const ACCENT: Color = Color::from_rgb(0.46, 0.84, 0.72);
pub const ERROR: Color = Color::from_rgb(0.96, 0.60, 0.60);

pub fn theme(_: &crate::app::App) -> Theme {
    Theme::custom(
        "RDM".to_owned(),
        iced::theme::Palette {
            background: Color::from_rgb(0.075, 0.085, 0.10),
            text: Color::from_rgb(0.94, 0.95, 0.97),
            primary: ACCENT,
            success: ACCENT,
            warning: Color::from_rgb(0.90, 0.73, 0.43),
            danger: ERROR,
        },
    )
}
