//! Design token system for WinUI front-end.
//!
//! This module provides semantic design tokens (colors, spacing, typography, radii, shadows)
//! to replace hardcoded values throughout the UI. Tokens can be swapped for high contrast
//! mode or custom themes.
//!
//! Pure color/contrast helpers compile on all targets (M3b-2) so Linux tests can
//! enforce WCAG contrast ratios.

#[cfg(windows)]
use mailin_winui_sys::FontWeight;

/// Parse `#RRGGBB` into linear-ish sRGB components for contrast math.
pub fn parse_hex(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()? as f64 / 255.0;
    let g = u8::from_str_radix(&h[2..4], 16).ok()? as f64 / 255.0;
    let b = u8::from_str_radix(&h[4..6], 16).ok()? as f64 / 255.0;
    Some((r, g, b))
}

fn to_linear(c: f64) -> f64 {
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn relative_luminance(hex: &str) -> Option<f64> {
    let (r, g, b) = parse_hex(hex)?;
    Some(0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b))
}

/// WCAG contrast ratio between two colors.
pub fn contrast_ratio(a: &str, b: &str) -> Option<f64> {
    let la = relative_luminance(a)?;
    let lb = relative_luminance(b)?;
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// Color tokens — semantic names that work in light and dark themes.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub destructive: &'static str,
    pub subtitle_opacity: f64,
    pub text_muted: &'static str,
    pub accent: &'static str,
    pub success: &'static str,
    pub warning: &'static str,
    pub error: &'static str,
    pub info: &'static str,
    pub on_accent: &'static str,
}

impl Colors {
    pub fn default() -> Self {
        Self {
            destructive: "#C42B1C",
            subtitle_opacity: 0.85,
            text_muted: "#6B6B6B",
            accent: "#0067C0",
            success: "#107C10",
            // #A05A00 passes ~4.6:1 on white (M3b-2); old #CC7700 failed AA.
            warning: "#A05A00",
            error: "#C42B1C",
            info: "#0067C0",
            on_accent: "#FFFFFF",
        }
    }

    pub fn dark() -> Self {
        Self {
            destructive: "#FF99A4",
            subtitle_opacity: 0.85,
            text_muted: "#C8C8C8",
            accent: "#4CC2FF",
            success: "#6CCB5F",
            warning: "#FCE100",
            error: "#FF99A4",
            info: "#4CC2FF",
            on_accent: "#000000",
        }
    }

    pub fn high_contrast() -> Self {
        Self {
            destructive: "#FF0000",
            subtitle_opacity: 1.0,
            text_muted: "#FFFFFF",
            accent: "#1AEBFF",
            success: "#3FF23F",
            warning: "#FFFF00",
            error: "#FF0000",
            info: "#1AEBFF",
            on_accent: "#000000",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Spacing {
    pub xs: f64,
    pub sm: f64,
    pub md: f64,
    pub lg: f64,
    pub xl: f64,
    pub xxl: f64,
}

impl Spacing {
    pub fn default() -> Self {
        Self {
            xs: 4.0,
            sm: 8.0,
            md: 12.0,
            lg: 16.0,
            xl: 24.0,
            xxl: 32.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Typography {
    pub caption_size: f64,
    pub body_size: f64,
    pub subheading_size: f64,
    pub h3_size: f64,
    pub h2_size: f64,
    pub h1_size: f64,
    pub weight_semibold: u16,
    pub weight_bold: u16,
}

impl Typography {
    pub fn default() -> Self {
        Self {
            caption_size: 12.0,
            body_size: 14.0,
            subheading_size: 16.0,
            h3_size: 18.0,
            h2_size: 20.0,
            h1_size: 24.0,
            weight_semibold: 600,
            weight_bold: 700,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Radii {
    pub sm: f64,
    pub md: f64,
    pub lg: f64,
}

impl Radii {
    pub fn default() -> Self {
        Self {
            sm: 4.0,
            md: 8.0,
            lg: 12.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Shadows {
    pub subtle: &'static str,
    pub medium: &'static str,
    pub strong: &'static str,
}

impl Shadows {
    pub fn default() -> Self {
        Self {
            subtle: "0 1px 2px rgba(0,0,0,0.05)",
            medium: "0 4px 8px rgba(0,0,0,0.1)",
            strong: "0 8px 16px rgba(0,0,0,0.15)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DesignTokens {
    pub colors: Colors,
    pub spacing: Spacing,
    pub typography: Typography,
    pub radii: Radii,
    pub shadows: Shadows,
}

impl DesignTokens {
    pub fn default() -> Self {
        Self {
            colors: Colors::default(),
            spacing: Spacing::default(),
            typography: Typography::default(),
            radii: Radii::default(),
            shadows: Shadows::default(),
        }
    }

    pub fn dark() -> Self {
        Self {
            colors: Colors::dark(),
            ..Self::default()
        }
    }

    pub fn high_contrast() -> Self {
        Self {
            colors: Colors::high_contrast(),
            ..Self::default()
        }
    }

    #[cfg(windows)]
    pub fn solid_brush(&self, hex: &str) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        use mailin_winui_sys::XamlReader;
        const XMLNS: &str = "http://schemas.microsoft.com/winfx/2006/xaml/presentation";
        let value = XamlReader::Load(&format!(
            "<SolidColorBrush xmlns=\"{XMLNS}\" Color=\"{hex}\" />"
        ))?;
        value.cast().map_err(Into::into)
    }

    #[cfg(windows)]
    pub fn destructive_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.destructive)
    }
    #[cfg(windows)]
    pub fn text_muted_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.text_muted)
    }
    #[cfg(windows)]
    pub fn accent_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.accent)
    }
    #[cfg(windows)]
    pub fn success_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.success)
    }
    #[cfg(windows)]
    pub fn warning_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.warning)
    }
    #[cfg(windows)]
    pub fn error_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.error)
    }
    #[cfg(windows)]
    pub fn info_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.info)
    }
    #[cfg(windows)]
    pub fn semibold_weight(&self) -> FontWeight {
        FontWeight {
            weight: self.typography.weight_semibold,
        }
    }
    #[cfg(windows)]
    pub fn bold_weight(&self) -> FontWeight {
        FontWeight {
            weight: self.typography.weight_bold,
        }
    }
}

