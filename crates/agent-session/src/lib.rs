//! `agent-session`: append-only JSONL persistence + per-directory resume (US-009,
//! ARCHITECTURE 7). Implements the `Session` trait of `agent-core` (injected into
//! the loop). Depends on `agent-core` for the canonical types.
//!
//! Guarantees:
//! - **per-entry durability**: every entry is serialized then `write_all` +
//!   `flush` + `sync_data` (fdatasync). Note: `write_all` can emit several
//!   `write()` syscalls; an OS crash in the middle can leave a PARTIAL line
//!   at the tail of the file, which is exactly what resume detects and ignores
//!   (last unparsable line, AC3). So we do not promise "all or nothing"
//!   at the byte level, but "any incomplete trailing line is ignored".
//! - **resume**: we replay the log; a last line truncated by a crash mid-write
//!   is ignored (AC3), the session resumes at the last valid state.
//! - a recent `CompactCheckpoint` resets the transcript into a single
//!   replayable entry. Older `CompactBoundary` logs stay supported:
//!   their `clear` is deferred until the first following `Message`, so an
//!   orphan boundary does not wipe the earlier transcript.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use agent_core::CompactKind;
use agent_core::compaction::is_summary_message;
use agent_core::message::{ContentBlock, Message, Role};
use agent_core::session::{FileSnapshot, Session, SessionEntry, SessionError};
use agent_runtime::event::{ForkOrigin, THREAD_EVENT_ENTRY, THREAD_META_ENTRY, ThreadEvent};
use agent_runtime::id::ThreadId;
use serde::{Deserialize, Serialize};

pub mod thread_store;

pub use thread_store::{JsonlThreadStore, branch_path};

/// Name of the session file in a working directory.
pub const SESSION_FILE: &str = "session.jsonl";
pub const SESSION_SCHEMA_VERSION: u32 = 1;

fn io_err(e: impl std::fmt::Display) -> SessionError {
    SessionError::Io(e.to_string())
}
fn serde_err(e: impl std::fmt::Display) -> SessionError {
    SessionError::Serde(e.to_string())
}

fn invalid_session(e: impl std::fmt::Display) -> SessionError {
    SessionError::Serde(e.to_string())
}

/// Lines the thread runtime adds to the SAME file as the v1 transcript (EP-001).
///
/// Additive by construction: a v1 reader deserializes them into
/// `SessionEntry::Unknown` and skips them, which is why `SESSION_SCHEMA_VERSION`
/// is deliberately NOT bumped. Bumping it would make every new file unreadable
/// by an older binary, which rejects `schema_version > 1` outright.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub(crate) enum ThreadLine {
    /// Binds the log to a thread. Written once at creation, and once more at
    /// the tail of a materialized branch: the LAST binding of a file is the one
    /// that owns it, which is what lets a fork copy a prefix verbatim instead of
    /// rewriting the bytes it inherited (US-010).
    ThreadMeta {
        thread_id: ThreadId,
        runtime_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<ForkOrigin>,
    },
    ThreadEvent(ThreadEvent),
}

pub(crate) enum LogLine {
    Session(SessionEntry),
    Thread(ThreadLine),
}

/// Reads one JSONL line in either format. The `entry` tag decides, so a v1
/// entry never goes through the runtime types and vice versa.
pub(crate) fn parse_log_line(line: &str) -> Result<LogLine, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    let is_thread_line = value
        .get("entry")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|tag| tag == THREAD_META_ENTRY || tag == THREAD_EVENT_ENTRY);
    if is_thread_line {
        serde_json::from_value::<ThreadLine>(value).map(LogLine::Thread)
    } else {
        serde_json::from_value::<SessionEntry>(value).map(LogLine::Session)
    }
}

pub(crate) fn validate_checkpoint(messages: &[Message]) -> Result<(), SessionError> {
    if messages.is_empty() {
        return Err(invalid_session("empty compaction checkpoint"));
    }
    if !is_summary_message(&messages[0]) {
        return Err(invalid_session(
            "compaction checkpoint without a summary as the first message",
        ));
    }
    for (i, message) in messages.iter().enumerate() {
        message
            .validate()
            .map_err(|e| invalid_session(format!("invalid checkpoint message {i}: {e}")))?;
    }
    Ok(())
}

pub(crate) fn redact_encrypted_reasoning(messages: &mut [Message]) {
    for message in messages {
        message
            .content
            .retain(|block| !matches!(block, ContentBlock::EncryptedReasoning { .. }));
    }
}

/// Append-only JSONL session. Keeps a cursor of the number of already written messages
/// so that `sync` only writes the delta (idempotent transcript-before-response).
pub struct JsonlSession {
    state: Mutex<WriterState>,
}

pub(crate) struct WriterState {
    file: File,
    cursor: usize,
    poisoned: bool,
}

