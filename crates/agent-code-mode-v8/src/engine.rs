//! One isolate and one OS thread per cell, behind the `CellEngine` contract.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_code_mode::{
    CellEngine, CellFailure, CellFailureKind, CellId, CellSink, ExecuteRequest, NestedTool,
    NestedToolBinding, NestedToolDispatcher, NoNestedTools, ShutdownReport,
};
use agent_core::sync::lock;

use crate::globals;

/// Headroom given back to V8 when the near-heap-limit callback fires. Without
/// it V8 calls `FatalProcessOutOfMemory` and takes the whole Pyxis process
/// down, which is precisely the silent kill US-005 forbids.
const HEAP_UNWIND_HEADROOM: usize = 8 * 1024 * 1024;
/// Microtask checkpoints one cell may need before its promise settles. A
/// checkpoint drains the whole queue, so this only bounds pathological
/// promise chains that keep re-enqueueing themselves.
const MAX_CHECKPOINTS: usize = 64;

/// Quotas applied to every cell of an engine. Defaults are the PRD security
/// budget: 256 MiB of V8 heap, 256 MiB of native memory, 30 s of CPU.
#[derive(Debug, Clone, Copy)]
pub struct EngineLimits {
    pub heap_bytes: usize,
    pub native_bytes: usize,
    pub cpu_budget: Duration,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            heap_bytes: 256 * 1024 * 1024,
            native_bytes: 256 * 1024 * 1024,
            cpu_budget: Duration::from_secs(30),
        }
    }
}

/// External stop switch of one cell. Held by the engine and by the worker, so
/// an interruption that lands before the isolate exists is not lost.
struct CellControl {
    cancelled: AtomicBool,
    handle: Mutex<Option<v8::IsolateHandle>>,
}

impl CellControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            handle: Mutex::new(None),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(handle) = lock(&self.handle).as_ref() {
            handle.terminate_execution();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Publishes the isolate handle, and stops the cell straight away when an
    /// interruption arrived while the isolate was still being created.
    ///
    /// The check runs UNDER the handle lock, which is what makes the two orders
    /// equivalent: either `cancel` set the flag before the check and this
    /// terminates, or it takes the lock afterwards and finds the handle. Taking
    /// the lock second would leave a window where neither side terminates.
    fn publish(&self, handle: v8::IsolateHandle) {
        let mut slot = lock(&self.handle);
        if self.is_cancelled() {
            handle.terminate_execution();
        }
        *slot = Some(handle);
    }
}

struct CellWorker {
    control: Arc<CellControl>,
    thread: std::thread::JoinHandle<()>,
}

/// Code Mode engine of ONE session.
pub struct IsolateEngine {
    limits: EngineLimits,
    /// Session store: what `store(key, value)` writes and `load(key)` reads.
    /// It is the only state that crosses cells, and it never crosses sessions.
    store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    workers: Mutex<HashMap<CellId, CellWorker>>,
    /// Pipeline the cells dispatch their nested calls through. It is bound per
    /// step, because the tool plan a step captured is what a nested call has to
    /// meet: binding it once at construction would let a cell dispatch against
    /// a stale permission and taint state.
    nested: NestedToolBinding,
}

impl IsolateEngine {
    pub fn new(limits: EngineLimits) -> Result<Self, crate::V8Error> {
        Self::with_nested_binding(limits, NestedToolBinding::default())
    }

    /// Builds an engine that reads the caller's binding, so one binding can be
    /// shared by the tool that sets it and by every session that reads it.
    pub fn with_nested_binding(
        limits: EngineLimits,
        nested: NestedToolBinding,
    ) -> Result<Self, crate::V8Error> {
        crate::ensure_initialized()?;
        Ok(Self {
            limits,
            store: Arc::new(Mutex::new(HashMap::new())),
            workers: Mutex::new(HashMap::new()),
            nested,
        })
    }

    /// Handle the `exec` tool binds the plan of the step in flight to.
    pub fn nested_tools(&self) -> NestedToolBinding {
        self.nested.clone()
    }

