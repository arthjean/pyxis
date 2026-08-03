//! The Code Mode session: one per thread, owner of its cells.
//!
//! The state machine lives HERE and the JavaScript engine lives behind
//! `CellEngine`. That split is what makes `exec`, `wait`, `terminate` and
//! `shutdown` testable without a JavaScript engine at all (US-006), and it is
//! also what lets US-005's verdict be revisited later: swapping an in-process
//! isolate for a process-owned host changes the engine, not this file.
//!
//! Two invariants hold for every path below:
//!   - a response only ever carries the output produced SINCE the previous
//!     yield of that cell, so nothing is delivered twice;
//!   - a command naming a cell this session does not own is refused BEFORE any
//!     output is read, so a guessed identifier observes nothing.
//!
//! Every wait in this file goes through `park`, and every cell that reaches its
//! end goes through `CellSlot::close`. Keeping those two single makes the
//! subtle parts subtle in ONE place: subscribing under the same lock that made
//! the decision (or a wake-up is missed), and choosing `Terminated` over
//! `Finished` from `stopped` (or a forced cell reports as a clean one).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agent_core::sync::lock;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::protocol::{
    CellFailure, CellFailureKind, CellId, CellState, CodeModeError, DEFAULT_TERMINATE_GRACE,
    ExecuteRequest, MAX_ACTIVE_CELLS, MAX_CELL_OUTPUT_BYTES, OutputItem, RuntimeResponse,
    SessionId, ShutdownReport, WaitRequest,
};

/// Bounds one session applies whatever the request asks for.
#[derive(Debug, Clone, Copy)]
pub struct SessionLimits {
    pub terminate_grace: Duration,
    pub max_active_cells: usize,
    pub max_output_bytes: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            terminate_grace: DEFAULT_TERMINATE_GRACE,
            max_active_cells: MAX_ACTIVE_CELLS,
            max_output_bytes: MAX_CELL_OUTPUT_BYTES,
        }
    }
}

/// Runs cells for one session. Implementations report progress through the
/// `CellSink` they are handed, never by returning it.
pub trait CellEngine: Send + Sync {
    /// Starts `request.source` as `cell`. Returning `Err` means nothing was
    /// started: the session removes the cell and reports it as unavailable.
    fn start(&self, cell: CellId, request: &ExecuteRequest, sink: CellSink) -> Result<(), String>;

    /// Asks a cell to stop. Must be idempotent and must not block: the session
    /// is the one that waits, under its own grace.
    fn interrupt(&self, cell: &CellId);

    /// Releases the engine. `joined: false` says a worker was left behind.
    fn shutdown(&self, deadline: Duration) -> ShutdownReport;
}

/// Handle an engine writes a cell's progress through.
#[derive(Clone)]
pub struct CellSink {
    state: Weak<SessionState>,
    cell: CellId,
}

impl CellSink {
    pub fn cell_id(&self) -> &CellId {
        &self.cell
    }

    /// Appends one output item. Items beyond the result ceiling are dropped and
    /// counted, never silently lost.
    pub fn push(&self, item: OutputItem) {
        self.with_slot(|slot| {
            let weight = item.weight();
            // Once the ceiling is reached, the REST of this yield is dropped
            // too. Letting a later, smaller item slip in would leave a hole in
            // the middle of the stream the model reads, with nothing saying
            // where it is; `omitted_bytes` only says how much is missing. The
            // budget is restored by the next yield, with `take_items`.
            let overflows = slot.pending_bytes.saturating_add(weight) > slot.max_output_bytes;
            if slot.omitted_bytes > 0 || overflows {
                slot.omitted_bytes = slot.omitted_bytes.saturating_add(weight);
            } else {
                slot.pending_bytes += weight;
                slot.pending.push(item);
            }
            if slot.state == CellState::Yielded {
                slot.state = CellState::Running;
            }
        });
    }

    pub fn push_text(&self, text: impl Into<String>) {
        self.push(OutputItem::text(text));
    }

    /// `yield_control()` on the JavaScript side: hand back what is accumulated
    /// without ending the cell.
    pub fn request_yield(&self) {
        self.with_slot(|slot| slot.yield_requested = true);
    }

