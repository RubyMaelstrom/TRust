//! Bounded native-controller handoff, independent of display availability.
//!
//! HTML's "update the rendering" reflects the current document; it does not
//! require presenting every intermediate paint. Only consecutive, complete,
//! diagnostic-free snapshots of the same document may supersede one another.
//! All other events are FIFO barriers (including patches and task results).
//! Finger updates likewise contain the complete received prefix. Consecutive
//! updates can coalesce until the final reply, which remains a FIFO barrier.
//! https://html.spec.whatwg.org/multipage/webappapis.html#update-the-rendering
//! https://html.spec.whatwg.org/multipage/webappapis.html#event-loop-processing-model

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::sync::mpsc::error::{SendError, TrySendError};

use super::{CoreEvent, InvalidationHandle};
use crate::js::PageEvt;

// In particular, a blocked native presenter must not turn the resident actor's
// bounded output into an unbounded history of entire GraphicalLayouts. Ordinary
// paint-only traffic occupies ONE slot; this limit also bounds barrier traffic.
const CAPACITY: usize = 16;

struct State {
    events: VecDeque<CoreEvent>,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    space: Notify,
    capacity: usize,
    invalidation: InvalidationHandle,
}

#[derive(Clone)]
pub(super) struct Sender(Arc<Shared>);

pub(super) struct Receiver(Arc<Shared>);

pub(super) fn channel(invalidation: InvalidationHandle) -> (Sender, Receiver) {
    with_capacity(CAPACITY, invalidation)
}

fn with_capacity(capacity: usize, invalidation: InvalidationHandle) -> (Sender, Receiver) {
    assert!(capacity > 0);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            events: VecDeque::new(),
            closed: false,
        }),
        space: Notify::new(),
        capacity,
        invalidation,
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl Sender {
    pub(super) async fn send(&self, event: CoreEvent) -> Result<(), Box<SendError<CoreEvent>>> {
        // Keep ownership here across full-queue retries without allocating.
        // Only a closed receiver needs to return the large event in a box.
        let mut event = Some(event);
        loop {
            // Register BEFORE checking capacity: another producer can consume
            // the released slot before we retry. Enabling each waiter prevents
            // notify_one's single stored permit from losing multiple wakeups.
            let space = self.0.space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            match self.try_send(&mut event) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(())) => {
                    return Err(Box::new(SendError(event.take().expect("pending event"))));
                }
                Err(TrySendError::Full(())) => {}
            }
            space.await;
        }
    }

    fn try_send(&self, event: &mut Option<CoreEvent>) -> Result<(), TrySendError<()>> {
        let (wake, retired) = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Err(TrySendError::Closed(()));
            }
            if let Some(previous) = state.events.back_mut()
                && supersedes(previous, event.as_ref().expect("pending event"))
            {
                (
                    false,
                    Some(std::mem::replace(
                        previous,
                        event.take().expect("pending event"),
                    )),
                )
            } else if state.events.len() < self.0.capacity {
                let wake = state.events.is_empty();
                state.events.push_back(event.take().expect("pending event"));
                (wake, None)
            } else {
                return Err(TrySendError::Full(()));
            }
        };
        // Large superseded layouts are freed on the forwarding thread, outside
        // the lock. Never hold up the native consumer while dropping a page.
        drop(retired);
        if wake {
            // One native wake per nonempty burst, not one per discarded frame.
            // The queue lock serializes this empty→nonempty edge with pop().
            self.0.invalidation.request_redraw();
        }
        Ok(())
    }
}

impl Receiver {
    pub(super) fn pop(&mut self) -> Option<CoreEvent> {
        let event = self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .pop_front();
        if event.is_some() {
            self.0.space.notify_one();
        }
        event
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let retired = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            std::mem::take(&mut state.events)
        };
        self.0.space.notify_waiters();
        // Sender clones must not keep queued layouts alive after the browser
        // closes. Pending async sends wake with their original event intact.
        drop(retired);
    }
}

