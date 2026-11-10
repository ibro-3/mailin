//! Run blocking work off the GTK main thread and continue on it.
//!
//! The core is synchronous (SQLite behind a mutex, keyring calls, RSA key
//! generation), so call screens must never run it inside widget handlers:
//! the UI freezes for as long as the call takes. Use [`spawn_result`] to push
//! the work to a background thread and receive the outcome back on the main
//! thread.

use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use gtk::glib;

/// Poll interval for the result ferry. Only used between the worker sending
/// its result and the next main-loop tick; keep it small so results feel
/// instant.
const RESULT_POLL: Duration = Duration::from_millis(20);

/// The background worker did not produce a result (it panicked).
#[derive(Debug)]
pub struct WorkerFailed {
    /// The panic message, when the worker could provide one.
    pub message: String,
}

impl std::fmt::Display for WorkerFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Result of a background worker: either its value or a failure.
pub type WorkerResult<T> = Result<T, WorkerFailed>;

/// Run `work` on a background thread and call `done` with its result on the
/// GTK main thread.
///
/// `done` runs on the main thread and may capture `Rc` handles and touch
/// widgets. `work` must never touch either: keep it to `Send` data such as a
/// cloned `Arc<Mailin>` handle and plain values captured before spawning.
///
/// `done` is called with [`WorkerFailed`] when the worker panics, so callers
/// can always clear their "work in progress" state instead of waiting for a
/// result that never arrives.
pub fn spawn_result<F, T, D>(work: F, done: D)
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
    D: FnOnce(WorkerResult<T>) + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        let _ = tx.send(outcome);
    });
    let mut done = Some(done);
    glib::timeout_add_local(RESULT_POLL, move || match rx.try_recv() {
        Ok(Ok(value)) => {
            if let Some(done) = done.take() {
                done(Ok(value));
            }
            glib::ControlFlow::Break
        }
        Ok(Err(payload)) => {
            if let Some(done) = done.take() {
                done(Err(WorkerFailed {
                    message: panic_message(payload),
                }));
            }
            glib::ControlFlow::Break
        }
        Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(TryRecvError::Disconnected) => {
            if let Some(done) = done.take() {
                done(Err(WorkerFailed {
                    message: "the worker stopped without a result".to_owned(),
                }));
            }
            glib::ControlFlow::Break
        }
    });
}

/// Best-effort description of a panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "the worker panicked".to_owned()
    }
}
