//! Design token system for WinUI front-end.
//!
//! This module provides semantic design tokens (colors, spacing, typography, radii, shadows)
//! to replace hardcoded values throughout the UI. Tokens can be swapped for high contrast
//! mode or custom themes.

use mailin_winui_sys::FontWeight;

/// Color tokens — semantic names that work in light and dark themes.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    /// Primary destructive action color (e.g., delete, remove account)
    pub destructive: &'static str,
    /// Subtitle/text secondary opacity (0.0-1.0)
    pub subtitle_opacity: f64,
}

impl Colors {
    /// Default (light/theme-aware) color palette.
    pub fn default() -> Self {
        Self {
            destructive: "#C42B1C",
            subtitle_opacity: 0.7,
        }
    }

    /// High contrast color palette.
    pub fn high_contrast() -> Self {
        Self {
            destructive: "#FF0000",
            subtitle_opacity: 1.0,
        }
    }
}

/// Spacing tokens — consistent spacing scale (multiples of 4).
#[derive(Debug, Clone, Copy)]
pub struct Spacing {
    /// Extra small (4)
    pub xs: f64,
    /// Small (8)
    pub sm: f64,
    /// Medium (12)
    pub md: f64,
    /// Large (16)
    pub lg: f64,
    /// Extra large (24)
    pub xl: f64,
    /// 2x large (32)
    pub xxl: f64,
}

impl Spacing {
    /// Default spacing scale.
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

/// Typography tokens — font sizes and weights.
#[derive(Debug, Clone, Copy)]
pub struct Typography {
    /// Caption / small label (12)
    pub caption_size: f64,
    /// Body text (14)
    pub body_size: f64,
    /// Subheading / section title (16)
    pub subheading_size: f64,
    /// Heading 3 / dialog section (18)
    pub h3_size: f64,
    /// Heading 2 / page section (20)
    pub h2_size: f64,
    /// Heading 1 / page title (24)
    pub h1_size: f64,
    /// Semi-bold weight (600)
    pub weight_semibold: u16,
    /// Bold weight (700)
    pub weight_bold: u16,
}

impl Typography {
    /// Default typography scale.
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

/// Radius tokens — corner radius scale.
#[derive(Debug, Clone, Copy)]
pub struct Radii {
    /// Small radius (4)
    pub sm: f64,
    /// Medium radius (8)
    pub md: f64,
    /// Large radius (12)
    pub lg: f64,
}

impl Radii {
    /// Default radius scale.
    pub fn default() -> Self {
        Self {
            sm: 4.0,
            md: 8.0,
            lg: 12.0,
        }
    }
}

/// Shadow/elevation tokens.
#[derive(Debug, Clone, Copy)]
pub struct Shadows {
    /// Subtle shadow for cards/panels
    pub subtle: &'static str,
    /// Medium shadow for dialogs
    pub medium: &'static str,
    /// Strong shadow for tooltips/popovers
    pub strong: &'static str,
}

impl Shadows {
    /// Default shadows (using CSS-style box-shadow strings for XAML).
    pub fn default() -> Self {
        Self {
            subtle: "0 1px 2px rgba(0,0,0,0.05)",
            medium: "0 4px 8px rgba(0,0,0,0.1)",
            strong: "0 8px 16px rgba(0,0,0,0.15)",
        }
    }
}

/// Complete design token set for a theme.
#[derive(Debug, Clone)]
pub struct DesignTokens {
    pub colors: Colors,
    pub spacing: Spacing,
    pub typography: Typography,
    pub radii: Radii,
    pub shadows: Shadows,
}

impl DesignTokens {
    /// Create default (light/theme-aware) tokens.
    pub fn default() -> Self {
        Self {
            colors: Colors::default(),
            spacing: Spacing::default(),
            typography: Typography::default(),
            radii: Radii::default(),
            shadows: Shadows::default(),
        }
    }

    /// Create high contrast tokens.
    pub fn high_contrast() -> Self {
        Self {
            colors: Colors::high_contrast(),
            spacing: Spacing::default(),
            typography: Typography::default(),
            radii: Radii::default(),
            shadows: Shadows::default(),
        }
    }

    /// Create a solid color brush from a hex string.
    pub fn solid_brush(&self, hex: &str) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        use mailin_winui_sys::XamlReader;
        const XMLNS: &str = "http://schemas.microsoft.com/winfx/2006/xaml/presentation";
        let value = XamlReader::Load(&format!(
            "<SolidColorBrush xmlns=\"{XMLNS}\" Color=\"{hex}\" />"
        ))?;
        value.cast().map_err(Into::into)
    }

    /// Get the destructive color brush.
    pub fn destructive_brush(&self) -> anyhow::Result<mailin_winui_sys::SolidColorBrush> {
        self.solid_brush(self.colors.destructive)
    }

    /// Get a FontWeight for semi-bold.
    pub fn semibold_weight(&self) -> FontWeight {
        FontWeight {
            weight: self.typography.weight_semibold,
        }
    }

    /// Get a FontWeight for bold.
    pub fn bold_weight(&self) -> FontWeight {
        FontWeight {
            weight: self.typography.weight_bold,
        }
    }
}

/// Global design token instance. Use [`set_tokens`] to switch themes at runtime.
static mut TOKENS: DesignTokens = DesignTokens {
    colors: Colors::default(),
    spacing: Spacing::default(),
    typography: Typography::default(),
    radii: Radii::default(),
    shadows: Shadows::default(),
};

/// Get the current design tokens.
pub fn tokens() -> &'static DesignTokens {
    // SAFETY: Single-threaded UI access; tokens only change on theme switch.
    unsafe { &TOKENS }
}

/// Set the global design tokens (e.g., when theme changes).
pub fn set_tokens(new_tokens: DesignTokens) {
    // SAFETY: Single-threaded UI access.
    unsafe { TOKENS = new_tokens };
}

/// Theme mode for the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    /// Follow system theme
    System,
    /// Force light theme
    Light,
    /// Force dark theme
    Dark,
}

/// Apply a theme mode by updating the global tokens.
pub fn apply_theme(mode: ThemeMode) {
    // Check for system high contrast setting
    let high_contrast = mailin_winui_sys::AccessibilitySettings::new()
        .map(|s| s.HighContrast().unwrap_or(false))
        .unwrap_or(false);
    
    let new_tokens = match mode {
        ThemeMode::System | ThemeMode::Light | ThemeMode::Dark => {
            if high_contrast {
                DesignTokens::high_contrast()
            } else {
                DesignTokens::default()
            }
        }
    };
    set_tokens(new_tokens);
}