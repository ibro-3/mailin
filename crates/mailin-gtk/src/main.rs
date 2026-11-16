//! Mailin GTK 4 front-end.

mod account_dialog;
mod app;
mod autoconfig;
mod background;
mod compose;
mod dialogs;
mod format;
mod html;
mod move_dialog;
mod pgp_keys;
mod preferences;
mod window;

fn main() -> gtk::glib::ExitCode {
    let paths = mailin_core::Paths::discover().ok();
    if let Some(paths) = &paths {
        if let Err(error) =
            mailin_core::logging::init_logging(paths, "mailin_core=info,mailin_gtk=info")
        {
            eprintln!("Mailin: could not initialize the log file: {error}");
        }
        mailin_core::logging::install_panic_hook(paths);
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "mailin_core=info,mailin_gtk=info".into()),
            )
            .with_target(false)
            .init();
    }

    app::run()
}
