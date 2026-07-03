//! `agent-session` — persistance JSONL append-only + resume par dossier (US-009,
//! ARCHITECTURE §7). Implémente le trait `Session` d'`agent-core` (injecté dans
//! la boucle). Dépend d'`agent-core` pour les types canoniques.
//!
//! Garanties :
//! - **durabilité par entrée** : chaque entrée est sérialisée puis `write_all` +
//!   `flush` + `sync_data` (fdatasync). Note : `write_all` peut émettre plusieurs
//!   syscalls `write()` ; un crash OS au milieu peut laisser une ligne PARTIELLE
//!   en queue de fichier — c'est précisément ce que le resume détecte et ignore
//!   (dernière ligne non parsable, AC3). On ne promet donc pas « tout ou rien »
//!   au niveau octet, mais « toute ligne incomplète en queue est ignorée ».
//! - **resume** : on rejoue le log ; une dernière ligne tronquée par un crash en
//!   plein écrit est ignorée (AC3), la session reprend au dernier état valide.
//! - un `CompactCheckpoint` récent réinitialise le transcript en une entrée
//!   replayable unique. Les anciens logs `CompactBoundary` restent supportés :
//!   leur `clear` est différé jusqu'au premier `Message` suivant, donc une
//!   frontière orpheline n'efface pas le transcript antérieur.
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

/// Nom du fichier de session dans un dossier de travail.
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

fn validate_checkpoint(messages: &[Message]) -> Result<(), SessionError> {
    if messages.is_empty() {
        return Err(invalid_session("checkpoint de compaction vide"));
    }
    if !is_summary_message(&messages[0]) {
        return Err(invalid_session(
            "checkpoint de compaction sans résumé en premier message",
        ));
    }
    for (i, message) in messages.iter().enumerate() {
        message
            .validate()
            .map_err(|e| invalid_session(format!("checkpoint message {i} invalide: {e}")))?;
    }
    Ok(())
}

fn redact_encrypted_reasoning(messages: &mut [Message]) {
    for message in messages {
        message
            .content
            .retain(|block| !matches!(block, ContentBlock::EncryptedReasoning { .. }));
    }
}

/// Session JSONL append-only. Tient un curseur du nombre de messages déjà écrits
/// pour que `sync` n'écrive que le delta (transcript-before-response idempotent).
pub struct JsonlSession {
    state: Mutex<WriterState>,
}

struct WriterState {
    file: File,
    cursor: usize,
    poisoned: bool,
}

fn open_prepared(path: &Path) -> Result<(WriterState, bool), SessionError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_err)?;
    file.try_lock()
        .map_err(|e| io_err(format!("session déjà ouverte ou verrou indisponible: {e}")))?;

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
    Ok((
        WriterState {
            file,
            cursor: resumed.messages.len(),
            poisoned: false,
        },
        is_empty,
    ))
}

fn resume_locked_file(file: &mut File) -> Result<ResumedSession, SessionError> {
    let mut content = String::new();
    file.seek(SeekFrom::Start(0)).map_err(io_err)?;
    file.read_to_string(&mut content).map_err(io_err)?;
    file.seek(SeekFrom::End(0)).map_err(io_err)?;
    resume_content(&content)
}