    /// Session store snapshot, for tests and for the surfaces that inspect it.
    pub fn stored_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = lock(&self.store).keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Joins the workers whose cell already ended, so a long session does not
    /// accumulate finished threads.
    fn reap(&self) {
        let mut workers = lock(&self.workers);
        let done: Vec<CellId> = workers
            .iter()
            .filter(|(_, worker)| worker.thread.is_finished())
            .map(|(cell, _)| cell.clone())
            .collect();
        for cell in done {
            if let Some(worker) = workers.remove(&cell) {
                let _ = worker.thread.join();
            }
        }
    }
}

impl CellEngine for IsolateEngine {
    fn start(&self, cell: CellId, request: &ExecuteRequest, sink: CellSink) -> Result<(), String> {
        self.reap();
        let control = Arc::new(CellControl::new());
        let store = Arc::clone(&self.store);
        let limits = self.limits;
        let source = request.source.clone();
        let catalog = request.tools.clone();
        // A session with nothing bound refuses every nested call, and the
        // canonical dispatcher that does so already exists. Resolving it HERE
        // keeps the option out of the cell, where it would only be a second
        // spelling of the same refusal.
        let dispatcher = self
            .nested
            .get()
            .unwrap_or_else(|| Arc::new(NoNestedTools) as Arc<dyn NestedToolDispatcher>);
        let worker_control = Arc::clone(&control);
        let cell_for_thread = cell.clone();
        // US-019 AC3: a cell runs on its OWN OS thread, where NEITHER the span
        // nor the collector is inherited (both are thread-local). Captured here,
        // inside the `exec` call, so what the cell traces is still correlated
        // thread -> turn -> call -> cell instead of arriving as an orphan line
        // or not arriving at all.
        let caller = tracing::Span::current();
        let collector = tracing::dispatcher::get_default(Clone::clone);
        let thread = std::thread::Builder::new()
            .name(format!("pyxis-cell-{}", cell.as_str()))
            .spawn(move || {
                run_cell(CellJob {
                    cell: cell_for_thread,
                    source,
                    catalog,
                    dispatcher,
                    sink,
                    control: worker_control,
                    limits,
                    store,
                    caller,
                    collector,
                });
            })
            .map_err(|error| format!("cannot spawn the cell thread: {error}"))?;
        lock(&self.workers).insert(cell, CellWorker { control, thread });
        Ok(())
    }

    fn interrupt(&self, cell: &CellId) {
        if let Some(worker) = lock(&self.workers).get(cell) {
            worker.control.cancel();
        }
    }

    fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        // Taken ONCE: a cell started after this point cannot slip between the
        // cancellation and the wait, and nothing is joined while the table lock
        // is held.
        let workers: Vec<(CellId, CellWorker)> = std::mem::take(&mut *lock(&self.workers))
            .into_iter()
            .collect();
        for (_, worker) in &workers {
            worker.control.cancel();
        }

        let until = Instant::now() + deadline;
        let mut unjoined = Vec::new();
        for (cell, worker) in workers {
            let finished = loop {
                if worker.thread.is_finished() {
                    break true;
                }
                if Instant::now() >= until {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            if finished {
                let _ = worker.thread.join();
            } else {
                // Deliberately NOT detached silently: the caller is told which
                // worker is still holding an isolate (US-007 AC4).
                unjoined.push(cell);
            }
        }

        if unjoined.is_empty() {
            return ShutdownReport::joined();
        }
        let mut names: Vec<String> = unjoined.iter().map(|cell| cell.to_string()).collect();
        names.sort();
        ShutdownReport::unjoined(format!(
            "{} cell worker(s) did not join within {} ms: {}",
            names.len(),
            deadline.as_millis(),
            names.join(", ")
        ))
    }
}

/// V8 heap ceiling of one cell, and the way back out of it.
struct Quotas {
    heap_hit: AtomicBool,
    handle: v8::IsolateHandle,
}

struct BudgetAllocator {
    budget: usize,
    live: AtomicUsize,
    /// Size of the refused request, and what was already live when it landed.
    /// Both are needed to say why a request smaller than the budget failed.
    refused_bytes: AtomicUsize,
    refused_live: AtomicUsize,
    refused: AtomicBool,
}

impl BudgetAllocator {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            live: AtomicUsize::new(0),
            refused_bytes: AtomicUsize::new(0),
            refused_live: AtomicUsize::new(0),
            refused: AtomicBool::new(false),
        }
    }

    fn take(&self, len: usize) -> bool {
        let mut current = self.live.load(Ordering::Acquire);
        loop {
            match current.checked_add(len) {
                Some(next) if next <= self.budget => {
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
                _ => {
                    // The two sizes are published BEFORE the flag, so a reader
                    // that sees the refusal reads the numbers that caused it.
                    self.refused_bytes.store(len, Ordering::Relaxed);
                    self.refused_live.store(current, Ordering::Relaxed);
                    self.refused.store(true, Ordering::Release);
                    return false;
                }
            }
        }
    }

    fn give_back(&self, len: usize) {
        self.live.fetch_sub(len, Ordering::AcqRel);
    }
}

