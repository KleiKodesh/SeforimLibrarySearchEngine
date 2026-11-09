//! Adaptive writer resources (the writer's "auto" mode, issue #19): use as
//! much of the machine as possible while it is idle, without the user ever
//! noticing the build.
//!
//! - Threads: one per core (the OS background mode throttles their CPU, disk
//!   and memory priority; see `tantivy::indexer::BackgroundPriority`), capped
//!   so that every thread's minimum buffer fits in a quarter of the free RAM.
//! - Memory: one small watcher thread per auto writer waits on OS signals and
//!   sets the writer's live memory budget (`tantivy::indexer::MemoryBudget`).
//!   Under memory pressure, or while the user is active, the budget drops to
//!   the minimum (15 MB per thread): every worker above it flushes its segment
//!   at its next batch, which frees its buffer. When the machine is idle again
//!   the budget grows back to a large value derived from the free RAM. The
//!   writer itself only reads one atomic per batch.
//!
//! OS signals (each checked against its official documentation):
//! - Windows: `CreateMemoryResourceNotification(LowMemoryResourceNotification)`, waited on
//!   with `WaitForSingleObject` (1 s timeout), and `GetLastInputInfo` vs `GetTickCount` for
//!   the user's last input (session-specific: a service session never sees input). Free RAM:
//!   `GlobalMemoryStatusEx` (`ullAvailPhys`).
//!   <https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-creatememoryresourcenotification>
//!   <https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject>
//!   <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getlastinputinfo>
//!   <https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-globalmemorystatusex>
//! - Linux and Android: a PSI trigger on `/proc/pressure/memory` (`some 150000 2000000`:
//!   150 ms of stall in a 2 s window, a window unprivileged users may set), polled for
//!   `POLLPRI` with a 1 s timeout; no user-activity signal (none is generic). Free RAM:
//!   `MemAvailable` in `/proc/meminfo`.
//!   <https://docs.kernel.org/accounting/psi.html>
//!   <https://man7.org/linux/man-pages/man5/proc_meminfo.5.html>
//! - Elsewhere (Apple platforms included): no watcher; the budget is fixed
//!   at open.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tantivy::indexer::MemoryBudget;
/// Background mode for the calling thread (a host's feeding thread):
/// nesting-safe begin / end pairs, see `tantivy::indexer::thread_priority`.
pub use tantivy::indexer::thread_priority::{begin_background, end_background};

/// tantivy's minimum buffer per indexing thread.
pub const MIN_BUDGET_PER_THREAD: usize = 15_000_000;
/// The largest buffer per thread the auto mode grants.
pub const MAX_BUDGET_PER_THREAD: usize = 512_000_000;
/// Budget per thread when the free RAM is unknown.
const UNKNOWN_RAM_BUDGET_PER_THREAD: usize = 64_000_000;
/// The user counts as active for this long after the last input.
const ACTIVE_FOR: Duration = Duration::from_secs(10);
/// While idle, the budget is re-derived from the free RAM this often.
const IDLE_REFRESH: Duration = Duration::from_secs(30);
/// Memory pressure counts for this long after the last signal.
const PRESSURE_FOR: Duration = Duration::from_secs(10);

/// What the watcher sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Load {
    /// Nobody needs the machine: large budget.
    Idle = 0,
    /// The user is working: minimum budget.
    Active = 1,
    /// The system is low on memory: minimum budget.
    Pressure = 2,
}

impl Load {
    fn from_u8(v: u8) -> Option<Load> {
        match v {
            0 => Some(Load::Idle),
            1 => Some(Load::Active),
            2 => Some(Load::Pressure),
            _ => None,
        }
    }
}

/// Free physical memory in bytes, if the OS tells.
pub fn available_memory() -> Option<u64> {
    imp::available_memory()
}

/// Threads for an auto writer: one per core, capped so that each thread's
/// minimum buffer fits in a quarter of the free RAM.
pub fn auto_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    match available_memory() {
        Some(free) => cores.min((free / 4 / MIN_BUDGET_PER_THREAD as u64).max(1) as usize),
        None => cores,
    }
}

