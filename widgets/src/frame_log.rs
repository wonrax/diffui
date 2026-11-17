//! A per-frame timing log for real windows, off unless `DIFFUI_FRAME_LOG` names
//! a file. Headless benchmarks can't see presentation or input pacing, so this
//! is how a stutter on a real display gets numbers.
//!
//! Each frame writes one line: milliseconds since the log opened, since the
//! previous frame began, and from the root's render to the last paint, then the
//! timed spans inside that frame as `name=ms`. Wheel events count as `wheel` spans,
//! with their vertical delta as `wy` and the distance a view drew that frame as `sy`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

struct Log {
    file: std::fs::File,
    opened: Instant,
    began: Option<Instant>,
    last_began: Option<Instant>,
    spans: Vec<(&'static str, f64)>,
}

static LOG: LazyLock<Option<Mutex<Log>>> = LazyLock::new(|| {
    let path = std::env::var_os("DIFFUI_FRAME_LOG")?;
    let file = std::fs::File::create(path).ok()?;
    Some(Mutex::new(Log {
        file,
        opened: Instant::now(),
        began: None,
        last_began: None,
        spans: Vec::new(),
    }))
});

/// Whether frames are being logged.
pub fn enabled() -> bool {
    LOG.is_some()
}

fn with(f: impl FnOnce(&mut Log)) {
    if let Some(log) = LOG.as_ref()
        && let Ok(mut log) = log.lock()
    {
        f(&mut log);
    }
}

/// A frame starts: the root view renders.
pub fn begin() {
    with(|log| {
        if log.began.is_none() {
            log.began = Some(Instant::now());
        }
    });
}

/// Something inside the frame took `took`.
pub fn span(name: &'static str, took: Duration) {
    value(name, took.as_secs_f64() * 1000.0);
}

/// A number from inside the frame, logged as `name=value`.
pub fn value(name: &'static str, value: f64) {
    with(|log| log.spans.push((name, value)));
}

/// Times `f` as span `name`.
pub fn time<R>(name: &'static str, f: impl FnOnce() -> R) -> R {
    if !enabled() {
        return f();
    }
    let started = Instant::now();
    let out = f();
    span(name, started.elapsed());
    out
}

/// The frame's last paint ran.
pub fn end() {
    with(|log| {
        let Some(began) = log.began.take() else {
            return;
        };
        let now = Instant::now();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let since_last = log
            .last_began
            .map_or(0.0, |last| ms(began.saturating_duration_since(last)));
        log.last_began = Some(began);
        let mut line = format!(
            "{:.2} {:.2} {:.2}",
            ms(began.saturating_duration_since(log.opened)),
            since_last,
            ms(now.saturating_duration_since(began)),
        );
        for (name, value) in log.spans.drain(..) {
            // A closure's type name (an `act` call site) holds spaces.
            let _ = write!(line, " {}={:.2}", name.replace(' ', ""), value);
        }
        line.push('\n');
        let _ = log.file.write_all(line.as_bytes());
    });
}
