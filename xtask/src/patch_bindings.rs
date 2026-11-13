//! Patch the generated bindings.rs to add public constructors and helpers.

use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bindings_path = Path::new("crates/mailin-winui-sys/src/bindings.rs");
    let mut content = fs::read_to_string(bindings_path)?;

    // Fix PropertyPath::CreateInstance to use HSTRING::from
    content = content.replace(
        "core::mem::transmute_copy(&path.into())",
        "core::mem::transmute_copy(&windows_core::HSTRING::from(path))",
    );

    // Also fix the transmute_copy with type annotation
    content = content.replace(
        "core::mem::transmute_copy::<Src, *mut c_void>(&path.into())",
        "core::mem::transmute_copy::<Src, *mut c_void>(&windows_core::HSTRING::from(path))",
    );

    fs::write("crates/mailin-winui-sys/src/bindings.rs", content)?;
    Ok(())
}
