//! Mailin WinUI 3 front-end.
//!
//! The Windows client uses the same UI-agnostic `mailin-core` store and engine
//! as the GTK client. This crate composes the WinUI shell from Rust over the
//! generated Windows App SDK projection in `mailin-winui-sys`; there is no C#
//! project and no XAML compiler in the build. The GTK client remains the
//! supported Linux front-end; this binary only builds and runs on Windows.

#[cfg(windows)]
mod account_dialog;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod bootstrap;
#[cfg(windows)]
mod compose;
#[cfg(windows)]
mod format;
#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod window;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "mailin_core=info,mailin_winui=info".into()),
            )
            .with_target(false)
            .init();
        app::run()
    }

    #[cfg(not(windows))]
    {
        anyhow::bail!("mailin-winui is a Windows front-end; use mailin-gtk on this platform")
    }
}