/// The idle budget per thread: a quarter of the free RAM shared by the
/// threads, within [15 MB, 512 MB].
pub fn idle_budget_per_thread(threads: usize) -> usize {
    match available_memory() {
        Some(free) => ((free / 4) as usize / threads.max(1))
            .clamp(MIN_BUDGET_PER_THREAD, MAX_BUDGET_PER_THREAD),
        None => UNKNOWN_RAM_BUDGET_PER_THREAD,
    }
}

/// True when this OS has a watcher (else the auto budget stays as opened).
pub const HAS_WATCHER: bool = imp::HAS_WATCHER;

/// Stops its watcher thread when dropped (the thread exits within about a
/// second; nothing waits for it).
pub struct Watcher {
    stop: Arc<AtomicBool>,
    /// A simulated load for tests and benchmarks (`u8::MAX` = none).
    simulated: Arc<AtomicU8>,
    budget: MemoryBudget,
    threads: usize,
}

impl Watcher {
    /// Starts watching for `budget`, shared by `threads` indexing threads.
    /// On an OS without signals no thread is started; [`Watcher::simulate`]
    /// still works.
    pub fn start(budget: MemoryBudget, threads: usize) -> Watcher {
        let watcher = Watcher {
            stop: Arc::new(AtomicBool::new(false)),
            simulated: Arc::new(AtomicU8::new(u8::MAX)),
            budget,
            threads,
        };
        if HAS_WATCHER {
            let stop = watcher.stop.clone();
            let simulated = watcher.simulated.clone();
            let budget = watcher.budget.clone();
            let spawned = std::thread::Builder::new()
                .name("seforim-resource-watcher".into())
                .spawn(move || watch(stop, simulated, budget, threads));
            // no thread: the budget simply stays as opened
            let _ = spawned;
        }
        watcher
    }

