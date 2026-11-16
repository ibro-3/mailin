//! Application bootstrap: attach the App SDK, initialise the core and run the
//! XAML message loop.
//!
//! The order matters:
//!
//! 1. bootstrap the Windows App SDK framework package;
//! 2. initialise the thread as an STA (WinUI requires one);
//! 3. open the same `mailin-core` store the GTK client uses and start its
//!    network engine;
//! 4. enter `Application::Start`, which initialises XAML, invokes our
//!    `OnLaunched` override and then pumps messages until the last window
//!    closes.

use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, anyhow};
use mailin_core::{Config, Engine, Mailin, Paths};
use mailin_winui_sys::{
    Application, ApplicationInitializationCallback, DispatcherShutdownMode, IApplication3,
    IApplicationOverrides, IApplicationOverrides_Impl, LaunchActivatedEventArgs,
    ToastNotificationManager, ToastNotifier,
};
use windows_core::{HRESULT, Interface, Ref, implement};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

use crate::bootstrap::Bootstrap;
use crate::window;

/// Application User Model ID (M4-J). For packaged builds this should align with
/// package identity; for unpackaged installs the Start Menu shortcut must
/// register the same AUMID for toast activation.
pub const AUMID: &str = "dev.mailin.Mailin";

/// Everything that lives for the whole process. The XAML objects hold an `Arc`
/// of this, so closing a window cannot drop the store while a callback runs.
#[derive(Debug)]
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub mailin: Arc<Mailin>,
    pub engine: Engine,
    pub toast_notifier: Option<ToastNotifier>,
    /// Set when startup had to reset a broken preferences file to defaults;
    /// shown as a banner once the window is up.
    pub config_warning: Option<String>,
}

impl AppState {
    /// Read-only accessor for the live configuration.
    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl AppState {
    pub(crate) fn open() -> Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        // A broken preferences file never blocks startup: defaults are
        // used and the broken file moved aside. The banner says so.
        let (config, config_warning) = Config::load_recovering(&paths)?;
        if let Some(warning) = &config_warning {
            tracing::warn!(%warning, "preferences were reset to defaults");
        }
        let mailin = Arc::new(Mailin::open(&paths)?);
        let engine = Engine::start(Arc::clone(&mailin))?;

        // Initialize toast notifier for Windows notifications (M4-I/J AUMID).
        let toast_notifier = ToastNotificationManager::CreateToastNotifierWithId(AUMID)
            .or_else(|_| ToastNotificationManager::CreateToastNotifier())
            .ok();

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            mailin,
            engine,
            toast_notifier,
            config_warning,
        })
    }
}

/// The composed `Microsoft.UI.Xaml.Application` subclass.
///
/// WinRT composition means the framework sees an `Application` object, while
/// `IApplicationOverrides::OnLaunched` is dispatched back into this Rust type.
#[implement(IApplicationOverrides)]
struct MailinApplication {
    /// `None` while startup data is unopenable; the recovery window fills it
    /// in after a successful Retry or cache reset.
    state: Arc<RwLock<Option<Arc<AppState>>>>,
    /// Friendly startup error shown by the recovery window.
    startup_error: Arc<Mutex<Option<String>>>,
}

impl IApplicationOverrides_Impl for MailinApplication_Impl {
    fn OnLaunched(&self, _args: Ref<LaunchActivatedEventArgs>) -> windows_core::Result<()> {
        let opened = self
            .state
            .read()
            .map_err(|_| to_windows_error(anyhow!("the application state lock is poisoned")))?
            .clone();
        if let Some(state) = opened {
            window::create(&state).map_err(to_windows_error)?;
        } else {
            let error = self
                .startup_error
                .lock()
                .map_err(|_| to_windows_error(anyhow!("the startup error lock is poisoned")))?
                .clone()
                .unwrap_or_else(|| "Mailin could not open its local data.".to_owned());
            crate::recovery::present(
                &error,
                Arc::clone(&self.state),
                Arc::clone(&self.startup_error),
            )
            .map_err(to_windows_error)?;
        }
        Ok(())
    }
}

/// The framework owns the application after `Application::Start`, but keeping a
/// strong reference here makes the lifetime explicit and keeps the composed
/// object alive across the whole message loop.
static APPLICATION: Mutex<Option<Application>> = Mutex::new(None);

/// Notice left by a panic in the previous run, surfaced once in the main
/// window. Set from `main` before the XAML loop starts (F1c).
static STARTUP_NOTICE: Mutex<Option<String>> = Mutex::new(None);

pub fn set_startup_notice(text: String) {
    if let Ok(mut slot) = STARTUP_NOTICE.lock() {
        *slot = Some(text);
    }
}

pub(crate) fn take_startup_notice() -> Option<String> {
    STARTUP_NOTICE.lock().ok()?.take()
}

/// Open the core, bootstrap WinUI and run until the last window is closed.
pub fn run() -> Result<()> {
    let _bootstrap = Bootstrap::initialize()?;
    let _apartment = Apartment::initialize()?;

    // A failure to open local data is no longer fatal here: XAML still starts
    // so the recovery window (Retry / Open data folder / Reset / Quit) can be
    // shown, mirroring the GTK client's A6 flow.
    let (initial_state, initial_error) = match AppState::open() {
        Ok(state) => (Some(Arc::new(state)), None),
        Err(error) => {
            tracing::error!(%error, "Mailin could not open its local data");
            let message = format!("{error:#}\n\nYour mail on the server is not affected.");
            if let Ok(paths) = Paths::discover() {
                tracing::error!(
                    path = %paths.database_file().display(),
                    "move the database and its -wal/-shm sidecars aside, or use Reset local cache"
                );
            }
            (None, Some(message))
        }
    };
    let state = Arc::new(RwLock::new(initial_state));
    let startup_error = Arc::new(Mutex::new(initial_error));

    if let Some(current) = state
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        let accounts = current
            .mailin
            .list_accounts()
            .map(|accounts| accounts.len())
            .unwrap_or_default();
        tracing::info!(
            accounts,
            page_size = current.config().ui.page_size,
            "starting the WinUI client"
        );
    }

    let callback = ApplicationInitializationCallback::new({
        let state = Arc::clone(&state);
        let startup_error = Arc::clone(&startup_error);
        move |_| {
            let state = Arc::clone(&state);
            let startup_error = Arc::clone(&startup_error);
            match Application::compose(MailinApplication {
                state,
                startup_error,
            }) {
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
