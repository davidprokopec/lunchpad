//! Who holds each pad. Presses come from three sources: the Launchpad itself,
//! a click on the on-screen pads, and a phone remote. A pad carries one gesture
//! at a time:
//!
//! * a press of a pad held by a **different** source is dropped (and its
//!   release later ignored), so the engine never sees two downs in a row;
//! * a press of a pad held by the **same** source (a hardware note-off or a
//!   click release that got lost) first ends the stale gesture, then starts
//!   the new one, so the first tap after a lost release works;
//! * a release only ends the gesture of the source that started it.
//!
//! Ownership is decided under the locks; events are dispatched after they are
//! released, through an emitter the caller supplies (the device manager emits
//! to the window and the engine; tests record).

use super::types::ButtonEvent;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PressSource {
    Hardware,
    /// A click on the on-screen pads.
    Desktop,
    /// A phone remote, by session.
    Remote(u64),
}

/// The held pads (shared with the LED renderer) and who holds each.
#[derive(Clone, Default)]
pub struct PadPresses {
    pressed: Arc<Mutex<HashSet<(u8, u8)>>>,
    owners: Arc<Mutex<HashMap<(u8, u8), PressSource>>>,
}

const NONE: &[bool] = &[];
const DOWN: &[bool] = &[true];
const UP: &[bool] = &[false];
const RESTART: &[bool] = &[false, true];

impl PadPresses {
    pub fn new(pressed: Arc<Mutex<HashSet<(u8, u8)>>>) -> PadPresses {
        PadPresses {
            pressed,
            owners: Arc::default(),
        }
    }

    /// The held pads, as the renderer reads them.
    pub fn pressed(&self) -> Arc<Mutex<HashSet<(u8, u8)>>> {
        self.pressed.clone()
    }

    pub fn owner(&self, x: u8, y: u8) -> Option<PressSource> {
        self.owners.lock().get(&(x, y)).copied()
    }

