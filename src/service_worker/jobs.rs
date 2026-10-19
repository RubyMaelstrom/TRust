//! Service Workers #schedule-job-algorithm / #finish-job-algorithm.
//! One FIFO per (storage key, scope); unrelated scopes proceed independently.
//! Equivalent unsettled jobs share work and each gets its own promise result.
use super::{Error, UpdateViaCache, WorkerType};
use futures::{FutureExt, future::BoxFuture};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_QUEUED_JOBS: usize = 256;
const MAX_EQUIVALENT_REPLIES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Register,
    Update,
    Unregister,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Spec {
    pub kind: Kind,
    pub key: String,
    pub scope: String,
    pub script: String,
    pub worker_type: WorkerType,
    pub update_via_cache: UpdateViaCache,
}

impl Spec {
    fn equivalent(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.key == other.key
            && self.scope == other.scope
            && (self.kind == Kind::Unregister
                || self.script == other.script
                    && self.worker_type == other.worker_type
                    && self.update_via_cache == other.update_via_cache)
    }
}

/// Keep the registration alive until each realm has received the object.
/// A failed install/unregister may remove it from the map before that task.
#[derive(Clone, Debug)]
pub(crate) struct Output {
    pub value: serde_json::Value,
    pub registrations: Vec<super::RegistrationInfo>,
}
impl Output {
    pub(crate) fn registrations(registrations: Vec<Arc<super::Registration>>, list: bool) -> Self {
        let registrations: Vec<_> = registrations.iter().map(|r| r.info()).collect();
        let values: Vec<_> = registrations
            .iter()
            .map(|r| serde_json::to_value(r).unwrap())
            .collect();
        let value = if list {
            serde_json::Value::Array(values)
        } else {
            values.into_iter().next().unwrap_or_default()
        };
        Self {
            value,
            registrations,
        }
    }
}
impl From<serde_json::Value> for Output {
    fn from(value: serde_json::Value) -> Self {
        Self {
            value,
            registrations: Vec::new(),
        }
    }
}
#[cfg(test)]
impl From<i32> for Output {
    fn from(value: i32) -> Self {
        serde_json::Value::from(value).into()
    }
}
#[cfg(test)]
impl From<bool> for Output {
    fn from(value: bool) -> Self {
        serde_json::Value::from(value).into()
    }
}
type ResultValue = Result<Output, Error>;
pub(crate) type Reply = Box<dyn FnOnce(ResultValue) + Send>;

#[derive(Default)]
struct CompletionState {
    result: Option<ResultValue>,
    replies: Vec<Reply>,
}

#[derive(Default)]
pub(crate) struct Completion {
    state: Mutex<CompletionState>,
    settled: AtomicBool,
}

impl Completion {
    pub(crate) fn resolve(&self, result: ResultValue) {
        let mut state = self.state.lock().unwrap();
        if state.result.is_some() {
            return;
        }
        state.result = Some(result.clone());
        let replies = std::mem::take(&mut state.replies);
        drop(state);
        for reply in replies {
            reply(result.clone());
        }
    }
    fn add(&self, reply: Reply) {
        let mut state = self.state.lock().unwrap();
        if let Some(result) = state.result.clone() {
            drop(state);
            reply(result);
        } else if state.replies.len() < MAX_EQUIVALENT_REPLIES {
            state.replies.push(reply);
        } else {
            drop(state);
            reply(Err(Error::new(
                "QuotaExceededError",
                "Too many equivalent service-worker requests",
            )));
        }
    }
    /// The initiating realm acknowledges actual promise settlement. Choosing
    /// a value and queueing its DOM task is not settlement yet; a late
    /// equivalent job still needs the already-chosen result in that interval.
    pub(crate) fn settled(&self) {
        self.settled.store(true, Ordering::Release);
    }
    pub(crate) fn result_chosen(&self) -> bool {
        self.state.lock().unwrap().result.is_some()
    }
}

type Run = Box<dyn FnOnce(Arc<Completion>) -> BoxFuture<'static, Result<(), Error>> + Send>;

struct Job {
    spec: Spec,
    completion: Arc<Completion>,
    run: Mutex<Option<Run>>,
}

type Key = (String, String);
#[derive(Default)]
pub(crate) struct Queues(Mutex<HashMap<Key, VecDeque<Arc<Job>>>>);

impl Queues {
    /// Returns the shared completion and whether this is the primary job's
    /// promise (only that promise's settlement governs later coalescing).
    pub(crate) fn schedule(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        spec: Spec,
        reply: Reply,
        run: Run,
    ) -> (Arc<Completion>, bool) {
        let key = (spec.key.clone(), spec.scope.clone());
        let mut queues = self.0.lock().unwrap();
        let queued = queues.values().map(VecDeque::len).sum::<usize>();
        let queue = queues.entry(key.clone()).or_default();
        if let Some(last) = queue.back()
            && last.spec.equivalent(&spec)
            && !last.completion.settled.load(Ordering::Acquire)
        {
            let completion = last.completion.clone();
            drop(queues);
            completion.add(reply);
            return (completion, false);
        }
        if queued >= MAX_QUEUED_JOBS {
            if queue.is_empty() {
                queues.remove(&key);
            }
            drop(queues);
            let completion = Arc::new(Completion::default());
            completion.add(reply);
            completion.resolve(Err(Error::new(
                "QuotaExceededError",
                "Too many queued service-worker jobs",
            )));
            completion.settled();
            return (completion, false);
        }
        let completion = Arc::new(Completion::default());
        completion.add(reply);
        let start = queue.is_empty();
        queue.push_back(Arc::new(Job {
            spec,
            completion: completion.clone(),
            run: Mutex::new(Some(run)),
        }));
        drop(queues);
        if start {
            let queues = self.clone();
            handle.spawn(async move {
                queues.run(key).await;
            });
        }
        (completion, true)
    }

