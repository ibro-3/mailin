//! The milestone 5 window: a grid with a header and a WebView2.
//!
//! This is intentionally small. It proves that the projection can create a
//! window, attach the WinUI default resources, lay out native controls and host
//! WebView2 content. Milestone 6 replaces the static page with the real
//! three-pane shell.

use std::sync::Mutex;

use anyhow::{Context, Result};
use mailin_winui_sys::{
    Application, Grid, GridLength, GridUnitType, IPanel, ResourceDictionary, RowDefinition,
    TextBlock, WebView2, Window, XamlControlsResources,
};
use windows_core::Interface;

use crate::app::AppState;
use crate::html;

/// WinUI keeps the window alive while it is visible; holding a reference here
/// also makes it possible to close the window from tests or future commands.
static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

/// Create and activate the main window.
pub fn create(state: &AppState) -> Result<Window> {
    install_control_resources()?;

    let root = Grid::new().context("could not create the root grid")?;
    add_rows(&root)?;

    let header = TextBlock::new().context("could not create the header")?;
    header
        .SetText(&format!("Mailin — {}", account_summary(state)))
        .context("could not set the header text")?;
    header
        .SetFontSize(18.0)
        .context("could not style the header")?;
    Grid::SetRow(&header, 0).context("could not place the header")?;

    let webview = WebView2::new().context("could not create the WebView2 control")?;
    webview
        .NavigateToString(&html::document(state))
        .context("could not render the welcome page")?;
    Grid::SetRow(&webview, 1).context("could not place the WebView2 control")?;

    let panel: IPanel = root.cast().context("the grid is not a panel")?;
    let children = panel
        .Children()
        .context("could not read the grid children")?;
    children
        .Append(&header)
        .context("could not append the header")?;
    children
        .Append(&webview)
        .context("could not append the WebView2 control")?;

    let window = Window::new().context("could not create the WinUI window")?;
    window
        .SetTitle("Mailin")
        .context("could not set the window title")?;
    window
        .SetContent(&root)
        .context("could not set the window content")?;

    // `app.rs` selects `DispatcherShutdownMode::OnLastWindowClose`, so the XAML
    // message loop ends when this window is closed; no `Closed` handler needed.
    window.Activate().context("could not show the window")?;
    tracing::info!("WinUI window activated");

    if let Ok(mut slot) = WINDOW.lock() {
        *slot = Some(window.clone());
    }
    Ok(window)
}

/// Add the WinUI default styles. Without `XamlControlsResources` the templated
/// controls (including WebView2) have no style and render as empty rectangles.
fn install_control_resources() -> Result<()> {
    let application = Application::Current().context("no current XAML application")?;
    let resources =
        ResourceDictionary::new().context("could not create the resource dictionary")?;
    resources
        .MergedDictionaries()
        .context("could not read the merged dictionaries")?
        .Append(&XamlControlsResources::new().context(
            "could not create XamlControlsResources; is the Windows App SDK runtime installed?",
        )?)
        .context("could not add XamlControlsResources")?;
    application
        .SetResources(&resources)
        .context("could not install the application resources")
}

/// Two rows: an auto-sized header and a star-sized WebView2.
fn add_rows(grid: &Grid) -> Result<()> {
    let rows = grid
        .RowDefinitions()
        .context("could not read the grid rows")?;

    let header = RowDefinition::new().context("could not create the header row")?;
    header
        .SetHeight(GridLength {
            value: 0.0,
            grid_unit_type: GridUnitType::Auto,
        })
        .context("could not size the header row")?;
    rows.Append(&header)
        .context("could not add the header row")?;

    let content = RowDefinition::new().context("could not create the content row")?;
    content
        .SetHeight(GridLength {
            value: 1.0,
            grid_unit_type: GridUnitType::Star,
        })
        .context("could not size the content row")?;
    rows.Append(&content)
        .context("could not add the content row")?;
    Ok(())
}

/// One-line description of the local store, shown above the WebView.
fn account_summary(state: &AppState) -> String {
    match state.mailin.list_accounts() {
        Ok(accounts) if accounts.is_empty() => "no accounts yet".to_owned(),
        Ok(accounts) if accounts.len() == 1 => "1 account".to_owned(),
        Ok(accounts) => format!("{} accounts", accounts.len()),
        Err(error) => format!("account list unavailable: {error}"),
    }
}
