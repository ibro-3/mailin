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
    AutomationProperties, Button, ColumnDefinition, ComboBox, ComboBoxItem, FontWeight,
    FrameworkElement, Grid, GridLength, GridUnitType, HorizontalAlignment, IButtonBase,
    IContentControl, IControl, IItemsControl, IPanel, ISelector, ITextBox, IUIElement,
    KeyboardAccelerator, Orientation, RowDefinition, ScrollBarVisibility, ScrollViewer,
    SolidColorBrush, StackPanel, TextBlock, TextBox, TextWrapping, Thickness,
    VerticalAlignment, Visibility, Window, XamlReader, XamlRoot,
};
use windows_core::{EventRevoker, Interface};

use crate::design_tokens;

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
/// 700); `heading_level` sets the accessibility heading level (Level1-Level9).
pub fn heading(
    value: &str,
    size: Option<f64>,
    weight: Option<u16>,
    heading_level: Option<i32>,
) -> Result<TextBlock> {
    let block = text(value)?;
    if let Some(size) = size {
        block.SetFontSize(size)?;
    }
    if let Some(weight) = weight {
        block.SetFontWeight(FontWeight { weight })?;
    }
    if let Some(level) = heading_level {
        AutomationProperties::SetHeadingLevel(&block, mailin_winui_sys::AutomationHeadingLevel(level))?;
    }
    Ok(block)
}

/// Heading 1 — page title (24pt, bold, Level1)
pub fn heading1(value: &str) -> Result<TextBlock> {
    let tokens = design_tokens::tokens();
    heading(
        value,
        Some(tokens.typography.h1_size),
        Some(tokens.typography.weight_bold),
        Some(1),
    )
}

/// Heading 2 — page section (20pt, semibold, Level2)
pub fn heading2(value: &str) -> Result<TextBlock> {
    let tokens = design_tokens::tokens();
    heading(
        value,
        Some(tokens.typography.h2_size),
        Some(tokens.typography.weight_semibold),
        Some(2),
    )
}

/// Heading 3 — dialog section (18pt, semibold, Level3)
pub fn heading3(value: &str) -> Result<TextBlock> {
    let tokens = design_tokens::tokens();
    heading(
        value,
        Some(tokens.typography.h3_size),
        Some(tokens.typography.weight_semibold),
        Some(3),
    )
}

/// Subheading — list section (16pt, semibold, Level4)
pub fn subheading(value: &str) -> Result<TextBlock> {
    let tokens = design_tokens::tokens();
    heading(
        value,
        Some(tokens.typography.subheading_size),
        Some(tokens.typography.weight_semibold),
        Some(4),
    )
}

/// Caption / small label (12pt, semibold)
pub fn caption(value: &str) -> Result<TextBlock> {
    let tokens = design_tokens::tokens();
    heading(
        value,
        Some(tokens.typography.caption_size),
        Some(tokens.typography.weight_semibold),
        None,
    )
}

/// Body text (14pt, normal)
pub fn body_text(value: &str) -> Result<TextBlock> {
    let tokens = design_tokens::tokens();
    heading(value, Some(tokens.typography.body_size), None, None)
}

/// A button whose content is a text block, so the label wraps and can be styled.
/// `accessible_name` sets the AutomationProperties.Name (for icon-only buttons).
/// `accessible_description` sets the AutomationProperties.HelpText.
pub fn button(
    label: &str,
    accessible_name: Option<&str>,
    accessible_description: Option<&str>,
) -> Result<Button> {
    let button = Button::new().context("could not create a button")?;
    let content = text(label)?;
    content.SetTextWrapping(TextWrapping::NoWrap)?;
    set_content(&button, &content)?;
    let control: IControl = button.cast()?;
    control.SetHorizontalContentAlignment(HorizontalAlignment::Left)?;
    control.SetVerticalContentAlignment(VerticalAlignment::Center)?;
    if let Some(name) = accessible_name {
        AutomationProperties::SetName(&button, name)?;
    }
    if let Some(desc) = accessible_description {
        AutomationProperties::SetHelpText(&button, desc)?;
    }
    Ok(button)
}

/// A button whose content is an arbitrary element, used for rich list rows.
/// `accessible_name` sets the AutomationProperties.Name (required for icon-only buttons).
/// `accessible_description` sets the AutomationProperties.HelpText.
pub fn content_button(
    content: &impl Interface,
    accessible_name: Option<&str>,
    accessible_description: Option<&str>,
) -> Result<Button> {
    let button = Button::new().context("could not create a button")?;
    set_content(&button, content)?;
    let control: IControl = button.cast()?;
    control.SetHorizontalContentAlignment(HorizontalAlignment::Stretch)?;
    control.SetVerticalContentAlignment(VerticalAlignment::Center)?;
    if let Some(name) = accessible_name {
        AutomationProperties::SetName(&button, name)?;
    }
    if let Some(desc) = accessible_description {
        AutomationProperties::SetHelpText(&button, desc)?;
    }
    Ok(button)
}

