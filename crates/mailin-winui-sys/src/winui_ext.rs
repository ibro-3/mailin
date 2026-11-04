//! Extension traits and helpers for ergonomic WinUI 3 usage.
//!
//! The generated projection is minimal: concrete types only deref to their
//! primary interface. These extensions provide the minimal helpers needed
//! by the Mailin WinUI front-end that survive projection regeneration.

use super::bindings::*;
use windows_core::Result;

// ============================================================================
// TypedEventHandler construction
// ============================================================================

/// Create a `TypedEventHandler<TSender, TResult>` from a closure.
///
/// The Windows crate provides `TypedEventHandler::new(|sender, args| { ... })`
/// but the minimal projection only generates the struct without constructors.
pub fn typed_handler<TSender, TResult, F>(handler: F) -> TypedEventHandler<TSender, TResult>
where
    TSender: windows_core::RuntimeType + 'static,
    TResult: windows_core::RuntimeType + 'static,
    F: Fn(windows_core::Ref<TSender>, windows_core::Ref<TResult>) + 'static,
{
    let delegate = windows_core::imp::DelegateBox::<TypedEventHandler<TSender, TResult>, F>::new(
        &TypedEventHandlerBox::<TSender, TResult, F>::VTABLE,
        handler,
    );
    unsafe { core::mem::transmute(windows_core::imp::box_new(delegate)) }
}

// ============================================================================
// Thickness construction helper
// ============================================================================

/// Create a `Thickness` with uniform value.
pub fn thickness_uniform(value: f64) -> Thickness {
    Thickness {
        left: value,
        top: value,
        right: value,
        bottom: value,
    }
}

/// Create a `Thickness` with horizontal/vertical values.
pub fn thickness_hv(horizontal: f64, vertical: f64) -> Thickness {
    Thickness {
        left: horizontal,
        top: vertical,
        right: horizontal,
        bottom: vertical,
    }
}

/// Create a `Thickness` with all four sides.
pub fn thickness(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness {
        left,
        top,
        right,
        bottom,
    }
}

// ============================================================================
// base64 decode helper
// ============================================================================

/// Decode base64 using the standard engine.
pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(input.as_bytes())
        .map_err(|e| {
            windows_core::Error::new(windows_core::HRESULT(0x80004005u32 as i32), e.to_string())
        })
}

// ============================================================================
// Style / Setter / TypeName / DependencyProperty helpers
// ============================================================================

/// Create a `Style` targeting a type name.
/// TypeKind::Custom = 2 (from Windows.UI.Xaml.Interop.TypeKind)
pub fn style(target_type: &str) -> Result<Style> {
    let tn = TypeName {
        name: target_type.into(),
        kind: TypeKind(2),
    };
    Style::CreateInstance(&tn)
}

/// Create a `TypeName` from a string.
pub fn type_name(name: &str) -> TypeName {
    TypeName {
        name: name.into(),
        kind: TypeKind(2),
    }
}

/// Create a `Thickness` from four values.
pub fn thickness_ltb(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness {
        left,
        top,
        right,
        bottom,
    }
}

// ============================================================================
// Color construction
// ============================================================================

/// Create a `Color` from ARGB components.
pub fn color_from_argb(a: u8, r: u8, g: u8, b: u8) -> Color {
    Color { a, r, g, b }
}

/// Create a `Color` from RGB (opaque).
pub fn color_from_rgb(r: u8, g: u8, b: u8) -> Color {
    Color { a: 255, r, g, b }
}

// ============================================================================
// Export common types for convenience
// ============================================================================

pub use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
