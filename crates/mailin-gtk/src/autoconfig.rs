//! Server suggestions for the account wizard.
//!
//! The table lives in [`mailin_core::autoconfig`] so the GTK and WinUI wizards
//! make the same suggestions.

pub use mailin_core::autoconfig::{Suggestion, suggest};
