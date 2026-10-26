//! Deadlock detection for the synchronization primitives
//!
//! Semaphores and blocking mutexes are the two places where a thread can wait
//! for a resource held by somebody else, so they are the only way a deadlock can
//! appear. The kernel keeps a resource-allocation graph for every process and
//! refuses a request (returning [`DEADLOCK_ERR`]) instead of letting the whole
//! process hang forever.
//!
//! * Counting semaphores are modelled with three vectors: how many units of each
//!   semaphore are still free (`avail`), how many each thread holds (`alloc`) and
//!   how many each blocked thread is asking for (`request`). A request is
//!   accepted only when the state stays *safe*, i.e. there is an order in which
//!   every thread can be satisfied and finish, which is exactly the safety test
//!   of the banker's algorithm.
//! * A blocking mutex has a single owner, so the corresponding graph is the
//!   wait-for chain `thread -> mutex -> owner -> mutex -> ...`; a request is
//!   refused when following the chain comes back to the requester.

use alloc::vec::Vec;

/// Error code returned to user space when a request would deadlock
pub const DEADLOCK_ERR: isize = -0xdead;

/// Upper bound of the wait-for chain walk, a guard against corrupted state
const MAX_CHAIN: usize = 64;

/// Per-process deadlock detection state
pub struct DeadlockDetect {
    /// whether detection is currently enabled by the process
    enabled: bool,
    /// free units of each semaphore, indexed by semaphore id
    avail: Vec<isize>,
    /// `alloc[tid][sem]`: units of `sem` held by thread `tid`
    alloc: Vec<Vec<isize>>,
    /// `request[tid][sem]`: units of `sem` thread `tid` is blocked on
    request: Vec<Vec<isize>>,
    /// owner of each blocking mutex, indexed by mutex id
    mutex_owner: Vec<Option<usize>>,
    /// the mutex each thread is waiting for
    mutex_wait: Vec<Option<usize>>,
}

impl DeadlockDetect {
    /// Create an empty record, detection is disabled by default
    pub fn new() -> Self {
        Self {
            enabled: false,
            avail: Vec::new(),
            alloc: Vec::new(),
            request: Vec::new(),
            mutex_owner: Vec::new(),
            mutex_wait: Vec::new(),
        }
    }

    /// Enable or disable detection for this process
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether detection is enabled
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Forget everything known about `tid`, used when it starts a new thread
    pub fn reset_thread(&mut self, tid: usize) {
        self.grow(tid);
        let row_alloc = &mut self.alloc[tid];
        for unit in row_alloc.iter_mut() {
            *unit = 0;
        }
        let row_request = &mut self.request[tid];
        for unit in row_request.iter_mut() {
            *unit = 0;
        }
        self.mutex_wait[tid] = None;
    }

    /// Register a freshly created semaphore with `res_count` units
    pub fn register_sem(&mut self, sem: usize, res_count: usize) {
        self.grow_res(sem);
        self.avail[sem] = res_count as isize;
        for tid in 0..self.alloc.len() {
            self.alloc[tid][sem] = 0;
            self.request[tid][sem] = 0;
        }
    }

    /// Record that `tid` obtained one unit of `sem`
    pub fn on_acquire(&mut self, tid: usize, sem: usize) {
        self.grow(tid);
        self.grow_res(sem);
        self.alloc[tid][sem] += 1;
        self.avail[sem] -= 1;
    }

    /// Record that one unit of `sem` has been returned
    ///
    /// An `up` does not carry the identity of the thread that owned the unit, so
    /// it is charged to the caller when it holds one, otherwise to any holder.
    pub fn on_release(&mut self, tid: usize, sem: usize) {
        self.grow(tid);
        self.grow_res(sem);
        self.avail[sem] += 1;
        if self.alloc[tid][sem] > 0 {
            self.alloc[tid][sem] -= 1;
            return;
        }
        let holder = (0..self.alloc.len()).find(|t| self.alloc[*t][sem] > 0);
        if let Some(holder) = holder {
            self.alloc[holder][sem] -= 1;
        }
    }

