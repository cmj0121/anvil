//! The TUI's [`Handoff`]: a blocking rendezvous with the main thread, which
//! owns the terminal.
//!
//! On the main thread `suspend`/`resume` leave and enter the screen directly.
//! From any other thread `suspend` sends [`Msg::Suspend`] and waits for the
//! main loop's ack, so the terminal is really back before an ssh or editor
//! child reads from it; `resume` wakes the main loop, which re-enters and
//! redraws, then acks again. Every send and receive tolerates a closed
//! channel: a handoff runs with term's state locked, so it must return.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, ThreadId};

use super::app::Msg;
use crate::term::{self, Handoff, HandoffGuard};

/// Leaves and enters raw mode plus the alternate screen. Must not panic.
pub trait Screen: Send + Sync {
    fn leave(&self);
    fn enter(&self);
}

pub struct TuiHandoff {
    main: ThreadId,
    screen: Arc<dyn Screen>,
    app: Sender<Msg>,
    resume: Sender<()>,
    resumed: Mutex<Receiver<()>>,
    interrupted: Arc<AtomicBool>,
}

/// The main loop's end of the rendezvous; dropping it releases any waiting
/// worker.
pub struct MainSide {
    resume: Receiver<()>,
    resumed: Sender<()>,
    interrupted: Arc<AtomicBool>,
}

/// The main loop's end of the session. Its fields drop in order, on unwind
/// too: `rx` and `side` first, so a worker blocked in a handoff, which holds
/// term's state, is released before the guard drop takes that state.
pub struct Session {
    pub rx: Receiver<Msg>,
    pub side: MainSide,
    _guard: HandoffGuard,
}

/// Installs a handoff whose main thread is the calling thread; workers'
/// suspend requests arrive on `rx`, the receiver of `tx`.
pub fn start(
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    screen: Arc<dyn Screen>,
    interrupted: Arc<AtomicBool>,
) -> Session {
    let (handoff, side) = pair(tx, screen, interrupted);
    Session {
        rx,
        side,
        _guard: term::install(Arc::new(handoff)),
    }
}

/// A handoff whose main thread is the calling thread, and the main loop's
/// end of it. Workers' suspend requests arrive on `app`.
fn pair(
    app: Sender<Msg>,
    screen: Arc<dyn Screen>,
    interrupted: Arc<AtomicBool>,
) -> (TuiHandoff, MainSide) {
    let (resume_tx, resume_rx) = mpsc::channel();
    let (resumed_tx, resumed_rx) = mpsc::channel();
    let handoff = TuiHandoff {
        main: thread::current().id(),
        screen,
        app,
        resume: resume_tx,
        resumed: Mutex::new(resumed_rx),
        interrupted: interrupted.clone(),
    };
    let side = MainSide {
        resume: resume_rx,
        resumed: resumed_tx,
        interrupted,
    };
    (handoff, side)
}

impl TuiHandoff {
    fn on_main(&self) -> bool {
        thread::current().id() == self.main
    }
}

