//! Process lifetime for resident page threads. Navigation cancels actors
//! asynchronously; process exit must also join them before native drivers run
//! their exit handlers. HTML #discard-a-document / #abort-a-document retain
//! cleanup on the owning agent; EGL 1.5 §3.7.2 releases its context there.

use std::sync::{Arc, Mutex, Weak};
use std::thread::{Builder, JoinHandle};

#[derive(Default)]
struct State {
    stopping: bool,
    threads: Vec<PageThread>,
}

struct PageThread {
    thread: JoinHandle<()>,
    interrupt: Arc<lumen::RuntimeInterrupt>,
    cache: Weak<crate::http::PageCache>,
}

#[derive(Default)]
struct PageThreads(Mutex<State>);

impl PageThreads {
    fn spawn(
        &self,
        builder: Builder,
        interrupt: Arc<lumen::RuntimeInterrupt>,
        cache: &Arc<crate::http::PageCache>,
        task: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopping {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        // Finished actors must not accumulate handles or retain page caches
        // across navigation. The lock also closes the spawn/shutdown race.
        let mut index = 0;
        while index < state.threads.len() {
            if state.threads[index].thread.is_finished() {
                // is_finished can precede native thread-local destruction;
                // join that last cleanup instead of detaching the handle.
                let entry = state.threads.swap_remove(index);
                let _ = entry.thread.join();
            } else {
                index += 1;
            }
        }
        let thread = builder.spawn(task)?;
        state.threads.push(PageThread {
            thread,
            interrupt,
            cache: Arc::downgrade(cache),
        });
        Ok(())
    }

    fn shutdown(&self) {
        let threads = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.stopping = true;
            std::mem::take(&mut state.threads)
        };
        for entry in &threads {
            entry.interrupt.cancel();
            if let Some(cache) = entry.cache.upgrade() {
                cache.cancel();
            }
        }
        for entry in threads {
            // A page panic remains contained. Join still guarantees that its
            // stack and native context have finished unwinding before exit.
            let _ = entry.thread.join();
        }
    }
}

static THREADS: PageThreads = PageThreads(Mutex::new(State {
    stopping: false,
    threads: Vec::new(),
}));

pub(crate) fn spawn(
    builder: Builder,
    interrupt: Arc<lumen::RuntimeInterrupt>,
    cache: &Arc<crate::http::PageCache>,
    task: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    THREADS.spawn(builder, interrupt, cache, task)
}

pub(crate) fn shutdown() {
    THREADS.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn page_thread_shutdown_waits_for_cleanup_and_prevents_new_actors() {
        struct Cleanup(Arc<AtomicBool>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let threads = Arc::new(PageThreads::default());
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleanup = Cleanup(cleaned.clone());
        let cache = Arc::new(crate::http::PageCache::default());
        let interrupt = Arc::new(lumen::RuntimeInterrupt::default());
        let (release, wait) = mpsc::channel();
        threads
            .spawn(Builder::new(), interrupt.clone(), &cache, move || {
                let _cleanup = cleanup;
                let _ = wait.recv();
            })
            .unwrap();
        let (done, completion) = mpsc::channel();
        let owner = threads.clone();
        let shutdown = std::thread::spawn(move || {
            owner.shutdown();
            done.send(()).unwrap();
        });
        assert!(completion.recv_timeout(Duration::from_millis(20)).is_err());
        release.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        shutdown.join().unwrap();
        assert!(cleaned.load(Ordering::Acquire));
        assert_eq!(
            interrupt.current_reason(),
            Some(lumen::InterruptReason::Cancelled)
        );
        assert_eq!(
            threads
                .spawn(Builder::new(), interrupt, &cache, || panic!("late actor"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Interrupted
        );
        threads.shutdown();
    }
}