fn write_buf_locked(state: &mut WriterState, buf: &str) -> Result<(), SessionError> {
    if state.poisoned {
        return Err(SessionError::Io(
            "writer session empoisonné après une erreur d'append ; rouvrir la session".into(),
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

fn write_entry_locked(state: &mut WriterState, entry: &SessionEntry) -> Result<(), SessionError> {
    let line = format!("{}\n", serde_json::to_string(entry).map_err(serde_err)?);
    write_buf_locked(state, &line)
}

impl JsonlSession {
    /// Crée (ou rouvre en append) le fichier de session dans `dir`.
    pub fn create_in(dir: &Path) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let path = dir.join(SESSION_FILE);
        let (state, is_empty) = open_prepared(&path)?;
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

    /// Crée (ou rouvre en append) une session sur un fichier nommé (un fichier
    /// par conversation : `<dir>/<id>.jsonl`). Crée le dossier parent au besoin.
    pub fn create_at(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let (state, is_empty) = open_prepared(path)?;
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

    /// Bascule le fichier de persistance vers `path` (resume d'une session
    /// passée) en repositionnant le curseur à `cursor` messages déjà écrits : les
    /// prochains `sync` n'appendront que le delta dans la session reprise.
    pub fn switch_to(&self, path: &Path, cursor: usize) -> Result<(), SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let (state, is_empty) = open_prepared(path)?;
        if state.cursor != cursor {
            return Err(invalid_session(format!(
                "curseur resume incohérent : attendu {}, reçu {cursor}",
                state.cursor
            )));
        }
        *self
            .state
            .lock()
            .map_err(|_| SessionError::Io("verrou session empoisonné".into()))? = state;
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
            .map_err(|_| SessionError::Io("verrou session empoisonné".into()))?;
        write_entry_locked(&mut state, entry)
    }
}

#[async_trait::async_trait]
impl Session for JsonlSession {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SessionError::Io("verrou session empoisonné".into()))?;
        let start = state.cursor.min(messages.len());
        for (offset, m) in messages[start..].iter().enumerate() {
            write_entry_locked(&mut state, &SessionEntry::Message(m.clone()))?;
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
            .map_err(|_| SessionError::Io("verrou session empoisonné".into()))?;
        write_entry_locked(
            &mut state,
            &SessionEntry::CompactCheckpoint {
                kind,
                messages: messages.to_vec(),
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

/// État reconstruit depuis un log de session.
#[derive(Debug, Default)]
pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub file_snapshots: Vec<FileSnapshot>,
    pub compactions: usize,
    /// Vrai si une dernière ligne partielle (crash mid-write) a été ignorée.
    pub skipped_partial: bool,
    /// Version de schéma déclarée par la première entrée `Meta`, si présente.
    pub schema_version: Option<u32>,
    /// Nombre d'octets rejoués comme log valide. Sert à tronquer une queue
    /// partielle avant de réappend dans le fichier.
    pub valid_bytes: u64,
    /// Vrai si le fichier lu terminait par `\n`.
    pub ended_with_newline: bool,
}

/// Reprend la session d'un dossier (`<dir>/session.jsonl`).
pub fn resume_dir(dir: &Path) -> Result<ResumedSession, SessionError> {
    resume_file(&dir.join(SESSION_FILE))
}

/// Reprend une session depuis un fichier JSONL. Fichier absent ⇒ session vide.
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
                    "schema_version {schema_version} non supporté (max {SESSION_SCHEMA_VERSION})"
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

    // clear DIFFÉRÉ : une frontière n'efface le transcript antérieur que lorsque
    // son premier Message de résumé arrive. Une frontière orpheline (crash entre
    // frontière et résumé) préserve donc le transcript d'avant.
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

    match serde_json::from_str::<SessionEntry>(line) {
        Ok(entry) => {
            apply_entry(out, pending_clear, entry)?;
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
                Err(SessionError::Serde(format!("ligne corrompue: {e}")))
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
            Ok(entry) => apply_scan_entry(&mut scan, &mut pending_clear, entry)?,
            Err(_) if !has_newline => break,
            Err(e) => return Err(SessionError::Serde(format!("ligne corrompue: {e}"))),
        }
    }
    Ok(scan)
}

#[derive(Default)]
struct SessionScan {
    message_count: usize,
    summary: Option<String>,
    prompts: Vec<String>,
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
                    "schema_version {schema_version} non supporté (max {SESSION_SCHEMA_VERSION})"
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

/// Métadonnée d'une session listée pour le menu `/resume`.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Nom de fichier (`<id>.jsonl`) — identifiant résolu côté CLI.
    pub id: String,
    /// Résumé : premier message utilisateur (vide si aucun).
    pub summary: String,
    pub message_count: usize,
    pub modified: SystemTime,
}

/// Liste les sessions reprenables d'un dossier (`*.jsonl`), triées du plus
/// récent au plus ancien. Ignore les sessions vides et celle de `exclude` (la
/// session courante). Tolérante : un fichier illisible est simplement sauté.
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
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Agrège les prompts utilisateur de TOUTES les sessions d'un dossier (ancien →
/// récent), pour l'historique navigable **par dossier** (façon Claude Code).
/// Exclut `exclude` (la session courante, encore vide), dédupe les doublons
/// déjà vues et garde au plus `cap` entrées (les plus récentes). Les sessions
/// sont ordonnées par date de modification (approx. chronologique).
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
    }); // ancien → récent

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

    /// Dossier temporaire isolé par test (pas de dépendance `rand`/`tempfile`).
    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis_sess_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn summary(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Summary { text: text.into() }],
        }
    }

    #[tokio::test]
    async fn write_then_resume_roundtrip() {
        let dir = tmp("roundtrip");
        let s = JsonlSession::create_in(&dir).unwrap();
        let msgs = vec![Message::user("salut"), Message::assistant_text("bonjour")];
        s.sync(&msgs).await.unwrap();
        // re-sync idempotent : n'ajoute rien
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
        s.checkpoint(CompactKind::Auto, &[summary("[résumé]")])
            .await
            .unwrap();
        drop(s);

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.compactions, 1);
        assert_eq!(
            resumed.messages.len(),
            1,
            "les vieux messages sont compactés"
        );
        assert_eq!(resumed.messages[0].text(), "[résumé]");
        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(
            raw.contains("\"entry\":\"compact_checkpoint\""),
            "nouveaux checkpoints en entrée unique"
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
                &[Message::assistant_text("pas un résumé")],
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

    // #9 : une frontière ORPHELINE (crash entre frontière et résumé) ne doit PAS
    // effacer le transcript antérieur (clear différé).
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
        // ...message, frontière, PUIS rien (crash avant l'écriture du résumé)
        std::fs::write(&path, format!("{msg}\n{boundary}\n")).unwrap();

        let resumed = resume_file(&path).unwrap();
        assert_eq!(resumed.compactions, 1);
        assert_eq!(resumed.messages.len(), 1, "le transcript antérieur survit");
        assert_eq!(resumed.messages[0].text(), "avant");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // US-009 AC1 : l'entrée discriminée FileHistorySnapshot s'écrit et se rejoue
    // proprement au resume.
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
        // une entrée valide + une ligne partielle (crash mid-write, pas de \n final)
        let valid = serde_json::to_string(&SessionEntry::Message(Message::user("ok"))).unwrap();
        std::fs::write(
            &path,
            format!("{valid}\n{{\"entry\":\"message\",\"role\":\"us"),
        )
        .unwrap();

        let resumed = resume_file(&path).unwrap();
        assert!(
            resumed.skipped_partial,
            "la ligne tronquée doit être ignorée"
        );
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
        s.sync(&[Message::user("ok"), Message::assistant_text("suite")])
            .await
            .unwrap();
        drop(s);

        let resumed = resume_file(&path).unwrap();
        assert!(!resumed.skipped_partial);
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.messages[0].text(), "ok");
        assert_eq!(resumed.messages[1].text(), "suite");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_at_reopens_with_cursor_from_existing_log() {
        let dir = tmp("reopen-cursor");
        let path = dir.join("same.jsonl");
        let first = JsonlSession::create_at(&path).unwrap();
        first.sync(&[Message::user("ancien")]).await.unwrap();
        drop(first);

        let second = JsonlSession::create_at(&path).unwrap();
        second
            .sync(&[Message::user("ancien"), Message::assistant_text("suite")])
            .await
            .unwrap();
        drop(second);

        let resumed = resume_file(&path).unwrap();
        assert_eq!(resumed.messages.len(), 2, "pas de duplication au reopen");
        assert_eq!(resumed.messages[0].text(), "ancien");
        assert_eq!(resumed.messages[1].text(), "suite");
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
        assert!(err.contains("verrou"));
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
            "une ligne finale corrompue terminée par newline n'est pas une troncature"
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
        let empty = JsonlSession::create_at(&dir.join("empty.jsonl")).unwrap(); // vide → ignorée
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
            Message::user("a2"), // doublon consécutif → dédupliqué
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
        assert_eq!(prompts.iter().filter(|p| *p == "a2").count(), 1, "dédup");
        assert!(pos("b1").is_some(), "agrégé depuis une autre session");
        assert!(pos("courant").is_none(), "session courante exclue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn switch_to_appends_to_resumed_file() {
        let dir = tmp("switch");
        let old = JsonlSession::create_at(&dir.join("old.jsonl")).unwrap();
        old.sync(&[Message::user("ancien")]).await.unwrap();
        drop(old);

        // session courante, puis bascule vers `old` au curseur 1 (1 msg présent).
        let s = JsonlSession::create_at(&dir.join("cur.jsonl")).unwrap();
        s.switch_to(&dir.join("old.jsonl"), 1).unwrap();
        s.sync(&[Message::user("ancien"), Message::assistant_text("suite")])
            .await
            .unwrap();
        drop(s);

        let resumed = resume_file(&dir.join("old.jsonl")).unwrap();
        assert_eq!(resumed.messages.len(), 2, "le delta s'est appendé à old");
        assert_eq!(resumed.messages[1].text(), "suite");
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
        // ligne corrompue AU MILIEU (suivie d'une ligne valide) → vraie corruption
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
