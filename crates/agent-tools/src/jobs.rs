//! Link between a tool and the background job registry of its thread (EP-042).
//!
//! The registry lives in `agent-runtime`, one per thread, and this crate holds
//! no thread. So the handle here is the same late binding
//! [`crate::agent::AgentHandle`] uses for the sub-agent supervisor: the binary
//! creates one, hands it to the tools, and rebinds it to the registry of the
//! thread it opens.
//!
//! An UNBOUND handle is not an error. A `ToolCtx` built outside a session, a
//! unit test, a tool exercised on its own: none of them has a thread, and a
//! terminal must still open in all three. What an unbound handle costs is the
//! accounting, not the behavior.
//!
//! The `list_jobs` tool at the bottom of this file is the other half (EP-043):
//! the same handle read from the model side. Without an argument it renders
//! what the thread is running; with a `job_id` it renders that job's result,
//! and marks it reported once the job is terminal, so the end-of-turn notice
//! stops naming a result the model already holds. `list_jobs` is the ONLY tool
//! that requires a bound handle: with none, there is no registry to read and
//! answering "no job" would be a lie.

use std::sync::{Arc, RwLock};

use agent_runtime::id::JobId;
use agent_runtime::jobs::{JobError, JobKind, JobOutput, JobRegistry, JobSnapshot};
use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ToolError;
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// The registry of the current thread, rebound each time a thread is opened.
#[derive(Default)]
pub struct JobHandle {
    current: RwLock<Option<Arc<JobRegistry>>>,
}

impl JobHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Points the handle at the registry of the thread being opened. The
    /// previous registry belongs to a thread that is closing; its jobs were
    /// already settled by its own teardown.
    pub fn bind(&self, registry: Arc<JobRegistry>) {
        if let Ok(mut current) = self.current.write() {
            *current = Some(registry);
        }
    }

    /// The registry, or `None` when nothing is bound.
    pub fn registry(&self) -> Option<Arc<JobRegistry>> {
        self.current.read().ok()?.clone()
    }
}

// ───────── list_jobs ─────────

/// Bytes of a command line the listing shows. A listing is an index: the model
/// asks for the job it recognizes, and the full command line it registered is
/// already in its own transcript.
const MAX_LISTED_COMMAND: usize = 160;

/// Marker the terminals use for what a cap dropped. Repeated here so a model
/// reads the same sentence whichever surface truncated its output.
fn omission_marker(omitted: u64) -> String {
    format!("... {omitted} bytes omitted ...\n")
}

/// Renders a command line for a model that must not be steered by it.
///
/// Two defenses, both required by US-138 AC4. Control characters are replaced
/// by a space, so a command carrying an escape sequence cannot repaint a
/// terminal or forge a line break in the middle of the listing; and the result
/// is cut on a CHAR boundary within a byte budget, so a multi-byte command
/// cannot produce invalid UTF-8 nor an unbounded line.
fn display_command(command: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for c in command.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if out.len() + c.len_utf8() > MAX_LISTED_COMMAND {
            truncated = true;
            break;
        }
        out.push(c);
    }
    if truncated {
        out.push_str("...");
    }
    out
}

/// The identity line of a job, in the shape `list_agents` uses.
///
/// `session` is the SECOND identifier of the same thing: the registry mints an
/// opaque `job_...`, and a terminal is also addressable by the shell session id
/// `write_stdin` takes. A model that has only one of the two would be unable to
/// reach the other, so the line carries both whenever the job is a terminal.
fn describe(job: &JobSnapshot, now_ms: u64) -> String {
    let session = match (job.kind, job.token) {
        (JobKind::Terminal, Some(token)) => format!(" session={token}"),
        _ => String::new(),
    };
    let ended = job.ended_at_ms.unwrap_or(now_ms);
    let elapsed = ended.saturating_sub(job.started_at_ms);
    let exit = job
        .exit_code
        .map(|code| format!(" exit_code={code}"))
        .unwrap_or_default();
    let cause = job
        .cause
        .as_deref()
        .map(|cause| format!(" cause={}", display_command(cause)))
        .unwrap_or_default();
    format!(
        "{} [{}] kind={}{session} elapsed={elapsed}ms reported={}{exit}{cause}\n  command: {}",
        job.job_id,
        job.status.as_str(),
        job.kind.as_str(),
        job.reported,
        display_command(&job.command),
    )
}