impl WriterState {
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Forces data AND metadata out. Used before copying a durable prefix.
    pub(crate) fn sync_all(&mut self) -> Result<(), SessionError> {
        self.file.sync_all().map_err(io_err)
    }
}

/// Opens (creating when needed) a session file, truncates a partial trailing
/// line and returns the writer, whether the file was empty and the state
/// replayed from it.
pub(crate) fn open_prepared(
    path: &Path,
) -> Result<(WriterState, bool, ResumedSession), SessionError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_err)?;
    file.try_lock()
        .map_err(|e| io_err(format!("session already open or lock unavailable: {e}")))?;

    let mut resumed = resume_locked_file(&mut file)?;
    if resumed.skipped_partial {
        file.set_len(resumed.valid_bytes).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        resumed = resume_locked_file(&mut file)?;
    }
    if resumed.valid_bytes > 0 && !resumed.ended_with_newline {
        file.seek(SeekFrom::End(0)).map_err(io_err)?;
        file.write_all(b"\n").map_err(io_err)?;
        file.flush().map_err(io_err)?;
        file.sync_data().map_err(io_err)?;
        resumed.valid_bytes += 1;
        resumed.ended_with_newline = true;
    }

    let is_empty = std::fs::metadata(path)
        .map(|m| m.len() == 0)
        .unwrap_or(true);
    let cursor = resumed.messages.len();
    Ok((
        WriterState {
            file,
            cursor,
            poisoned: false,
        },
        is_empty,
        resumed,
    ))
}

fn resume_locked_file(file: &mut File) -> Result<ResumedSession, SessionError> {
    let mut content = String::new();
    file.seek(SeekFrom::Start(0)).map_err(io_err)?;
    file.read_to_string(&mut content).map_err(io_err)?;
    file.seek(SeekFrom::End(0)).map_err(io_err)?;
    resume_content(&content)
}

pub(crate) fn write_buf_locked(state: &mut WriterState, buf: &str) -> Result<(), SessionError> {
    if state.poisoned {
        return Err(SessionError::Io(
            "session writer poisoned after an append error; reopen the session".into(),
        ));
    }
    if let Err(e) = state.file.seek(SeekFrom::End(0)) {
        state.poisoned = true;
        return Err(io_err(e));
    }
    if let Err(e) = state.file.write_all(buf.as_bytes()) {
        state.poisoned = true;
        return Err(io_err(e));
    }
    if let Err(e) = state.file.flush() {
        state.poisoned = true;
        return Err(io_err(e));
    }
    if let Err(e) = state.file.sync_data() {
        state.poisoned = true;
        return Err(io_err(e));
    }
    Ok(())
}

pub(crate) fn write_entry_locked(
    state: &mut WriterState,
    entry: &SessionEntry,
) -> Result<(), SessionError> {
    let line = format!("{}\n", serde_json::to_string(entry).map_err(serde_err)?);
    write_buf_locked(state, &line)
}

impl JsonlSession {
    /// Creates (or reopens in append mode) the session file in `dir`.
    pub fn create_in(dir: &Path) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let path = dir.join(SESSION_FILE);
        let (state, is_empty, _) = open_prepared(&path)?;
        let session = Self {
            state: Mutex::new(state),
        };
        if is_empty {
            session.append(&SessionEntry::Meta {
                schema_version: SESSION_SCHEMA_VERSION,
            })?;
        }
        Ok(session)
    }

    /// Creates (or reopens in append mode) a session on a named file (one file
    /// per conversation: `<dir>/<id>.jsonl`). Creates the parent directory when needed.
    pub fn create_at(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let (state, is_empty, _) = open_prepared(path)?;
        let session = Self {
            state: Mutex::new(state),
        };
        if is_empty {
            session.append(&SessionEntry::Meta {
                schema_version: SESSION_SCHEMA_VERSION,
            })?;
        }
        Ok(session)
    }

    /// Switches the persistence file to `path` (resuming a past
    /// session) by repositioning the cursor at `cursor` already written messages: the
    /// next `sync` calls will only append the delta into the resumed session.
    pub fn switch_to(&self, path: &Path, cursor: usize) -> Result<(), SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let (state, is_empty, _) = open_prepared(path)?;
        if state.cursor != cursor {
            return Err(invalid_session(format!(
                "incoherent resume cursor: expected {}, got {cursor}",
                state.cursor
            )));
        }
        *self
            .state
            .lock()
            .map_err(|_| SessionError::Io("poisoned session lock".into()))? = state;
        if is_empty {
            self.append(&SessionEntry::Meta {
                schema_version: SESSION_SCHEMA_VERSION,
            })?;
        }
        Ok(())
    }

    fn append(&self, entry: &SessionEntry) -> Result<(), SessionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SessionError::Io("poisoned session lock".into()))?;
        write_entry_locked(&mut state, entry)
    }
}

