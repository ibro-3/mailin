//! Patch the generated bindings.rs to add public constructors and helpers.

use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bindings_path = Path::new("crates/mailin-winui-sys/src/bindings.rs");
    let mut content = fs::read_to_string(bindings_path)?;

    // 1. Add public new() method to TypedEventHandler
    let typed_handler_def = r#"pub struct TypedEventHandler<TSender, TResult>(
    windows_core::IUnknown,
    core::marker::PhantomData<TSender>,
    core::marker::PhantomData<TResult>,
)
where
    TSender: windows_core::RuntimeType + 'static,
    TResult: windows_core::RuntimeType + 'static;
"#;

    let new_method = r#"
impl<TSender: windows_core::RuntimeType + 'static, TResult: windows_core::RuntimeType + 'static>
    TypedEventHandler<TSender, TResult>
{
    /// Create a new `TypedEventHandler` from a closure.
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(windows_core::Ref<TSender>, windows_core::Ref<TResult>) + 'static,
    {
        let delegate = windows_core::imp::DelegateBox::<Self, F>::new(
            &TypedEventHandlerBox::<TSender, TResult, F>::VTABLE,
            handler,
        );
        unsafe { core::mem::transmute(windows_core::imp::box_new(delegate)) }
    }
}
"#;

    content = content.replace(
        typed_handler_def,
        &format!("{typed_handler_def}\n{new_method}"),
    );

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

    // Make TypedEventHandlerBox::VTABLE public
    content = content.replace(
        "const VTABLE : TypedEventHandler_Vtbl",
        "pub const VTABLE : TypedEventHandler_Vtbl",
    );

    // Make TypedEventHandlerBox public
    content = content.replace(
        "struct TypedEventHandlerBox<",
        "pub struct TypedEventHandlerBox<",
    );

    fs::write("crates/mailin-winui-sys/src/bindings.rs", content)?;
    Ok(())
}