fn render_list(jobs: &[JobSnapshot], now_ms: u64) -> String {
    if jobs.is_empty() {
        return "no background job".to_string();
    }
    jobs.iter()
        .map(|job| describe(job, now_ms))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One job and everything it produced.
///
/// Non-UTF-8 bytes go through `from_utf8_lossy`, exactly like a terminal's own
/// result: an invalid sequence becomes U+FFFD and nothing panics (US-139 AC6).
/// A process that writes a binary artifact to stdout is a real case, and losing
/// the exit code because of it would be the worse failure.
fn render_result(job: &JobSnapshot, output: &JobOutput, now_ms: u64) -> String {
    let mut body = describe(job, now_ms);
    if job.status.is_active() {
        body.push_str("\n  still running: this is the output so far, not a final result");
    }
    body.push_str(&format!(
        "\n--- output ({} bytes) ---\n",
        output.bytes.len()
    ));
    if output.omitted > 0 {
        body.push_str(&omission_marker(output.omitted));
    }
    body.push_str(&String::from_utf8_lossy(&output.bytes));
    body
}

#[derive(Debug, Deserialize)]
pub struct ListJobsInput {
    /// One job's full result. Absent lists every job of the thread.
    #[serde(default)]
    pub job_id: Option<String>,
}

/// What the model sees of the background jobs of its thread (US-138, US-139).
///
/// One tool, two questions, because they are the same question at two zoom
/// levels: what is running, and what did THIS one produce. Splitting them would
/// spend a second slot of the model's tool budget on a listing that is already
/// the natural way to find the identifier the second call needs.
///
/// Listing has NO effect: nothing settles, nothing is marked, no slot moves. A
/// relève of a FINISHED job does mark it reported, because at that moment the
/// result has genuinely reached the model and owes it no second announcement
/// (US-140 AC1). A relève of a running job marks nothing: it is a poll, not a
/// result.
pub struct ListJobs {
    jobs: Arc<JobHandle>,
}

impl ListJobs {
    pub fn new(jobs: Arc<JobHandle>) -> Self {
        Self { jobs }
    }

    /// The registry, or the named refusal.
    ///
    /// An empty list would be a LIE here: "this thread runs nothing" and "this
    /// thread cannot account for anything" are different facts, and a model
    /// that read the first would stop looking (US-138 AC7).
    fn registry(&self) -> Result<Arc<JobRegistry>, ToolError> {
        self.jobs
            .registry()
            .ok_or_else(|| ToolError::Rejected(JobError::Detached.to_string()))
    }
}

#[async_trait]
impl Tool for ListJobs {
    type Input = ListJobsInput;

    fn name(&self) -> &str {
        "list_jobs"
    }

    fn description(&self) -> String {
        "List the background jobs of this conversation: job id, kind, shell session id, status, \
         elapsed time and command. Pass a job id to get that job's full output instead, which can \
         be read again and returns the same bytes. Reads only: it never starts, stops or consumes \
         anything."
            .to_string()
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": ["string", "null"],
                    "description": "Return this job's status and full output. Null lists every background job."
                }
            },
            "required": ["job_id"],
            "additionalProperties": false
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn is_sensitive(&self) -> bool {
        false
    }

    /// Always exempt, on the precedent of the empty poll of `write_stdin`.
    ///
    /// Waiting on a background job IS repeating this call: the tool answers
    /// with the state of processes it does not drive, so two identical calls
    /// returning the same thing means the world has not moved, not that the
    /// model is stuck. Nothing here starts, stops or consumes anything, so the
    /// repetition costs a tool result and no side effect.
    fn loop_guard_exempt(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn call(&self, input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let registry = self.registry()?;
        let now_ms = registry.now_ms();
        let Some(raw) = input
            .job_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolOutput::text(render_list(&registry.snapshots(), now_ms)));
        };
        let job_id = JobId::parse(raw).map_err(|_| {
            ToolError::Rejected(format!(
                "`job_id` is not a job identifier: expected the `{}` form list_jobs returns",
                JobId::PREFIX
            ))
        })?;
        // A job of another thread is not in this registry, so it reads as
        // absent. "Unknown" and "forbidden" would be two answers a probe could
        // tell apart, and the model has nothing to do with the difference.
        let (job, output) = registry
            .read_output(job_id)
            .await
            .map_err(|err| ToolError::Rejected(err.to_string()))?;
        let body = render_result(&job, &output, now_ms);
        // Marked AFTER the result is built and only for a job that is over: the
        // bytes above are the whole result, so nothing is owed a second
        // announcement. A running job is a poll and stays unreported.
        if job.status.is_terminal()
            && let Err(err) = registry.mark_reported(job_id).await
        {
            tracing::debug!(
                target: "pyxis::tools",
                job_id = %job_id,
                error = %err,
                "background job result was delivered but not marked reported"
            );
        }
        Ok(ToolOutput::text(body))
    }
}