#[async_trait::async_trait]
impl Session for JsonlSession {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SessionError::Io("poisoned session lock".into()))?;
        let start = state.cursor.min(messages.len());
        for (offset, m) in messages[start..].iter().enumerate() {
            let mut persisted = m.clone();
            redact_encrypted_reasoning(std::slice::from_mut(&mut persisted));
            write_entry_locked(&mut state, &SessionEntry::Message(persisted))?;
            state.cursor = start + offset + 1;
        }
        Ok(())
    }

    async fn checkpoint(
        &self,
        kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        validate_checkpoint(messages)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SessionError::Io("poisoned session lock".into()))?;
        let mut persisted = messages.to_vec();
        redact_encrypted_reasoning(&mut persisted);
        write_entry_locked(
            &mut state,
            &SessionEntry::CompactCheckpoint {
                kind,
                messages: persisted,
            },
        )?;
        state.cursor = messages.len();
        Ok(())
    }

    async fn redact_encrypted_reasoning(&self) -> Result<(), SessionError> {
        self.append(&SessionEntry::EncryptedReasoningRedacted)
    }

    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError> {
        self.append(&SessionEntry::FileHistorySnapshot(snapshot))
    }
}

/// State rebuilt from a session log.
#[derive(Debug, Default)]
pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub file_snapshots: Vec<FileSnapshot>,
    pub compactions: usize,
    /// True when a partial last line (crash mid-write) was ignored.
    pub skipped_partial: bool,
    /// Schema version declared by the first `Meta` entry, when present.
    pub schema_version: Option<u32>,
    /// Number of bytes replayed as a valid log. Used to truncate a partial
    /// tail before appending into the file again.
    pub valid_bytes: u64,
    /// True when the read file ended with `\n`.
    pub ended_with_newline: bool,
    /// Thread this log is bound to (EP-001). `None` for a v1 session that has
    /// never been opened by the runtime.
    pub thread_id: Option<ThreadId>,
    /// Branch this log was cut from (US-010). Read from the SAME binding line as
    /// `thread_id`, so a copied prefix never lends its provenance to the branch
    /// that inherited it.
    pub origin: Option<ForkOrigin>,
    /// Orchestration events replayed in file order (EP-001).
    pub thread_events: Vec<ThreadEvent>,
}

/// Resumes the session of a directory (`<dir>/session.jsonl`).
pub fn resume_dir(dir: &Path) -> Result<ResumedSession, SessionError> {
    resume_file(&dir.join(SESSION_FILE))
}

/// Resumes a session from a JSONL file. Missing file means an empty session.
pub fn resume_file(path: &Path) -> Result<ResumedSession, SessionError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ResumedSession::default());
        }
        Err(e) => return Err(io_err(e)),
    };
    resume_content(&content)
}

fn apply_entry(
    out: &mut ResumedSession,
    pending_clear: &mut bool,
    entry: SessionEntry,
) -> Result<(), SessionError> {
    match entry {
        SessionEntry::Message(m) => {
            if *pending_clear {
                out.messages.clear();
                *pending_clear = false;
            }
            out.messages.push(m);
        }
        SessionEntry::CompactBoundary { .. } => {
            *pending_clear = true;
            out.compactions += 1;
        }
        SessionEntry::CompactCheckpoint { messages, .. } => {
            validate_checkpoint(&messages)?;
            out.messages = messages;
            *pending_clear = false;
            out.compactions += 1;
        }
        SessionEntry::EncryptedReasoningRedacted => {
            redact_encrypted_reasoning(&mut out.messages);
        }
        SessionEntry::FileHistorySnapshot(snapshot) => {
            out.file_snapshots.push(snapshot);
        }
        SessionEntry::Meta { schema_version } => {
            if schema_version > SESSION_SCHEMA_VERSION {
                return Err(invalid_session(format!(
                    "unsupported schema_version {schema_version} (max {SESSION_SCHEMA_VERSION})"
                )));
            }
            out.schema_version = Some(schema_version);
        }
        SessionEntry::Unknown => {}
    }
    Ok(())
}

fn resume_content(content: &str) -> Result<ResumedSession, SessionError> {
    let mut out = ResumedSession {
        ended_with_newline: content.ends_with('\n'),
        ..ResumedSession::default()
    };

    // DEFERRED clear: a boundary only wipes the earlier transcript when
    // its first summary Message arrives. An orphan boundary (crash between
    // boundary and summary) therefore preserves the transcript from before.
    let mut pending_clear = false;
    let mut start = 0usize;

    while start < content.len() {
        let Some(rel_end) = content[start..].find('\n') else {
            let line = &content[start..];
            replay_line(content, &mut out, &mut pending_clear, start, line, false)?;
            break;
        };
        let end = start + rel_end;
        let line = &content[start..end];
        replay_line(content, &mut out, &mut pending_clear, start, line, true)?;
        start = end + 1;
    }
    Ok(out)
}

