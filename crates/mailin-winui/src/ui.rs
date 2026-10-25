//! Small helpers for composing WinUI controls from Rust.
//!
//! There is no XAML compiler in a plain Cargo build, so the window is built from
//! constructor calls and a handful of declarative fragments parsed by
//! `XamlReader::Load`. Every helper returns `anyhow::Result` because WinRT
//! activation can fail (for example when the Windows App SDK runtime is
//! missing); callers can then decide whether to abort window creation.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use mailin_winui_sys::{
    Button, ColumnDefinition, ComboBox, ComboBoxItem, FontWeight, FrameworkElement, Grid,
    GridLength, GridUnitType, HorizontalAlignment, IButtonBase, IContentControl, IControl,
    IItemsControl, IPanel, ISelector, ITextBox, IUIElement, Orientation, RowDefinition,
    ScrollBarVisibility, ScrollViewer, StackPanel, TextBlock, TextBox, TextWrapping, Thickness,
    VerticalAlignment, Visibility, Window, XamlReader,
};
use windows_core::{EventRevoker, Interface};

/// The default namespace used by every runtime XAML fragment.
const XMLNS: &str = "http://schemas.microsoft.com/winfx/2006/xaml/presentation";

/// XAML keeps event handlers and the window alive in a cycle. Keep an explicit
/// reference as well so a top-level window cannot be collected while the XAML
/// message loop is still running.
static WINDOWS: Mutex<Vec<(u64, Window)>> = Mutex::new(Vec::new());
static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

/// Remember a top-level window while it is open. Returns a key for [`release`]
/// so dialog windows can be dropped once they close; the main window keeps its
/// key for the process lifetime.
pub fn keep_alive(window: &Window) -> u64 {
    let id = NEXT_WINDOW.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut windows) = WINDOWS.lock() {
        windows.push((id, window.clone()));
    }
    id
}

/// Drop the strong reference taken by [`keep_alive`].
pub fn release(id: u64) {
    if let Ok(mut windows) = WINDOWS.lock() {
        windows.retain(|(key, _)| *key != id);
    }
}

/// Event subscriptions that live as long as their window.
///
/// `EventRevoker::drop` unsubscribes, so the tokens must be stored somewhere
/// owned by the same UI object. Lists that are rebuilt clear their [`Revokers`]
/// first; dialogs clear theirs from the `Closed` callback.
#[derive(Clone, Default)]
pub struct Revokers(Rc<RefCell<Vec<EventRevoker>>>);