#[cfg(windows)]
static TOKENS: std::sync::LazyLock<std::sync::RwLock<DesignTokens>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(DesignTokens::default()));

/// Current tokens snapshot (M3b-2: no `static mut`; safe to read from UI thread).
#[cfg(windows)]
pub fn tokens() -> DesignTokens {
    TOKENS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(windows)]
pub fn set_tokens(new_tokens: DesignTokens) {
    *TOKENS.write().unwrap_or_else(|e| e.into_inner()) = new_tokens;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

#[cfg(windows)]
pub fn apply_theme(mode: ThemeMode) {
    let high_contrast = mailin_winui_sys::AccessibilitySettings::new()
        .map(|s| s.HighContrast().unwrap_or(false))
        .unwrap_or(false);

    let new_tokens = if high_contrast {
        DesignTokens::high_contrast()
    } else {
        match mode {
            ThemeMode::Dark => DesignTokens::dark(),
            ThemeMode::System | ThemeMode::Light => DesignTokens::default(),
        }
    };
    set_tokens(new_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_contrast(fg: &str, bg: &str, min: f64, label: &str) {
        let ratio = contrast_ratio(fg, bg).expect(label);
        assert!(
            ratio >= min,
            "{label}: {fg} on {bg} contrast {ratio:.2}:1 < {min}:1"
        );
    }

    #[test]
    fn default_tokens_meet_wcag_aa_for_text() {
        let c = Colors::default();
        let bg = "#FFFFFF";
        // text-like tokens against light background
        assert_contrast(c.text_muted, bg, 4.5, "muted");
        assert_contrast(c.accent, bg, 4.5, "accent");
        assert_contrast(c.destructive, bg, 4.5, "destructive");
        assert_contrast(c.error, bg, 4.5, "error");
        assert_contrast(c.success, bg, 3.0, "success (large/icon)");
        assert_contrast(c.warning, bg, 4.5, "warning");
    }

    #[test]
    fn dark_tokens_meet_wcag_aa_for_text() {
        let c = Colors::dark();
        let bg = "#202020";
        assert_contrast(c.text_muted, bg, 4.5, "dark muted");
        assert_contrast(c.accent, bg, 4.5, "dark accent");
        assert_contrast(c.destructive, bg, 4.5, "dark destructive");
        assert_contrast(c.warning, bg, 4.5, "dark warning");
    }
}

/// Cross-platform color snapshot for non-Windows compile contexts and tests.
#[cfg(not(windows))]
pub fn tokens() -> DesignTokens {
    DesignTokens::default()
}

#[cfg(not(windows))]
pub fn set_tokens(_new_tokens: DesignTokens) {}
