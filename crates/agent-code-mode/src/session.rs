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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

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
        let Some(slot) = cells.get_mut(&self.cell) else {
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
}

struct SessionState {
    id: SessionId,
    limits: SessionLimits,
    engine: Arc<dyn CellEngine>,
    cells: Mutex<HashMap<CellId, CellSlot>>,
    ordinal: AtomicU64,
    closed: AtomicBool,
}

impl SessionState {
    /// A poisoned lock is recovered rather than propagated: losing the cell map
    /// would strand every running cell, which is worse than reading a map a
    /// panicking thread left consistent (nothing is written across an unwind).
    fn lock(&self) -> MutexGuard<'_, HashMap<CellId, CellSlot>> {
        match self.cells.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
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
                cells: Mutex::new(HashMap::new()),
                ordinal: AtomicU64::new(0),
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
        self.state.lock().get(cell).map(|slot| slot.state)
    }

    /// Every cell still open, sorted by identifier so the view is stable.
    pub fn cells(&self) -> Vec<(CellId, CellState)> {
        let cells = self.state.lock();
        let mut view: Vec<(CellId, CellState)> = cells
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
            if cells.len() >= self.state.limits.max_active_cells {
                return Err(CodeModeError::TooManyCells {
                    session: self.state.id.clone(),
                    active: cells.len(),
                    limit: self.state.limits.max_active_cells,
                });
            }
            let ordinal = self.state.ordinal.fetch_add(1, Ordering::AcqRel);
            let cell = CellId::new(&self.state.id, ordinal);
            // The counter cannot collide on its own; the check is what makes a
            // reused identifier impossible even if one ever were replayed.
            if cells.contains_key(&cell) {
                return Err(CodeModeError::DuplicateCell {
                    cell_id: cell,
                    session: self.state.id.clone(),
                });
            }
            cells.insert(cell.clone(), CellSlot::new(max_output_bytes));
            cell
        };

        let sink = CellSink {
            state: Arc::downgrade(&self.state),
            cell: cell.clone(),
        };
        if let Err(detail) = self.state.engine.start(cell.clone(), &request, sink) {
            self.state.lock().remove(&cell);
            return Err(CodeModeError::EngineUnavailable { detail });
        }

        self.collect(&cell, request.yield_time).await
    }

    /// Resumes a yielded cell, or stops it when `terminate` is set.
    pub async fn wait(&self, request: WaitRequest) -> Result<RuntimeResponse, CodeModeError> {
        self.check_owned(&request.cell_id)?;
        if request.terminate {
            return self.terminate(&request.cell_id).await;
        }
        self.collect(&request.cell_id, request.yield_time).await
    }

    /// Stops one cell and closes it, forcing a terminal state if the engine
    /// does not confirm within the session grace.
    pub async fn terminate(&self, cell: &CellId) -> Result<RuntimeResponse, CodeModeError> {
        self.check_owned(cell)?;
        {
            let mut cells = self.state.lock();
            let Some(slot) = cells.get_mut(cell) else {
                return Err(CodeModeError::UnknownCell {
                    cell_id: cell.clone(),
                });
            };
            slot.stopped = true;
            slot.notify();
        }
        self.state.engine.interrupt(cell);
        self.await_terminal(cell, self.state.limits.terminate_grace)
            .await;
        let mut cells = self.state.lock();
        let Some(mut slot) = cells.remove(cell) else {
            return Err(CodeModeError::UnknownCell {
                cell_id: cell.clone(),
            });
        };
        let forced = !slot.state.is_terminal();
        let (items, omitted_bytes) = slot.take_items();
        let failure = slot.failure.take().unwrap_or_else(|| {
            if forced {
                CellFailure::interrupted(format!(
                    "engine did not confirm termination within {} ms",
                    self.state.limits.terminate_grace.as_millis()
                ))
            } else {
                CellFailure::interrupted("cell terminated on request")
            }
        });
        Ok(RuntimeResponse::Terminated {
            cell_id: cell.clone(),
            items,
            failure,
            omitted_bytes,
        })
    }

    /// Closes the session: every open cell reaches a terminal state inside
    /// `deadline`, every waiter is woken, and the engine is released.
    pub async fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        self.state.closed.store(true, Ordering::Release);
        let open: Vec<CellId> = self.state.lock().keys().cloned().collect();
        for cell in &open {
            if let Some(slot) = self.state.lock().get_mut(cell) {
                slot.stopped = true;
                slot.notify();
            }
            self.state.engine.interrupt(cell);
        }

        let until = Instant::now() + deadline;
        for cell in &open {
            let remaining = until.saturating_duration_since(Instant::now());
            self.await_terminal(cell, remaining).await;
        }

        let mut forced_cells = Vec::new();
        {
            let mut cells = self.state.lock();
            for (cell, slot) in cells.iter_mut() {
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

    /// Waits for a cell to finish, to yield explicitly, or for the deadline.
    async fn collect(
        &self,
        cell: &CellId,
        yield_time: Duration,
    ) -> Result<RuntimeResponse, CodeModeError> {
        let until = Instant::now() + yield_time;
        loop {
            let receiver = {
                let mut cells = self.state.lock();
                let Some(slot) = cells.get_mut(cell) else {
                    return Err(CodeModeError::UnknownCell {
                        cell_id: cell.clone(),
                    });
                };
                if slot.state.is_terminal() {
                    let stopped = slot.stopped;
                    let (items, omitted_bytes) = slot.take_items();
                    let failure = slot.failure.clone();
                    // The cell is closed by its own terminal response: a later
                    // `wait` on it is an unknown cell, never a second delivery.
                    cells.remove(cell);
                    return Ok(if stopped {
                        RuntimeResponse::Terminated {
                            cell_id: cell.clone(),
                            items,
                            failure: failure.unwrap_or_else(|| {
                                CellFailure::interrupted("cell terminated on request")
                            }),
                            omitted_bytes,
                        }
                    } else {
                        RuntimeResponse::Finished {
                            cell_id: cell.clone(),
                            items,
                            failure,
                            omitted_bytes,
                        }
                    });
                }
                if slot.yield_requested || Instant::now() >= until {
                    slot.yield_requested = false;
                    slot.state = CellState::Yielded;
                    let (items, omitted_bytes) = slot.take_items();
                    return Ok(RuntimeResponse::Yielded {
                        cell_id: cell.clone(),
                        items,
                        omitted_bytes,
                    });
                }
                slot.state = CellState::Running;
                slot.version.subscribe()
            };
            let remaining = until.saturating_duration_since(Instant::now());
            let mut receiver = receiver;
            let _ = tokio::time::timeout(remaining, receiver.changed()).await;
        }
    }

    /// Parks until the cell is terminal or `grace` elapses. Never removes it.
    async fn await_terminal(&self, cell: &CellId, grace: Duration) {
        let until = Instant::now() + grace;
        loop {
            let receiver = {
                let cells = self.state.lock();
                let Some(slot) = cells.get(cell) else {
                    return;
                };
                if slot.state.is_terminal() || Instant::now() >= until {
                    return;
                }
                slot.version.subscribe()
            };
            let remaining = until.saturating_duration_since(Instant::now());
            let mut receiver = receiver;
            if tokio::time::timeout(remaining, receiver.changed())
                .await
                .is_err()
            {
                return;
            }
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
        let open: Vec<CellId> = self.state.lock().keys().cloned().collect();
        for cell in &open {
            self.state.engine.interrupt(cell);
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