/// Body of both allocator entries: reserve the budget FIRST, and give it back
/// on every path that ends without memory, or a cell would pay for allocations
/// it never received.
unsafe fn allocate(handle: &BudgetAllocator, len: usize, zeroed: bool) -> *mut c_void {
    if !handle.take(len) {
        return std::ptr::null_mut();
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len.max(1), 8) else {
        handle.give_back(len);
        return std::ptr::null_mut();
    };
    let data = unsafe {
        if zeroed {
            std::alloc::alloc_zeroed(layout)
        } else {
            std::alloc::alloc(layout)
        }
    };
    if data.is_null() {
        handle.give_back(len);
    }
    data.cast()
}

unsafe extern "C" fn alloc_zeroed(handle: &BudgetAllocator, len: usize) -> *mut c_void {
    unsafe { allocate(handle, len, true) }
}

unsafe extern "C" fn alloc_uninit(handle: &BudgetAllocator, len: usize) -> *mut c_void {
    unsafe { allocate(handle, len, false) }
}

unsafe extern "C" fn alloc_free(handle: &BudgetAllocator, data: *mut c_void, len: usize) {
    handle.give_back(len);
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
    quotas.handle.terminate_execution();
    current_heap_limit + HEAP_UNWIND_HEADROOM
}

/// Everything that can stop a cell from OUTSIDE its script, in one place, so
/// `classify` reads one value instead of four.
struct CellGuards {
    allocator: Arc<BudgetAllocator>,
    quotas: Arc<Quotas>,
    control: Arc<CellControl>,
    limits: EngineLimits,
}

/// Body of one cell thread. It always ends by publishing a terminal state on
/// the sink, whatever happens, so no cell can be left running in the session.
struct CellJob {
    cell: CellId,
    source: String,
    catalog: Vec<NestedTool>,
    dispatcher: Arc<dyn NestedToolDispatcher>,
    sink: CellSink,
    control: Arc<CellControl>,
    limits: EngineLimits,
    store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// Span of the `exec` call that opened this cell, carried across the thread
    /// boundary so the cell's traces keep their correlation.
    caller: tracing::Span,
    /// Collector in force where the cell was requested. A worker thread starts
    /// with none, so without this a cell's traces would be dropped instead of
    /// reaching the file the binary opened.
    collector: tracing::Dispatch,
}