    /// Terminal state of the cell. The FIRST call wins: a late duplicate from a
    /// racing engine cannot reopen a cell nor overwrite its cause.
    pub fn finish(&self, failure: Option<CellFailure>) {
        self.with_slot(|slot| {
            if slot.state.is_terminal() {
                return;
            }
            slot.state = match failure {
                Some(_) => CellState::Failed,
                None => CellState::Completed,
            };
            slot.failure = failure;
        });
    }

    fn with_slot(&self, apply: impl FnOnce(&mut CellSlot)) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut cells = state.lock();
        let Some(slot) = cells.slots.get_mut(&self.cell) else {
            return;
        };
        apply(slot);
        slot.notify();
    }
}

struct CellSlot {
    state: CellState,
    pending: Vec<OutputItem>,
    pending_bytes: usize,
    omitted_bytes: usize,
    max_output_bytes: usize,
    failure: Option<CellFailure>,
    /// The cell was stopped from outside rather than reaching its own end.
    stopped: bool,
    yield_requested: bool,
    version: watch::Sender<u64>,
}

impl CellSlot {
    fn new(max_output_bytes: usize) -> Self {
        let (version, _) = watch::channel(0);
        Self {
            state: CellState::Running,
            pending: Vec::new(),
            pending_bytes: 0,
            omitted_bytes: 0,
            max_output_bytes,
            failure: None,
            stopped: false,
            yield_requested: false,
            version,
        }
    }

    /// Wakes every waiter of this cell. Called after ANY state change, which is
    /// what makes `terminate` and `shutdown` unable to leave a waiter parked.
    fn notify(&self) {
        self.version.send_modify(|version| *version += 1);
    }

    fn take_items(&mut self) -> (Vec<OutputItem>, usize) {
        self.pending_bytes = 0;
        (
            std::mem::take(&mut self.pending),
            std::mem::take(&mut self.omitted_bytes),
        )
    }

    /// The one terminal response of this cell.
    ///
    /// `Terminated` versus `Finished` is decided from `stopped` alone, and the
    /// cause the engine reported always wins over the session's own wording.
    /// `forced` is the message for a cell whose terminal state the session had
    /// to write itself because the engine never confirmed one.
    fn close(&mut self, cell: &CellId, forced: Option<String>) -> RuntimeResponse {
        let (items, omitted_bytes) = self.take_items();
        let failure = self.failure.take();
        if self.stopped || forced.is_some() {
            let failure = failure.unwrap_or_else(|| {
                CellFailure::interrupted(
                    forced.unwrap_or_else(|| "cell terminated on request".to_string()),
                )
            });
            return RuntimeResponse::Terminated {
                cell_id: cell.clone(),
                items,
                failure,
                omitted_bytes,
            };
        }
        RuntimeResponse::Finished {
            cell_id: cell.clone(),
            items,
            failure,
            omitted_bytes,
        }
    }
}

/// The open cells of a session, and the counter that names the next one.
///
/// The ordinal lives under the SAME lock as the map it indexes, so there is no
/// second synchronization mechanism to keep consistent, and it is monotonic for
/// the whole life of the session: a cell identifier is never reused, so it can
/// never be confused with a cell that has already been closed and drained.
#[derive(Default)]
struct CellTable {
    slots: HashMap<CellId, CellSlot>,
    next_ordinal: u64,
}

impl CellTable {
    fn open(&mut self, session: &SessionId, max_output_bytes: usize) -> CellId {
        let cell = CellId::new(session, self.next_ordinal);
        self.next_ordinal += 1;
        self.slots
            .insert(cell.clone(), CellSlot::new(max_output_bytes));
        cell
    }
}

struct SessionState {
    id: SessionId,
    limits: SessionLimits,
    engine: Arc<dyn CellEngine>,
    cells: Mutex<CellTable>,
    closed: AtomicBool,
}

impl SessionState {
    fn lock(&self) -> MutexGuard<'_, CellTable> {
        lock(&self.cells)
    }
}

/// A durable Code Mode session owned by one thread.
pub struct CodeModeSession {
    state: Arc<SessionState>,
}

impl CodeModeSession {
    pub fn new(id: SessionId, engine: Arc<dyn CellEngine>) -> Self {
        Self::with_limits(id, engine, SessionLimits::default())
    }

