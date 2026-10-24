//! Application bootstrap: attach the App SDK, initialise the core and run the
//! XAML message loop.
//!
//! The order matters:
//!
//! 1. bootstrap the Windows App SDK framework package;
//! 2. initialise the thread as an STA (WinUI requires one);
//! 3. open the same `mailin-core` store the GTK client uses;
//! 4. enter `Application::Start`, which initialises XAML, invokes our
//!    `OnLaunched` override and then pumps messages until the last window
//!    closes.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use mailin_core::{Config, Mailin, Paths};
use mailin_winui_sys::{
    Application, ApplicationInitializationCallback, DispatcherShutdownMode, IApplication3,
    IApplicationOverrides, IApplicationOverrides_Impl, LaunchActivatedEventArgs,
};
use windows_core::{HRESULT, Interface, Ref, implement};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

use crate::bootstrap::Bootstrap;
use crate::window;

/// Everything that lives for the whole process. The XAML objects hold an `Arc`
/// of this, so closing a window cannot drop the store while a callback runs.
#[derive(Debug)]
pub struct AppState {
    pub config: Config,
    pub mailin: Arc<Mailin>,
}

impl AppState {
    fn open() -> Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        let config = Config::load(&paths)?;
        config.validate()?;
        let mailin = Arc::new(Mailin::open(&paths)?);
        Ok(Self { config, mailin })
    }
}

/// The composed `Microsoft.UI.Xaml.Application` subclass.
///
/// WinRT composition means the framework sees an `Application` object, while
/// `IApplicationOverrides::OnLaunched` is dispatched back into this Rust type.
#[implement(IApplicationOverrides)]
struct MailinApplication {
    state: Arc<AppState>,
}

impl IApplicationOverrides_Impl for MailinApplication_Impl {
    fn OnLaunched(&self, _args: Ref<LaunchActivatedEventArgs>) -> windows_core::Result<()> {
        window::create(&self.state).map_err(to_windows_error)?;
        Ok(())
    }
}

/// The framework owns the application after `Application::Start`, but keeping a
/// strong reference here makes the lifetime explicit and keeps the composed
/// object alive across the whole message loop.
static APPLICATION: Mutex<Option<Application>> = Mutex::new(None);

/// Open the core, bootstrap WinUI and run until the last window is closed.
pub fn run() -> Result<()> {
    let _bootstrap = Bootstrap::initialize()?;
    let _apartment = Apartment::initialize()?;

    let state = Arc::new(AppState::open().context("could not open the Mailin store")?);
    let accounts = state
        .mailin
        .list_accounts()
        .map(|accounts| accounts.len())
        .unwrap_or_default();
    tracing::info!(
        accounts,
        page_size = state.config.ui.page_size,
        "starting the WinUI spike"
    );

    let callback = ApplicationInitializationCallback::new({
        let state = Arc::clone(&state);
        move |_| {
            let state = Arc::clone(&state);
            match Application::compose(MailinApplication { state }) {
                Ok(application) => {
                    if let Err(error) =
                        application.cast::<IApplication3>().and_then(|application| {
                            application.SetDispatcherShutdownMode(
                                DispatcherShutdownMode::OnLastWindowClose,
                            )
                        })
                    {
                        tracing::warn!(%error, "could not set the XAML shutdown mode");
                    }
                    match APPLICATION.lock() {
                        Ok(mut slot) => *slot = Some(application),
                        Err(_) => tracing::error!("the application slot is poisoned"),
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "could not compose the XAML application");
                }
            }
        }
    });

    Application::Start(&callback).context("the WinUI message loop failed")?;
    Ok(())
}

/// RAII wrapper for the COM apartment.
struct Apartment;

impl Apartment {
    fn initialize() -> Result<Self> {
        // S_FALSE means the thread was already initialised; that is still success.
        let result = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        if result < 0 {
            return Err(anyhow!(
                "could not initialise the UI thread as an STA (HRESULT 0x{:08X})",
                result as u32
            ));
        }
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// Convert an `anyhow` error into the `HRESULT` the XAML callback expects.
fn to_windows_error(error: anyhow::Error) -> windows_core::Error {
    windows_core::Error::new(
        HRESULT(0x8000_4005u32 as i32), // E_FAIL
        format!("{error:#}"),
    )
}