fn run_cell(job: CellJob) {
    let CellJob {
        cell,
        source,
        catalog,
        dispatcher,
        sink,
        control,
        limits,
        store,
        caller,
        collector,
    } = job;
    // Both guards live for the whole cell: `run_cell` is synchronous, so neither
    // crosses an await point. Order matters, the span has to be created under
    // the collector that will receive it.
    let _collecting = tracing::dispatcher::set_default(&collector);
    let span = tracing::info_span!(
        target: "pyxis::code_mode",
        parent: &caller,
        "cell",
        cell_id = %cell
    );
    let _entered = span.enter();
    if control.is_cancelled() {
        sink.finish(Some(CellFailure::interrupted(
            "cell interrupted before it started",
        )));
        return;
    }

    let allocator = Arc::new(BudgetAllocator::new(limits.native_bytes));
    // SAFETY: the raw pointer is the `Arc` V8 owns from here on; the `drop`
    // entry of the vtable gives it back to Rust exactly once.
    let v8_allocator =
        unsafe { v8::new_rust_allocator(Arc::into_raw(Arc::clone(&allocator)), &ALLOCATOR_VTABLE) };
    let params = v8::CreateParams::default()
        .heap_limits(0, limits.heap_bytes)
        .array_buffer_allocator(v8_allocator.make_shared());
    let mut isolate = v8::Isolate::new(params);
    let handle = isolate.thread_safe_handle();
    // Same ownership scheme as the allocator, so the two V8 callback payloads
    // of this function are read and released the same way: a live `Arc` here,
    // a raw one V8 holds, given back once the isolate is gone.
    let quotas = Arc::new(Quotas {
        heap_hit: AtomicBool::new(false),
        handle: handle.clone(),
    });
    let quotas_ptr = Arc::into_raw(Arc::clone(&quotas));
    isolate.add_near_heap_limit_callback(near_heap_limit, quotas_ptr.cast_mut().cast());
    control.publish(handle.clone());

    let in_tool = Arc::new(AtomicU64::new(0));
    let watchdog = Watchdog::start(handle, limits.cpu_budget, Arc::clone(&in_tool));
    let snapshot = lock(&store).clone();
    let outcome = evaluate(
        &mut isolate,
        CellContext {
            cell: cell.clone(),
            source: &source,
            sink: sink.clone(),
            stored: snapshot,
            catalog,
            dispatcher,
            in_tool,
        },
    );
    let expired = watchdog.stop();

    // A termination request outlives the script that absorbed it. Clearing it
    // is what keeps the isolate healthy for its own teardown.
    isolate.cancel_terminate_execution();

    let guards = CellGuards {
        allocator,
        quotas,
        control,
        limits,
    };
    let failure = classify(&outcome, &guards, expired);
    // A `store(...)` a cell already executed is committed even when the cell
    // then failed, exactly as the baseline does: the write happened, and
    // dropping it would make a partial cell silently lose work the model can
    // see it performed.
    if !outcome.writes.is_empty() {
        let mut stored = lock(&store);
        for (key, value) in outcome.writes.iter() {
            stored.insert(key.clone(), value.clone());
        }
    }
    // A cell that failed says so at `warn`, with its KIND: that is the line a
    // user reaches for when a Code Mode turn produced nothing (US-019 AC3). The
    // script source and its output never appear.
    match &failure {
        Some(failure) => tracing::warn!(
            target: "pyxis::code_mode",
            kind = failure.kind.label(),
            "code mode cell failed"
        ),
        None => tracing::debug!(target: "pyxis::code_mode", "code mode cell finished"),
    }
    sink.finish(failure);
    // Order matters: the isolate has to go first, because a callback of its
    // teardown could still reach the quotas.
    drop(isolate);
    // SAFETY: the isolate is gone, so V8 can no longer reach the `Arc` it was
    // handed, and this is the one place that gives it back.
    drop(unsafe { Arc::from_raw(quotas_ptr) });
}