fn supersedes(previous: &CoreEvent, next: &CoreEvent) -> bool {
    if let (
        CoreEvent::Finger {
            generation: a,
            reply: first,
        },
        CoreEvent::Finger { generation: b, .. },
    ) = (previous, next)
    {
        return a == b && !first.finished;
    }
    fn paint_generation(event: &CoreEvent) -> Option<(u64, usize)> {
        match event {
            CoreEvent::Page {
                generation,
                event: PageEvt::Updated { outcome, .. },
            } if outcome.rendered.is_some()
                && outcome.errors.is_empty()
                && outcome.console.is_empty()
                && !outcome.panicked
                && outcome.modules_skipped == 0 =>
            {
                // drain_diagnostics reports a CUMULATIVE fetch count on every
                // snapshot, including quiet clocks long after resources load.
                // Preserve counter changes as barriers, but a repeated nonzero
                // count must not disable coalescing for the rest of the page.
                Some((*generation, outcome.fetches))
            }
            _ => None,
        }
    }
    paint_generation(previous).is_some_and(|generation| paint_generation(next) == Some(generation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::Outcome;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counted_channel(capacity: usize) -> (Sender, Receiver, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = wakes.clone();
        let (tx, rx) = with_capacity(
            capacity,
            InvalidationHandle {
                wake: Arc::new(move || {
                    count.fetch_add(1, Ordering::Relaxed);
                }),
            },
        );
        (tx, rx, wakes)
    }

    fn rendered() -> crate::http::RenderedPage {
        crate::http::render_arena(
            &crate::dom::Dom::parse_document("<p>current frame</p>"),
            &url::Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport {
                width: 320.0,
                height: 240.0,
            },
            1.0,
            None,
            &Default::default(),
        )
    }

    fn paint(generation: u64, html: &str, rendered: crate::http::RenderedPage) -> CoreEvent {
        CoreEvent::Page {
            generation,
            event: PageEvt::Updated {
                html: html.into(),
                outcome: Outcome {
                    rendered: Some(Box::new(rendered)),
                    ..Default::default()
                },
            },
        }
    }

    fn semantic(id: usize) -> CoreEvent {
        CoreEvent::Page {
            generation: 1,
            event: PageEvt::ScrollToFragment(id.to_string()),
        }
    }

    fn pop_id(rx: &mut Receiver) -> usize {
        let Some(CoreEvent::Page {
            event: PageEvt::ScrollToFragment(id),
            ..
        }) = rx.pop()
        else {
            panic!("expected FIFO event")
        };
        id.parse().unwrap()
    }

    #[tokio::test]
    async fn finger_updates_coalesce_but_final_replies_and_generations_are_barriers() {
        let (tx, mut rx, wakes) = counted_channel(4);
        for (generation, text, finished) in [
            (1, "a", false),
            (1, "ab", false),
            (1, "abc", true),
            (1, "later", false),
            (2, "different", false),
        ] {
            tx.send(CoreEvent::Finger {
                generation,
                reply: crate::finger::Reply {
                    body: text.as_bytes().to_vec(),
                    finished,
                    notice: None,
                },
            })
            .await
            .unwrap();
        }
        for expected in ["abc", "later", "different"] {
            let Some(CoreEvent::Finger { reply, .. }) = rx.pop() else {
                panic!()
            };
            assert_eq!(reply.body, expected.as_bytes());
        }
        assert!(rx.pop().is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn stalled_consumer_retains_only_latest_complete_paint_and_one_wake() {
        let (tx, mut rx, wakes) = counted_channel(1);
        let first = rendered();
        let old_layout = Arc::downgrade(&first.layout);
        tx.send(paint(1, "old", first)).await.unwrap();
        let latest = rendered();
        for _ in 0..10_000 {
            tx.send(paint(1, "latest", latest.clone())).await.unwrap();
            assert_eq!(rx.0.state.lock().unwrap().events.len(), 1);
        }
        assert!(
            old_layout.upgrade().is_none(),
            "superseded layout must be released"
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        let Some(CoreEvent::Page {
            event: PageEvt::Updated { html, outcome },
            ..
        }) = rx.pop()
        else {
            panic!("latest complete snapshot missing")
        };
        assert_eq!(html, "latest");
        assert!(Arc::ptr_eq(
            &outcome.rendered.unwrap().layout,
            &latest.layout
        ));
        assert!(rx.pop().is_none());
        tx.send(semantic(7)).await.unwrap();
        assert_eq!(wakes.load(Ordering::Relaxed), 2, "draining rearms the wake");
    }

    #[tokio::test]
    async fn semantic_barriers_and_generations_are_not_crossed() {
        let (tx, mut rx, _) = counted_channel(16);
        let frame = rendered();
        tx.send(paint(1, "before input", frame.clone()))
            .await
            .unwrap();
        tx.send(semantic(4)).await.unwrap();
        tx.send(paint(1, "after input", frame.clone()))
            .await
            .unwrap();
        tx.send(paint(2, "new document", frame)).await.unwrap();
        assert!(matches!(
            rx.pop(),
            Some(CoreEvent::Page {
                generation: 1,
                event: PageEvt::Updated { .. }
            })
        ));
        assert_eq!(pop_id(&mut rx), 4);
        assert!(matches!(
            rx.pop(),
            Some(CoreEvent::Page {
                generation: 1,
                event: PageEvt::Updated { .. }
            })
        ));
        assert!(matches!(
            rx.pop(),
            Some(CoreEvent::Page {
                generation: 2,
                event: PageEvt::Updated { .. }
            })
        ));
        assert!(rx.pop().is_none());
    }

    #[tokio::test]
    async fn cumulative_fetch_count_does_not_disable_quiet_paint_coalescing() {
        let (tx, mut rx, wakes) = counted_channel(2);
        let frame = rendered();
        for fetches in [7, 7, 7, 8] {
            let mut event = paint(1, "loaded page", frame.clone());
            let CoreEvent::Page {
                event: PageEvt::Updated { outcome, .. },
                ..
            } = &mut event
            else {
                unreachable!()
            };
            outcome.fetches = fetches;
            tx.send(event).await.unwrap();
        }
        for expected in [7, 8] {
            let Some(CoreEvent::Page {
                event: PageEvt::Updated { outcome, .. },
                ..
            }) = rx.pop()
            else {
                panic!("missing snapshot")
            };
            assert_eq!(outcome.fetches, expected);
        }
        assert!(rx.pop().is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn diagnostics_incomplete_snapshots_and_patches_are_not_replaceable() {
        let frame = rendered();
        for kind in 0..6 {
            let mut barrier = paint(1, "important", frame.clone());
            let CoreEvent::Page {
                event: PageEvt::Updated { outcome, .. },
                ..
            } = &mut barrier
            else {
                unreachable!()
            };
            match kind {
                0 => outcome.errors.push("error".into()),
                1 => outcome.console.push("console".into()),
                2 => outcome.panicked = true,
                3 => outcome.modules_skipped = 1,
                4 => outcome.fetches = 1,
                5 => outcome.rendered = None,
                _ => unreachable!(),
            }
            let next = paint(1, "next", frame.clone());
            assert!(
                !supersedes(&barrier, &next),
                "lost information, case {kind}"
            );
            assert!(!supersedes(&next, &barrier), "crossed barrier, case {kind}");
        }
        for event in [
            PageEvt::Patched {
                patches: Vec::new(),
                outcome: Outcome::default(),
            },
            PageEvt::Static {
                html: String::new(),
                outcome: Outcome::default(),
            },
            PageEvt::Navigate("https://example.test/next".into()),
            PageEvt::Replace("https://example.test/replace".into()),
            PageEvt::HistoryUpdate {
                url: "https://example.test/history".into(),
                replace: false,
            },
            PageEvt::KeyDefault { prevented: true },
            PageEvt::Settled,
            PageEvt::Trouble(vec!["error".into()]),
        ] {
            let barrier = CoreEvent::Page {
                generation: 1,
                event,
            };
            assert!(!supersedes(&barrier, &paint(1, "next", frame.clone())));
            assert!(!supersedes(&paint(1, "previous", frame.clone()), &barrier));
        }
    }

    #[tokio::test]
    async fn full_barrier_queue_backpressures_without_dropping_or_reordering() {
        let (tx, mut rx, _) = counted_channel(2);
        tx.send(semantic(0)).await.unwrap();
        tx.send(semantic(1)).await.unwrap();
        let send = tx.send(semantic(2));
        tokio::pin!(send);
        assert!(futures::poll!(&mut send).is_pending());
        assert_eq!(rx.0.state.lock().unwrap().events.len(), 2);
        assert_eq!(pop_id(&mut rx), 0);
        assert!(futures::poll!(&mut send).is_ready());
        assert_eq!(pop_id(&mut rx), 1);
        assert_eq!(pop_id(&mut rx), 2);
    }

    #[tokio::test]
    async fn closing_receiver_releases_layouts_and_wakes_all_blocked_producers() {
        let (tx, rx, _) = counted_channel(1);
        let frame = rendered();
        let layout = Arc::downgrade(&frame.layout);
        tx.send(paint(1, "queued", frame)).await.unwrap();
        let first = tx.send(semantic(1));
        let second = tx.send(semantic(2));
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        drop(rx);
        assert!(layout.upgrade().is_none());
        for (result, expected) in [
            (first.await, "1"),
            (second.await, "2"),
            (tx.send(semantic(3)).await, "3"),
        ] {
            let SendError(event) = *result.unwrap_err();
            assert!(
                matches!(
                    event,
                    CoreEvent::Page {
                        event: PageEvt::ScrollToFragment(id), ..
                    } if id == expected
                ),
                "closed sends must return their original event"
            );
        }
    }

    #[tokio::test]
    async fn multiple_waiters_and_cancelled_sends_do_not_lose_space_wakeups() {
        let (tx, mut rx, _) = counted_channel(2);
        tx.send(semantic(0)).await.unwrap();
        tx.send(semantic(1)).await.unwrap();
        {
            let cancelled = tx.send(semantic(99));
            tokio::pin!(cancelled);
            assert!(futures::poll!(&mut cancelled).is_pending());
        }
        let first = tx.send(semantic(2));
        let second = tx.send(semantic(3));
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(pop_id(&mut rx), 0);
        assert_eq!(pop_id(&mut rx), 1);
        // Both notifications precede either retry: a single unregistered
        // Notify permit would strand the second producer despite free space.
        assert!(futures::poll!(&mut first).is_ready());
        assert!(futures::poll!(&mut second).is_ready());
        assert_eq!(pop_id(&mut rx), 2);
        assert_eq!(pop_id(&mut rx), 3);
        assert!(rx.pop().is_none());
    }
}