    /// Record a press or release; the `pressed` values to dispatch, in order.
    /// Lock order: `pressed`, then `owners`.
    pub fn gesture(&self, source: PressSource, x: u8, y: u8, down: bool) -> &'static [bool] {
        let mut set = self.pressed.lock();
        let mut owners = self.owners.lock();
        let held_by = owners.get(&(x, y)).copied();
        match (down, held_by) {
            (true, None) => {
                set.insert((x, y));
                owners.insert((x, y), source);
                DOWN
            }
            // A phone repeating its own press changes nothing.
            (true, Some(owner)) if owner == source && matches!(source, PressSource::Remote(_)) => {
                NONE
            }
            // The same device pressing again without a release in between: the release got lost.
            (true, Some(owner)) if owner == source => RESTART,
            (true, Some(_)) => NONE,
            (false, Some(owner)) if owner == source => {
                set.remove(&(x, y));
                owners.remove(&(x, y));
                UP
            }
            (false, _) => NONE,
        }
    }

    /// Record `event` from `source` and dispatch what follows through `emit`, in
    /// order (a synthetic release first when a stale gesture is ended). False when
    /// nothing was dispatched (the pad belongs to another source, or a release
    /// without a press).
    pub fn apply(
        &self,
        source: PressSource,
        event: ButtonEvent,
        emit: &dyn Fn(&ButtonEvent),
    ) -> bool {
        let steps = self.gesture(source, event.x, event.y, event.pressed);
        for &down in steps {
            if down {
                emit(&event);
            } else {
                emit(&ButtonEvent {
                    pressed: false,
                    value: 0,
                    ..event
                });
            }
        }
        !steps.is_empty()
    }

    /// End every gesture of the sources `which` selects, a release each through `emit`.
    pub fn release_where(
        &self,
        which: impl Fn(PressSource) -> bool,
        release: impl Fn(u8, u8) -> ButtonEvent,
        emit: &dyn Fn(&ButtonEvent),
    ) {
        let ended: Vec<(u8, u8)> = {
            let mut set = self.pressed.lock();
            let mut owners = self.owners.lock();
            let pads: Vec<(u8, u8)> = owners
                .iter()
                .filter(|(_, s)| which(**s))
                .map(|(p, _)| *p)
                .collect();
            for pad in &pads {
                owners.remove(pad);
                set.remove(pad);
            }
            pads
        };
        for (x, y) in ended {
            emit(&release(x, y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PressSource::*;

    fn ev(x: u8, y: u8, pressed: bool) -> ButtonEvent {
        ButtonEvent {
            x,
            y,
            pressed,
            note: 11,
            cc: false,
            value: if pressed { 100 } else { 0 },
        }
    }

    /// Applies events and returns what reached the emitter as (x, y, pressed).
    struct Rig {
        presses: PadPresses,
        seen: Mutex<Vec<(u8, u8, bool)>>,
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                presses: PadPresses::default(),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn apply(&self, source: PressSource, x: u8, y: u8, pressed: bool) -> bool {
            self.presses.apply(source, ev(x, y, pressed), &|e| {
                self.seen.lock().push((e.x, e.y, e.pressed))
            })
        }
        fn take(&self) -> Vec<(u8, u8, bool)> {
            std::mem::take(&mut *self.seen.lock())
        }
        fn held(&self) -> bool {
            self.presses.pressed().lock().contains(&(0, 0))
        }
    }

    #[test]
    fn a_hardware_double_press_ends_the_stale_hold_then_starts_anew() {
        let r = Rig::new();
        assert!(r.apply(Hardware, 0, 0, true));
        // The note-off got lost; the next press must still be one clean tap.
        assert!(r.apply(Hardware, 0, 0, true));
        assert!(r.apply(Hardware, 0, 0, false));
        assert_eq!(
            r.take(),
            vec![(0, 0, true), (0, 0, false), (0, 0, true), (0, 0, false)],
            "never two downs in a row"
        );
        assert!(!r.held());
        assert!(
            !r.apply(Hardware, 0, 0, false),
            "a release without a press does nothing"
        );
        assert!(r.take().is_empty());
    }

    #[test]
    fn the_synthetic_release_carries_no_velocity() {
        let presses = PadPresses::default();
        let seen = Mutex::new(Vec::new());
        presses.apply(Hardware, ev(1, 2, true), &|e| seen.lock().push(*e));
        presses.apply(Hardware, ev(1, 2, true), &|e| seen.lock().push(*e));
        let seen: Vec<_> = seen
            .into_inner()
            .iter()
            .map(|e| (e.x, e.y, e.pressed, e.note, e.cc, e.value))
            .collect();
        assert_eq!(seen[1], (1, 2, false, 11, false, 0));
        assert_eq!(seen[2], (1, 2, true, 11, false, 100));
    }

    #[test]
    fn a_phone_held_pad_ignores_the_hardware_until_the_phone_lets_go() {
        let r = Rig::new();
        assert!(r.apply(Remote(1), 0, 0, true));
        assert!(
            !r.apply(Hardware, 0, 0, true),
            "the hardware press is dropped"
        );
        assert!(!r.apply(Hardware, 0, 0, false), "and so is its release");
        assert!(r.held(), "the phone still holds the pad");
        assert!(r.apply(Remote(1), 0, 0, false));
        assert_eq!(r.take(), vec![(0, 0, true), (0, 0, false)]);
        assert!(r.apply(Hardware, 0, 0, true), "free again");
    }

    #[test]
    fn a_hardware_held_pad_refuses_phones_and_clicks() {
        let r = Rig::new();
        assert!(r.apply(Hardware, 0, 0, true));
        assert!(!r.apply(Remote(1), 0, 0, true));
        assert!(!r.apply(Remote(1), 0, 0, false));
        assert!(!r.apply(Desktop, 0, 0, true));
        assert!(!r.apply(Desktop, 0, 0, false));
        assert!(r.held());
        assert!(r.apply(Hardware, 0, 0, false));
        assert_eq!(r.take(), vec![(0, 0, true), (0, 0, false)]);
    }

    #[test]
    fn desktop_clicks_and_phones_do_not_take_each_others_pads() {
        let r = Rig::new();
        assert!(r.apply(Desktop, 0, 0, true));
        assert!(!r.apply(Remote(1), 0, 0, true), "the phone is refused");
        assert!(
            r.apply(Desktop, 0, 0, true),
            "a lost click release restarts the click's own gesture"
        );
        assert!(r.apply(Desktop, 0, 0, false));
        assert!(r.apply(Remote(1), 0, 0, true));
        assert!(!r.apply(Desktop, 0, 0, true), "the click is refused");
        assert!(!r.apply(Remote(2), 0, 0, true), "another phone too");
        assert!(
            !r.apply(Remote(1), 0, 0, true),
            "the same phone repeating its press changes nothing"
        );
        assert!(!r.apply(Remote(2), 0, 0, false));
        assert!(r.apply(Remote(1), 0, 0, false));
        assert_eq!(
            r.take(),
            vec![
                (0, 0, true),
                (0, 0, false),
                (0, 0, true),
                (0, 0, false),
                (0, 0, true),
                (0, 0, false)
            ]
        );
    }

    #[test]
    fn a_disconnect_or_revoke_releases_only_what_that_source_holds() {
        let r = Rig::new();
        r.apply(Remote(1), 0, 0, true);
        r.apply(Remote(1), 1, 0, true);
        r.apply(Remote(2), 2, 0, true);
        r.apply(Hardware, 3, 0, true);
        r.take();
        r.presses
            .release_where(|s| s == Remote(1), |x, y| ev(x, y, false), &|e| {
                r.seen.lock().push((e.x, e.y, e.pressed))
            });
        let mut released = r.take();
        released.sort();
        assert_eq!(released, vec![(0, 0, false), (1, 0, false)]);
        assert_eq!(r.presses.owner(2, 0), Some(Remote(2)));
        assert_eq!(r.presses.owner(3, 0), Some(Hardware));
        assert!(
            !r.apply(Remote(1), 0, 0, false),
            "a late release from the revoked phone does nothing"
        );
        r.presses
            .release_where(|s| matches!(s, Remote(_)), |x, y| ev(x, y, false), &|e| {
                r.seen.lock().push((e.x, e.y, e.pressed))
            });
        assert_eq!(r.take(), vec![(2, 0, false)]);
        let held: Vec<(u8, u8)> = r.presses.pressed().lock().iter().copied().collect();
        assert_eq!(held, vec![(3, 0)], "the hardware's hold is untouched");
    }
}
