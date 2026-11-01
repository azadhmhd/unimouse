//! Smooth scrolling: wheel notches in, small per-frame deltas out, eased out like a trackpad.
//! Platform-independent; each platform passes its notches in and its `post(dx, dy)` function.

use std::{
    sync::{Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

// ponytail: sleep-paced ~120 Hz frames. Sync to the display (CVDisplayLink / DwmFlush) if it ever judders.
const FRAME: Duration = Duration::from_micros(8_333);
/// Ease-out time constant in seconds. The glide lasts about 3 * TAU.
const TAU: f64 = 0.07;
/// Notches closer together than this count as a flick and speed up.
const STREAK_WINDOW: Duration = Duration::from_millis(120);

pub static SCROLLER: Scroller = Scroller { glide: Mutex::new(Glide::new()), wake: Condvar::new() };

pub struct Scroller {
    glide: Mutex<Glide>,
    wake: Condvar,
}

impl Scroller {
    /// Starts the animation thread. `post(dx, dy)` sends one platform scroll event.
    pub fn spawn(&'static self, post: fn(i32, i32)) {
        thread::spawn(move || loop {
            let mut g = self.glide.lock().unwrap();
            while g.idle() {
                g = self.wake.wait(g).unwrap();
                g.last_frame = None;
            }
            let now = Instant::now();
            let dt = g.last_frame.map_or(FRAME, |t| now - t).min(Duration::from_millis(50));
            g.last_frame = Some(now);
            let (dx, dy) = g.advance(dt.as_secs_f64());
            drop(g);
            if (dx, dy) != (0, 0) {
                post(dx, dy)
            }
            thread::sleep(FRAME);
        });
    }

    /// One wheel event: `dx`/`dy` in notches, `units` = distance per notch in platform units.
    /// `smooth: false` sends the distance in one go (used when only `reverse` is on).
    pub fn notch(&self, dx: f64, dy: f64, units: f64, smooth: bool) {
        self.glide.lock().unwrap().add(Instant::now(), dx * units, dy * units, smooth);
        self.wake.notify_one();
    }
}

pub struct Glide {
    remaining: [f64; 2],
    /// Sub-unit leftovers, so many small steps still add up to the exact distance.
    carry: [f64; 2],
    last_notch: Option<Instant>,
    streak: u32,
    last_frame: Option<Instant>,
    instant: bool,
}

impl Glide {
    const fn new() -> Glide {
        Glide { remaining: [0.0; 2], carry: [0.0; 2], last_notch: None, streak: 0, last_frame: None, instant: false }
    }

    fn idle(&self) -> bool {
        self.remaining == [0.0; 2]
    }

    fn add(&mut self, now: Instant, dx: f64, dy: f64, smooth: bool) {
        let d = [dx, dy];
        // Scrolling the other way stops the glide at once instead of fighting it.
        let reversed = (0..2).any(|i| d[i] * self.remaining[i] < 0.0);
        if reversed {
            *self = Glide::new();
        }
        let flick = !reversed && self.last_notch.is_some_and(|t| now - t < STREAK_WINDOW);
        self.streak = if flick { (self.streak + 1).min(10) } else { 0 };
        self.last_notch = Some(now);
        self.instant = !smooth;
        // ponytail: linear ramp to 3x on fast flicks. Swap in a curve (see Mac Mouse Fix's Shared/Math/Curves) if it feels off.
        let accel = if smooth { 1.0 + 0.2 * self.streak as f64 } else { 1.0 };
        for (r, d) in self.remaining.iter_mut().zip(d) {
            *r += d * accel;
        }
    }

    /// Each frame covers the same fraction of what's left, so the feel doesn't depend on frame rate.
    #[allow(clippy::needless_range_loop)] // indexes three parallel arrays
    fn advance(&mut self, dt: f64) -> (i32, i32) {
        let k = if self.instant { 1.0 } else { 1.0 - (-dt / TAU).exp() };
        let mut out = [0i32; 2];
        for i in 0..2 {
            let r = self.remaining[i];
            let step = if r.abs() <= 1.0 { r } else { r * k };
            self.remaining[i] -= step;
            self.carry[i] += step;
            let whole = if self.remaining[i] == 0.0 { self.carry[i].round() } else { self.carry[i].trunc() };
            self.carry[i] = if self.remaining[i] == 0.0 { 0.0 } else { self.carry[i] - whole };
            out[i] = whole as i32;
        }
        (out[0], out[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a glide to the end at 120 Hz. Returns (total y distance, frames taken).
    fn run(g: &mut Glide) -> (i32, usize) {
        let (mut total, mut frames) = (0, 0);
        while !g.idle() {
            total += g.advance(1.0 / 120.0).1;
            frames += 1;
            assert!(frames < 1000);
        }
        (total, frames)
    }

    #[test]
    fn one_notch_glides_the_exact_distance() {
        let mut g = Glide::new();
        g.add(Instant::now(), 0.0, 50.0, true);
        let first = g.advance(1.0 / 120.0).1;
        assert!(first > 0 && first < 50, "eases in steps, got {first}");
        let (rest, frames) = run(&mut g);
        assert_eq!(first + rest, 50);
        assert!((20..60).contains(&frames), "glide should last ~0.2-0.5 s, took {frames} frames");
    }

    #[test]
    fn flicks_speed_up_and_reversal_stops() {
        let t = Instant::now();
        let mut g = Glide::new();
        for i in 0..3 {
            g.add(t + Duration::from_millis(30 * i), 0.0, -50.0, true);
        }
        assert_eq!(run(&mut g).0, -(50 + 60 + 70)); // 1.0x, 1.2x, 1.4x
        g.add(t, 0.0, 50.0, true);
        g.add(t + Duration::from_millis(10), 0.0, -50.0, true); // reverse mid-glide
        assert_eq!(run(&mut g).0, -50);
    }

    #[test]
    fn not_smooth_is_one_jump() {
        let mut g = Glide::new();
        g.add(Instant::now(), 0.0, -120.0, false);
        assert_eq!(g.advance(1.0 / 120.0), (0, -120));
        assert!(g.idle());
    }
}