impl Handoff for TuiHandoff {
    fn suspend(&self) {
        if self.on_main() {
            self.screen.leave();
            return;
        }
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.app.send(Msg::Suspend(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    fn resume(&self) {
        if self.on_main() {
            self.screen.enter();
            note_interrupt(&self.interrupted);
            return;
        }
        if self.resume.send(()).is_ok() {
            let resumed = self.resumed.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = resumed.recv();
        }
    }
}

impl MainSide {
    /// Serves a worker's [`Msg::Suspend`]: leaves the screen, acks, blocks
    /// until the worker resumes (reading no keys meanwhile), then enters,
    /// runs `redraw` and acks again.
    pub fn serve(&self, screen: &dyn Screen, ack: Sender<()>, redraw: impl FnOnce()) {
        screen.leave();
        let _ = ack.send(());
        let _ = self.resume.recv();
        screen.enter();
        redraw();
        note_interrupt(&self.interrupted);
        let _ = self.resumed.send(());
    }
}

/// Ctrl-C at a prompt during the handoff reached anvil as SIGINT (D8).
fn note_interrupt(interrupted: &AtomicBool) {
    if interrupted.swap(false, Ordering::Relaxed) {
        log::warn!("interrupted at a prompt");
    }
}

#[cfg(test)]
pub mod tests {
    use std::time::Duration;

    use super::*;

    /// Records the screen's leave/enter calls and the test's own marks, in order.
    #[derive(Default)]
    pub struct Fake(Mutex<Vec<&'static str>>);

    impl Fake {
        pub fn mark(&self, what: &'static str) {
            self.0.lock().unwrap().push(what);
        }

        pub fn events(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Screen for Fake {
        fn leave(&self) {
            self.mark("leave");
        }

        fn enter(&self) {
            self.mark("enter");
        }
    }

    fn setup() -> (Arc<Fake>, Arc<TuiHandoff>, MainSide, Receiver<Msg>) {
        let fake = Arc::new(Fake::default());
        let (tx, rx) = mpsc::channel();
        let (handoff, side) = pair(tx, fake.clone(), Arc::new(AtomicBool::new(false)));
        (fake, Arc::new(handoff), side, rx)
    }

    /// Runs `f` on a thread and fails instead of hanging when it blocks.
    fn returns_soon(f: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            f();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the handoff blocked");
    }

    /// The main loop: serves one suspend request, then stops.
    fn serve_one(fake: &Fake, side: &MainSide, rx: &Receiver<Msg>) {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Suspend(ack)) => side.serve(fake, ack, || fake.mark("redraw")),
            _ => panic!("expected Msg::Suspend"),
        }
    }

    #[test]
    fn a_worker_suspend_and_resume_rendezvous_with_the_main_loop() {
        let (fake, handoff, side, rx) = setup();
        let worker = {
            let (fake, handoff) = (fake.clone(), handoff.clone());
            thread::spawn(move || {
                handoff.suspend();
                fake.mark("worker suspended");
                handoff.resume();
                fake.mark("worker resumed");
            })
        };
        serve_one(&fake, &side, &rx);
        worker.join().unwrap();
        assert_eq!(
            fake.events(),
            [
                "leave",
                "worker suspended",
                "enter",
                "redraw",
                "worker resumed"
            ]
        );
    }

    #[test]
    fn with_terminal_on_a_worker_goes_through_the_main_loop() {
        let _lock = term::test_lock();
        let (fake, handoff, side, rx) = setup();
        let _guard = term::install(handoff);
        let worker = {
            let fake = fake.clone();
            thread::spawn(move || term::with_terminal(|| fake.mark("child runs")))
        };
        serve_one(&fake, &side, &rx);
        worker.join().unwrap();
        assert_eq!(fake.events(), ["leave", "child runs", "enter", "redraw"]);
    }

    #[test]
    fn on_the_main_thread_the_screen_is_left_and_entered_directly() {
        let (fake, handoff, _side, rx) = setup();
        handoff.suspend();
        handoff.resume();
        assert_eq!(fake.events(), ["leave", "enter"]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_closed_app_channel_makes_suspend_and_resume_return() {
        let (fake, handoff, side, rx) = setup();
        drop(rx);
        drop(side);
        returns_soon(move || {
            handoff.suspend();
            handoff.resume();
        });
        assert!(fake.events().is_empty());
    }

    #[test]
    fn a_suspend_the_main_loop_drops_unserved_returns() {
        let (_fake, handoff, side, rx) = setup();
        let worker = {
            let handoff = handoff.clone();
            thread::spawn(move || handoff.suspend())
        };
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(request);
        drop(rx);
        drop(side);
        returns_soon(move || {
            worker.join().unwrap();
            handoff.resume();
        });
    }

    #[test]
    fn a_main_side_panic_while_a_worker_waits_in_suspend_tears_down() {
        let _lock = term::test_lock();
        returns_soon(|| {
            let fake: Arc<dyn Screen> = Arc::new(Fake::default());
            let (tx, rx) = mpsc::channel();
            let requeue = tx.clone();
            let session = start(tx, rx, fake, Arc::new(AtomicBool::new(false)));
            let worker = thread::spawn(|| term::with_terminal(|| ()));
            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                // The worker now waits in suspend with term's state locked;
                // its request goes back in the queue, unserved.
                let request = session.rx.recv().unwrap();
                assert!(matches!(request, Msg::Suspend(_)));
                requeue.send(request).unwrap();
                let _session = session;
                panic!("main loop panics");
            }));
            assert!(unwound.is_err());
            worker.join().unwrap();
        });
    }

    #[test]
    fn serving_clears_an_interrupt_from_the_prompt() {
        let (fake, handoff, side, rx) = setup();
        side.interrupted.store(true, Ordering::Relaxed);
        let worker = {
            let handoff = handoff.clone();
            thread::spawn(move || {
                handoff.suspend();
                handoff.resume();
            })
        };
        serve_one(&fake, &side, &rx);
        worker.join().unwrap();
        assert!(!handoff.interrupted.load(Ordering::Relaxed));
    }
}