    pub fn with_limits(id: SessionId, engine: Arc<dyn CellEngine>, limits: SessionLimits) -> Self {
        Self {
            state: Arc::new(SessionState {
                id,
                limits,
                engine,
                cells: Mutex::new(CellTable::default()),
                closed: AtomicBool::new(false),
            }),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.state.id
    }

    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    /// State of one cell, for the surfaces that display it. `None` once the
    /// cell has been closed by its terminal response.
    pub fn cell_state(&self, cell: &CellId) -> Option<CellState> {
        self.state.lock().slots.get(cell).map(|slot| slot.state)
    }

    /// Every cell still open, sorted by identifier so the view is stable.
    pub fn cells(&self) -> Vec<(CellId, CellState)> {
        let cells = self.state.lock();
        let mut view: Vec<(CellId, CellState)> = cells
            .slots
            .iter()
            .map(|(cell, slot)| (cell.clone(), slot.state))
            .collect();
        view.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        view
    }

    /// Starts a cell and waits up to `request.yield_time` for it to finish.
    pub async fn execute(&self, request: ExecuteRequest) -> Result<RuntimeResponse, CodeModeError> {
        if self.is_closed() {
            return Err(CodeModeError::SessionClosed {
                session: self.state.id.clone(),
            });
        }
        let max_output_bytes = request
            .max_output_bytes
            .min(self.state.limits.max_output_bytes);
        let cell = {
            let mut cells = self.state.lock();
            if cells.slots.len() >= self.state.limits.max_active_cells {
                return Err(CodeModeError::TooManyCells {
                    session: self.state.id.clone(),
                    active: cells.slots.len(),
                    limit: self.state.limits.max_active_cells,
                });
            }
            cells.open(&self.state.id, max_output_bytes)
        };

        let sink = CellSink {
            state: Arc::downgrade(&self.state),
            cell: cell.clone(),
        };
        if let Err(detail) = self.state.engine.start(cell.clone(), &request, sink) {
            self.state.lock().slots.remove(&cell);
            return Err(CodeModeError::EngineUnavailable { detail });
        }

        self.collect(&cell, request.yield_time).await
    }

    /// Resumes a yielded cell, or stops it when `terminate` is set.
    pub async fn wait(&self, request: WaitRequest) -> Result<RuntimeResponse, CodeModeError> {
        self.check_owned(&request.cell_id)?;
        if request.terminate {
            return self.stop(&request.cell_id).await;
        }
        self.collect(&request.cell_id, request.yield_time).await
    }

    /// Stops one cell and closes it, forcing a terminal state if the engine
    /// does not confirm within the session grace.
    pub async fn terminate(&self, cell: &CellId) -> Result<RuntimeResponse, CodeModeError> {
        self.check_owned(cell)?;
        self.stop(cell).await
    }

    /// Closes the session: every open cell reaches a terminal state inside
    /// `deadline`, every waiter is woken, and the engine is released.
    ///
    pub async fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        self.state.closed.store(true, Ordering::Release);
        let open: Vec<CellId> = {
            let mut cells = self.state.lock();
            for slot in cells.slots.values_mut() {
                slot.stopped = true;
                slot.notify();
            }
            cells.slots.keys().cloned().collect()
        };
        for cell in &open {
            self.state.engine.interrupt(cell);
        }

        let until = Instant::now() + deadline;
        for cell in &open {
            self.await_terminal(cell, until).await;
        }

        let mut forced_cells = Vec::new();
        {
            let mut cells = self.state.lock();
            for (cell, slot) in cells.slots.iter_mut() {
                if slot.state.is_terminal() {
                    continue;
                }
                slot.state = CellState::Failed;
                slot.failure = Some(CellFailure::new(
                    CellFailureKind::Interrupted,
                    "session shut down before the cell finished",
                ));
                slot.notify();
                forced_cells.push(cell.clone());
            }
        }
        forced_cells.sort_by(|left, right| left.as_str().cmp(right.as_str()));

        let remaining = until.saturating_duration_since(Instant::now());
        let mut report = self.state.engine.shutdown(remaining);
        report.forced_cells = forced_cells;
        report
    }

    fn check_owned(&self, cell: &CellId) -> Result<(), CodeModeError> {
        if !cell.belongs_to(&self.state.id) {
            return Err(CodeModeError::ForeignCell {
                cell_id: cell.clone(),
                session: self.state.id.clone(),
            });
        }
        Ok(())
    }

