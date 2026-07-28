//! Minimal V8 host: one isolate, one dedicated OS thread, one job channel.
//!
//! The spike only needs what the US-005 verdict depends on: an isolate that
//! lives outside the Tokio runtime, an external handle able to stop a running
//! script, and quotas whose breach is DISTINGUISHABLE (`CellFailure`). Nothing
//! here is a Code Mode session: no `store`, no nested tools, no yield.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// Headroom handed back to V8 when the near-heap-limit callback fires. Without
/// it V8 calls `FatalProcessOutOfMemory` and the whole process dies, which is
/// exactly the silent kill US-005 AC3 forbids.
const HEAP_UNWIND_HEADROOM: usize = 8 * 1024 * 1024;

/// Why a cell stopped short of a value. Every variant is a QUOTA the port must
/// keep reporting separately; collapsing them would hide which limit fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellFailure {
    /// The script threw, or the module failed to compile.
    Js(String),
    /// The external watchdog terminated the script.
    CpuBudget {
        budget_ms: u64,
        stopped_after_ms: u64,
    },
    /// V8 approached the heap ceiling of the isolate.
    HeapLimit { limit_bytes: usize },
    /// The array-buffer allocator refused: native memory, not V8 heap.
    NativeMemory {
        budget_bytes: usize,
        requested_bytes: usize,
    },
    /// The worker thread died or the host was already shut down.
    HostGone(String),
}

impl std::fmt::Display for CellFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Js(message) => write!(formatter, "javascript error: {message}"),
            Self::CpuBudget {
                budget_ms,
                stopped_after_ms,
            } => write!(
                formatter,
                "cpu budget exceeded: {budget_ms} ms budget, stopped after {stopped_after_ms} ms"
            ),
            Self::HeapLimit { limit_bytes } => {
                write!(formatter, "v8 heap limit reached: {limit_bytes} bytes")
            }
            Self::NativeMemory {
                budget_bytes,
                requested_bytes,
            } => write!(
                formatter,
                "native memory budget exceeded: {requested_bytes} bytes requested, {budget_bytes} bytes allowed"
            ),
            Self::HostGone(detail) => write!(formatter, "isolate host unavailable: {detail}"),
        }
    }
}

/// Quotas of one isolate. The defaults mirror the PRD security budget
/// (256 MiB heap, 30 s per cell).
#[derive(Debug, Clone, Copy)]
pub struct IsolateLimits {
    pub heap_bytes: usize,
    pub native_bytes: usize,
    pub cpu_budget: Duration,
}

impl Default for IsolateLimits {
    fn default() -> Self {
        Self {
            heap_bytes: 256 * 1024 * 1024,
            native_bytes: 256 * 1024 * 1024,
            cpu_budget: Duration::from_secs(30),
        }
    }
}

/// JIT policy. V8 reads `--jitless` once per process, so the port cannot flip
/// it per session: US-007 AC2 turns that into an explicit refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JitMode {
    #[default]
    Enabled,
    Jitless,
}

struct PlatformInit {
    _platform: v8::SharedRef<v8::Platform>,
    jit: JitMode,
    /// Wall time the one and only process-wide V8 initialization took.
    cold_init: Duration,
}

static PLATFORM: OnceLock<Result<PlatformInit, String>> = OnceLock::new();

/// Initializes V8 once for the process. A second call asking for another JIT
/// mode fails instead of silently running under the first one.
pub fn initialize(jit: JitMode) -> Result<Duration, String> {
    match PLATFORM.get_or_init(|| initialize_once(jit)) {
        Ok(init) if init.jit == jit => Ok(init.cold_init),
        Ok(init) => Err(format!(
            "v8 already initialized with jit={:?}, cannot switch to jit={jit:?}",
            init.jit
        )),
        Err(detail) => Err(detail.clone()),
    }
}

fn initialize_once(jit: JitMode) -> Result<PlatformInit, String> {
    let started = Instant::now();
    v8::icu::set_common_data_77(deno_core_icudata::ICU_DATA)
        .map_err(|code| format!("icu data rejected by v8 (code {code})"))?;
    if jit == JitMode::Jitless {
        v8::V8::set_flags_from_string("--jitless");
    }
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform.clone());
    v8::V8::initialize();
    Ok(PlatformInit {
        _platform: platform,
        jit,
        cold_init: started.elapsed(),
    })
}

/// State the near-heap-limit callback and the allocator write into. Owned by
/// the worker thread, which outlives the isolate.
struct Quotas {
    heap_hit: AtomicBool,
    heap_limit: usize,
    handle: OnceLock<v8::IsolateHandle>,
}