/// Terminates a cell whose CPU budget expired, from outside the isolate.
struct Watchdog {
    done: Arc<AtomicBool>,
    expired: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(handle: v8::IsolateHandle, budget: Duration, in_tool: Arc<AtomicU64>) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let expired = Arc::new(AtomicBool::new(false));
        let thread = {
            let done = Arc::clone(&done);
            let expired = Arc::clone(&expired);
            std::thread::Builder::new()
                .name("pyxis-cell-watchdog".into())
                .spawn(move || {
                    // The budget is CPU the CELL burns. Time the cell spends
                    // parked inside a nested tool is the tool's, so it is not
                    // accumulated at all rather than added back afterwards:
                    // adding it back would fire the watchdog mid-call.
                    let mut consumed = Duration::ZERO;
                    let mut last = Instant::now();
                    loop {
                        if done.load(Ordering::Acquire) {
                            return;
                        }
                        let now = Instant::now();
                        let elapsed = now.saturating_duration_since(last);
                        last = now;
                        if in_tool.load(Ordering::Acquire) == 0 {
                            consumed += elapsed;
                        }
                        if consumed >= budget {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if !done.load(Ordering::Acquire) {
                        expired.store(true, Ordering::Release);
                        handle.terminate_execution();
                    }
                })
                .ok()
        };
        Self {
            done,
            expired,
            thread,
        }
    }

    fn stop(self) -> bool {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread {
            let _ = thread.join();
        }
        self.expired.load(Ordering::Acquire)
    }
}

/// Why the JavaScript side stopped, before quotas are taken into account.
///
/// The three cases are kept apart because they are answered by three different
/// parties: the model wrote the script, V8 answers for a termination, and the
/// host answers for a cell it could not even set up. Collapsing them into one
/// string is how a host failure ends up reported to the model as its own bug.
enum CellStop {
    /// The script threw or failed to compile. The text is what the model reads.
    Script(String),
    /// V8 stopped the script from outside. The cause is in the quota flags.
    Terminated,
    /// The host could not build the cell. Never the model's fault.
    Host(String),
}

/// What the JavaScript side produced, before quotas are taken into account.
struct Evaluation {
    stop: Option<CellStop>,
    exited: bool,
    writes: HashMap<String, serde_json::Value>,
}

struct CellContext<'a> {
    cell: CellId,
    source: &'a str,
    sink: CellSink,
    stored: HashMap<String, serde_json::Value>,
    catalog: Vec<NestedTool>,
    dispatcher: Arc<dyn NestedToolDispatcher>,
    in_tool: Arc<AtomicU64>,
}

fn evaluate(isolate: &mut v8::OwnedIsolate, context_data: CellContext<'_>) -> Evaluation {
    let source = context_data.source;
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, v8::ContextOptions::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    scope.set_slot(globals::CellRuntimeState {
        cell: context_data.cell,
        sink: context_data.sink,
        stored: context_data.stored,
        writes: HashMap::new(),
        exit_requested: false,
        catalog: context_data.catalog,
        dispatcher: context_data.dispatcher,
        next_call: 0,
        in_tool: context_data.in_tool,
    });

    if let Err(detail) = globals::install(scope) {
        return Evaluation {
            stop: Some(CellStop::Host(detail)),
            exited: false,
            writes: HashMap::new(),
        };
    }

    let stop = run_wrapped(scope, source);
    let (exited, writes) = scope
        .get_slot_mut::<globals::CellRuntimeState>()
        .map(|state| (state.exit_requested, std::mem::take(&mut state.writes)))
        .unwrap_or_default();
    Evaluation {
        stop,
        exited,
        writes,
    }
}

/// Wraps the model source in an async IIFE so `await` works at top level, then
/// drains the microtask queue until the returned promise settles.
///
/// The wrapper prefix stays on the FIRST line so a reported line number still
/// matches the source the model wrote for every line but that one.
fn run_wrapped(scope: &mut v8::PinScope<'_, '_>, source: &str) -> Option<CellStop> {
    /// A termination leaves NO exception behind, which is `Terminated`. Written
    /// as a macro because the concrete scope type of a `TryCatch` is not
    /// nameable without pinning down every lifetime.
    macro_rules! caught {
        ($try_catch:expr) => {{
            match $try_catch.exception() {
                Some(exception) if !$try_catch.has_terminated() => {
                    CellStop::Script(describe($try_catch, exception))
                }
                _ => CellStop::Terminated,
            }
        }};
    }

    let wrapped = format!("(async () => {{ {source}\n}})();");
    v8::tc_scope!(let try_catch, scope);

    let Some(code) = v8::String::new(try_catch, &wrapped) else {
        return Some(CellStop::Host(
            "cell source is not representable in v8".to_string(),
        ));
    };
    let Some(script) = v8::Script::compile(try_catch, code, None) else {
        return Some(caught!(try_catch));
    };
    let Some(value) = script.run(try_catch) else {
        return Some(caught!(try_catch));
    };
    let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) else {
        // Cannot happen with the wrapper above, but a non-promise completion
        // is a success rather than an invented error.
        return None;
    };

    for _ in 0..MAX_CHECKPOINTS {
        try_catch.perform_microtask_checkpoint();
        match promise.state() {
            v8::PromiseState::Pending => {}
            v8::PromiseState::Fulfilled => return None,
            v8::PromiseState::Rejected => {
                let rejection = promise.result(try_catch);
                return Some(CellStop::Script(describe(try_catch, rejection)));
            }
        }
        if try_catch.has_terminated() {
            return Some(caught!(try_catch));
        }
    }
    // The cell's OWN promise is still pending with the queue drained. Nothing
    // can settle it: the allowlist has no timer and no I/O, so a nested call is
    // the only thing that ever resolves one and it answers synchronously. That
    // is a cell blocked on itself, and reporting it as a success would hand the
    // model a truncated result with nothing saying it is truncated.
    Some(CellStop::Script(format!(
        "cell is still awaiting a promise nothing can settle, after {MAX_CHECKPOINTS} microtask checkpoints"
    )))
}

