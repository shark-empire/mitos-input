//! Multi-finger gesture recognition on top of per-frame touch snapshots,
//! plus the mapping from recognized gestures to desktop-level actions.
//!
//! Single-finger taps are detected inline in [`crate::touch`] (they're
//! naturally a property of one slot's lifecycle). This module handles
//! genuinely multi-finger patterns -- swipe, pinch, and hold -- which need a
//! view across every currently-active contact on a device *at once*.
//! [`GestureEngine::feed_frame`] takes the full set of active points as of
//! one `SYN_REPORT`, rather than one point at a time: comparing centroids
//! after only *some* of a frame's fingers have been updated would see
//! spurious spreads/contractions that aren't really there. [`crate::touch::TouchDevice`]
//! owns a `GestureEngine` and calls it once per report, which is why this
//! module depends on [`crate::touch::TouchPoint`] rather than driving the
//! detection from individual events itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::device::DeviceId;
use crate::events::{GestureEvent, Timestamp};
use crate::touch::TouchPoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDirection {
    Up,
    Down,
    Left,
    Right,
}

/// A recognized gesture translated into what it means for the desktop shell
/// (workspace switching, overview, ...). Bindings here mirror common
/// desktop-touchpad conventions (3-finger for window/overview actions,
/// 4-finger for workspaces).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopGesture {
    SwitchWorkspaceNext,
    SwitchWorkspacePrev,
    ShowOverview,
    ShowDesktop,
}

/// Map a recognized [`GestureEvent`] to a desktop-level action, if any
/// binding applies. Not every gesture has one (e.g. a 2-finger swipe is
/// usually just scrolling, handled elsewhere).
pub fn map_to_desktop_gesture(event: &GestureEvent) -> Option<DesktopGesture> {
    match event {
        GestureEvent::Swipe { fingers: 3, direction: SwipeDirection::Up, .. } => {
            Some(DesktopGesture::ShowOverview)
        }
        GestureEvent::Swipe { fingers: 3, direction: SwipeDirection::Down, .. } => {
            Some(DesktopGesture::ShowDesktop)
        }
        GestureEvent::Swipe { fingers: 4, direction: SwipeDirection::Left, .. } => {
            Some(DesktopGesture::SwitchWorkspaceNext)
        }
        GestureEvent::Swipe { fingers: 4, direction: SwipeDirection::Right, .. } => {
            Some(DesktopGesture::SwitchWorkspacePrev)
        }
        _ => None,
    }
}

struct DeviceGestureState {
    /// slot -> normalized (x, y), as of the last frame fed in.
    points: HashMap<usize, (f64, f64)>,
    last_centroid: Option<(f64, f64)>,
    last_avg_distance: Option<f64>,
    stable_since: Option<Instant>,
    hold_fired: bool,
}

impl DeviceGestureState {
    fn new() -> Self {
        DeviceGestureState {
            points: HashMap::new(),
            last_centroid: None,
            last_avg_distance: None,
            stable_since: None,
            hold_fired: false,
        }
    }

    /// Clear the frame-to-frame baseline so the next comparison doesn't use
    /// a now-stale centroid/spread (called whenever the finger count
    /// changes, since that shifts both discontinuously).
    fn reset_baseline(&mut self) {
        self.last_centroid = None;
        self.last_avg_distance = None;
        self.stable_since = None;
        self.hold_fired = false;
    }
}

fn centroid_of(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(ax, ay), (x, y)| (ax + x, ay + y));
    (sx / n, sy / n)
}

fn average_pairwise_distance(points: &[(f64, f64)]) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }
    let mut sum = 0.0;
    let mut count = 0u32;
    for i in 0..points.len() {
        for j in (i + 1)..points.len() {
            let dx = points[i].0 - points[j].0;
            let dy = points[i].1 - points[j].1;
            sum += (dx * dx + dy * dy).sqrt();
            count += 1;
        }
    }
    sum / count as f64
}

/// Recognizes swipe/pinch/hold gestures from per-frame touch snapshots,
/// across possibly many devices at once (state is keyed by device id).
pub struct GestureEngine {
    active: HashMap<DeviceId, DeviceGestureState>,
    /// Normalized centroid movement per frame before a swipe is reported.
    swipe_threshold: f64,
    /// Fractional change in average finger spacing before a pinch is reported.
    pinch_threshold: f64,
    /// How long fingers must stay still (within the thresholds above) before a `Hold` fires.
    hold_duration: Duration,
}

impl GestureEngine {
    pub fn new() -> Self {
        GestureEngine {
            active: HashMap::new(),
            swipe_threshold: 0.03,
            pinch_threshold: 0.08,
            hold_duration: Duration::from_millis(500),
        }
    }

    pub fn with_thresholds(mut self, swipe: f64, pinch: f64, hold: Duration) -> Self {
        self.swipe_threshold = swipe;
        self.pinch_threshold = pinch;
        self.hold_duration = hold;
        self
    }

    /// Feed the complete set of currently-active points for `device` as of
    /// one `SYN_REPORT`. Fewer than two points can't be a multi-finger
    /// gesture (and clears any tracked state for the device); a finger
    /// count that changed since the last frame resets the baseline instead
    /// of comparing across the discontinuity.
    pub fn feed_frame(&mut self, device: DeviceId, points: &[TouchPoint], time: Timestamp) -> Option<GestureEvent> {
        if points.len() < 2 {
            self.active.remove(&device);
            return None;
        }
        let state = self.active.entry(device).or_insert_with(DeviceGestureState::new);
        let prev_finger_count = state.points.len();
        state.points = points.iter().map(|p| (p.slot, (p.x, p.y))).collect();
        if prev_finger_count != points.len() {
            state.reset_baseline();
            return None;
        }
        self.detect(device, time)
    }