    async fn run(self: Arc<Self>, key: Key) {
        loop {
            let job = self
                .0
                .lock()
                .unwrap()
                .get(&key)
                .and_then(|q| q.front())
                .cloned();
            let Some(job) = job else {
                return;
            };
            let run = job
                .run
                .lock()
                .unwrap()
                .take()
                .expect("one runner per scope");
            let result = std::panic::AssertUnwindSafe(async { run(job.completion.clone()).await })
                .catch_unwind()
                .await;
            match result {
                Ok(Err(error)) => job.completion.resolve(Err(error)),
                Err(_) => job
                    .completion
                    .resolve(Err(Error::type_error("Service-worker job failed"))),
                Ok(Ok(())) if !job.completion.result_chosen() => {
                    job.completion.resolve(Err(Error::type_error(
                        "Service-worker job ended without a result",
                    )));
                }
                _ => {}
            }
            let mut queues = self.0.lock().unwrap();
            let queue = queues.get_mut(&key).expect("running job's queue");
            queue.pop_front();
            if queue.is_empty() {
                queues.remove(&key);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn spec(scope: &str) -> Spec {
        Spec {
            kind: Kind::Register,
            key: "https://worker.test".into(),
            scope: scope.into(),
            script: "https://worker.test/worker.js".into(),
            worker_type: WorkerType::Classic,
            update_via_cache: UpdateViaCache::Imports,
        }
    }
    fn reply() -> (Reply, tokio::sync::oneshot::Receiver<ResultValue>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn equivalent_jobs_share_work_and_late_replies_before_settlement() {
        let queues = Arc::new(Queues::default());
        let handle = tokio::runtime::Handle::current();
        let runs = Arc::new(AtomicUsize::new(0));
        let count = runs.clone();
        let (release, wait) = tokio::sync::oneshot::channel();
        let (first_reply, first) = reply();
        let (completion, primary) = queues.schedule(
            &handle,
            spec("/"),
            first_reply,
            Box::new(move |done| {
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    done.resolve(Ok(1.into()));
                    let _ = wait.await;
                    Ok(())
                }
                .boxed()
            }),
        );
        assert!(primary);
        assert_eq!(first.await.unwrap().unwrap().value, 1);
        let (second_reply, second) = reply();
        let (_, primary) = queues.schedule(
            &handle,
            spec("/"),
            second_reply,
            Box::new(|_| async { panic!("equivalent work ran twice") }.boxed()),
        );
        assert!(!primary);
        assert_eq!(second.await.unwrap().unwrap().value, 1);
        completion.settled();
        let (third_reply, third) = reply();
        let count = runs.clone();
        let (_, primary) = queues.schedule(
            &handle,
            spec("/"),
            third_reply,
            Box::new(move |done| {
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    done.resolve(Ok(3.into()));
                    Ok(())
                }
                .boxed()
            }),
        );
        assert!(primary);
        release.send(()).unwrap();
        assert_eq!(third.await.unwrap().unwrap().value, 3);
        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn different_scopes_run_independently_and_failed_jobs_release_the_queue() {
        let queues = Arc::new(Queues::default());
        let handle = tokio::runtime::Handle::current();
        let (release, wait) = tokio::sync::oneshot::channel();
        let (blocked_reply, blocked) = reply();
        queues.schedule(
            &handle,
            spec("/blocked/"),
            blocked_reply,
            Box::new(move |_| {
                async move {
                    let _ = wait.await;
                    Err(Error::type_error("script fetch failed"))
                }
                .boxed()
            }),
        );
        let (independent_reply, independent) = reply();
        queues.schedule(
            &handle,
            spec("/independent/"),
            independent_reply,
            Box::new(|done| {
                async move {
                    done.resolve(Ok(true.into()));
                    Ok(())
                }
                .boxed()
            }),
        );
        assert_eq!(independent.await.unwrap().unwrap().value, true);
        let (next_reply, next) = reply();
        let mut unregister = spec("/blocked/");
        unregister.kind = Kind::Unregister;
        queues.schedule(
            &handle,
            unregister,
            next_reply,
            Box::new(|done| {
                async move {
                    done.resolve(Ok(false.into()));
                    Ok(())
                }
                .boxed()
            }),
        );
        release.send(()).unwrap();
        assert_eq!(blocked.await.unwrap().unwrap_err().name, "TypeError");
        assert_eq!(next.await.unwrap().unwrap().value, false);
    }
}
