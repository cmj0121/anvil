//! Runs one job per target on up to N worker threads and reports each target
//! as it starts and ends, on the caller's thread.
//!
//! Workers take targets in inventory order from a shared counter, so targets
//! start `local` first, then by name; they end in any order, and the results
//! come back in inventory order. Once `cancel` is set, every target not yet
//! taken is reported as skipped without running its job. A job that panics is
//! a failed target; the other targets go on.

use std::any::Any;
use std::num::NonZeroU8;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use crate::inventory::Target;

/// What [`fan_out`] reports, in this order: `Begin` once, then a `Start`
/// and a `Done` per target; `Done` carries its index in the targets. Each
/// result is also reported once as `Ordered`, in the order of the targets,
/// right after the `Done` that leaves every earlier target ended.
pub enum Event<'a, R> {
    Begin(&'a [Target]),
    Start(&'a Target),
    Done(usize, &'a R),
    Ordered(&'a R),
}

enum Msg<R> {
    Start(usize),
    Done(usize, R),
}

/// Runs `one` on each of `targets`, `parallel` at a time, calling `on` with
/// each [`Event`] on this thread. A target taken once `cancel` is set gets
/// `skipped` instead; a target whose `one` panics gets `panicked` with the
/// panic's message. Returns the results in the order of `targets`.
pub fn fan_out<R: Send>(
    targets: &[Target],
    parallel: NonZeroU8,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Event<'_, R>),
    one: impl Fn(&Target) -> R + Sync,
    skipped: impl Fn(&Target) -> R + Sync,
    panicked: impl Fn(&Target, String) -> R + Sync,
) -> Vec<R> {
    on(Event::Begin(targets));
    let mut results: Vec<Option<R>> = targets.iter().map(|_| None).collect();
    // How many results, from the first, were reported as Ordered.
    let mut shown = 0;
    let next = AtomicUsize::new(0);
    let workers = usize::from(parallel.get()).min(targets.len());
    thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let (next, one, skipped, panicked) = (&next, &one, &skipped, &panicked);
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let tx = tx.clone();
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(target) = targets.get(index) else {
                            break;
                        };
                        let _ = tx.send(Msg::Start(index));
                        let result = if cancel.load(Ordering::Relaxed) {
                            skipped(target)
                        } else {
                            panic::catch_unwind(AssertUnwindSafe(|| one(target))).unwrap_or_else(
                                |payload| panicked(target, panic_message(payload.as_ref())),
                            )
                        };
                        let _ = tx.send(Msg::Done(index, result));
                    }
                })
            })
            .collect();
        // Only the workers hold a sender now: the loop ends once they all
        // have, a worker that died included.
        drop(tx);
        for msg in rx {
            match msg {
                Msg::Start(index) => on(Event::Start(&targets[index])),
                Msg::Done(index, result) => {
                    on(Event::Done(index, results[index].insert(result)));
                    show_ordered(&results, &mut shown, on);
                }
            }
        }
        for handle in handles {
            let _ = handle.join();
        }
    });
    // A worker that died left its targets without a result: each fails,
    // and its Done lets `on` go past it.
    for (index, target) in targets.iter().enumerate() {
        if results[index].is_none() {
            let failed = results[index].insert(panicked(target, STOPPED.to_owned()));
            on(Event::Done(index, failed));
            show_ordered(&results, &mut shown, on);
        }
    }
    results.into_iter().map(Option::unwrap).collect()
}