fn replay_line(
    content: &str,
    out: &mut ResumedSession,
    pending_clear: &mut bool,
    start: usize,
    line: &str,
    has_newline: bool,
) -> Result<(), SessionError> {
    let raw_len = line.len();
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.trim().is_empty() {
        out.valid_bytes = if has_newline {
            (start + raw_len + 1) as u64
        } else {
            content.len() as u64
        };
        return Ok(());
    }

    match parse_log_line(line) {
        Ok(parsed) => {
            match parsed {
                LogLine::Session(entry) => apply_entry(out, pending_clear, entry)?,
                LogLine::Thread(ThreadLine::ThreadMeta {
                    thread_id, origin, ..
                }) => {
                    out.thread_id = Some(thread_id);
                    out.origin = origin;
                }
                LogLine::Thread(ThreadLine::ThreadEvent(event)) => out.thread_events.push(event),
            }
            out.valid_bytes = if has_newline {
                (start + raw_len + 1) as u64
            } else {
                content.len() as u64
            };
            Ok(())
        }
        Err(e) => {
            if !has_newline {
                out.skipped_partial = true;
                out.valid_bytes = start as u64;
                Ok(())
            } else {
                Err(SessionError::Corrupt {
                    offset: start as u64,
                    detail: e.to_string(),
                })
            }
        }
    }
}

fn scan_session(path: &Path) -> Result<SessionScan, SessionError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SessionScan::default()),
        Err(e) => return Err(io_err(e)),
    };
    let mut scan = SessionScan::default();
    let mut pending_clear = false;
    let mut line = String::new();
    let mut reader = BufReader::new(file);
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).map_err(io_err)?;
        if bytes == 0 {
            break;
        }
        let has_newline = line.ends_with('\n');
        let parsed = line.trim_end_matches(['\r', '\n']);
        if parsed.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEntry>(parsed) {
            // A binding line is `Unknown` to a v1 reader, and the listing is
            // exactly where it matters: it is the only place a branch declares
            // its parent (US-011). Re-parsed here rather than in the hot path,
            // so a session that carries none pays nothing.
            Ok(SessionEntry::Unknown) => {
                if let Ok(ThreadLine::ThreadMeta {
                    thread_id, origin, ..
                }) = serde_json::from_str::<ThreadLine>(parsed)
                {
                    // Last binding wins, as at resume: a materialized branch
                    // inherits the binding of its source before declaring its
                    // own.
                    scan.thread_id = Some(thread_id);
                    scan.origin = origin;
                }
            }
            Ok(entry) => apply_scan_entry(&mut scan, &mut pending_clear, entry)?,
            Err(_) if !has_newline => break,
            Err(e) => return Err(SessionError::Serde(format!("corrupt line: {e}"))),
        }
    }
    Ok(scan)
}

#[derive(Default)]
struct SessionScan {
    message_count: usize,
    summary: Option<String>,
    prompts: Vec<String>,
    thread_id: Option<ThreadId>,
    origin: Option<ForkOrigin>,
}

fn apply_scan_entry(
    scan: &mut SessionScan,
    pending_clear: &mut bool,
    entry: SessionEntry,
) -> Result<(), SessionError> {
    match entry {
        SessionEntry::Message(m) => {
            if *pending_clear {
                scan.message_count = 0;
                scan.summary = None;
                scan.prompts.clear();
                *pending_clear = false;
            }
            scan_push_message(scan, &m);
        }
        SessionEntry::CompactBoundary { .. } => {
            *pending_clear = true;
        }
        SessionEntry::CompactCheckpoint { messages, .. } => {
            validate_checkpoint(&messages)?;
            scan.message_count = 0;
            scan.summary = None;
            scan.prompts.clear();
            *pending_clear = false;
            for message in &messages {
                scan_push_message(scan, message);
            }
        }
        SessionEntry::Meta { schema_version } => {
            if schema_version > SESSION_SCHEMA_VERSION {
                return Err(invalid_session(format!(
                    "unsupported schema_version {schema_version} (max {SESSION_SCHEMA_VERSION})"
                )));
            }
        }
        SessionEntry::EncryptedReasoningRedacted
        | SessionEntry::FileHistorySnapshot(_)
        | SessionEntry::Unknown => {}
    }
    Ok(())
}

fn scan_push_message(scan: &mut SessionScan, message: &Message) {
    scan.message_count += 1;
    if message.role == Role::User {
        let text = message.text();
        if scan.summary.is_none() {
            scan.summary = Some(text.clone());
        }
        if !text.trim().is_empty() {
            scan.prompts.push(text);
        }
    }
}