    /// Forces a load (for tests and benchmarks), applied at once; `None`
    /// returns to the OS signals.
    pub fn simulate(&self, load: Option<Load>) {
        self.simulated
            .store(load.map_or(u8::MAX, |l| l as u8), Ordering::Relaxed);
        if let Some(load) = load {
            apply(&self.budget, load, self.threads);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn apply(budget: &MemoryBudget, load: Load, threads: usize) {
    match load {
        Load::Idle => budget.set_per_thread(idle_budget_per_thread(threads)),
        Load::Active | Load::Pressure => budget.set_per_thread(MIN_BUDGET_PER_THREAD),
    };
}

fn watch(stop: Arc<AtomicBool>, simulated: Arc<AtomicU8>, budget: MemoryBudget, threads: usize) {
    let mut signals = imp::Signals::open();
    let mut current = Load::Idle;
    let mut pressure_until: Option<Instant> = None;
    let mut last_apply = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // blocks up to ~1 s on the OS's memory signal
        if signals.wait_pressure() {
            pressure_until = Some(Instant::now() + PRESSURE_FOR);
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(forced) = Load::from_u8(simulated.load(Ordering::Relaxed)) {
            // `simulate` applied it; remember it so the real load re-applies
            // once the simulation ends
            current = forced;
            continue;
        }
        let load = if pressure_until.is_some_and(|t| Instant::now() < t) {
            Load::Pressure
        } else if signals.user_active(ACTIVE_FOR) {
            Load::Active
        } else {
            Load::Idle
        };
        // idle: re-derive the budget from the free RAM now and then (it was
        // measured while our own buffers were in use)
        let refresh = load == Load::Idle && last_apply.elapsed() >= IDLE_REFRESH;
        if load != current || refresh {
            apply(&budget, load, threads);
            current = load;
            last_apply = Instant::now();
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use winapi::shared::minwindef::FALSE;
    use winapi::shared::winerror::WAIT_TIMEOUT;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::memoryapi::{CreateMemoryResourceNotification, LowMemoryResourceNotification};
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::sysinfoapi::{GetTickCount, GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use winapi::um::winbase::WAIT_OBJECT_0;
    use winapi::um::winnt::HANDLE;
    use winapi::um::winuser::{GetLastInputInfo, LASTINPUTINFO};

    pub(super) const HAS_WATCHER: bool = true;

    pub(super) fn available_memory() -> Option<u64> {
        // SAFETY: dwLength set as the API requires; the struct is ours.
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            (GlobalMemoryStatusEx(&mut status) != FALSE).then_some(status.ullAvailPhys)
        }
    }

    pub(super) struct Signals {
        low_memory: HANDLE,
    }

    impl Signals {
        pub(super) fn open() -> Signals {
            // SAFETY: plain call; a null handle means no signal.
            let low_memory =
                unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) };
            Signals { low_memory }
        }

        /// Waits up to 1 s for the low-memory state; true while it holds.
        pub(super) fn wait_pressure(&mut self) -> bool {
            if self.low_memory.is_null() {
                std::thread::sleep(Duration::from_secs(1));
                return false;
            }
            // SAFETY: a live notification handle (closed only on drop).
            let r = unsafe { WaitForSingleObject(self.low_memory, 1000) };
            if r == WAIT_OBJECT_0 {
                // the object stays signaled while memory is low: pace the loop
                std::thread::sleep(Duration::from_secs(1));
                true
            } else {
                if r != WAIT_TIMEOUT {
                    std::thread::sleep(Duration::from_secs(1));
                }
                false
            }
        }

        /// True when the session's last input is newer than `window`.
        pub(super) fn user_active(&self, window: Duration) -> bool {
            // SAFETY: cbSize set as the API requires.
            unsafe {
                let mut info = LASTINPUTINFO {
                    cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                    dwTime: 0,
                };
                if GetLastInputInfo(&mut info) == FALSE {
                    return false;
                }
                // tick counts wrap after ~49.7 days; the input tick may also be
                // slightly AHEAD of now (the API notes it is not incremental)
                let idle_ms = GetTickCount().wrapping_sub(info.dwTime);
                (idle_ms as u64) < window.as_millis() as u64 || idle_ms > u32::MAX / 2
            }
        }
    }

    impl Drop for Signals {
        fn drop(&mut self) {
            if !self.low_memory.is_null() {
                // SAFETY: our handle, closed once.
                unsafe { CloseHandle(self.low_memory) };
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    pub(super) const HAS_WATCHER: bool = true;

    pub(super) fn available_memory() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }

    pub(super) struct Signals {
        psi: Option<std::fs::File>,
    }

    impl Signals {
        pub(super) fn open() -> Signals {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let psi = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open("/proc/pressure/memory")
                .ok()
                .and_then(|mut f| f.write_all(b"some 150000 2000000\0").ok().map(|_| f));
            Signals { psi }
        }

        /// Polls the PSI trigger up to 1 s; true when it fired.
        pub(super) fn wait_pressure(&mut self) -> bool {
            let Some(psi) = &self.psi else {
                std::thread::sleep(Duration::from_secs(1));
                return false;
            };
            let mut fds = libc::pollfd { fd: psi.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut fds, 1, 1000) };
            if n > 0 && fds.revents & libc::POLLERR != 0 {
                // the trigger is gone: stop using it
                self.psi = None;
                return false;
            }
            n > 0 && fds.revents & libc::POLLPRI != 0
        }

        pub(super) fn user_active(&self, _window: Duration) -> bool {
            false
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
mod imp {
    use std::time::Duration;

    pub(super) const HAS_WATCHER: bool = false;

    pub(super) fn available_memory() -> Option<u64> {
        None
    }

    pub(super) struct Signals;

    impl Signals {
        pub(super) fn open() -> Signals {
            Signals
        }

        pub(super) fn wait_pressure(&mut self) -> bool {
            std::thread::sleep(Duration::from_secs(1));
            false
        }

        pub(super) fn user_active(&self, _window: Duration) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_stay_in_range() {
        let threads = auto_threads();
        assert!(threads >= 1);
        let idle = idle_budget_per_thread(threads);
        assert!((MIN_BUDGET_PER_THREAD..=MAX_BUDGET_PER_THREAD).contains(&idle));
    }
}
