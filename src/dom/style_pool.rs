//! Persistent worker threads for parallel style passes.
//!
//! In a pass, the calling thread runs a `lead` closure, which may use its
//! thread's non-`Sync` state, while woken workers run one shared `Sync` job.
//! The lead decides how many workers to wake, and when, from the work it
//! finds. Jobs divide their own work (for example by claiming chunks from an
//! atomic counter), so a late or slow participant — a little core, a
//! preempted worker — only takes fewer chunks. A worker that wakes after the
//! pass has closed never touches the job. Workers park between passes and
//! never spin while idle.
//!
//! Policy (read once): `TRUST_STYLE_THREADS` sets the participant count; `0`
//! disables parallel passes and `1` runs them on the calling thread alone.
//! `TRUST_STYLE_CORES=all` lets workers run on any CPU; by default, on a
//! heterogeneous CPU, they are confined to the highest-capacity cores.

use std::cell::UnsafeCell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;

type Job = dyn Fn(usize) + Sync;

struct Shared {
    /// `epoch << 1 | closed` of the current pass. Workers act on an open
    /// ticket they have not served yet.
    ticket: AtomicU64,
    /// Workers `0..helpers` are invited to the current pass.
    helpers: AtomicUsize,
    /// Workers currently inside (or registering for) a pass.
    active: AtomicUsize,
    panicked: AtomicBool,
    /// The current pass's job, with its lifetime erased. Written only by the
    /// caller while no worker can be registered, read only by registered
    /// workers of the same open ticket.
    job: UnsafeCell<Option<*const Job>>,
    /// The calling thread, unparked by the last worker to leave a closed pass.
    caller: Mutex<Option<Thread>>,
}

// SAFETY: `job` is the only non-`Sync` field. It is written by `Pool::run`
// while it holds the `busy` lock and no worker is registered (`active` is 0
// after the previous pass closed), and read by workers only after
// registering under the matching open ticket; `run` does not write it again
// until the pass is closed and `active` has drained back to 0.
unsafe impl Sync for Shared {}
// SAFETY: the raw job pointer is only dereferenced under the protocol above.
unsafe impl Send for Shared {}

pub(super) struct Pool {
    shared: Arc<Shared>,
    workers: Vec<Thread>,
    /// One pass at a time; a concurrent caller runs its job alone.
    busy: Mutex<()>,
    participants: usize,
}

/// The configured participant count, or `None` when parallel passes are
/// disabled (`TRUST_STYLE_THREADS=0`).
pub(super) fn configured_participants() -> Option<usize> {
    static N: OnceLock<Option<usize>> = OnceLock::new();
    *N.get_or_init(|| {
        match std::env::var("TRUST_STYLE_THREADS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            Some(0) => None,
            Some(n) => Some(n.min(64)),
            None => Some(default_participants()),
        }
    })
}

/// The process-wide pool, created on first use.
pub(super) fn pool() -> Option<&'static Pool> {
    static POOL: OnceLock<Option<Pool>> = OnceLock::new();
    POOL.get_or_init(|| configured_participants().map(Pool::new))
        .as_ref()
}

/// CPUs whose `cpu_capacity` is within 10% of the largest (Linux exposes the
/// relative compute capacity of each core on heterogeneous systems). Empty
/// when the information is unavailable or every core is alike.
fn fast_cpus() -> Vec<usize> {
    let mut capacities = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(index) = name
            .to_str()
            .and_then(|n| n.strip_prefix("cpu"))
            .and_then(|n| n.parse::<usize>().ok())
        else {
            continue;
        };
        let Some(capacity) = std::fs::read_to_string(entry.path().join("cpu_capacity"))
            .ok()
            .and_then(|c| c.trim().parse::<u32>().ok())
        else {
            continue;
        };
        capacities.push((index, capacity));
    }
    let Some(max) = capacities.iter().map(|&(_, c)| c).max() else {
        return Vec::new();
    };
    if capacities.iter().all(|&(_, c)| c == max) {
        return Vec::new();
    }
    let mut fast: Vec<usize> = capacities
        .iter()
        .filter(|&&(_, c)| c * 10 >= max * 9)
        .map(|&(i, _)| i)
        .collect();
    fast.sort_unstable();
    fast
}

fn confine_to_fast_cores() -> bool {
    std::env::var("TRUST_STYLE_CORES").as_deref() != Ok("all")
}

/// Participants when unconfigured: every fast core of a heterogeneous CPU,
/// else every available core, at most 16. Passes wake only as many as
/// their work repays.
fn default_participants() -> usize {
    let fast = fast_cpus();
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    let n = if fast.is_empty() || !confine_to_fast_cores() {
        available
    } else {
        fast.len()
    };
    n.clamp(1, 16)
}