/// Metadata of a session listed for the `/resume` menu.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// File name (`<id>.jsonl`): identifier resolved on the CLI side.
    pub id: String,
    /// Summary: first user message (empty when there is none).
    pub summary: String,
    pub message_count: usize,
    pub modified: SystemTime,
    /// Thread this session is bound to. `None` for a v1 session the runtime
    /// never opened.
    pub thread_id: Option<ThreadId>,
    /// Branch this session was cut from (US-011). Present makes it a branch,
    /// and names both its parent and the turn it was cut at, without opening
    /// the thread.
    pub origin: Option<ForkOrigin>,
}

impl SessionInfo {
    /// True when this session is a branch of another one.
    pub fn is_branch(&self) -> bool {
        self.origin.is_some()
    }
}

/// Lists the resumable sessions of a directory (`*.jsonl`), sorted from the most
/// recent to the oldest. Ignores empty sessions and the one in `exclude` (the
/// current session). Tolerant: an unreadable file is simply skipped.
pub fn list_sessions(dir: &Path, exclude: Option<&Path>) -> Vec<SessionInfo> {
    let exclude = exclude.and_then(|p| p.canonicalize().ok());
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(ex) = &exclude
            && path.canonicalize().ok().as_ref() == Some(ex)
        {
            continue;
        }
        let Ok(scan) = scan_session(&path) else {
            continue;
        };
        if scan.message_count == 0 {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(SessionInfo {
            id: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            summary: scan.summary.unwrap_or_default(),
            message_count: scan.message_count,
            modified,
            thread_id: scan.thread_id,
            origin: scan.origin,
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Aggregates the user prompts of ALL the sessions of a directory (oldest ->
/// most recent), for the **per-directory** navigable history (Claude Code style).
/// Excludes `exclude` (the current session, still empty), dedupes the duplicates
/// already seen and keeps at most `cap` entries (the most recent ones). The sessions
/// are ordered by modification date (roughly chronological).
pub fn workspace_prompts(dir: &Path, exclude: Option<&Path>, cap: usize) -> Vec<String> {
    let exclude = exclude.and_then(|p| p.canonicalize().ok());
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(ex) = &exclude
            && path.canonicalize().ok().as_ref() == Some(ex)
        {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((mtime, path));
    }
    files.sort_by(|(a_time, a_path), (b_time, b_path)| {
        a_time.cmp(b_time).then_with(|| a_path.cmp(b_path))
    }); // oldest -> most recent

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (_, path) in files {
        let Ok(scan) = scan_session(&path) else {
            continue;
        };
        for text in scan.prompts {
            if seen.remove(&text) {
                out.retain(|p| p != &text);
            }
            seen.insert(text.clone());
            out.push(text);
        }
    }
    if out.len() > cap {
        out.drain(0..out.len() - cap);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::message::Message;

    /// Temporary directory isolated per test (no `rand`/`tempfile` dependency).
    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis_sess_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn summary(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Summary {
                text: text.into(),
                source_untrusted: false,
            }],
        }
    }

    #[tokio::test]
    async fn write_then_resume_roundtrip() {
        let dir = tmp("roundtrip");
        let s = JsonlSession::create_in(&dir).unwrap();
        let msgs = vec![Message::user("salut"), Message::assistant_text("bonjour")];
        s.sync(&msgs).await.unwrap();
        // idempotent re-sync: adds nothing
        s.sync(&msgs).await.unwrap();
        drop(s);

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.messages[0].text(), "salut");
        assert_eq!(resumed.messages[1].text(), "bonjour");
        assert!(!resumed.skipped_partial);
        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(raw.lines().next().unwrap().contains("\"entry\":\"meta\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_resets_transcript() {
        let dir = tmp("compact");
        let s = JsonlSession::create_in(&dir).unwrap();
        s.sync(&[Message::user("vieux 1"), Message::assistant_text("vieux 2")])
            .await
            .unwrap();
        s.checkpoint(CompactKind::Auto, &[summary("[summary]")])
            .await
            .unwrap();
        drop(s);

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.compactions, 1);
        assert_eq!(resumed.messages.len(), 1, "old messages are compacted");
        assert_eq!(resumed.messages[0].text(), "[summary]");
        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(
            raw.contains("\"entry\":\"compact_checkpoint\""),
            "new checkpoints as one entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sync_does_not_persist_encrypted_reasoning() {
        let dir = tmp("sync-redact-reasoning");
        let s = JsonlSession::create_in(&dir).unwrap();
        let assistant = Message::assistant(vec![
            ContentBlock::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC_SECRET".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
        ]);
        s.sync(&[assistant]).await.unwrap();
        drop(s);

        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(!raw.contains("ENC_SECRET"));
        let resumed = resume_dir(&dir).unwrap();
        assert!(
            resumed.messages[0]
                .content
                .iter()
                .all(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }))
        );
        assert!(
            resumed.messages[0]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_does_not_persist_encrypted_reasoning() {
        let dir = tmp("checkpoint-redact-reasoning");
        let s = JsonlSession::create_in(&dir).unwrap();
        let assistant = Message::assistant(vec![
            ContentBlock::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC_SECRET".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
        ]);
        s.checkpoint(CompactKind::Auto, &[summary("[summary]"), assistant])
            .await
            .unwrap();
        drop(s);

        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(!raw.contains("ENC_SECRET"));
        let resumed = resume_dir(&dir).unwrap();
        assert!(
            resumed
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .all(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }))
        );
        assert!(
            resumed
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_rejects_missing_summary() {
        let dir = tmp("bad-checkpoint");
        let s = JsonlSession::create_in(&dir).unwrap();
        s.sync(&[Message::user("avant")]).await.unwrap();
        let err = s
            .checkpoint(
                CompactKind::Auto,
                &[Message::assistant_text("not a summary")],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Serde(_)));
        drop(s);

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.messages[0].text(), "avant");
        assert_eq!(resumed.compactions, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_compact_checkpoint_preserves_prior_transcript() {
        let dir = tmp("truncated-checkpoint");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        let old = serde_json::to_string(&SessionEntry::Message(Message::user("avant"))).unwrap();
        let checkpoint = serde_json::to_string(&SessionEntry::CompactCheckpoint {
            kind: CompactKind::Auto,
            messages: vec![summary("resume"), Message::user("courant")],
        })
        .unwrap();
        let cut = checkpoint.len() / 2;
        std::fs::write(&path, format!("{old}\n{}", &checkpoint[..cut])).unwrap();

        let resumed = resume_file(&path).unwrap();
        assert!(resumed.skipped_partial);
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.messages[0].text(), "avant");
        assert_eq!(resumed.compactions, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #9: an ORPHAN boundary (crash between boundary and summary) must NOT
    // wipe the earlier transcript (deferred clear).
    #[test]
    fn dangling_boundary_preserves_prior_transcript() {
        let dir = tmp("dangling");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        let msg = serde_json::to_string(&SessionEntry::Message(Message::user("avant"))).unwrap();
        let boundary = serde_json::to_string(&SessionEntry::CompactBoundary {
            kind: CompactKind::Auto,
        })
        .unwrap();
        // ...message, boundary, THEN nothing (crash before the summary is written)
        std::fs::write(&path, format!("{msg}\n{boundary}\n")).unwrap();

        let resumed = resume_file(&path).unwrap();
        assert_eq!(resumed.compactions, 1);
        assert_eq!(resumed.messages.len(), 1, "le transcript antérieur survit");
        assert_eq!(resumed.messages[0].text(), "avant");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // US-009 AC1: the discriminated FileHistorySnapshot entry is written and replayed
    // cleanly at resume time.
    #[tokio::test]
    async fn file_snapshot_roundtrips() {
        let dir = tmp("snapshot");
        let s = JsonlSession::create_in(&dir).unwrap();
        s.sync(&[Message::user("hi")]).await.unwrap();
        s.record_file_snapshot(FileSnapshot {
            path: "src/main.rs".into(),
            content: "fn main() {}".into(),
        })
        .await
        .unwrap();
        drop(s);

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.file_snapshots.len(), 1);
        assert_eq!(resumed.file_snapshots[0].path, "src/main.rs");
        assert_eq!(resumed.file_snapshots[0].content, "fn main() {}");
        assert!(!resumed.skipped_partial);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_skips_truncated_last_line() {
        let dir = tmp("truncated");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        // one valid entry + a partial line (crash mid-write, no trailing \n)
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        std::fs::write(
            &path,
            format!("{valid}\n{{\"entry\":\"message\",\"role\":\"us"),
        )
        .unwrap();

        let resumed = resume_file(&path).unwrap();
        assert!(resumed.skipped_partial, "truncated line should be ignored");
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.messages[0].text(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reopen_repairs_truncated_tail_before_append() {
        let dir = tmp("repair-tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.jsonl");
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        std::fs::write(
            &path,
            format!("{valid}\n{{\"entry\":\"message\",\"role\":\"us"),
        )
        .unwrap();

        let s = JsonlSession::create_at(&path).unwrap();
        s.sync(&[Message::user("ok"), Message::assistant_text("next")])
            .await
            .unwrap();
        drop(s);

        let resumed = resume_file(&path).unwrap();
        assert!(!resumed.skipped_partial);
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.messages[0].text(), "ok");
        assert_eq!(resumed.messages[1].text(), "next");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_at_reopens_with_cursor_from_existing_log() {
        let dir = tmp("reopen-cursor");
        let path = dir.join("same.jsonl");
        let first = JsonlSession::create_at(&path).unwrap();
        first.sync(&[Message::user("old")]).await.unwrap();
        drop(first);

        let second = JsonlSession::create_at(&path).unwrap();
        second
            .sync(&[Message::user("old"), Message::assistant_text("next")])
            .await
            .unwrap();
        drop(second);

        let resumed = resume_file(&path).unwrap();
        assert_eq!(resumed.messages.len(), 2, "pas de duplication au reopen");
        assert_eq!(resumed.messages[0].text(), "old");
        assert_eq!(resumed.messages[1].text(), "next");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_at_rejects_second_live_writer_for_same_path() {
        let dir = tmp("same-writer");
        let path = dir.join("same.jsonl");
        let _first = JsonlSession::create_at(&path).unwrap();
        let err = match JsonlSession::create_at(&path) {
            Ok(_) => "unexpected success".to_string(),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("session already open"));
        drop(_first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_errors_on_corrupt_final_line_with_newline() {
        let dir = tmp("corrupt-final-newline");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        std::fs::write(&path, format!("{valid}\nGARBAGE\n")).unwrap();

        assert!(
            resume_file(&path).is_err(),
            "a corrupt final line ending with newline is not a truncation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_rejects_future_schema_version() {
        let dir = tmp("future-schema");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        std::fs::write(
            &path,
            format!("{{\"entry\":\"meta\",\"schema_version\":999}}\n{valid}\n"),
        )
        .unwrap();
        let err = resume_file(&path).unwrap_err().to_string();
        assert!(err.contains("schema_version 999"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_sessions_excludes_current_and_empties() {
        let dir = tmp("list");
        let a = JsonlSession::create_at(&dir.join("a.jsonl")).unwrap();
        a.sync(&[Message::user("session A")]).await.unwrap();
        let b = JsonlSession::create_at(&dir.join("b.jsonl")).unwrap();
        b.sync(&[Message::user("session B"), Message::assistant_text("ok")])
            .await
            .unwrap();
        let empty = JsonlSession::create_at(&dir.join("empty.jsonl")).unwrap(); // empty -> ignored
        drop(b);
        drop(empty);

        let list = list_sessions(&dir, Some(&dir.join("a.jsonl")));
        assert_eq!(list.len(), 1, "a exclue, empty ignorée → reste b");
        assert_eq!(list[0].id, "b.jsonl");
        assert_eq!(list[0].summary, "session B");
        assert_eq!(list[0].message_count, 2);
        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// US-011: a branch is inspectable FROM THE LISTING. Nothing here opens a
    /// thread or reads a transcript: the parent and the cut point come from the
    /// branch's own binding line, which is why they survive the deletion of the
    /// source.
    #[tokio::test]
    async fn a_listed_branch_names_its_parent_and_its_fork_point() {
        use agent_runtime::id::{EventId, SequentialIds, ThreadId, TurnId};
        use agent_runtime::lifecycle::TurnState;
        use agent_runtime::store::{ForkPoint, ThreadStore};

        let dir = tmp("list-branch");
        std::fs::create_dir_all(&dir).unwrap();
        let parent_path = dir.join("parent.jsonl");

        let ids = SequentialIds::new();
        let parent_thread_id = ThreadId::generate(&ids);
        let child_thread_id = ThreadId::generate(&ids);
        let fork_turn_id = TurnId::generate(&ids);

        let store = JsonlThreadStore::open(&parent_path).unwrap();
        store.create(&parent_thread_id).await.unwrap();
        // A listed session needs a transcript; the branch inherits it.
        std::fs::write(
            &parent_path,
            format!(
                "{}{}\n",
                std::fs::read_to_string(&parent_path).unwrap(),
                serde_json::to_string(&SessionEntry::Message(Message::user("la question")))
                    .unwrap()
            ),
        )
        .unwrap();
        let cut = agent_runtime::event::ThreadEvent {
            event_id: EventId::generate(&ids),
            thread_id: parent_thread_id,
            seq: 1,
            at_ms: 1,
            payload: agent_runtime::event::ThreadEventPayload::TurnStateChanged {
                turn_id: fork_turn_id,
                from: Some(TurnState::Running),
                to: TurnState::Completed,
                cause: None,
                context: None,
            },
        };
        store.append(&cut).await.unwrap();
        let point = ForkPoint {
            parent_thread_id,
            child_thread_id,
            fork_turn_id,
            fork_event_id: cut.event_id,
        };
        let branch = store.fork(&point).await.unwrap();
        branch.close().await.unwrap();
        store.close().await.unwrap();

        let listed = list_sessions(&dir, None);
        let parent = listed
            .iter()
            .find(|s| s.id == "parent.jsonl")
            .expect("the source is listed");
        assert_eq!(parent.thread_id, Some(parent_thread_id));
        assert!(!parent.is_branch(), "the source is not a branch");

        let child = listed
            .iter()
            .find(|s| s.id == format!("{child_thread_id}.jsonl"))
            .expect("the branch is listed");
        assert_eq!(child.thread_id, Some(child_thread_id));
        assert!(child.is_branch());
        assert_eq!(child.origin, Some(point.origin()));
        assert_eq!(
            child.origin.map(|o| o.parent_thread_id),
            Some(parent_thread_id)
        );
        assert_eq!(child.origin.map(|o| o.fork_turn_id), Some(fork_turn_id));
        assert_eq!(child.message_count, parent.message_count);

        // The relation is read from the branch alone: deleting the source
        // changes nothing about what the listing can say.
        std::fs::remove_file(&parent_path).unwrap();
        let after = list_sessions(&dir, None);
        let child = after
            .iter()
            .find(|s| s.id == format!("{child_thread_id}.jsonl"))
            .expect("the branch outlives its source in the listing");
        assert_eq!(child.origin, Some(point.origin()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn workspace_prompts_dedupes_globally_and_caps_recent() {
        let dir = tmp("wprompts-cap");
        let a = JsonlSession::create_at(&dir.join("a.jsonl")).unwrap();
        a.sync(&[
            Message::user("first"),
            Message::assistant_text("r"),
            Message::user("repeat"),
        ])
        .await
        .unwrap();
        drop(a);
        let b = JsonlSession::create_at(&dir.join("b.jsonl")).unwrap();
        b.sync(&[
            Message::user("middle"),
            Message::assistant_text("r"),
            Message::user("repeat"),
            Message::user("last"),
        ])
        .await
        .unwrap();
        drop(b);

        let prompts = workspace_prompts(&dir, None, 3);
        assert_eq!(prompts, vec!["middle", "repeat", "last"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn workspace_prompts_aggregates_across_sessions() {
        let dir = tmp("wprompts");
        let a = JsonlSession::create_at(&dir.join("a.jsonl")).unwrap();
        a.sync(&[
            Message::user("a1"),
            Message::assistant_text("r"),
            Message::user("a2"),
            Message::user("a2"), // consecutive duplicate -> deduped
        ])
        .await
        .unwrap();
        drop(a);
        let b = JsonlSession::create_at(&dir.join("b.jsonl")).unwrap();
        b.sync(&[Message::user("b1")]).await.unwrap();
        let cur = JsonlSession::create_at(&dir.join("cur.jsonl")).unwrap();
        cur.sync(&[Message::user("courant")]).await.unwrap();
        drop(b);
        drop(cur);

        let prompts = workspace_prompts(&dir, Some(&dir.join("cur.jsonl")), 100);
        let pos = |x: &str| prompts.iter().position(|p| p == x);
        assert!(
            pos("a1").unwrap() < pos("a2").unwrap(),
            "ordre intra-session"
        );
        assert_eq!(prompts.iter().filter(|p| *p == "a2").count(), 1, "dedup");
        assert!(pos("b1").is_some(), "aggregated from another session");
        assert!(pos("courant").is_none(), "session courante exclue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn switch_to_appends_to_resumed_file() {
        let dir = tmp("switch");
        let old = JsonlSession::create_at(&dir.join("old.jsonl")).unwrap();
        old.sync(&[Message::user("old")]).await.unwrap();
        drop(old);

        // current session, then switches to `old` at cursor 1 (1 msg present).
        let s = JsonlSession::create_at(&dir.join("cur.jsonl")).unwrap();
        s.switch_to(&dir.join("old.jsonl"), 1).unwrap();
        s.sync(&[Message::user("old"), Message::assistant_text("next")])
            .await
            .unwrap();
        drop(s);

        let resumed = resume_file(&dir.join("old.jsonl")).unwrap();
        assert_eq!(resumed.messages.len(), 2, "the delta was appended to old");
        assert_eq!(resumed.messages[1].text(), "next");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_missing_file_is_empty() {
        let dir = tmp("missing");
        let resumed = resume_dir(&dir).unwrap();
        assert!(resumed.messages.is_empty());
        assert_eq!(resumed.compactions, 0);
    }

    #[test]
    fn resume_corrupt_middle_line_errors() {
        let dir = tmp("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        // corrupted line IN THE MIDDLE (followed by a valid line) -> real corruption
        std::fs::write(&path, format!("{valid}\nGARBAGE\n{valid}\n")).unwrap();
        assert!(resume_file(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_ignores_unknown_future_entry() {
        let dir = tmp("unknown");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        std::fs::write(
            &path,
            format!("{{\"entry\":\"future_feature\",\"x\":1}}\n{valid}\n"),
        )
        .unwrap();
        let resumed = resume_file(&path).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.messages[0].text(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn encrypted_reasoning_redaction_replays() {
        let dir = tmp("redact-reasoning");
        let s = JsonlSession::create_in(&dir).unwrap();
        let assistant = Message::assistant(vec![
            ContentBlock::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
        ]);
        s.sync(&[assistant]).await.unwrap();
        s.redact_encrypted_reasoning().await.unwrap();
        drop(s);

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert!(
            resumed.messages[0]
                .content
                .iter()
                .all(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }))
        );
        assert!(
            resumed.messages[0]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