impl Revokers {
    pub fn keep(&self, revoker: EventRevoker) {
        self.0.borrow_mut().push(revoker);
    }

    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

/// Parse a small XAML fragment and cast it to the expected WinRT type.
pub fn load<T: Interface>(xaml: &str) -> Result<T> {
    let value = XamlReader::Load(xaml).context("could not parse an inline XAML fragment")?;
    value
        .cast()
        .context("the inline XAML fragment has an unexpected type")
}

/// A plain text block, wrapped by default so labels survive narrow panes.
pub fn text(value: &str) -> Result<TextBlock> {
    let block: TextBlock = load(&format!(
        "<TextBlock xmlns=\"{XMLNS}\" Text=\"\" TextWrapping=\"Wrap\" />"
    ))?;
    block.SetText(value)?;
    Ok(block)
}

/// A heading-sized text block. `weight` follows the OpenType scale (400, 600,
/// 700); the XAML accessibility tree still sees it as ordinary text.
pub fn heading(value: &str, size: Option<f64>, weight: Option<u16>) -> Result<TextBlock> {
    let block = text(value)?;
    if let Some(size) = size {
        block.SetFontSize(size)?;
    }
    if let Some(weight) = weight {
        block.SetFontWeight(FontWeight { weight })?;
    }
    Ok(block)
}

/// A button whose content is a text block, so the label wraps and can be styled.
pub fn button(label: &str) -> Result<Button> {
    let button = Button::new().context("could not create a button")?;
    let content = text(label)?;
    content.SetTextWrapping(TextWrapping::NoWrap)?;
    set_content(&button, &content)?;
    let control: IControl = button.cast()?;
    control.SetHorizontalContentAlignment(HorizontalAlignment::Left)?;
    control.SetVerticalContentAlignment(VerticalAlignment::Center)?;
    Ok(button)
}

/// A button whose content is an arbitrary element, used for rich list rows.
pub fn content_button(content: &impl Interface) -> Result<Button> {
    let button = Button::new().context("could not create a button")?;
    set_content(&button, content)?;
    let control: IControl = button.cast()?;
    control.SetHorizontalContentAlignment(HorizontalAlignment::Stretch)?;
    control.SetVerticalContentAlignment(VerticalAlignment::Center)?;
    Ok(button)
}

/// Replace a text button's label, keeping its wrapping text block.
pub fn set_button_label(button: &Button, label: &str) -> Result<()> {
    let content = text(label)?;
    content.SetTextWrapping(TextWrapping::NoWrap)?;
    set_content(button, &content)
}

/// Attach a click handler. The closure runs on the UI thread; the button's
/// lifetime keeps it alive for as long as the control exists.
pub fn on_click(button: &Button, handler: impl Fn() + 'static) -> Result<EventRevoker> {
    let base: IButtonBase = button.cast()?;
    Ok(base.Click(move |_, _| handler())?)
}

/// Attach a `TextChanged` handler to a text box.
pub fn on_text_changed(textbox: &TextBox, handler: impl Fn() + 'static) -> Result<EventRevoker> {
    let textbox: ITextBox = textbox.cast()?;
    Ok(textbox.TextChanged(move |_, _| handler())?)
}

/// Attach a `SelectionChanged` handler to a selector (for example a combo box).
pub fn on_selection_changed(
    selector: &impl Interface,
    handler: impl Fn() + 'static,
) -> Result<EventRevoker> {
    let selector: ISelector = selector.cast()?;
    Ok(selector.SelectionChanged(move |_, _| handler())?)
}

/// Create a vertically or horizontally stacked panel with a uniform gap.
pub fn stack(orientation: Orientation, spacing: f64) -> Result<StackPanel> {
    let panel = StackPanel::new().context("could not create a stack panel")?;
    panel.SetOrientation(orientation)?;
    panel.SetSpacing(spacing)?;
    Ok(panel)
}

/// Append an element to a panel.
pub fn append(panel: &impl Interface, child: &impl Interface) -> Result<()> {
    let panel: IPanel = panel.cast()?;
    let child: mailin_winui_sys::UIElement = child.cast()?;
    panel
        .Children()
        .context("could not read a panel's children")?
        .Append(&child)?;
    Ok(())
}

/// Remove every child of a panel. Used to rebuild folder and message lists.
pub fn clear(panel: &impl Interface) -> Result<()> {
    let panel: IPanel = panel.cast()?;
    panel
        .Children()
        .context("could not read a panel's children")?
        .Clear()?;
    Ok(())
}

/// Remove every item from an items control such as a combo box.
pub fn clear_items(control: &impl Interface) -> Result<()> {
    let control: IItemsControl = control.cast()?;
    control
        .Items()
        .context("could not read an items control")?
        .Clear()?;
    Ok(())
}

/// Append a text item to a combo box.
pub fn add_combo_item(combo: &ComboBox, label: &str) -> Result<()> {
    let item = ComboBoxItem::new().context("could not create a combo box item")?;
    let content = text(label)?;
    set_content(&item, &content)?;
    let control: IItemsControl = combo.cast()?;
    control
        .Items()
        .context("could not read combo box items")?
        .Append(&item)?;
    Ok(())
}

/// Put an element inside a content control such as a button or scroll viewer.
pub fn set_content(control: &impl Interface, content: &impl Interface) -> Result<()> {
    let control: IContentControl = control.cast()?;
    let content: windows_core::IInspectable = content.cast()?;
    control.SetContent(&content)?;
    Ok(())
}

/// Wrap an element in a vertical scroll viewer.
pub fn scroll(content: &impl Interface) -> Result<ScrollViewer> {
    let viewer: ScrollViewer = load(&format!("<ScrollViewer xmlns=\"{XMLNS}\" />"))?;
    viewer.SetHorizontalScrollBarVisibility(ScrollBarVisibility::Disabled)?;
    viewer.SetVerticalScrollBarVisibility(ScrollBarVisibility::Auto)?;
    set_content(&viewer, content)?;
    Ok(viewer)
}

/// Create a combo box and fill it with text items.
pub fn combo(labels: &[&str], selected: usize) -> Result<ComboBox> {
    let combo = ComboBox::new().context("could not create a combo box")?;
    let items: IItemsControl = combo.cast()?;
    let items = items.Items().context("could not read combo box items")?;
    for label in labels {
        let item = ComboBoxItem::new().context("could not create a combo box item")?;
        let content = text(label)?;
        set_content(&item, &content)?;
        items.Append(&item)?;
    }
    if !labels.is_empty() {
        let selector: ISelector = combo.cast()?;
        let index = selected.min(labels.len() - 1);
        selector.SetSelectedIndex(index as i32)?;
    }
    Ok(combo)
}

/// Read the selected index of a selector.
pub fn selected_index(selector: &impl Interface) -> usize {
    selector
        .cast::<ISelector>()
        .and_then(|selector| selector.SelectedIndex())
        .map(|index| index.max(0) as usize)
        .unwrap_or_default()
}

/// Select an item by index, ignoring failures on a detached control.
pub fn select_index(selector: &impl Interface, index: usize) {
    if let Ok(selector) = selector.cast::<ISelector>() {
        let _ = selector.SetSelectedIndex(index as i32);
    }
}

/// Set a grid child's row.
pub fn set_row(element: &impl Interface, row: i32) -> Result<()> {
    let element: FrameworkElement = element.cast()?;
    Grid::SetRow(&element, row)?;
    Ok(())
}

/// Set a grid child's column.
pub fn set_column(element: &impl Interface, column: i32) -> Result<()> {
    let element: FrameworkElement = element.cast()?;
    Grid::SetColumn(&element, column)?;
    Ok(())
}

/// Append a row definition. `value` is ignored for auto rows.
pub fn add_row(grid: &Grid, unit: GridUnitType, value: f64) -> Result<()> {
    let row: RowDefinition = load(&format!("<RowDefinition xmlns=\"{XMLNS}\" />"))?;
    row.SetHeight(GridLength {
        value,
        grid_unit_type: unit,
    })?;
    grid.RowDefinitions()
        .context("could not read grid rows")?
        .Append(&row)?;
    Ok(())
}

/// Append a column definition. `value` is ignored for auto columns.
pub fn add_column(grid: &Grid, unit: GridUnitType, value: f64) -> Result<()> {
    let column: ColumnDefinition = load(&format!("<ColumnDefinition xmlns=\"{XMLNS}\" />"))?;
    column.SetWidth(GridLength {
        value,
        grid_unit_type: unit,
    })?;
    grid.ColumnDefinitions()
        .context("could not read grid columns")?
        .Append(&column)?;
    Ok(())
}

/// Convenience wrappers for the three grid sizing modes.
pub fn auto_row(grid: &Grid) -> Result<()> {
    add_row(grid, GridUnitType::Auto, 0.0)
}

pub fn star_row(grid: &Grid) -> Result<()> {
    add_row(grid, GridUnitType::Star, 1.0)
}

pub fn star_column(grid: &Grid) -> Result<()> {
    add_column(grid, GridUnitType::Star, 1.0)
}

/// Pixel-sized column, used for the folder and message panes.
pub fn pixel_column(grid: &Grid, width: f64) -> Result<()> {
    add_column(grid, GridUnitType::Pixel, width)
}

/// Show or hide an element without removing it from its parent.
pub fn set_visible(element: &impl Interface, visible: bool) -> Result<()> {
    let element: IUIElement = element.cast()?;
    element.SetVisibility(if visible {
        Visibility::Visible
    } else {
        Visibility::Collapsed
    })?;
    Ok(())
}

/// Enable or disable an interactive control.
pub fn set_enabled(control: &impl Interface, enabled: bool) -> Result<()> {
    let control: IControl = control.cast()?;
    control.SetIsEnabled(enabled)?;
    Ok(())
}

/// Set a uniform margin around a framework element.
pub fn margin(
    element: &impl Interface,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
) -> Result<()> {
    let element: FrameworkElement = element.cast()?;
    element.SetMargin(Thickness {
        left,
        top,
        right,
        bottom,
    })?;
    Ok(())
}

/// Give a framework element a minimum width.
pub fn min_width(element: &impl Interface, width: f64) -> Result<()> {
    let element: FrameworkElement = element.cast()?;
    element.SetMinWidth(width)?;
    Ok(())
}

/// Give a framework element a minimum height.
pub fn min_height(element: &impl Interface, height: f64) -> Result<()> {
    let element: FrameworkElement = element.cast()?;
    element.SetMinHeight(height)?;
    Ok(())
}

/// Stretch a framework element horizontally in its parent.
pub fn stretch_horizontal(element: &impl Interface) -> Result<()> {
    let element: FrameworkElement = element.cast()?;
    element.SetHorizontalAlignment(HorizontalAlignment::Stretch)?;
    Ok(())
}

/// The default padding used for list rows and form fields.
pub fn padding(
    element: &impl Interface,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
) -> Result<()> {
    let control: IControl = element.cast()?;
    control.SetPadding(Thickness {
        left,
        top,
        right,
        bottom,
    })?;
    Ok(())
}

/// Create a text box with a placeholder. The caller decides whether it accepts
/// newlines with [`TextBox::SetAcceptsReturn`].
pub fn text_box(placeholder: &str) -> Result<mailin_winui_sys::TextBox> {
    let textbox = mailin_winui_sys::TextBox::new().context("could not create a text box")?;
    textbox.SetPlaceholderText(placeholder)?;
    Ok(textbox)
}

/// Create a password box. The type has no activation factory in the filtered
/// projection, so it comes from XAML.
pub fn password_box(placeholder: &str) -> Result<mailin_winui_sys::PasswordBox> {
    let password: mailin_winui_sys::PasswordBox =
        load(&format!("<PasswordBox xmlns=\"{XMLNS}\" />"))?;
    password.SetPlaceholderText(placeholder)?;
    Ok(password)
}

/// Stack a small caption above a control, the form layout used by the account
/// wizard and the composer.
pub fn field(label: &str, control: &impl Interface) -> Result<StackPanel> {
    let row = stack(Orientation::Vertical, 4.0)?;
    let caption = heading(label, Some(12.0), Some(600))?;
    append(&row, &caption)?;
    append(&row, control)?;
    Ok(row)
}

/// Read the current text of a text box.
pub fn text_value(textbox: &impl Interface) -> String {
    textbox
        .cast::<ITextBox>()
        .and_then(|textbox| textbox.Text())
        .unwrap_or_default()
}

/// Read the current password of a password box.
pub fn password_value(password: &mailin_winui_sys::PasswordBox) -> String {
    password.Password().unwrap_or_default()
}