/// Reports as Ordered each result past the first `shown` whose earlier
/// targets have all ended.
fn show_ordered<R>(results: &[Option<R>], shown: &mut usize, on: &mut dyn FnMut(Event<'_, R>)) {
    while let Some(Some(result)) = results.get(*shown) {
        on(Event::Ordered(result));
        *shown += 1;
    }
}

/// The panic message of a target whose worker died before it ended.
const STOPPED: &str = "the worker stopped";

/// Wraps `block` so it gets each target's result in the order of the
/// targets, from the Ordered events: [`fan_out`] keeps every result, so
/// nothing is copied or held here.
pub fn in_order<R>(mut block: impl FnMut(&R)) -> impl FnMut(Event<'_, R>) {
    move |event| {
        if let Event::Ordered(result) = event {
            block(result);
        }
    }
}

/// The message a panic was raised with.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::inventory::TargetKind;
    use crate::test_util::{Gauge, Meet};

    fn targets(names: &[&str]) -> Vec<Target> {
        names
            .iter()
            .map(|name| Target {
                name: (*name).to_owned(),
                kind: TargetKind::Local,
                pkg: None,
                sudo_secret: None,
                tags: Vec::new(),
            })
            .collect()
    }

    fn n(parallel: u8) -> NonZeroU8 {
        NonZeroU8::new(parallel).unwrap()
    }

    /// [`fan_out`] with `one`, `skipped` as `skipped NAME` and `panicked` as
    /// `panicked NAME: MESSAGE`; returns the results and the events as
    /// `begin N` / `start I` / `done I RESULT`.
    fn run(
        names: &[&str],
        parallel: u8,
        cancel: &AtomicBool,
        one: impl Fn(&Target) -> String + Sync,
    ) -> (Vec<String>, Vec<String>) {
        let targets = targets(names);
        let mut events = Vec::new();
        let results = fan_out(
            &targets,
            n(parallel),
            cancel,
            &mut |event| {
                events.push(match event {
                    // in_order's tests cover these.
                    Event::Ordered(_) => return,
                    Event::Begin(targets) => format!("begin {}", targets.len()),
                    Event::Start(target) => {
                        let index = names.iter().position(|name| *name == target.name);
                        format!("start {}", index.unwrap())
                    }
                    Event::Done(index, result) => format!("done {index} {result}"),
                })
            },
            one,
            |target| format!("skipped {}", target.name),
            |target, message| format!("panicked {}: {message}", target.name),
        );
        (results, events)
    }

    fn ran(target: &Target) -> String {
        format!("ran {}", target.name)
    }

    #[test]
    fn one_worker_runs_the_targets_one_by_one_in_order() {
        let gauge = Gauge::default();
        let (results, events) = run(&["local", "a", "b"], 1, &AtomicBool::new(false), |t| {
            gauge.enter();
            gauge.leave();
            ran(t)
        });
        assert_eq!(results, ["ran local", "ran a", "ran b"]);
        assert_eq!(
            events,
            [
                "begin 3",
                "start 0",
                "done 0 ran local",
                "start 1",
                "done 1 ran a",
                "start 2",
                "done 2 ran b"
            ]
        );
        assert_eq!(gauge.peak(), 1);
    }

    #[test]
    fn two_workers_run_two_targets_at_once() {
        let meet = Meet::new(2);
        let (results, _) = run(&["a", "b"], 2, &AtomicBool::new(false), |t| {
            if meet.arrive() {
                ran(t)
            } else {
                format!("alone {}", t.name)
            }
        });
        assert_eq!(results, ["ran a", "ran b"]);
    }

    #[test]
    fn no_more_than_n_targets_run_at_once() {
        let gauge = Gauge::default();
        let meet = Meet::new(2);
        let (results, events) = run(
            &["a", "b", "c", "d", "e"],
            2,
            &AtomicBool::new(false),
            |t| {
                gauge.enter();
                // The first two meet, so two did run at once.
                if t.name.as_str() <= "b" {
                    assert!(meet.arrive());
                }
                gauge.leave();
                ran(t)
            },
        );
        assert_eq!(results, ["ran a", "ran b", "ran c", "ran d", "ran e"]);
        assert_eq!(gauge.peak(), 2);
        assert_eq!(events[0], "begin 5");
        let at = |event: String| events.iter().position(|e| *e == event).unwrap();
        for (index, result) in results.iter().enumerate() {
            assert!(at(format!("start {index}")) < at(format!("done {index} {result}")));
        }
        assert_eq!(events.len(), 11);
    }

    #[test]
    fn more_workers_than_targets_is_fine_and_no_target_is_nothing() {
        let (results, _) = run(&["a"], 32, &AtomicBool::new(false), ran);
        assert_eq!(results, ["ran a"]);
        let (results, events) = run(&[], 4, &AtomicBool::new(false), ran);
        assert!(results.is_empty());
        assert_eq!(events, ["begin 0"]);
    }

    #[test]
    fn once_cancelled_no_target_starts_its_job() {
        for parallel in [1, 2] {
            let cancel = AtomicBool::new(false);
            let (started_both, cancelled) =
                (Meet::new(parallel.into()), Meet::new(parallel.into()));
            let started = Mutex::new(Vec::new());
            let (results, events) = run(&["a", "b", "c", "d"], parallel, &cancel, |t| {
                started.lock().unwrap().push(t.name.clone());
                // With two workers, b is running while a cancels.
                assert!(started_both.arrive());
                if t.name == "a" {
                    cancel.store(true, Ordering::Relaxed);
                }
                assert!(cancelled.arrive());
                ran(t)
            });
            let expected: &[&str] = if parallel == 1 {
                &["ran a", "skipped b", "skipped c", "skipped d"]
            } else {
                &["ran a", "ran b", "skipped c", "skipped d"]
            };
            assert_eq!(results, expected, "--parallel {parallel}");
            assert_eq!(started.lock().unwrap().len(), usize::from(parallel));
            assert!(events.contains(&"start 3".to_owned()));
            assert!(events.contains(&"done 3 skipped d".to_owned()));
        }
    }

    #[test]
    fn a_panicking_job_is_its_target_only() {
        for parallel in [1, 2] {
            let (results, events) = run(&["a", "b", "c"], parallel, &AtomicBool::new(false), |t| {
                assert_ne!(t.name, "b", "b blew up");
                ran(t)
            });
            assert_eq!(results[0], "ran a");
            assert!(
                results[1].starts_with("panicked b: assertion `left != right` failed: b blew up"),
                "{}",
                results[1]
            );
            assert_eq!(results[2], "ran c");
            assert_eq!(events.len(), 7, "{events:?}");
        }
    }

    #[test]
    fn a_worker_that_dies_leaves_failed_targets_and_fan_out_still_returns() {
        let targets = targets(&["a", "b"]);
        let mut events = 0;
        let results = fan_out(
            &targets,
            n(1),
            &AtomicBool::new(true),
            &mut |event| events += usize::from(!matches!(event, Event::Ordered(_))),
            ran,
            |target| panic!("skipping {} blew up", target.name),
            |target, message| format!("panicked {}: {message}", target.name),
        );
        assert_eq!(
            results,
            [
                "panicked a: the worker stopped",
                "panicked b: the worker stopped"
            ]
        );
        assert_eq!(events, 4, "Begin, a's Start, then a Done for each");
    }

    #[test]
    fn in_order_shows_every_block_when_a_worker_dies() {
        let targets = targets(&["a", "b", "c"]);
        let mut shown = Vec::new();
        let mut on = in_order(|result: &String| shown.push(result.clone()));
        let results = fan_out(
            &targets,
            n(2),
            &AtomicBool::new(false),
            &mut on,
            |target| {
                assert_ne!(target.name, "a", "a blew up");
                ran(target)
            },
            |target| format!("skipped {}", target.name),
            // Reporting a's panic panics too: its worker dies.
            |target, message| {
                assert_eq!(message, STOPPED, "the report blew up");
                format!("panicked {}: {message}", target.name)
            },
        );
        drop(on);
        let expected = ["panicked a: the worker stopped", "ran b", "ran c"];
        assert_eq!(results, expected);
        assert_eq!(shown, expected, "b and c are not held behind a");
    }

    #[test]
    fn in_order_holds_a_result_until_every_earlier_one_ended() {
        let targets = targets(&["local", "a", "b"]);
        // local ends only once a's Done is in.
        let a_done = Meet::new(2);
        let log = Mutex::new(Vec::new());
        let mut shown =
            in_order(|result: &String| log.lock().unwrap().push(format!("shown {result}")));
        fan_out(
            &targets,
            n(2),
            &AtomicBool::new(false),
            &mut |event| {
                if let Event::Done(index, result) = &event {
                    log.lock().unwrap().push(format!("done {result}"));
                    if *index == 1 {
                        assert!(a_done.arrive(), "local never waited");
                    }
                }
                shown(event);
            },
            |target| {
                if target.name == "local" {
                    assert!(a_done.arrive(), "a never ended");
                }
                target.name.clone()
            },
            |target| format!("skipped {}", target.name),
            |target, message| format!("panicked {}: {message}", target.name),
        );
        drop(shown);
        let log = log.into_inner().unwrap();
        let at = |line: &str| log.iter().position(|l| l == line).unwrap();
        assert_eq!(log[0], "done a", "{log:?}");
        assert!(at("done local") < at("shown local"), "{log:?}");
        assert_eq!(
            at("shown local") + 1,
            at("shown a"),
            "a waited for local: {log:?}"
        );
        assert!(at("shown a") < at("shown b"), "{log:?}");
        assert_eq!(log.len(), 6, "{log:?}");
    }
}