/// Human-readable form of a thrown value: the `stack` of an `Error` when there
/// is one, its `message` otherwise, and the stringified value as a last resort.
fn describe(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::Value>) -> String {
    if let Ok(object) = v8::Local::<v8::Object>::try_from(value) {
        for property in ["stack", "message"] {
            let Some(key) = v8::String::new(scope, property) else {
                continue;
            };
            if let Some(found) = object.get(scope, key.into())
                && !found.is_undefined()
                && !found.is_null()
            {
                let text = found.to_rust_string_lossy(scope);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    value.to_rust_string_lossy(scope)
}

/// Turns a raw evaluation into the typed failure the session persists. The
/// order matters: V8 routes a refused array-buffer allocation through the
/// near-heap-limit callback too, so the allocator flag has to be read first or
/// a native breach would be reported as a heap breach (US-005 finding).
fn classify(outcome: &Evaluation, guards: &CellGuards, cpu_expired: bool) -> Option<CellFailure> {
    let stop = outcome.stop.as_ref()?;
    // A cell the host could not build says so with the kind that means it, so
    // the model is never blamed for a failure it had no part in.
    if let CellStop::Host(detail) = stop {
        return Some(CellFailure::new(
            CellFailureKind::EngineLost,
            detail.clone(),
        ));
    }
    // `exit()` stops the script from the inside; that is a normal end. An error
    // the model raised AFTER catching the exit does not carry the marker, so it
    // is still reported as the failure it is.
    let exiting = match stop {
        CellStop::Script(text) => text.contains(globals::EXIT_MARKER),
        CellStop::Terminated => true,
        CellStop::Host(_) => false,
    };
    if outcome.exited && exiting {
        return None;
    }

    let allocator = &guards.allocator;
    if allocator.refused.load(Ordering::Acquire) {
        return Some(CellFailure::new(
            CellFailureKind::NativeMemory,
            format!(
                "native memory budget exceeded: {} bytes requested with {} bytes live, {} bytes allowed",
                allocator.refused_bytes.load(Ordering::Relaxed),
                allocator.refused_live.load(Ordering::Relaxed),
                allocator.budget
            ),
        ));
    }
    if guards.quotas.heap_hit.load(Ordering::Acquire) {
        return Some(CellFailure::new(
            CellFailureKind::HeapLimit,
            format!("v8 heap limit reached: {} bytes", guards.limits.heap_bytes),
        ));
    }
    if guards.control.is_cancelled() {
        return Some(CellFailure::interrupted("cell interrupted"));
    }
    if cpu_expired {
        return Some(CellFailure::new(
            CellFailureKind::CpuBudget,
            format!(
                "cpu budget exceeded: {} ms",
                guards.limits.cpu_budget.as_millis()
            ),
        ));
    }
    match stop {
        CellStop::Script(text) => Some(CellFailure::new(CellFailureKind::Script, text.clone())),
        // Terminated, but no guard claims it: the engine stopped the cell and
        // cannot say why, which is exactly what `EngineLost` names.
        _ => Some(CellFailure::new(
            CellFailureKind::EngineLost,
            "cell stopped without a reportable cause",
        )),
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
