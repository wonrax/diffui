//! Even scroll motion from unevenly timed wheel events.
//!
//! A trackpad's events don't land one per refresh: on a 120 Hz display about
//! one refresh in twelve gets none, and the next doesn't get two. Drawing
//! wherever the events have got to stalls the content for that refresh, about
//! ten times a second. So a view scrolls to where the events had it
//! [`delay`] ago, read off a line through the events' times: a hole shorter than
//! the delay is spanned by the line, and the motion stays even.
//!
//! iced never showed this: it drew a frame per event into a queue a couple of
//! frames deep, which hid the holes at the cost of about as much lag as
//! [`DEFAULT_DELAY`]. GPUI draws once per refresh from the latest state.
//!
//! `DIFFUI_SCROLL_RESAMPLE_MS` sets the delay; 0 turns resampling off.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How far behind the events the view is drawn, unless overridden.
///
/// Only interpolation, never prediction: a view never runs ahead of the
/// events, so a stop or a reversal can't overshoot and spring back. The price
/// is that a hole is bridged only if it's shorter than the delay. The holes
/// measured on a 120 Hz ProMotion trackpad (a 2026-11 frame log) were one
/// missing event, a gap of two refreshes or 16.7ms, and 17ms is the least
/// delay that spans that. Longer holes were rare and still stall.
///
/// Measured on that one machine only. Input that arrives at 60 Hz would have
/// 33ms holes, and a wheel's notches have no rhythm to smooth; a delay sized
/// from each gesture's event gaps would serve both. Android's touch resampler
/// (`RESAMPLE_LATENCY` in AOSP's `InputTransport.cpp`) looks back only 5ms and
/// predicts at most 8ms past the newest event: less lag, but it smooths jitter
/// in event timing rather than spanning a missing event.
const DEFAULT_DELAY: Duration = Duration::from_millis(17);

/// A gap in events this long starts a new gesture.
const GESTURE_GAP: Duration = Duration::from_millis(60);

/// The time a gesture's first event is spread over, one 120 Hz refresh: it
/// arrives without a previous event to say how long it took.
const FIRST_STEP: Duration = Duration::from_micros(8_333);

/// The delay in microseconds, `u64::MAX` until first read.
static DELAY_MICROS: AtomicU64 = AtomicU64::new(u64::MAX);

/// How far behind the events a view is drawn; zero when resampling is off.
pub fn delay() -> Duration {
    let mut micros = DELAY_MICROS.load(Ordering::Relaxed);
    if micros == u64::MAX {
        let from_env = std::env::var("DIFFUI_SCROLL_RESAMPLE_MS")
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok())
            .map_or(DEFAULT_DELAY, Duration::from_millis)
            .as_micros() as u64;
        // A `turn_off` that got here first wins.
        micros = match DELAY_MICROS.compare_exchange(
            u64::MAX,
            from_env,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => from_env,
            Err(set) => set,
        };
    }
    Duration::from_micros(micros)
}

/// Scrolls straight to each event from now on, for headless scenes that read
/// an offset right after its wheel event.
pub fn turn_off() {
    DELAY_MICROS.store(0, Ordering::Relaxed);
}

/// One axis of a view's wheel scrolling: [`push`](Self::push) each event's
/// delta, and each frame [`take`](Self::take) how far to move.
#[derive(Debug, Default)]
pub struct Resampler {
    /// When each recent event landed, and the sum of every delta up to it.
    events: VecDeque<(Instant, f64)>,
    /// The sum of every delta pushed.
    total: f64,
    /// How much of `total` has been handed out.
    taken: f64,
}

impl Resampler {
    /// A wheel event moved `delta` at `now`.
    pub fn push(&mut self, now: Instant, delta: f64) {
        if delta == 0.0 {
            return;
        }
        let new_gesture = self
            .events
            .back()
            .is_none_or(|&(at, _)| now.saturating_duration_since(at) >= GESTURE_GAP);
        if new_gesture {
            // Whatever the last gesture still owed goes out with this event.
            self.events.clear();
            self.events.push_back((now - FIRST_STEP, self.taken));
        }
        self.total += delta;
        self.events.push_back((now, self.total));
    }

    /// How far to move this frame, drawn at `now`.
    pub fn take(&mut self, now: Instant) -> f64 {
        let at = self.sampled(now);
        let delta = at - self.taken;
        self.taken = at;
        // Keep the newest event that sits before the sampled time, the start
        // of the segment the next frames are read from.
        let sample = now.checked_sub(delay());
        while self.events.len() > 2 && sample.is_some_and(|sample| self.events[1].0 <= sample) {
            self.events.pop_front();
        }
        delta
    }

    /// Whether motion is still owed: the view needs another frame.
    pub fn pending(&self) -> bool {
        self.taken != self.total
    }

    /// Drops the motion still owed: something else set the offset.
    pub fn reset(&mut self) {
        self.events.clear();
        self.taken = self.total;
    }

    fn sampled(&self, now: Instant) -> f64 {
        let delay = delay();
        let Some(sample) = now.checked_sub(delay).filter(|_| !delay.is_zero()) else {
            return self.total;
        };
        let mut previous: Option<(Instant, f64)> = None;
        for &(at, sum) in &self.events {
            if at >= sample {
                return match previous {
                    Some((from, start)) if at > from => {
                        let span = (at - from).as_secs_f64();
                        let into = (sample - from).as_secs_f64();
                        start + (sum - start) * (into / span)
                    }
                    Some((_, start)) => start,
                    None => self.taken,
                };
            }
            previous = Some((at, sum));
        }
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_hole_in_the_events_still_moves_every_frame() {
        let start = Instant::now();
        let mut r = Resampler::default();
        // A steady 10 every 8ms, but nothing lands at 24ms: its motion comes
        // with the event at 32ms.
        for (t, delta) in [
            (0, 10.0),
            (8, 10.0),
            (16, 10.0),
            (32, 20.0),
            (40, 10.0),
            (48, 10.0),
        ] {
            r.push(start + ms(t), delta);
        }
        let mut moved = Vec::new();
        for frame in 0..10 {
            moved.push(r.take(start + ms(20 + frame * 8)));
        }
        // After the first frame, which hands out the gesture's start at once,
        // every frame moves the same until the events run out.
        assert!(
            moved[1..5].iter().all(|step| (step - 10.0).abs() < 1e-9),
            "{moved:?}"
        );
        let sum: f64 = moved.iter().sum();
        assert!(
            (sum - 70.0).abs() < 1e-9,
            "every delta is handed out: {sum}"
        );
        assert!(!r.pending());
    }

    #[test]
    fn a_new_gesture_drops_nothing_and_starts_fresh() {
        let start = Instant::now();
        let mut r = Resampler::default();
        r.push(start, 30.0);
        r.push(start + ms(200), 5.0);
        let sum: f64 = (0..10).map(|f| r.take(start + ms(200 + f * 8))).sum();
        assert!((sum - 35.0).abs() < 1e-9, "{sum}");
    }

    #[test]
    fn reset_owes_nothing() {
        let start = Instant::now();
        let mut r = Resampler::default();
        r.push(start, 30.0);
        r.reset();
        assert!(!r.pending());
        assert_eq!(r.take(start + ms(100)), 0.0);
    }
}
