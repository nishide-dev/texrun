//! Turning SIGINT / SIGTERM / SIGHUP into a [`CancelToken`].
//!
//! latexmk runs in its own process group, so a Ctrl-C in the terminal only
//! reaches texrun. Instead of dying (which would orphan latexmk and skip the
//! workspace cleanup in `Drop`), texrun sets the cancel token; the engine
//! then kills the process group and returns a `Cancelled` result, and texrun
//! finishes through its normal path: artifacts collected, workspace dropped,
//! then the exit code `128 + signal` is returned from `main`.
//!
//! Further signals while stopping are reported and otherwise ignored.
//! SIGQUIT (Ctrl-\) keeps its default action as a last resort; it skips
//! the cleanup.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use texrun_core::CancelToken;

/// Records the first termination signal and cancels the token.
#[derive(Debug, Clone)]
pub struct SignalGuard {
    received: Arc<AtomicI32>,
}

impl SignalGuard {
    /// Installs the handlers and starts the thread that forwards signals to
    /// `cancel`.
    pub fn install(cancel: CancelToken) -> io::Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
        let received = Arc::new(AtomicI32::new(0));
        let seen = Arc::clone(&received);
        std::thread::Builder::new()
            .name("texrun-signals".to_owned())
            .spawn(move || {
                for signal in signals.forever() {
                    if seen
                        .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        cancel.cancel();
                        eprintln!("texrun: {}, stopping the compile...", name(signal));
                    } else {
                        eprintln!("texrun: {}, still stopping the compile...", name(signal));
                    }
                }
            })?;
        Ok(Self { received })
    }

    /// The first signal received, if any.
    pub fn received(&self) -> Option<i32> {
        match self.received.load(Ordering::SeqCst) {
            0 => None,
            s => Some(s),
        }
    }
}

fn name(signal: i32) -> &'static str {
    match signal {
        SIGINT => "interrupted (SIGINT)",
        SIGTERM => "terminated (SIGTERM)",
        SIGHUP => "hung up (SIGHUP)",
        _ => "signal received",
    }
}
