//! A run's cancel token: the first reason it was stopped for, read lock-free
//! by the workers, ssh and the signal handler.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;

/// Why a run's cancel token was set, which ended its remaining targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AbortReason {
    /// The user: Ctrl-C (SIGINT), or Esc or `q` in the TUI.
    Cancelled = 1,
    /// A record save failed, so the run stopped before writing more it could
    /// not record.
    SaveFailed = 2,
    /// The first signal was SIGTERM.
    Terminated = 3,
    /// The first signal was SIGHUP.
    HungUp = 4,
}

impl AbortReason {
    pub(crate) fn is_cancelled(&self) -> bool {
        *self == AbortReason::Cancelled
    }

    /// What a target's end line says.
    pub fn text(self) -> &'static str {
        match self {
            AbortReason::Cancelled => "cancelled",
            AbortReason::SaveFailed => "aborted: a record save failed",
            AbortReason::Terminated => "terminated",
            AbortReason::HungUp => "hung up",
        }
    }

    /// The exit status of a run this ended: 128 plus the signal's number for
    /// SIGTERM and SIGHUP, else none (the failure's own).
    pub fn exit_code(self) -> Option<u8> {
        match self {
            AbortReason::Cancelled | AbortReason::SaveFailed => None,
            AbortReason::Terminated => Some(143),
            AbortReason::HungUp => Some(129),
        }
    }
}

/// A run's cancel token: unset, or the first reason it was stopped for.
#[derive(Debug, Default)]
pub struct Cancel(AtomicU8);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    /// Sets `reason` unless a reason is already set; true when this call set
    /// it. The first reason always wins.
    pub fn stop(&self, reason: AbortReason) -> bool {
        (self.0)
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Whether the run was stopped, for any reason.
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }

    /// The reason, once set.
    pub fn reason(&self) -> Option<AbortReason> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            1 => Some(AbortReason::Cancelled),
            2 => Some(AbortReason::SaveFailed),
            3 => Some(AbortReason::Terminated),
            4 => Some(AbortReason::HungUp),
            other => unreachable!("cancel reason {other}"),
        }
    }

    /// A token already stopped as [`AbortReason::Cancelled`].
    #[cfg(any(test, feature = "test-util"))]
    pub fn cancelled() -> Cancel {
        Cancel::stopped(true)
    }

    /// A token stopped as [`AbortReason::Cancelled`] when `set`, else unset.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn stopped(set: bool) -> Cancel {
        let cancel = Cancel::new();
        if set {
            cancel.stop(AbortReason::Cancelled);
        }
        cancel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn only_a_signal_has_an_exit_code() {
        assert_eq!(AbortReason::Cancelled.exit_code(), None);
        assert_eq!(AbortReason::SaveFailed.exit_code(), None);
        assert_eq!(AbortReason::Terminated.exit_code(), Some(143));
        assert_eq!(AbortReason::HungUp.exit_code(), Some(129));
    }

    #[test]
    fn every_reason_reads_back_as_stored() {
        for reason in [
            AbortReason::Cancelled,
            AbortReason::SaveFailed,
            AbortReason::Terminated,
            AbortReason::HungUp,
        ] {
            let cancel = Cancel::new();
            cancel.stop(reason);
            assert_eq!(cancel.reason(), Some(reason));
        }
    }

    #[test]
    fn reasons_serialize_in_snake_case() {
        let json = serde_json::to_string(&[AbortReason::Terminated, AbortReason::HungUp]).unwrap();
        assert_eq!(json, r#"["terminated","hung_up"]"#);
    }

    #[test]
    fn a_save_failure_then_sigterm_keeps_the_save_failure() {
        let cancel = Cancel::new();
        cancel.stop(AbortReason::SaveFailed);
        assert!(!cancel.stop(AbortReason::Terminated));
        assert_eq!(cancel.reason(), Some(AbortReason::SaveFailed));
        assert_eq!(cancel.reason().and_then(AbortReason::exit_code), None);
    }

    #[test]
    fn a_new_token_is_unset() {
        let cancel = Cancel::new();
        assert!(!cancel.is_set());
        assert_eq!(cancel.reason(), None);
    }

    #[test]
    fn the_first_stop_wins_and_a_later_reason_is_ignored() {
        let cancel = Cancel::new();
        assert!(cancel.stop(AbortReason::SaveFailed));
        assert!(!cancel.stop(AbortReason::Cancelled));
        assert!(!cancel.stop(AbortReason::SaveFailed));
        assert!(cancel.is_set());
        assert_eq!(cancel.reason(), Some(AbortReason::SaveFailed));
    }

    #[test]
    fn of_racing_stops_exactly_one_wins_and_its_reason_is_kept() {
        for _ in 0..50 {
            let cancel = Arc::new(Cancel::new());
            let barrier = Arc::new(Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let cancel = Arc::clone(&cancel);
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        let reason = if i % 2 == 0 {
                            AbortReason::Cancelled
                        } else {
                            AbortReason::SaveFailed
                        };
                        barrier.wait();
                        cancel.stop(reason).then_some(reason)
                    })
                })
                .collect();
            let winners: Vec<AbortReason> = handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect();
            assert_eq!(winners.len(), 1);
            assert_eq!(cancel.reason(), Some(winners[0]));
        }
    }
}