    /// Record the number of units of `sem` that `tid` is waiting for
    pub fn set_request(&mut self, tid: usize, sem: usize, count: isize) {
        self.grow(tid);
        self.grow_res(sem);
        self.request[tid][sem] = count;
    }

    /// Would the pending request leave the process in an unsafe state?
    ///
    /// This is the safety test of the banker's algorithm: pretend every thread
    /// whose outstanding request can be met finishes and gives its units back,
    /// and repeat. A thread that is left with an unmet request when no progress
    /// is possible any more is the one that would block forever.
    pub fn sem_would_deadlock(&self) -> bool {
        let nthread = self.alloc.len();
        let nres = self.avail.len();
        let mut work = self.avail.clone();
        let mut finish = Vec::new();
        finish.resize(nthread, false);
        loop {
            let mut progressed = false;
            for tid in 0..nthread {
                if finish[tid] {
                    continue;
                }
                let satisfiable = (0..nres)
                    .all(|res| self.request[tid].get(res).copied().unwrap_or(0) <= work[res]);
                if satisfiable {
                    finish[tid] = true;
                    progressed = true;
                    for (res, unit) in self.alloc[tid].iter().enumerate() {
                        work[res] += *unit;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        // a thread without an outstanding request never blocks, so only the
        // blocked ones have to be reachable in the simulation
        (0..nthread).any(|tid| !finish[tid] && self.request[tid].iter().any(|unit| *unit != 0))
    }

    /// Register a freshly created mutex
    pub fn register_mutex(&mut self, mid: usize) {
        if self.mutex_owner.len() <= mid {
            self.mutex_owner.resize(mid + 1, None);
            self.mutex_wait.resize(mid + 1, None);
        }
        self.mutex_owner[mid] = None;
    }

    /// Record that `tid` owns `mid`
    pub fn on_mutex_acquire(&mut self, mid: usize, tid: usize) {
        self.register_mutex(mid);
        self.mutex_owner[mid] = Some(tid);
        if self.mutex_wait.len() > tid {
            self.mutex_wait[tid] = None;
        }
    }

    /// Record that `mid` has been released
    pub fn on_mutex_release(&mut self, mid: usize) {
        if let Some(owner) = self.mutex_owner.get_mut(mid) {
            *owner = None;
        }
    }

    /// Remember that `tid` is about to wait for `mid`
    pub fn set_mutex_wait(&mut self, tid: usize, mid: Option<usize>) {
        self.grow(tid);
        self.mutex_wait[tid] = mid;
    }

    /// Would waiting for `mid` close a cycle in the wait-for chain?
    pub fn mutex_would_deadlock(&self, tid: usize, mid: usize) -> bool {
        let mut owner = match self.mutex_owner.get(mid).copied().flatten() {
            Some(owner) => owner,
            None => return false,
        };
        for _ in 0..MAX_CHAIN {
            if owner == tid {
                return true;
            }
            let next_mutex = match self.mutex_wait.get(owner).copied().flatten() {
                Some(next_mutex) => next_mutex,
                None => return false,
            };
            owner = match self.mutex_owner.get(next_mutex).copied().flatten() {
                Some(next_owner) => next_owner,
                None => return false,
            };
        }
        false
    }

    /// Make sure the rows of `tid` exist
    fn grow(&mut self, tid: usize) {
        while self.alloc.len() <= tid {
            self.alloc.push(Vec::new());
            self.request.push(Vec::new());
            self.mutex_wait.push(None);
        }
    }

    /// Make sure column `sem` exists in every row
    fn grow_res(&mut self, sem: usize) {
        if self.avail.len() <= sem {
            self.avail.resize(sem + 1, 0);
        }
        for tid in 0..self.alloc.len() {
            if self.alloc[tid].len() <= sem {
                self.alloc[tid].resize(sem + 1, 0);
            }
            if self.request[tid].len() <= sem {
                self.request[tid].resize(sem + 1, 0);
            }
        }
    }
}
