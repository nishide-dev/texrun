//! Bounded capture of stdout / stderr.

use std::io::{self, Read};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Instant;

/// The first bytes of one output stream of a child process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CapturedOutput {
    /// The kept bytes (at most the [`Capture::Keep`](crate::Capture::Keep)
    /// limit).
    pub bytes: Vec<u8>,
    /// How many bytes the process wrote in total (as far as they were read
    /// before the readers were given up, see
    /// [`READER_GRACE`](crate::READER_GRACE)).
    pub total_bytes: u64,
}

impl CapturedOutput {
    /// Whether bytes were discarded.
    pub fn is_truncated(&self) -> bool {
        self.total_bytes > self.bytes.len() as u64
    }
}

/// A thread draining one pipe.
pub(crate) struct Reader {
    shared: Arc<Mutex<CapturedOutput>>,
    done: mpsc::Receiver<()>,
}

impl Reader {
    /// A reader for no pipe: finishes at once with nothing captured.
    pub(crate) fn none() -> Self {
        let (tx, done) = mpsc::channel();
        let _ = tx.send(());
        Self {
            shared: Arc::default(),
            done,
        }
    }

    /// Starts a thread reading `pipe` to EOF and keeping its first `cap`
    /// bytes.
    pub(crate) fn spawn<R: Read + Send + 'static>(pipe: R, cap: usize) -> Self {
        let shared = Arc::new(Mutex::new(CapturedOutput::default()));
        let (tx, done) = mpsc::channel();
        let out = Arc::clone(&shared);
        thread::spawn(move || {
            drain(pipe, cap, &out);
            let _ = tx.send(());
        });
        Self { shared, done }
    }

    /// Waits until `deadline` for EOF and returns what was captured so far.
    pub(crate) fn finish(self, deadline: Instant) -> CapturedOutput {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    }
}

/// Reads `pipe` to EOF, keeping the first `cap` bytes in `out`.
pub(crate) fn drain<R: Read>(mut pipe: R, cap: usize, out: &Mutex<CapturedOutput>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
        out.total_bytes = out.total_bytes.saturating_add(n as u64);
        let room = cap.saturating_sub(out.bytes.len());
        out.bytes.extend_from_slice(&buf[..n.min(room)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_keeps_the_head_and_counts_everything() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let out = Mutex::new(CapturedOutput::default());
        drain(io::Cursor::new(&data), 100_000, &out);
        let out = out.into_inner().unwrap();
        assert_eq!(out.total_bytes, 200_000);
        assert_eq!(out.bytes, data[..100_000]);
        assert!(out.is_truncated());

        let out = Mutex::new(CapturedOutput::default());
        drain(io::Cursor::new(b"short"), 100, &out);
        let out = out.into_inner().unwrap();
        assert_eq!(out.bytes, b"short");
        assert!(!out.is_truncated());
    }

    /// Fails with `EINTR` once, then yields its data.
    struct Interrupted(Option<io::Cursor<&'static [u8]>>, bool);

    impl Read for Interrupted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.1 {
                self.1 = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.0.as_mut().map_or(Ok(0), |c| c.read(buf))
        }
    }

    #[test]
    fn drain_retries_eintr() {
        let out = Mutex::new(CapturedOutput::default());
        drain(Interrupted(Some(io::Cursor::new(b"data")), false), 10, &out);
        assert_eq!(out.into_inner().unwrap().bytes, b"data");
    }
}