/// Replace a text button's label, keeping its wrapping text block.
pub fn set_button_label(button: &Button, label: &str) -> Result<()> {
    let content = text(label)?;
    content.SetTextWrapping(TextWrapping::NoWrap)?;
    set_content(button, &content)
}

/// A text button tinted with the platform destructive colour, used for
/// actions like removing an account.
/// `accessible_name` sets the AutomationProperties.Name.
/// `accessible_description` sets the AutomationProperties.HelpText.
pub fn destructive_button(
    label: &str,
    accessible_name: Option<&str>,
    accessible_description: Option<&str>,
) -> Result<Button> {
    let button = button(label, accessible_name, accessible_description)?;
    let content: IContentControl = button.cast()?;
    let content: TextBlock = content.Content()?.cast()?;
    let brush = design_tokens::tokens().destructive_brush()?;
    set_foreground(&content, &brush)?;
    Ok(button)
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

/// Create a vertical stack with default extra-small spacing (4).
pub fn stack_xs(orientation: Orientation) -> Result<StackPanel> {
    let tokens = design_tokens::tokens();
    stack(orientation, tokens.spacing.xs)
}

/// Create a vertical stack with default small spacing (8).
pub fn stack_sm(orientation: Orientation) -> Result<StackPanel> {
    let tokens = design_tokens::tokens();
    stack(orientation, tokens.spacing.sm)
}

/// Create a vertical stack with default medium spacing (12).
pub fn stack_md(orientation: Orientation) -> Result<StackPanel> {
    let tokens = design_tokens::tokens();
    stack(orientation, tokens.spacing.md)
}

/// Create a vertical stack with default large spacing (16).
pub fn stack_lg(orientation: Orientation) -> Result<StackPanel> {
    let tokens = design_tokens::tokens();
    stack(orientation, tokens.spacing.lg)
}

/// Create a vertical stack with default extra-large spacing (24).
pub fn stack_xl(orientation: Orientation) -> Result<StackPanel> {
    let tokens = design_tokens::tokens();
    stack(orientation, tokens.spacing.xl)
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
/// `accessible_name` sets the AutomationProperties.Name.
pub fn combo(labels: &[&str], selected: usize, accessible_name: Option<&str>) -> Result<ComboBox> {
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
    if let Some(name) = accessible_name {
        AutomationProperties::SetName(&combo, name)?;
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

/// Star-sized column with minimum width (responsive).
/// Uses GridUnitType::Star with a MinWidth constraint.
pub fn star_column_min(grid: &Grid, min_width: f64, max_width: f64) -> Result<()> {
    let column: ColumnDefinition = load(&format!("<ColumnDefinition xmlns=\"{XMLNS}\" MinWidth=\"{min_width}\" />"))?;
    if max_width > 0.0 {
        column.SetMaxWidth(max_width)?;
    }
    column.SetWidth(GridLength {
        value: 1.0,
        grid_unit_type: GridUnitType::Star,
    })?;
    grid.ColumnDefinitions()
        .context("could not read grid columns")?
        .Append(&column)?;
    Ok(())
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

/// Set margin using design token spacing (all sides same).
pub fn margin_all(element: &impl Interface, spacing: f64) -> Result<()> {
    margin(element, spacing, spacing, spacing, spacing)
}

/// Set margin using design token extra-small (4).
pub fn margin_xs(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    margin_all(element, tokens.spacing.xs)
}

/// Set margin using design token small (8).
pub fn margin_sm(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    margin_all(element, tokens.spacing.sm)
}

/// Set margin using design token medium (12).
pub fn margin_md(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    margin_all(element, tokens.spacing.md)
}

/// Set margin using design token large (16).
pub fn margin_lg(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    margin_all(element, tokens.spacing.lg)
}

/// Set margin using design token extra-large (24).
pub fn margin_xl(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    margin_all(element, tokens.spacing.xl)
}

/// Set horizontal margin (left/right) using design token.
pub fn margin_horizontal(element: &impl Interface, spacing: f64) -> Result<()> {
    margin(element, spacing, 0.0, spacing, 0.0)
}

/// Set vertical margin (top/bottom) using design token.
pub fn margin_vertical(element: &impl Interface, spacing: f64) -> Result<()> {
    margin(element, 0.0, spacing, 0.0, spacing)
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

/// Set padding using design token spacing (all sides same).
pub fn padding_all(element: &impl Interface, spacing: f64) -> Result<()> {
    padding(element, spacing, spacing, spacing, spacing)
}

/// Set padding using design token extra-small (4).
pub fn padding_xs(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    padding_all(element, tokens.spacing.xs)
}

/// Set padding using design token small (8).
pub fn padding_sm(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    padding_all(element, tokens.spacing.sm)
}

/// Set padding using design token medium (12).
pub fn padding_md(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    padding_all(element, tokens.spacing.md)
}

/// Set padding using design token large (16).
pub fn padding_lg(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    padding_all(element, tokens.spacing.lg)
}

/// Set padding using design token extra-large (24).
pub fn padding_xl(element: &impl Interface) -> Result<()> {
    let tokens = design_tokens::tokens();
    padding_all(element, tokens.spacing.xl)
}

/// Create a text box with a placeholder. The caller decides whether it accepts
/// newlines with [`TextBox::SetAcceptsReturn`].
/// `accessible_name` sets the AutomationProperties.Name.
pub fn text_box(placeholder: &str, accessible_name: Option<&str>) -> Result<mailin_winui_sys::TextBox> {
    let textbox = mailin_winui_sys::TextBox::new().context("could not create a text box")?;
    textbox.SetPlaceholderText(placeholder)?;
    if let Some(name) = accessible_name {
        AutomationProperties::SetName(&textbox, name)?;
    }
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
/// wizard and the composer. Associates the caption with the control via
/// AutomationProperties.LabeledBy so screen readers announce the label.
pub fn field(label: &str, control: &impl Interface) -> Result<StackPanel> {
    let tokens = design_tokens::tokens();
    let row = stack(Orientation::Vertical, tokens.spacing.xs)?;
    let caption = caption(label)?;
    append(&row, &caption)?;
    append(&row, control)?;
    // Associate the caption (label) with the control for accessibility.
    let caption_element: IUIElement = caption.cast()?;
    let control_element: IUIElement = control.cast()?;
    AutomationProperties::SetLabeledBy(&control_element, &caption_element)?;
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

/// A labelled toggle row: title and subtitle on the left, the switch on the
/// right. Returns both the row (to append to a panel) and the switch (to read
/// the value back when saving).
/// `accessible_name` sets the AutomationProperties.Name on the toggle switch.
/// `accessible_on`/`accessible_off` set the On/Off labels for screen readers.
pub fn switch_row(
    title: &str,
    subtitle: &str,
    active: bool,
    accessible_name: Option<&str>,
    accessible_on: Option<&str>,
    accessible_off: Option<&str>,
) -> Result<(StackPanel, mailin_winui_sys::ToggleSwitch)> {
    let tokens = design_tokens::tokens();
    let row = stack(Orientation::Horizontal, tokens.spacing.sm)?;
    let labels = stack(Orientation::Vertical, tokens.spacing.xs)?;
    let title_block = text(title)?;
    title_block.SetFontWeight(tokens.semibold_weight())?;
    let subtitle_block = text(subtitle)?;
    let subtitle_element: IUIElement = subtitle_block.cast()?;
    subtitle_element.SetOpacity(tokens.colors.subtitle_opacity)?;
    append(&labels, &title_block)?;
    append(&labels, &subtitle_block)?;
    stretch_horizontal(&labels)?;
    append(&row, &labels)?;

    let toggle = mailin_winui_sys::ToggleSwitch::new().context("could not create a toggle")?;
    toggle.SetIsOn(active)?;
    // Set accessibility properties on the toggle switch
    if let Some(name) = accessible_name {
        AutomationProperties::SetName(&toggle, name)?;
    }
    if let Some(on_label) = accessible_on {
        toggle.SetOnContent(&text(on_label)?)?;
    }
    if let Some(off_label) = accessible_off {
        toggle.SetOffContent(&text(off_label)?)?;
    }
    let toggle_element: FrameworkElement = toggle.cast()?;
    toggle_element.SetVerticalAlignment(VerticalAlignment::Center)?;
    append(&row, &toggle)?;
    Ok((row, toggle))
}

/// A solid colour brush parsed from a `#RRGGBB` hex string, used for state
/// colours such as the outbox entry status.
pub fn solid_brush(hex: &str) -> Result<SolidColorBrush> {
    load(&format!(
        "<SolidColorBrush xmlns=\"{XMLNS}\" Color=\"{hex}\" />"
    ))
}

/// Colour a text block. Pair with [`solid_brush`].
pub fn set_foreground(block: &TextBlock, brush: &SolidColorBrush) -> Result<()> {
    block.SetForeground(brush)?;
    Ok(())
}

/// The XAML root of an open window. Dialogs (`ContentDialog`) need it before
/// they can be shown; it exists once the window's content is attached.
pub fn window_xaml_root(window: &Window) -> Result<XamlRoot> {
    // `XamlRoot` lives on `IUIElement`, which is what `Content()` returns
    // through the `UIElement` deref chain.
    let content: mailin_winui_sys::UIElement = window.Content()?;
    content
        .XamlRoot()
        .map_err(|error| anyhow::anyhow!("the window has no XAML root yet: {error}"))
}

/// Attach a keyboard accelerator to an element. Attach it to the window's
/// content root so the shortcut works wherever focus is inside the window.
/// `description` sets the KeyboardAccelerator.String for key tips / help.
pub fn add_keyboard_accelerator(
    element: &impl Interface,
    accelerator: &KeyboardAccelerator,
    description: Option<&str>,
) -> Result<()> {
    if let Some(desc) = description {
        accelerator.SetDescription(desc)?;
    }
    let element: IUIElement = element.cast()?;
    element.KeyboardAccelerators()?.Append(accelerator)?;
    Ok(())
}