impl Pool {
    pub(super) fn new(participants: usize) -> Self {
        let shared = Arc::new(Shared {
            ticket: AtomicU64::new(0),
            helpers: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            panicked: AtomicBool::new(false),
            job: UnsafeCell::new(None),
            caller: Mutex::new(None),
        });
        let cpus = if confine_to_fast_cores() {
            fast_cpus()
        } else {
            Vec::new()
        };
        let mut workers = Vec::new();
        for index in 0..participants.saturating_sub(1) {
            let shared = shared.clone();
            let cpus = cpus.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("trust-style-{}", index + 1))
                .stack_size(8 << 20)
                .spawn(move || {
                    set_affinity(&cpus);
                    worker(&shared, index)
                });
            match spawned {
                Ok(handle) => workers.push(handle.thread().clone()),
                // A missing helper only lowers parallelism.
                Err(_) => break,
            }
        }
        Pool {
            shared,
            participants: workers.len() + 1,
            workers,
            busy: Mutex::new(()),
        }
    }

    /// The most participants a pass can have, including the caller.
    pub(super) fn participants(&self) -> usize {
        self.participants
    }

    /// Run `lead` on the calling thread while workers run `job(participant)`
    /// (participants `1..participants`), returning once `lead` and every
    /// worker that started have finished. Workers start when `lead` calls
    /// `Lead::wake`; when the pool is busy with another caller's pass, no
    /// worker starts and `lead` must do all the work. A panic in any
    /// participant is resumed here after the pass has drained.
    pub(super) fn run<R>(
        &self,
        participants: usize,
        job: &(dyn Fn(usize) + Sync),
        lead: impl FnOnce(&Lead<'_>) -> R,
    ) -> R {
        let helpers = participants.saturating_sub(1).min(self.workers.len());
        let guard = match self.busy.try_lock() {
            Ok(guard) if helpers > 0 => guard,
            _ => return lead(&Lead::alone(&self.shared)),
        };
        let shared = &*self.shared;
        // SAFETY: `busy` serializes callers and the previous pass drained
        // `active` to 0 before it returned, so no worker reads `job` now.
        // The lifetime is erased; `Close` (below) keeps every registered
        // worker from outliving this call.
        unsafe {
            let job: *const (dyn Fn(usize) + Sync + '_) = job;
            *shared.job.get() = Some(std::mem::transmute::<
                *const (dyn Fn(usize) + Sync + '_),
                *const Job,
            >(job));
        }
        *shared.caller.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current());
        shared.panicked.store(false, Ordering::Relaxed);
        shared.helpers.store(helpers, Ordering::Relaxed);
        let open = (shared.ticket.load(Ordering::Relaxed) | 1) + 1;
        shared.ticket.store(open, Ordering::SeqCst);
        let close = Close { shared, open };
        let handle = Lead {
            shared,
            workers: &self.workers[..helpers],
            woken: std::cell::Cell::new(0),
        };
        let result = catch_unwind(AssertUnwindSafe(|| lead(&handle)));
        drop(close);
        // SAFETY: every registered worker has left; none can register for
        // this closed ticket any more.
        unsafe { *shared.job.get() = None };
        drop(guard);
        match result {
            Err(panic) => resume_unwind(panic),
            Ok(_) if shared.panicked.load(Ordering::Relaxed) => panic!("style worker panicked"),
            Ok(result) => result,
        }
    }
}

/// The calling thread's handle on a running pass.
pub(super) struct Lead<'a> {
    shared: &'a Shared,
    workers: &'a [Thread],
    woken: std::cell::Cell<usize>,
}

impl<'a> Lead<'a> {
    fn alone(shared: &'a Shared) -> Self {
        Lead {
            shared,
            workers: &[],
            woken: std::cell::Cell::new(0),
        }
    }

    /// Bring the pass to `participants`, counting the calling thread.
    pub(super) fn wake(&self, participants: usize) {
        let target = participants.saturating_sub(1).min(self.workers.len());
        while self.woken.get() < target {
            self.workers[self.woken.get()].unpark();
            self.woken.set(self.woken.get() + 1);
        }
    }

    /// Participants woken so far, counting the calling thread.
    pub(super) fn participants(&self) -> usize {
        self.woken.get() + 1
    }

    /// Whether a worker's job panicked; its unit of work will not finish.
    pub(super) fn worker_panicked(&self) -> bool {
        !self.workers.is_empty() && self.shared.panicked.load(Ordering::Relaxed)
    }
}

