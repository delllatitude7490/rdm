mod app;
mod backend;
mod theme;

fn main() -> iced::Result {
    iced::application(app::App::boot, app::App::update, app::App::view)
        .title("RDM — Download quietly")
        .theme(theme::theme)
        .subscription(app::App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(820.0, 700.0),
            min_size: Some(iced::Size::new(640.0, 640.0)),
            exit_on_close_request: false,
            ..Default::default()
        })
        .run()
}
