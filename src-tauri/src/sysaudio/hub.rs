//! Change notifications from the audio worker to any number of subscribers.
//!
//! A notification only says *what* changed (devices, apps, status); the
//! subscriber then reads the authoritative snapshot from the handle.
//! Notifications coalesce per subscriber: a slow subscriber holds at most one
//! wake-up token plus a bit set, so the worker never blocks and memory stays
//! bounded however many changes arrive. Notifying is a few atomic operations
//! and a non-blocking `try_send` under a leaf lock: safe to call while the
//! engine holds its own publication locks, and it never runs subscriber code.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What changed since the subscriber last looked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes(u8);

impl Changes {
    pub const DEVICES: Changes = Changes(1);
    pub const APPS: Changes = Changes(2);
    pub const STATUS: Changes = Changes(4);
    pub const ALL: Changes = Changes(7);

    pub fn devices(self) -> bool {
        self.0 & Self::DEVICES.0 != 0
    }
    pub fn apps(self) -> bool {
        self.0 & Self::APPS.0 != 0
    }
    pub fn status(self) -> bool {
        self.0 & Self::STATUS.0 != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Changes {
    type Output = Changes;
    fn bitor(self, other: Changes) -> Changes {
        Changes(self.0 | other.0)
    }
}

struct Subscriber {
    pending: Arc<AtomicU8>,
    wake: SyncSender<()>,
}

#[derive(Default)]
pub struct Hub {
    subscribers: Mutex<Vec<Subscriber>>,
}

impl Hub {
    pub fn subscribe(&self) -> Subscription {
        let (wake, rx) = mpsc::sync_channel(1);
        let pending = Arc::new(AtomicU8::new(0));
        lock(&self.subscribers).push(Subscriber {
            pending: Arc::clone(&pending),
            wake,
        });
        Subscription { pending, wake: rx }
    }

    /// Records `changes` for every subscriber and wakes it. Subscribers whose
    /// `Subscription` was dropped are forgotten.
    pub fn notify(&self, changes: Changes) {
        if changes.is_empty() {
            return;
        }
        lock(&self.subscribers).retain(|s| {
            s.pending.fetch_or(changes.0, Ordering::AcqRel);
            // Full: a wake-up is already queued and will see the new bits.
            !matches!(s.wake.try_send(()), Err(TrySendError::Disconnected(_)))
        });
    }

    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        lock(&self.subscribers).len()
    }
}

/// The receiving end of [`super::SystemAudio::subscribe`]. Drop it to unsubscribe.
pub struct Subscription {
    pending: Arc<AtomicU8>,
    wake: Receiver<()>,
}

impl Subscription {
    fn take(&self) -> Changes {
        Changes(self.pending.swap(0, Ordering::AcqRel))
    }

    /// Blocks until something changed. `None` once the audio service is gone.
    pub fn recv(&self) -> Option<Changes> {
        loop {
            self.wake.recv().ok()?;
            let changes = self.take();
            if !changes.is_empty() {
                return Some(changes);
            }
        }
    }

    /// Like [`Self::recv`] with a deadline. `Ok(empty)` never happens.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Changes, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            self.wake
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            let changes = self.take();
            if !changes.is_empty() {
                return Ok(changes);
            }
        }
    }

    /// Whatever changed so far, without waiting (may be empty).
    pub fn try_recv(&self) -> Result<Changes, TryRecvError> {
        match self.wake.try_recv() {
            Ok(()) | Err(TryRecvError::Empty) => Ok(self.take()),
            Err(TryRecvError::Disconnected) => Err(TryRecvError::Disconnected),
        }
    }
}

/// The hub's critical section is a retain over atomics and try_send, so a
/// poisoned lock still holds consistent data.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_coalesce_per_subscriber_without_blocking() {
        let hub = Hub::default();
        let a = hub.subscribe();
        let b = hub.subscribe();
        for _ in 0..10_000 {
            hub.notify(Changes::DEVICES);
        }
        hub.notify(Changes::APPS);
        assert_eq!(
            a.recv_timeout(Duration::from_secs(1)),
            Ok(Changes::DEVICES | Changes::APPS)
        );
        assert_eq!(
            a.try_recv(),
            Ok(Changes::default()),
            "one wake-up, not ten thousand"
        );
        assert_eq!(b.recv(), Some(Changes::DEVICES | Changes::APPS));
        hub.notify(Changes::STATUS);
        assert_eq!(a.recv(), Some(Changes::STATUS));
    }

    #[test]
    fn dropped_subscriptions_are_forgotten_and_a_dropped_hub_ends_recv() {
        let hub = Hub::default();
        let kept = hub.subscribe();
        drop(hub.subscribe());
        hub.notify(Changes::DEVICES);
        assert_eq!(hub.subscriber_count(), 1);
        assert_eq!(kept.recv(), Some(Changes::DEVICES));
        drop(hub);
        assert_eq!(kept.recv(), None);
        assert_eq!(
            kept.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn empty_notifications_do_not_wake() {
        let hub = Hub::default();
        let sub = hub.subscribe();
        hub.notify(Changes::default());
        assert_eq!(
            sub.recv_timeout(Duration::from_millis(20)),
            Err(RecvTimeoutError::Timeout)
        );
    }
}