    /// Stops an owned cell: mark, interrupt, wait under the grace, close.
    async fn stop(&self, cell: &CellId) -> Result<RuntimeResponse, CodeModeError> {
        {
            let mut cells = self.state.lock();
            let Some(slot) = cells.slots.get_mut(cell) else {
                return Err(CodeModeError::UnknownCell {
                    cell_id: cell.clone(),
                });
            };
            slot.stopped = true;
            slot.notify();
        }
        self.state.engine.interrupt(cell);
        let grace = self.state.limits.terminate_grace;
        self.await_terminal(cell, Instant::now() + grace).await;

        let mut cells = self.state.lock();
        let Some(slot) = cells.slots.get_mut(cell) else {
            return Err(CodeModeError::UnknownCell {
                cell_id: cell.clone(),
            });
        };
        let forced = (!slot.state.is_terminal()).then(|| {
            format!(
                "engine did not confirm termination within {} ms",
                grace.as_millis()
            )
        });
        let response = slot.close(cell, forced);
        cells.slots.remove(cell);
        Ok(response)
    }

    /// Waits for a cell to finish, to yield explicitly, or for the deadline.
    async fn collect(
        &self,
        cell: &CellId,
        yield_time: Duration,
    ) -> Result<RuntimeResponse, CodeModeError> {
        let until = Instant::now() + yield_time;
        self.park(cell, until, |cells, cell, expired| {
            let slot = cells.slots.get_mut(cell)?;
            if slot.state.is_terminal() {
                let response = slot.close(cell, None);
                // The cell is closed by its own terminal response: a later
                // `wait` on it is an unknown cell, never a second delivery.
                cells.slots.remove(cell);
                return Some(response);
            }
            if slot.yield_requested || expired {
                slot.yield_requested = false;
                slot.state = CellState::Yielded;
                let (items, omitted_bytes) = slot.take_items();
                return Some(RuntimeResponse::Yielded {
                    cell_id: cell.clone(),
                    items,
                    omitted_bytes,
                });
            }
            slot.state = CellState::Running;
            None
        })
        .await
        .ok_or_else(|| CodeModeError::UnknownCell {
            cell_id: cell.clone(),
        })
    }

    /// Parks until the cell is terminal or `until` passes. Never removes it.
    async fn await_terminal(&self, cell: &CellId, until: Instant) {
        self.park(cell, until, |cells, cell, _expired| {
            cells
                .slots
                .get(cell)
                .filter(|slot| slot.state.is_terminal())
                .map(|_| ())
        })
        .await;
    }

    /// Parks on `cell` until `decide` produces an answer or `until` passes.
    ///
    /// `decide` runs under the table lock, and the wake-up receiver is
    /// subscribed under that SAME lock: that is the whole reason this loop
    /// exists once instead of at each call site, since taking the two
    /// separately is how a notification gets missed. `decide` is called a last
    /// time with `expired`, so a caller can turn a deadline into its own
    /// answer. `None` comes back when the cell is gone, or when the deadline
    /// passed and `decide` still had nothing to say.
    async fn park<T>(
        &self,
        cell: &CellId,
        until: Instant,
        mut decide: impl FnMut(&mut CellTable, &CellId, bool) -> Option<T>,
    ) -> Option<T> {
        loop {
            let receiver = {
                let mut cells = self.state.lock();
                let expired = Instant::now() >= until;
                match decide(&mut cells, cell, expired) {
                    Some(answer) => return Some(answer),
                    None if expired => return None,
                    None => cells.slots.get(cell)?.version.subscribe(),
                }
            };
            let remaining = until.saturating_duration_since(Instant::now());
            let mut receiver = receiver;
            // Both arms loop back: a change re-runs `decide`, a timeout re-runs
            // it with `expired`. A closed sender (the slot was removed) is the
            // same as a change, and `decide` reports the cell as gone.
            let _ = tokio::time::timeout(remaining, receiver.changed()).await;
        }
    }
}

impl Drop for CodeModeSession {
    /// A dropped session must not leave a cell running behind it. This is a
    /// safety net, not the shutdown path: `shutdown` is what reports.
    fn drop(&mut self) {
        if self.state.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let open: Vec<CellId> = self.state.lock().slots.keys().cloned().collect();
        for cell in &open {
            self.state.engine.interrupt(cell);
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
