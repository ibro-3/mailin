//! Copy the Windows App SDK bootstrapper next to the executable.
//!
//! Unpackaged WinUI 3 applications must load `Microsoft.WindowsAppRuntime.Bootstrap.dll`
//! from their own directory. Cargo has no notion of runtime assets, so the DLL is
//! taken from `MAILIN_WINDOWSAPPSDK_BOOTSTRAP` or from the default location
//! written by `cargo xtask winui-runtime`, then copied into the profile
//! directory here. Without it the app still builds; the bootstrap module then
//! reports an actionable error at startup.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=MAILIN_WINDOWSAPPSDK_BOOTSTRAP");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // `cargo xtask winui-runtime` writes the DLL to the workspace target
    // directory; an explicit environment variable still wins so CI and
    // self-contained builds can point at another copy.
    let source = std::env::var_os("MAILIN_WINDOWSAPPSDK_BOOTSTRAP")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/winui-runtime/Microsoft.WindowsAppRuntime.Bootstrap.dll")
        });

    if !source.is_file() {
        println!(
            "cargo:warning=Microsoft.WindowsAppRuntime.Bootstrap.dll is not available at {}; \
             run `cargo xtask winui-runtime` or set MAILIN_WINDOWSAPPSDK_BOOTSTRAP",
            source.display()
        );
        return;
    }

    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    // OUT_DIR is target/[<triple>/]<profile>/build/<package>/out. Locate the
    // `build` directory instead of counting levels so the copy also works when
    // the build does not pass `--target` and drops the triple component.
    let Some(profile_dir) = PathBuf::from(out_dir)
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .map(PathBuf::from)
    else {
        return;
    };
    let destination = profile_dir.join("Microsoft.WindowsAppRuntime.Bootstrap.dll");
    if let Err(error) = std::fs::copy(&source, &destination) {
        println!(
            "cargo:warning=could not copy the Windows App SDK bootstrapper to {}: {error}",
            destination.display()
        );
    }
}
