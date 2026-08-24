use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// A thread-safe request to abort JavaScript which is already executing.
///
/// Embedders use this for host resource deadlines and lifecycle cancellation.
/// Polling is performed by the VM instruction loop, so an interrupt can unwind
/// a script even when it never reaches an explicit ECMAScript loop opcode or
/// returns to the embedder between callbacks.
#[derive(Debug, Default)]
pub struct RuntimeInterrupt {
    cancelled: AtomicBool,
    user_navigation_requested: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

impl RuntimeInterrupt {
    /// Request that the current execution stop at the next VM interrupt poll.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Request that author JavaScript yield so the embedder can service a
    /// queued user navigation. Unlike [`Self::cancel`], this is a reusable
    /// per-document signal: the embedder clears it at the next user-interaction
    /// task boundary and rearms that task's ordinary deadline.
    pub fn request_user_navigation(&self) {
        self.user_navigation_requested
            .store(true, Ordering::Release);
    }

    /// Clear a pending user-navigation yield at the boundary where the
    /// embedder begins running the next user-interaction task.
    pub fn begin_user_interaction(&self) {
        self.user_navigation_requested
            .store(false, Ordering::Release);
    }

    /// Whether a user navigation is waiting for the currently running page
    /// task to yield. Async host-job drivers use this alongside VM polling so
    /// a task parked outside the bytecode loop also returns promptly.
    pub fn user_navigation_requested(&self) -> bool {
        self.user_navigation_requested.load(Ordering::Acquire)
    }

    /// Set or clear the wall-clock deadline for JavaScript execution.
    pub fn set_deadline(&self, deadline: Option<Instant>) {
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = deadline;
    }

    pub(crate) fn reason(&self) -> Option<&'static str> {
        if self.cancelled.load(Ordering::Acquire) {
            return Some("JavaScript execution cancelled by the host");
        }
        if self.user_navigation_requested.load(Ordering::Acquire) {
            return Some("JavaScript execution yielded to user navigation");
        }
        self.deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|deadline| Instant::now() >= deadline)
            .then_some("JavaScript execution deadline exceeded")
    }
}

/// Represents the limits of different runtime operations.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    /// Max stack size before an error is thrown.
    stack_size: usize,

    /// Max loop iterations before an error is thrown.
    loop_iteration: u64,

    /// Max backtrace count in exception.
    backtrace_limit: usize,

    /// Max function recursion limit
    resursion: usize,
}

impl Default for RuntimeLimits {
    #[inline]
    fn default() -> Self {
        Self {
            loop_iteration: u64::MAX,
            resursion: 512,
            backtrace_limit: 50,
            stack_size: 1024 * 10,
        }
    }
}

impl RuntimeLimits {
    /// Return the loop iteration limit.
    ///
    /// If the limit is exceeded in a loop it will throw and errror.
    ///
    /// The limit value [`u64::MAX`] means that there is no limit.
    #[inline]
    #[must_use]
    pub const fn loop_iteration_limit(&self) -> u64 {
        self.loop_iteration
    }

    /// Set the loop iteration limit.
    ///
    /// If the limit is exceeded in a loop it will throw and errror.
    ///
    /// Setting the limit to [`u64::MAX`] means that there is no limit.
    #[inline]
    pub fn set_loop_iteration_limit(&mut self, value: u64) {
        self.loop_iteration = value;
    }

    /// Disable loop iteration limit.
    #[inline]
    pub fn disable_loop_iteration_limit(&mut self) {
        self.loop_iteration = u64::MAX;
    }

    /// Get max backtrace limit for an exception.
    ///
    /// Default is 50.
    #[inline]
    #[must_use]
    pub const fn backtrace_limit(&self) -> usize {
        self.backtrace_limit
    }

    /// Set max backtrace limit for an exception.
    #[inline]
    pub fn set_backtrace_limit(&mut self, value: usize) {
        self.backtrace_limit = value;
    }

    /// Get max stack size.
    #[inline]
    #[must_use]
    pub const fn stack_size_limit(&self) -> usize {
        self.stack_size
    }

    /// Set max stack size before an error is thrown.
    #[inline]
    pub fn set_stack_size_limit(&mut self, value: usize) {
        self.stack_size = value;
    }

    /// Get recursion limit.
    #[inline]
    #[must_use]
    pub const fn recursion_limit(&self) -> usize {
        self.resursion
    }

    /// Set recursion limit before an error is thrown.
    #[inline]
    pub fn set_recursion_limit(&mut self, value: usize) {
        self.resursion = value;
    }
}
