//! Mailin GTK 4 front-end.

mod account_dialog;
mod app;
mod autoconfig;
mod compose;
mod format;
mod html;
mod preferences;
mod window;

fn main() -> gtk::glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mailin_core=info,mailin_gtk=info".into()),
        )
        .with_target(false)
        .init();

    app::run()
}