    fn detect(&mut self, device: DeviceId, time: Timestamp) -> Option<GestureEvent> {
        let state = self.active.get_mut(&device)?;
        let fingers = state.points.len();

        let coords: Vec<(f64, f64)> = state.points.values().copied().collect();
        let centroid = centroid_of(&coords);
        let avg_dist = average_pairwise_distance(&coords);
        let mut result = None;

        if let (Some(prev_c), Some(prev_d)) = (state.last_centroid, state.last_avg_distance) {
            let scale_ratio = if prev_d > f64::EPSILON { avg_dist / prev_d } else { 1.0 };
            let dx = centroid.0 - prev_c.0;
            let dy = centroid.1 - prev_c.1;
            let moved = (dx * dx + dy * dy).sqrt();

            if (scale_ratio - 1.0).abs() > self.pinch_threshold {
                result = Some(GestureEvent::Pinch {
                    device,
                    time,
                    fingers: fingers as u8,
                    scale: scale_ratio,
                    rotation: 0.0,
                });
                state.stable_since = None;
            } else if moved > self.swipe_threshold {
                let direction = if dx.abs() > dy.abs() {
                    if dx > 0.0 { SwipeDirection::Right } else { SwipeDirection::Left }
                } else if dy > 0.0 {
                    SwipeDirection::Down
                } else {
                    SwipeDirection::Up
                };
                result = Some(GestureEvent::Swipe {
                    device,
                    time,
                    fingers: fingers as u8,
                    direction,
                    dx,
                    dy,
                });
                state.stable_since = None;
            } else if state.stable_since.is_none() {
                state.stable_since = Some(Instant::now());
                state.hold_fired = false;
            } else if !state.hold_fired && state.stable_since.unwrap().elapsed() > self.hold_duration {
                result = Some(GestureEvent::Hold {
                    device,
                    time,
                    fingers: fingers as u8,
                    x: centroid.0,
                    y: centroid.1,
                });
                state.hold_fired = true;
            }
        } else {
            state.stable_since = Some(Instant::now());
            state.hold_fired = false;
        }

        state.last_centroid = Some(centroid);
        state.last_avg_distance = Some(avg_dist);
        result
    }
}

impl Default for GestureEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    fn p(slot: usize, x: f64, y: f64) -> TouchPoint {
        TouchPoint {
            slot,
            tracking_id: slot as i32,
            x,
            y,
            pressure: 0.0,
            major: 0.0,
            minor: 0.0,
        }
    }

    #[test]
    fn two_finger_lockstep_translation_is_a_swipe() {
        let mut engine = GestureEngine::new();
        let device = DeviceId::new();
        let t = StdDuration::from_secs(0);

        // Frame 1: fingers appear (establishes finger count only).
        assert!(engine.feed_frame(device, &[p(0, 0.3, 0.5), p(1, 0.6, 0.5)], t).is_none());
        // Frame 2: same count -> establishes the position/spacing baseline.
        assert!(engine.feed_frame(device, &[p(0, 0.35, 0.5), p(1, 0.65, 0.5)], t).is_none());
        // Frame 3: both shifted right by the same amount -> pure translation.
        let result = engine.feed_frame(device, &[p(0, 0.4, 0.5), p(1, 0.7, 0.5)], t);

        match result.expect("expected a recognized gesture") {
            GestureEvent::Swipe { fingers, direction, .. } => {
                assert_eq!(fingers, 2);
                assert_eq!(direction, SwipeDirection::Right);
            }
            other => panic!("expected Swipe, got {other:?}"),
        }
    }

    #[test]
    fn spreading_fingers_is_a_pinch() {
        let mut engine = GestureEngine::new();
        let device = DeviceId::new();
        let t = StdDuration::from_secs(0);

        assert!(engine.feed_frame(device, &[p(0, 0.45, 0.5), p(1, 0.55, 0.5)], t).is_none());
        assert!(engine.feed_frame(device, &[p(0, 0.45, 0.5), p(1, 0.55, 0.5)], t).is_none());
        let result = engine.feed_frame(device, &[p(0, 0.3, 0.5), p(1, 0.7, 0.5)], t);

        match result.expect("expected a recognized gesture") {
            GestureEvent::Pinch { fingers, scale, .. } => {
                assert_eq!(fingers, 2);
                assert!(scale > 1.0, "expected outward pinch (scale > 1), got {scale}");
            }
            other => panic!("expected Pinch, got {other:?}"),
        }
    }

    #[test]
    fn single_finger_never_produces_a_gesture() {
        let mut engine = GestureEngine::new();
        let device = DeviceId::new();
        let t = StdDuration::from_secs(0);
        engine.feed_frame(device, &[p(0, 0.3, 0.5)], t);
        let result = engine.feed_frame(device, &[p(0, 0.9, 0.9)], t);
        assert!(result.is_none());
    }

    #[test]
    fn lifting_a_finger_clears_state() {
        let mut engine = GestureEngine::new();
        let device = DeviceId::new();
        let t = StdDuration::from_secs(0);
        engine.feed_frame(device, &[p(0, 0.3, 0.5), p(1, 0.6, 0.5)], t);
        engine.feed_frame(device, &[p(0, 0.3, 0.5)], t); // finger 1 lifted
        assert!(!engine.active.contains_key(&device));
    }
}