/// Closes the pass and waits for registered workers, also on unwind.
struct Close<'a> {
    shared: &'a Shared,
    open: u64,
}

impl Drop for Close<'_> {
    fn drop(&mut self) {
        let shared = self.shared;
        shared.ticket.store(self.open | 1, Ordering::SeqCst);
        // Registered workers finish at most their current unit of work.
        let mut spins = 0u32;
        while shared.active.load(Ordering::SeqCst) != 0 {
            if spins < 2048 {
                spins += 1;
                std::hint::spin_loop();
            } else {
                std::thread::park_timeout(std::time::Duration::from_micros(200));
            }
        }
    }
}

fn worker(shared: &Shared, index: usize) {
    let mut served = 0u64;
    loop {
        let ticket = shared.ticket.load(Ordering::SeqCst);
        if ticket & 1 == 1 || ticket == served || index >= shared.helpers.load(Ordering::Relaxed) {
            std::thread::park();
            continue;
        }
        served = ticket;
        shared.active.fetch_add(1, Ordering::SeqCst);
        // Registered: the caller cannot finish closing before we leave. Only
        // act if the pass is still the open one we saw.
        if shared.ticket.load(Ordering::SeqCst) == ticket {
            // SAFETY: registered under the open ticket; see `Shared`.
            let job = unsafe { (*shared.job.get()).expect("open pass has a job") };
            // SAFETY: `run` keeps the job alive until `active` drains.
            let job = unsafe { &*job };
            if catch_unwind(AssertUnwindSafe(|| job(index + 1))).is_err() {
                shared.panicked.store(true, Ordering::Relaxed);
            }
        }
        if shared.active.fetch_sub(1, Ordering::SeqCst) == 1
            && shared.ticket.load(Ordering::SeqCst) & 1 == 1
            && let Some(caller) = shared
                .caller
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            caller.unpark();
        }
    }
}

#[cfg(target_os = "linux")]
fn set_affinity(cpus: &[usize]) {
    if cpus.is_empty() {
        return;
    }
    let mut set = rustix::thread::CpuSet::new();
    for &cpu in cpus {
        if cpu < rustix::thread::CpuSet::MAX_CPU {
            set.set(cpu);
        }
    }
    // Placement is a performance hint; the default affinity remains valid.
    let _ = rustix::thread::sched_setaffinity(None, &set);
}

#[cfg(not(target_os = "linux"))]
fn set_affinity(_cpus: &[usize]) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn every_unit_runs_once_and_late_workers_skip_closed_passes() {
        let pool = Pool::new(4);
        for round in 0..200 {
            let units = 1 + round % 37;
            let next = AtomicUsize::new(0);
            let done: Vec<AtomicUsize> = (0..units).map(|_| AtomicUsize::new(0)).collect();
            let work = |_| {
                loop {
                    let unit = next.fetch_add(1, Ordering::Relaxed);
                    if unit >= units {
                        break;
                    }
                    done[unit].fetch_add(1, Ordering::Relaxed);
                }
            };
            pool.run(4, &work, |lead| {
                lead.wake(1 + round % 4);
                work(0);
            });
            assert!(done.iter().all(|d| d.load(Ordering::Relaxed) == 1));
        }
    }

    #[test]
    fn worker_panics_reach_the_caller_after_the_pass_drains() {
        let pool = Pool::new(3);
        let started = AtomicUsize::new(0);
        let result = catch_unwind(AssertUnwindSafe(|| {
            pool.run(
                3,
                &|_| {
                    started.fetch_add(1, Ordering::SeqCst);
                    panic!("worker failure");
                },
                |lead| {
                    lead.wake(3);
                    // Give a worker time to wake so its panic is observed.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !lead.worker_panicked() && std::time::Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                },
            );
        }));
        assert!(result.is_err());
        assert!(started.load(Ordering::SeqCst) >= 1);
        // The pool remains usable.
        let count = AtomicUsize::new(0);
        let job = |_| {
            count.fetch_add(1, Ordering::SeqCst);
        };
        pool.run(3, &job, |lead| {
            lead.wake(3);
            job(0);
        });
        assert!((1..=3).contains(&count.load(Ordering::SeqCst)));
    }

    #[test]
    fn a_busy_pool_leaves_the_whole_pass_to_its_caller() {
        let pool = Pool::new(3);
        let ran = AtomicUsize::new(0);
        // A parked worker may wake spuriously; jobs here must tolerate that.
        pool.run(3, &|_| {}, |_| {
            let inner = pool.run(3, &|_| {}, |lead| lead.participants());
            assert_eq!(inner, 1);
            ran.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
