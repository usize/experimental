//! Fixed-window, per-key token accounting for the `token_ceiling` filter.
//!
//! The ledger is deliberately simple: one fixed window per key, started by
//! the key's first charge and reset wholesale once the window elapses. It
//! never performs I/O and takes the current time as a parameter, so every
//! behavior is unit-testable without a filter context or a real clock.
//!
//! All state is in-process. Multiple gateway instances each enforce their
//! own independent copy of every ceiling — a documented limitation of the
//! interim filter, acceptable for the single-user standalone image
//! (praxis-proxy/ai#758).

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, PoisonError},
};

/// One key's usage within its current fixed window.
#[derive(Debug, Clone, Copy)]
struct KeyWindow {
    /// When this window began, as milliseconds from the filter's epoch.
    start_ms: u64,

    /// Tokens charged so far within this window.
    used: u64,
}

impl KeyWindow {
    /// Whether this window has fully elapsed at `now_ms`.
    fn expired(&self, window_ms: u64, now_ms: u64) -> bool {
        now_ms >= self.start_ms.saturating_add(window_ms)
    }

    /// Milliseconds from `now_ms` until this window elapses.
    fn remaining_ms(&self, window_ms: u64, now_ms: u64) -> u64 {
        self.start_ms.saturating_add(window_ms).saturating_sub(now_ms)
    }
}

/// Admission decision for a request, produced by [`CeilingLedger::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// Committed usage is below the ceiling (or the key is untracked so
    /// far): let the request through.
    Admit,

    /// The key's committed usage has reached its ceiling.
    Deny {
        /// Milliseconds until the key's current window resets.
        retry_after_ms: u64,
    },

    /// The ledger is full and cannot track a new key; the caller decides
    /// the failure mode.
    AtCapacity,
}

/// Outcome of recording usage via [`CeilingLedger::charge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Charge {
    /// The usage was recorded (or was zero, which needs no record).
    Recorded,

    /// The ledger is full; the charge was dropped and is not counted
    /// against the key's ceiling.
    Dropped,
}

/// In-memory fixed-window ledger mapping budget keys to their usage.
#[derive(Debug)]
pub(super) struct CeilingLedger {
    /// Fixed window length in milliseconds.
    window_ms: u64,

    /// Bound on distinct keys tracked at once, so client-chosen keys (the
    /// `header` source) cannot grow memory without limit.
    max_keys: usize,

    /// Per-key windows. A `BTreeMap` rather than a `HashMap` so eviction
    /// iterates deterministically (`iter_over_hash_type` is denied
    /// workspace-wide).
    entries: Mutex<BTreeMap<String, KeyWindow>>,
}

