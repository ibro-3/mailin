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

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use mailin_core::{Config, Engine, Mailin, OutgoingMessage, Paths};
use mailin_winui_sys::{
    Application, ApplicationInitializationCallback, DispatcherQueue, DispatcherShutdownMode,
    IApplication3, IApplicationOverrides, IApplicationOverrides_Impl, LaunchActivatedEventArgs,
    ProtocolActivatedEventArgs, ToastNotification, ToastNotificationActivatedEventArgs,
    ToastNotificationManager, ToastNotifier, ToastTemplateType, XmlDocument,
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
    pub engine: Engine,
    pub toast_notifier: Option<ToastNotifier>,
}

impl AppState {
    fn open() -> Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        let config = Config::load(&paths)?;
        config.validate()?;
        let mailin = Arc::new(Mailin::open(&paths)?);
        let engine = Engine::start(Arc::clone(&mailin))?;

        // Initialize toast notifier for Windows notifications
        let toast_notifier = ToastNotificationManager::CreateToastNotifier().ok();

        Ok(Self {
            config,
            mailin,
            engine,
            toast_notifier,
        })
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

    fn OnActivated(
        &self,
        args: Ref<mailin_winui_sys::IActivatedEventArgs>,
    ) -> windows_core::Result<()> {
        // Handle toast notification activation
        if let Ok(toast_args) = args.cast::<ToastNotificationActivatedEventArgs>() {
            if let Ok(argument) = toast_args.Argument() {
                tracing::info!(%argument, "toast notification activated");
                let _ = window::create(&self.state);
            }
            return Ok(());
        }

        // Handle mailto: protocol activation
        if let Ok(protocol_args) = args.cast::<ProtocolActivatedEventArgs>() {
            if let Ok(uri) = protocol_args.Uri() {
                if let Ok(uri_str) = uri.AbsoluteUri() {
                    tracing::info!(%uri_str, "mailto: protocol activated");
                    if let Some(draft) = parse_mailto_uri(&uri_str) {
                        let state = Arc::clone(&self.state);
                        // Use the dispatcher queue to run on UI thread
                        if let Ok(queue) = DispatcherQueue::GetForCurrentThread() {
                            let _ = queue.TryEnqueue(move || {
                                let _ = window::create_with_draft(&state, draft);
                            });
                        }
                    }
                }
            }
            return Ok(());
        }
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
        "starting the WinUI client"
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

/// Parse a mailto: URI into an OutgoingMessage draft.
/// Supports: mailto:addr?subject=...&body=...&cc=...&bcc=...
fn parse_mailto_uri(uri: &str) -> Option<OutgoingMessage> {
    let uri = uri.strip_prefix("mailto:")?;
    let (addr_part, query_part) = uri.split_once('?').unwrap_or((uri, ""));
    let to = percent_decode(addr_part).ok()?;
    let mut draft = OutgoingMessage {
        to,
        ..OutgoingMessage::default()
    };
    for pair in query_part.split('&') {
        let (key, value) = pair.split_once('=')?;
        let value = percent_decode(value).ok()?;
        match key {
            "subject" => draft.subject = value,
            "body" => draft.body = value,
            "cc" => draft.cc = value,
            "bcc" => draft.bcc = value,
            _ => {}
        }
    }
    Some(draft)
}

fn percent_decode(input: &str) -> Option<String> {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex1 = chars.next()?;
            let hex2 = chars.next()?;
            let byte = u8::from_str_radix(&format!("{hex1}{hex2}"), 16).ok()?;
            result.push(byte as char);
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    Some(result)
}
