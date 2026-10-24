//! Mailin WinUI 3 front-end.
//!
//! Milestone 5 is a binding spike: prove that a generated Windows App SDK
//! projection, the App SDK bootstrapper, a XAML window and a WebView2 control
//! work together from plain Rust. The GTK client remains the supported Linux
//! front-end; this binary only builds and runs on Windows.

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod bootstrap;
#[cfg(windows)]
mod html;
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