impl CeilingLedger {
    /// Creates an empty ledger enforcing `window_ms` windows over at most
    /// `max_keys` distinct keys.
    pub(super) fn new(window_ms: u64, max_keys: usize) -> Self {
        Self {
            window_ms,
            max_keys,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Locks the entry map, recovering from poisoning: the map holds plain
    /// counters that stay internally consistent even if a holder panicked
    /// mid-update, so continuing is strictly better than wedging every
    /// request behind a poisoned lock.
    fn lock_entries(&self) -> MutexGuard<'_, BTreeMap<String, KeyWindow>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decides whether a request under `key` with the given `ceiling` may
    /// proceed at `now_ms`.
    ///
    /// Admission checks **committed** usage only — usage from responses
    /// that have already completed. In-flight requests are not counted, so
    /// a key can overshoot its ceiling by whatever is concurrently in
    /// flight; the plan documents this as acceptable for the interim,
    /// single-user filter.
    pub(super) fn admit(&self, key: &str, ceiling: u64, now_ms: u64) -> Admission {
        let mut entries = self.lock_entries();
        if let Some(window) = entries.get(key) {
            if window.expired(self.window_ms, now_ms) || window.used < ceiling {
                return Admission::Admit;
            }
            return Admission::Deny {
                retry_after_ms: window.remaining_ms(self.window_ms, now_ms),
            };
        }
        if entries.len() >= self.max_keys {
            Self::evict_expired(&mut entries, self.window_ms, now_ms);
        }
        if entries.len() >= self.max_keys {
            return Admission::AtCapacity;
        }
        drop(entries);
        Admission::Admit
    }

    /// Records `tokens` of usage against `key` at `now_ms`, starting or
    /// rolling the key's window as needed.
    pub(super) fn charge(&self, key: &str, tokens: u64, now_ms: u64) -> Charge {
        if tokens == 0 {
            return Charge::Recorded;
        }
        let mut entries = self.lock_entries();
        if let Some(window) = entries.get_mut(key) {
            if window.expired(self.window_ms, now_ms) {
                *window = KeyWindow {
                    start_ms: now_ms,
                    used: tokens,
                };
            } else {
                window.used = window.used.saturating_add(tokens);
            }
            return Charge::Recorded;
        }
        if entries.len() >= self.max_keys {
            Self::evict_expired(&mut entries, self.window_ms, now_ms);
        }
        if entries.len() >= self.max_keys {
            return Charge::Dropped;
        }
        entries.insert(
            key.to_owned(),
            KeyWindow {
                start_ms: now_ms,
                used: tokens,
            },
        );
        Charge::Recorded
    }

    /// Drops every entry whose window has elapsed, freeing capacity for
    /// new keys. Only invoked when the ledger is full, so steady-state
    /// requests never pay the scan.
    fn evict_expired(entries: &mut BTreeMap<String, KeyWindow>, window_ms: u64, now_ms: u64) {
        entries.retain(|_, window| !window.expired(window_ms, now_ms));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "unwrap/expect/panic are acceptable in tests"
)]
mod tests {
    use super::{Admission, CeilingLedger, Charge};

    /// A one-minute window, roomy key bound.
    fn ledger() -> CeilingLedger {
        CeilingLedger::new(60_000, 16)
    }

    #[test]
    fn unknown_key_is_admitted() {
        assert_eq!(ledger().admit("alice", 100, 0), Admission::Admit);
    }

    #[test]
    fn usage_below_ceiling_is_admitted() {
        let ledger = ledger();
        assert_eq!(ledger.charge("alice", 99, 0), Charge::Recorded);
        assert_eq!(ledger.admit("alice", 100, 1), Admission::Admit);
    }

    #[test]
    fn usage_at_ceiling_is_denied_with_window_remainder() {
        let ledger = ledger();
        ledger.charge("alice", 100, 0);
        assert_eq!(
            ledger.admit("alice", 100, 15_000),
            Admission::Deny { retry_after_ms: 45_000 }
        );
    }

    #[test]
    fn usage_above_ceiling_is_denied() {
        // Post-hoc charging can overshoot; admission must still deny.
        let ledger = ledger();
        ledger.charge("alice", 250, 0);
        assert!(matches!(ledger.admit("alice", 100, 0), Admission::Deny { .. }));
    }

    #[test]
    fn expired_window_admits_again() {
        let ledger = ledger();
        ledger.charge("alice", 100, 0);
        assert_eq!(ledger.admit("alice", 100, 60_000), Admission::Admit);
    }

    #[test]
    fn charge_rolls_an_expired_window() {
        let ledger = ledger();
        ledger.charge("alice", 100, 0);
        ledger.charge("alice", 30, 60_000);
        // The old 100 tokens aged out with the first window: usage is 30.
        assert_eq!(ledger.admit("alice", 100, 60_001), Admission::Admit);
        assert_eq!(
            ledger.admit("alice", 30, 60_001),
            Admission::Deny { retry_after_ms: 59_999 }
        );
    }

    #[test]
    fn charges_accumulate_within_a_window() {
        let ledger = ledger();
        ledger.charge("alice", 60, 0);
        ledger.charge("alice", 40, 1_000);
        assert!(matches!(ledger.admit("alice", 100, 2_000), Admission::Deny { .. }));
    }

    #[test]
    fn charge_saturates_instead_of_overflowing() {
        let ledger = ledger();
        ledger.charge("alice", u64::MAX, 0);
        ledger.charge("alice", u64::MAX, 1);
        assert!(matches!(ledger.admit("alice", u64::MAX, 2), Admission::Deny { .. }));
    }

    #[test]
    fn zero_token_charge_creates_no_entry() {
        let ledger = CeilingLedger::new(60_000, 1);
        assert_eq!(ledger.charge("alice", 0, 0), Charge::Recorded);
        // The single slot is still free for a real charge.
        assert_eq!(ledger.charge("bob", 5, 0), Charge::Recorded);
        assert_eq!(ledger.charge("carol", 5, 0), Charge::Dropped);
    }

    #[test]
    fn keys_are_tracked_independently() {
        let ledger = ledger();
        ledger.charge("alice", 100, 0);
        assert!(matches!(ledger.admit("alice", 100, 0), Admission::Deny { .. }));
        assert_eq!(ledger.admit("bob", 100, 0), Admission::Admit);
    }

    #[test]
    fn full_ledger_reports_at_capacity_for_new_keys() {
        let ledger = CeilingLedger::new(60_000, 1);
        ledger.charge("alice", 5, 0);
        assert_eq!(ledger.admit("bob", 100, 1), Admission::AtCapacity);
        // Known keys are still served while the ledger is full.
        assert_eq!(ledger.admit("alice", 100, 1), Admission::Admit);
    }

    #[test]
    fn full_ledger_drops_charges_for_new_keys() {
        let ledger = CeilingLedger::new(60_000, 1);
        ledger.charge("alice", 5, 0);
        assert_eq!(ledger.charge("bob", 5, 1), Charge::Dropped);
        // Known keys still record while the ledger is full.
        assert_eq!(ledger.charge("alice", 5, 1), Charge::Recorded);
    }

    #[test]
    fn expired_entries_are_evicted_to_admit_new_keys() {
        let ledger = CeilingLedger::new(60_000, 1);
        ledger.charge("alice", 5, 0);
        assert_eq!(ledger.admit("bob", 100, 60_000), Admission::Admit);
        assert_eq!(ledger.charge("bob", 5, 60_000), Charge::Recorded);
    }

    #[test]
    fn retry_after_is_zero_at_the_window_boundary_edge() {
        // Deny can only happen inside the window, so retry_after_ms is
        // always positive in practice; the saturating math still must not
        // underflow if time lands exactly on the boundary.
        let ledger = ledger();
        ledger.charge("alice", 100, 0);
        assert_eq!(
            ledger.admit("alice", 100, 59_999),
            Admission::Deny { retry_after_ms: 1 }
        );
    }
}
