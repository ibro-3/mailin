//! Windows App SDK bootstrapping for an unpackaged application.
//!
//! WinUI 3 lives in a framework package, not in the OS. An unpackaged process
//! has to add that package to its own package graph before any
//! `Microsoft.UI.Xaml` class can be activated. The official way to do that is
//! `MddBootstrapInitialize2` from `Microsoft.WindowsAppRuntime.Bootstrap.dll`,
//! which is redistributed next to the executable (see `build.rs`).
//!
//! The API is a plain C export, not WinRT metadata, so it is loaded with
//! `LoadLibraryW`/`GetProcAddress` instead of being projected by bindgen.

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// Version of the Windows App SDK framework package this build targets.
///
/// The value is `0xMMMMNNNN`, so 1.8 is `0x0001_0008`.
const MAJOR_MINOR: u32 = 0x0001_0008;
const VERSION_TAG: *const u16 = std::ptr::null();
const BOOTSTRAP_DLL: &str = "Microsoft.WindowsAppRuntime.Bootstrap.dll";

type InitializeFn = unsafe extern "system" fn(u32, *const u16, PackageVersion, u32) -> i32;
type ShutdownFn = unsafe extern "system" fn();

/// `PACKAGE_VERSION` is an eight byte union; only the numeric form is needed.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PackageVersion {
    version: u64,
}

/// A process-wide Windows App SDK bootstrap session.
///
/// The runtime is detached when this value is dropped, so the front-end keeps
/// it alive until the XAML message loop has returned.
#[derive(Debug)]
pub struct Bootstrap {
    module: HMODULE,
    shutdown: ShutdownFn,
}

impl Bootstrap {
    /// Attach the Windows App SDK 1.8 framework package to this process.
    pub fn initialize() -> Result<Self> {
        let module = load_bootstrap_dll().context(
            "the Windows App SDK bootstrapper is not next to mailin-winui.exe; \
             install the Windows App SDK 1.8 runtime and run `cargo xtask winui-runtime` \
             (or set MAILIN_WINDOWSAPPSDK_BOOTSTRAP) before building",
        )?;

        let initialize = unsafe { function::<InitializeFn>(module, b"MddBootstrapInitialize2\0") }
            .context("the bootstrapper does not export MddBootstrapInitialize2")?;
        let shutdown = unsafe { function::<ShutdownFn>(module, b"MddBootstrapShutdown\0") }
            .context("the bootstrapper does not export MddBootstrapShutdown")?;

        let result = unsafe {
            initialize(
                MAJOR_MINOR,
                VERSION_TAG,
                PackageVersion::default(),
                0, // MddBootstrapInitializeOptions_None
            )
        };
        if result < 0 {
            unsafe {
                FreeLibrary(module);
            }
            bail!(
                "could not attach the Windows App SDK 1.8 runtime (HRESULT 0x{:08X}); \
                 install the matching runtime or set MAILIN_WINDOWSAPPSDK_BOOTSTRAP",
                result as u32
            );
        }

        tracing::info!(version = "1.8", "attached the Windows App SDK runtime");
        Ok(Self { module, shutdown })
    }
}

impl Drop for Bootstrap {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)();
            FreeLibrary(self.module);
        }
    }
}

/// Load the redistributed bootstrapper, preferring the executable directory.
fn load_bootstrap_dll() -> Result<HMODULE> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("MAILIN_WINDOWSAPPSDK_BOOTSTRAP") {
        candidates.push(std::path::PathBuf::from(path));
    }
    candidates.push(std::path::PathBuf::from(BOOTSTRAP_DLL));

    let mut last_error = None;
    for candidate in candidates {
        let wide = wide(&candidate.to_string_lossy());
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if !module.is_null() {
            return Ok(module);
        }
        last_error = Some(candidate);
    }

    match last_error {
        Some(path) => bail!("could not load {}", path.display()),
        None => bail!("no candidate path for {BOOTSTRAP_DLL}"),
    }
}

/// Resolve an exported function from a loaded module.
unsafe fn function<T: Copy>(module: HMODULE, name: &[u8]) -> Option<T> {
    let address = unsafe { GetProcAddress(module, name.as_ptr()) }?;
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&address));
    Some(unsafe { std::mem::transmute_copy(&address) })
}

/// NUL-terminated UTF-16, as expected by the Win32 `W` APIs.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_encoding_matches_the_app_sdk_convention() {
        assert_eq!(MAJOR_MINOR, 0x0001_0008);
        assert_eq!(std::mem::size_of::<PackageVersion>(), 8);
    }

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(wide("ab"), vec![b'a' as u16, b'b' as u16, 0]);
    }
}
