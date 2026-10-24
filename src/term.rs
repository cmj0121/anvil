//! Lending the real terminal to a child or a prompt while a front end owns it.
//!
//! The CLI installs no handoff, so [`with_terminal`] just runs its closure. A
//! front end that keeps the terminal in raw mode on the alternate screen (the
//! TUI) installs one; [`with_terminal`] then gives the terminal back for the
//! duration of the closure. Calls nest and overlap across threads through one
//! depth counter: only the first suspends and only the last out resumes.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Implemented by a front end that owns the terminal (the TUI).
pub trait Handoff: Send + Sync {
    /// Give the real terminal back (leave raw mode + alternate screen). Blocks until done.
    ///
    /// Must not panic, and must return rather than block when the channel to
    /// the thread that owns the terminal is closed: it runs with term's state
    /// locked.
    fn suspend(&self);
    /// Take it again and redraw. Blocks until done.
    ///
    /// Must not panic (it also runs during unwind, from a drop guard), and must
    /// return rather than block when the channel to the thread that owns the
    /// terminal is closed: it runs with term's state locked.
    fn resume(&self);
}

struct State {
    installed: Option<Arc<dyn Handoff>>,
    /// The handoff that suspended, kept so the matching resume reaches it.
    active: Option<Arc<dyn Handoff>>,
    depth: usize,
}

static STATE: Mutex<State> = Mutex::new(State {
    installed: None,
    active: None,
    depth: 0,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Installs `h` process-wide until the guard drops.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "installed by the TUI front end")
)]
pub fn install(h: Arc<dyn Handoff>) -> HandoffGuard {
    state().installed = Some(h);
    HandoffGuard
}

/// Uninstalls the handoff on drop.
#[must_use = "the handoff is uninstalled when the guard drops"]
pub struct HandoffGuard;

impl Drop for HandoffGuard {
    fn drop(&mut self) {
        state().installed = None;
    }
}

/// Runs `f` with the real terminal. No handoff installed, or one already
/// active (depth > 0, any thread): just runs `f`. Otherwise suspend → f → resume,
/// with resume also on unwind (a drop guard).
pub fn with_terminal<R>(f: impl FnOnce() -> R) -> R {
    let mut state = state();
    if state.depth == 0 {
        let Some(handoff) = state.installed.clone() else {
            drop(state);
            return f();
        };
        // Suspended under the lock: a concurrent caller waits until the
        // terminal is really back before it runs. STATE stays locked while
        // suspend (and later resume) blocks, so the thread that serves the
        // handoff must never call into this module while one is in flight,
        // and a Handoff must return, not block forever, once its channel is
        // closed.
        handoff.suspend();
        state.active = Some(handoff);
    }
    state.depth += 1;
    drop(state);
    let _lent = Lent;
    f()
}

/// One active [`with_terminal`] call; the last to drop resumes.
struct Lent;

impl Drop for Lent {
    fn drop(&mut self) {
        let mut state = state();
        state.depth -= 1;
        if state.depth == 0
            && let Some(handoff) = state.active.take()
        {
            handoff.resume();
        }
    }
}

/// Serializes tests that install a handoff (process-wide state).
#[cfg(test)]
pub fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub mod testing {
    use std::sync::{Arc, Mutex};
    use std::thread::{self, ThreadId};

    use super::{Handoff, HandoffGuard, install};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Event {
        Suspend,
        Resume,
    }

    /// Records each suspend and resume with the thread it came from.
    #[derive(Default)]
    pub struct Recorder {
        events: Mutex<Vec<(ThreadId, Event)>>,
    }

    impl Recorder {
        /// Installs a fresh recorder; hold [`super::test_lock`] meanwhile.
        pub fn install() -> (Arc<Recorder>, HandoffGuard) {
            let recorder = Arc::new(Recorder::default());
            let guard = install(recorder.clone());
            (recorder, guard)
        }

        /// The events from the calling thread, so tests elsewhere that also
        /// reach `with_terminal` cannot add to them.
        pub fn events(&self) -> Vec<Event> {
            let me = thread::current().id();
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == me)
                .map(|(_, event)| *event)
                .collect()
        }

        /// How many events arrived from any thread.
        pub fn count(&self) -> usize {
            self.events.lock().unwrap().len()
        }

        fn push(&self, event: Event) {
            let id = thread::current().id();
            self.events.lock().unwrap().push((id, event));
        }
    }

    impl Handoff for Recorder {
        fn suspend(&self) {
            self.push(Event::Suspend);
        }

        fn resume(&self) {
            self.push(Event::Resume);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Event, Recorder};
    use super::*;

    #[test]
    fn without_a_handoff_f_just_runs() {
        let _lock = test_lock();
        assert_eq!(with_terminal(|| 7), 7);
    }

    #[test]
    fn suspend_and_resume_wrap_f() {
        let _lock = test_lock();
        let (recorder, _guard) = Recorder::install();
        let seen = with_terminal(|| recorder.events());
        assert_eq!(seen, [Event::Suspend]);
        assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
    }

    #[test]
    fn nested_calls_suspend_once() {
        let _lock = test_lock();
        let (recorder, _guard) = Recorder::install();
        with_terminal(|| with_terminal(|| with_terminal(|| ())));
        assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
    }

    #[test]
    fn a_call_from_another_thread_while_active_just_runs() {
        let _lock = test_lock();
        let (recorder, _guard) = Recorder::install();
        with_terminal(|| {
            let other = Arc::clone(&recorder);
            std::thread::spawn(move || {
                with_terminal(|| ());
                assert_eq!(other.count(), 1);
            })
            .join()
            .unwrap();
        });
        assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
    }

    #[test]
    fn resume_runs_on_panic() {
        let _lock = test_lock();
        let (recorder, _guard) = Recorder::install();
        let result = std::panic::catch_unwind(|| with_terminal(|| panic!("boom")));
        assert!(result.is_err());
        assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
        // The depth is back to zero: the next call suspends again.
        with_terminal(|| ());
        assert_eq!(recorder.events().len(), 4);
    }

    #[test]
    fn dropping_the_guard_uninstalls() {
        let _lock = test_lock();
        let (recorder, guard) = Recorder::install();
        drop(guard);
        with_terminal(|| ());
        assert!(recorder.events().is_empty());
    }
}