struct BudgetAllocator {
    budget: usize,
    live: AtomicUsize,
    refused_bytes: AtomicUsize,
    refused: AtomicBool,
}

impl BudgetAllocator {
    fn take(&self, len: usize) -> bool {
        let mut current = self.live.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(len) else {
                return false;
            };
            if next > self.budget {
                self.refused.store(true, Ordering::Release);
                self.refused_bytes.store(next, Ordering::Release);
                return false;
            }
            match self.live.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }
}

/// SAFETY: the three functions below are the vtable V8 calls. `handle` is the
/// `Arc<BudgetAllocator>` pointer handed to `new_rust_allocator`, kept alive by
/// V8 until it calls `drop`.
unsafe extern "C" fn alloc_zeroed(handle: &BudgetAllocator, len: usize) -> *mut c_void {
    if !handle.take(len) {
        return std::ptr::null_mut();
    }
    let layout = match std::alloc::Layout::from_size_align(len.max(1), 8) {
        Ok(layout) => layout,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { std::alloc::alloc_zeroed(layout).cast() }
}

unsafe extern "C" fn alloc_uninit(handle: &BudgetAllocator, len: usize) -> *mut c_void {
    if !handle.take(len) {
        return std::ptr::null_mut();
    }
    let layout = match std::alloc::Layout::from_size_align(len.max(1), 8) {
        Ok(layout) => layout,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { std::alloc::alloc(layout).cast() }
}

unsafe extern "C" fn alloc_free(handle: &BudgetAllocator, data: *mut c_void, len: usize) {
    handle.live.fetch_sub(len, Ordering::AcqRel);
    if let Ok(layout) = std::alloc::Layout::from_size_align(len.max(1), 8) {
        unsafe { std::alloc::dealloc(data.cast(), layout) };
    }
}

unsafe extern "C" fn alloc_drop(handle: *const BudgetAllocator) {
    drop(unsafe { Arc::from_raw(handle) });
}

static ALLOCATOR_VTABLE: v8::RustAllocatorVtable<BudgetAllocator> = v8::RustAllocatorVtable {
    allocate: alloc_zeroed,
    allocate_uninitialized: alloc_uninit,
    free: alloc_free,
    drop: alloc_drop,
};

unsafe extern "C" fn near_heap_limit(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    let quotas = unsafe { &*(data as *const Quotas) };
    quotas.heap_hit.store(true, Ordering::Release);
    if let Some(handle) = quotas.handle.get() {
        handle.terminate_execution();
    }
    // Raising the limit is what lets V8 unwind the stack and report a normal
    // termination instead of aborting the process.
    current_heap_limit + HEAP_UNWIND_HEADROOM
}

struct Job {
    source: String,
    budget: Duration,
    reply: mpsc::Sender<Result<String, CellFailure>>,
}

/// Snapshot of what the isolate is holding, as V8 accounts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeapSnapshot {
    pub used_bytes: usize,
    pub total_bytes: usize,
    pub limit_bytes: usize,
    pub external_bytes: usize,
}

/// An isolate pinned to its own OS thread.
pub struct IsolateHost {
    jobs: Option<mpsc::Sender<Job>>,
    heap_probe: mpsc::Sender<mpsc::Sender<HeapSnapshot>>,
    handle: v8::IsolateHandle,
    worker: Option<std::thread::JoinHandle<()>>,
    limits: IsolateLimits,
}

impl IsolateHost {
    /// Creates the isolate and returns once it is ready to take a cell.
    pub fn start(limits: IsolateLimits) -> Result<Self, String> {
        initialize(JitMode::Enabled)?;
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (probe_tx, probe_rx) = mpsc::channel::<mpsc::Sender<HeapSnapshot>>();
        let (ready_tx, ready_rx) = mpsc::channel::<v8::IsolateHandle>();
        let worker = std::thread::Builder::new()
            .name("spike-v8-isolate".into())
            .spawn(move || worker_main(limits, job_rx, probe_rx, ready_tx))
            .map_err(|error| format!("cannot spawn isolate thread: {error}"))?;
        let handle = ready_rx
            .recv()
            .map_err(|_| "isolate thread died before becoming ready".to_string())?;
        Ok(Self {
            jobs: Some(job_tx),
            heap_probe: probe_tx,
            handle,
            worker: Some(worker),
            limits,
        })
    }

    pub fn limits(&self) -> IsolateLimits {
        self.limits
    }

    /// Evaluates `source` under the isolate's default CPU budget.
    pub fn eval(&self, source: &str) -> Result<String, CellFailure> {
        self.eval_with_budget(source, self.limits.cpu_budget)
    }

    pub fn eval_with_budget(&self, source: &str, budget: Duration) -> Result<String, CellFailure> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(CellFailure::HostGone("host already shut down".into()));
        };
        jobs.send(Job {
            source: source.to_string(),
            budget,
            reply: reply_tx,
        })
        .map_err(|_| CellFailure::HostGone("isolate thread is gone".into()))?;
        reply_rx
            .recv()
            .map_err(|_| CellFailure::HostGone("isolate thread died mid-cell".into()))?
    }

    /// External stop handle. Callable from any thread, including while the
    /// isolate is spinning inside JavaScript.
    pub fn interrupt(&self) -> bool {
        self.handle.terminate_execution()
    }

    pub fn heap(&self) -> Option<HeapSnapshot> {
        let (tx, rx) = mpsc::channel();
        self.heap_probe.send(tx).ok()?;
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    /// Closes the job channel and joins the worker within `deadline`. Returns
    /// `false` when the worker did NOT join: US-007 AC4 turns that into a
    /// reported failure rather than a detached thread.
    pub fn shutdown(mut self, deadline: Duration) -> bool {
        self.close(deadline)
    }

    fn close(&mut self, deadline: Duration) -> bool {
        self.jobs = None;
        let Some(worker) = self.worker.take() else {
            return true;
        };
        self.handle.terminate_execution();
        let started = Instant::now();
        while started.elapsed() < deadline {
            if worker.is_finished() {
                return worker.join().is_ok();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Deliberately NOT joined: the caller is told, and the thread stays
        // accounted for instead of vanishing from the report.
        false
    }
}

impl Drop for IsolateHost {
    fn drop(&mut self) {
        let _ = self.close(Duration::from_secs(2));
    }
}

fn worker_main(
    limits: IsolateLimits,
    jobs: mpsc::Receiver<Job>,
    probes: mpsc::Receiver<mpsc::Sender<HeapSnapshot>>,
    ready: mpsc::Sender<v8::IsolateHandle>,
) {
    let allocator = Arc::new(BudgetAllocator {
        budget: limits.native_bytes,
        live: AtomicUsize::new(0),
        refused_bytes: AtomicUsize::new(0),
        refused: AtomicBool::new(false),
    });
    let quotas = Box::new(Quotas {
        heap_hit: AtomicBool::new(false),
        heap_limit: limits.heap_bytes,
        handle: OnceLock::new(),
    });
    let allocator_for_v8 = Arc::clone(&allocator);
    // SAFETY: the raw pointer is the `Arc` V8 owns from now on; the `drop`
    // entry of the vtable gives it back to Rust exactly once.
    let v8_allocator =
        unsafe { v8::new_rust_allocator(Arc::into_raw(allocator_for_v8), &ALLOCATOR_VTABLE) };
    let params = v8::CreateParams::default()
        .heap_limits(0, limits.heap_bytes)
        .array_buffer_allocator(v8_allocator.make_shared());
    let mut isolate = v8::Isolate::new(params);
    let handle = isolate.thread_safe_handle();
    let _ = quotas.handle.set(handle.clone());
    let quotas_ptr: *mut Quotas = Box::into_raw(quotas);
    isolate.add_near_heap_limit_callback(near_heap_limit, quotas_ptr.cast());
    if ready.send(handle.clone()).is_err() {
        // Nobody is listening any more: tear down instead of idling.
        drop(isolate);
        drop(unsafe { Box::from_raw(quotas_ptr) });
        return;
    }

    loop {
        // A probe never blocks a pending cell: both channels are drained, jobs
        // first, and the loop only parks when neither has work.
        match jobs.try_recv() {
            Ok(job) => {
                let outcome = run_cell(&mut isolate, &handle, quotas_ptr, &allocator, &job);
                let _ = job.reply.send(outcome);
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match probes.try_recv() {
            Ok(reply) => {
                let _ = reply.send(heap_snapshot(&mut isolate));
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
            Err(mpsc::TryRecvError::Empty) => {}
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    drop(isolate);
    // SAFETY: the isolate is gone, so V8 can no longer call back into `quotas`.
    drop(unsafe { Box::from_raw(quotas_ptr) });
}

fn heap_snapshot(isolate: &mut v8::OwnedIsolate) -> HeapSnapshot {
    let stats = isolate.get_heap_statistics();
    HeapSnapshot {
        used_bytes: stats.used_heap_size(),
        total_bytes: stats.total_heap_size(),
        limit_bytes: stats.heap_size_limit(),
        external_bytes: stats.external_memory(),
    }
}

fn run_cell(
    isolate: &mut v8::OwnedIsolate,
    handle: &v8::IsolateHandle,
    quotas_ptr: *mut Quotas,
    allocator: &Arc<BudgetAllocator>,
    job: &Job,
) -> Result<String, CellFailure> {
    // SAFETY: the worker owns the box for the whole isolate lifetime.
    let quotas = unsafe { &*quotas_ptr };
    quotas.heap_hit.store(false, Ordering::Release);
    allocator.refused.store(false, Ordering::Release);
    allocator.refused_bytes.store(0, Ordering::Release);

    let watchdog_fired = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + job.budget;
    let watchdog = {
        let handle = handle.clone();
        let fired = Arc::clone(&watchdog_fired);
        let budget = job.budget;
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = Arc::clone(&done);
        let joiner = std::thread::Builder::new()
            .name("spike-v8-watchdog".into())
            .spawn(move || {
                let step = Duration::from_millis(2);
                let end = Instant::now() + budget;
                while Instant::now() < end {
                    if done_for_thread.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(step);
                }
                if !done_for_thread.load(Ordering::Acquire) {
                    fired.store(true, Ordering::Release);
                    handle.terminate_execution();
                }
            })
            .ok();
        (done, joiner)
    };

    let outcome = eval_source(isolate, &job.source);

    watchdog.0.store(true, Ordering::Release);
    if let Some(joiner) = watchdog.1 {
        let _ = joiner.join();
    }
    // A termination request outlives the script that absorbed it; clearing it
    // is what keeps the session usable for the NEXT cell (US-007 AC3).
    isolate.cancel_terminate_execution();

    match outcome {
        Ok(value) => Ok(value),
        Err(js_error) => {
            // The allocator is checked FIRST on purpose: V8 treats a refused
            // array-buffer allocation as heap pressure and runs the
            // near-heap-limit callback on its way out, so `heap_hit` is set in
            // both cases. The allocator flag is the only one that is not.
            if allocator.refused.load(Ordering::Acquire) {
                Err(CellFailure::NativeMemory {
                    budget_bytes: allocator.budget,
                    requested_bytes: allocator.refused_bytes.load(Ordering::Acquire),
                })
            } else if quotas.heap_hit.load(Ordering::Acquire) {
                Err(CellFailure::HeapLimit {
                    limit_bytes: quotas.heap_limit,
                })
            } else if watchdog_fired.load(Ordering::Acquire) {
                let overshoot = Instant::now().saturating_duration_since(deadline);
                Err(CellFailure::CpuBudget {
                    budget_ms: job.budget.as_millis() as u64,
                    stopped_after_ms: (job.budget + overshoot).as_millis() as u64,
                })
            } else {
                Err(CellFailure::Js(js_error))
            }
        }
    }
}

/// Compiles and runs `source` in a fresh context, returning its completion
/// value as a string. `Err` carries the JavaScript message, or an empty string
/// when the isolate was terminated (V8 reports no exception then).
///
/// Running model-supplied source IS the feature under test here; no JavaScript
/// `eval()` is involved. The guarantee the spike has to establish is not that
/// the code is trusted but that its blast radius is bounded: a fresh context
/// per cell, no host bindings, and the three quotas above.
fn eval_source(isolate: &mut v8::OwnedIsolate, source: &str) -> Result<String, String> {
    // A termination leaves NO exception behind, so the empty string is what
    // tells `run_cell` to fall back to the quota flags.
    macro_rules! caught {
        ($try_catch:expr) => {{
            if $try_catch.has_terminated() {
                String::new()
            } else {
                match $try_catch.exception() {
                    Some(exception) => exception.to_rust_string_lossy($try_catch),
                    None => String::new(),
                }
            }
        }};
    }

    v8::scope!(let scope, isolate);
    // A fresh context per cell: a cell never inherits another cell's globals.
    // Cross-cell state is the session store's job, not the global object's.
    let context = v8::Context::new(scope, v8::ContextOptions::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(let try_catch, scope);

    let Some(code) = v8::String::new(try_catch, source) else {
        return Err("source is not valid UTF-16 for v8".to_string());
    };
    let Some(script) = v8::Script::compile(try_catch, code, None) else {
        return Err(caught!(try_catch));
    };
    let Some(value) = script.run(try_catch) else {
        return Err(caught!(try_catch));
    };
    Ok(value.to_rust_string_lossy(try_catch))
}
