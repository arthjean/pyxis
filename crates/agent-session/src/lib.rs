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
//! - une `CompactBoundary` réinitialise le transcript reconstruit (les messages
//!   d'avant ont été compactés). Le `clear` est **différé** jusqu'au premier
//!   `Message` suivant : une frontière orpheline (crash entre frontière et
//!   résumé) n'efface alors PAS le transcript antérieur.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use agent_core::CompactKind;
use agent_core::message::{Message, Role};
use agent_core::session::{FileSnapshot, Session, SessionEntry, SessionError};

/// Nom du fichier de session dans un dossier de travail.
pub const SESSION_FILE: &str = "session.jsonl";

fn io_err(e: impl std::fmt::Display) -> SessionError {
    SessionError::Io(e.to_string())
}
fn serde_err(e: impl std::fmt::Display) -> SessionError {
    SessionError::Serde(e.to_string())
}

/// Session JSONL append-only. Tient un curseur du nombre de messages déjà écrits
/// pour que `sync` n'écrive que le delta (transcript-before-response idempotent).
pub struct JsonlSession {
    file: Mutex<File>,
    cursor: Mutex<usize>,
}

impl JsonlSession {
    /// Crée (ou rouvre en append) le fichier de session dans `dir`.
    pub fn create_in(dir: &Path) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(SESSION_FILE))
            .map_err(io_err)?;
        Ok(Self {
            file: Mutex::new(file),
            cursor: Mutex::new(0),
        })
    }

    /// Crée (ou rouvre en append) une session sur un fichier nommé (un fichier
    /// par conversation : `<dir>/<id>.jsonl`). Crée le dossier parent au besoin.
    pub fn create_at(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io_err)?;
        Ok(Self {
            file: Mutex::new(file),
            cursor: Mutex::new(0),
        })
    }

    /// Bascule le fichier de persistance vers `path` (resume d'une session
    /// passée) en repositionnant le curseur à `cursor` messages déjà écrits : les
    /// prochains `sync` n'appendront que le delta dans la session reprise.
    pub fn switch_to(&self, path: &Path, cursor: usize) -> Result<(), SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io_err)?;
        *self
            .file
            .lock()
            .map_err(|_| SessionError::Io("verrou fichier empoisonné".into()))? = file;
        *self
            .cursor
            .lock()
            .map_err(|_| SessionError::Io("verrou curseur empoisonné".into()))? = cursor;
        Ok(())
    }

    fn append(&self, entry: &SessionEntry) -> Result<(), SessionError> {
        let line = format!("{}\n", serde_json::to_string(entry).map_err(serde_err)?);
        self.write_locked(&line)
    }

    /// Écrit un buffer déjà sérialisé (≥ 1 ligne) sous le verrou fichier, avec
    /// durabilité (`flush` + `sync_data`).
    fn write_locked(&self, buf: &str) -> Result<(), SessionError> {
        let mut f = self
            .file
            .lock()
            .map_err(|_| SessionError::Io("verrou fichier empoisonné".into()))?;
        f.write_all(buf.as_bytes()).map_err(io_err)?;
        f.flush().map_err(io_err)?;
        f.sync_data().map_err(io_err)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Session for JsonlSession {
    async fn sync(&self, messages: &[Message]) -> Result<(), SessionError> {
        let mut cur = self
            .cursor
            .lock()
            .map_err(|_| SessionError::Io("verrou curseur empoisonné".into()))?;
        let start = (*cur).min(messages.len());
        for m in &messages[start..] {
            self.append(&SessionEntry::Message(m.clone()))?;
        }
        *cur = messages.len();
        Ok(())
    }

    async fn checkpoint(
        &self,
        kind: CompactKind,
        messages: &[Message],
    ) -> Result<(), SessionError> {
        // Frontière + transcript post-compaction écrits en UN SEUL write_locked :
        // pas de fenêtre où une frontière existe sans son résumé (#9).
        let mut buf = format!(
            "{}\n",
            serde_json::to_string(&SessionEntry::CompactBoundary { kind }).map_err(serde_err)?
        );
        for m in messages {
            buf.push_str(
                &serde_json::to_string(&SessionEntry::Message(m.clone())).map_err(serde_err)?,
            );
            buf.push('\n');
        }
        self.write_locked(&buf)?;
        *self
            .cursor
            .lock()
            .map_err(|_| SessionError::Io("verrou curseur empoisonné".into()))? = messages.len();
        Ok(())
    }

    async fn record_file_snapshot(&self, snapshot: FileSnapshot) -> Result<(), SessionError> {
        self.append(&SessionEntry::FileHistorySnapshot(snapshot))
    }
}

/// État reconstruit depuis un log de session.
#[derive(Debug, Default)]
pub struct ResumedSession {
    pub messages: Vec<Message>,
    pub compactions: usize,
    /// Vrai si une dernière ligne partielle (crash mid-write) a été ignorée.
    pub skipped_partial: bool,
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

    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let mut out = ResumedSession::default();
    // clear DIFFÉRÉ : une frontière n'efface le transcript antérieur que lorsque
    // son premier Message de résumé arrive. Une frontière orpheline (crash entre
    // frontière et résumé) préserve donc le transcript d'avant.
    let mut pending_clear = false;

    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEntry>(line) {
            Ok(SessionEntry::Message(m)) => {
                if pending_clear {
                    out.messages.clear();
                    pending_clear = false;
                }
                out.messages.push(m);
            }
            Ok(SessionEntry::CompactBoundary { .. }) => {
                pending_clear = true;
                out.compactions += 1;
            }
            Ok(SessionEntry::FileHistorySnapshot(_)) => {}
            Err(e) => {
                if i == n - 1 {
                    // dernière ligne tronquée par un crash → ignorée (AC3).
                    out.skipped_partial = true;
                } else {
                    return Err(SessionError::Serde(format!("ligne {i} corrompue: {e}")));
                }
            }
        }
    }
    Ok(out)
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
        let Ok(resumed) = resume_file(&path) else {
            continue;
        };
        if resumed.messages.is_empty() {
            continue;
        }
        let summary = resumed
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.text())
            .unwrap_or_default();
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(SessionInfo {
            id: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            summary,
            message_count: resumed.messages.len(),
            modified,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Agrège les prompts utilisateur de TOUTES les sessions d'un dossier (ancien →
/// récent), pour l'historique navigable **par dossier** (façon Claude Code).
/// Exclut `exclude` (la session courante, encore vide), dédupe les doublons
/// consécutifs et garde au plus `cap` entrées (les plus récentes). Les sessions
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
    files.sort_by_key(|(t, _)| *t); // ancien → récent

    let mut out: Vec<String> = Vec::new();
    for (_, path) in files {
        let Ok(resumed) = resume_file(&path) else {
            continue;
        };
        for m in &resumed.messages {
            if m.role == Role::User {
                let text = m.text();
                if !text.trim().is_empty() && out.last().map(String::as_str) != Some(text.as_str()) {
                    out.push(text);
                }
            }
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
        let dir = std::env::temp_dir().join(format!("numen_sess_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn write_then_resume_roundtrip() {
        let dir = tmp("roundtrip");
        let s = JsonlSession::create_in(&dir).unwrap();
        let msgs = vec![Message::user("salut"), Message::assistant_text("bonjour")];
        s.sync(&msgs).await.unwrap();
        // re-sync idempotent : n'ajoute rien
        s.sync(&msgs).await.unwrap();

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.messages[0].text(), "salut");
        assert_eq!(resumed.messages[1].text(), "bonjour");
        assert!(!resumed.skipped_partial);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_resets_transcript() {
        let dir = tmp("compact");
        let s = JsonlSession::create_in(&dir).unwrap();
        s.sync(&[Message::user("vieux 1"), Message::assistant_text("vieux 2")])
            .await
            .unwrap();
        // checkpoint atomique : frontière + transcript post-compaction ([résumé]).
        s.checkpoint(CompactKind::Auto, &[Message::user("[résumé]")])
            .await
            .unwrap();

        let resumed = resume_dir(&dir).unwrap();
        assert_eq!(resumed.compactions, 1);
        assert_eq!(
            resumed.messages.len(),
            1,
            "les vieux messages sont compactés"
        );
        assert_eq!(resumed.messages[0].text(), "[résumé]");
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

    // US-009 AC1 : l'entrée discriminée FileHistorySnapshot s'écrit et est ignorée
    // proprement au resume.
    #[tokio::test]
    async fn file_snapshot_roundtrips_and_is_skipped() {
        let dir = tmp("snapshot");
        let s = JsonlSession::create_in(&dir).unwrap();
        s.sync(&[Message::user("hi")]).await.unwrap();
        s.record_file_snapshot(FileSnapshot {
            path: "src/main.rs".into(),
            content: "fn main() {}".into(),
        })
        .await
        .unwrap();

        let resumed = resume_dir(&dir).unwrap();
        // le snapshot est une entrée valide, ignorée pour la reconstruction du transcript
        assert_eq!(resumed.messages.len(), 1);
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
    async fn list_sessions_excludes_current_and_empties() {
        let dir = tmp("list");
        let a = JsonlSession::create_at(&dir.join("a.jsonl")).unwrap();
        a.sync(&[Message::user("session A")]).await.unwrap();
        let b = JsonlSession::create_at(&dir.join("b.jsonl")).unwrap();
        b.sync(&[Message::user("session B"), Message::assistant_text("ok")])
            .await
            .unwrap();
        JsonlSession::create_at(&dir.join("empty.jsonl")).unwrap(); // vide → ignorée

        let list = list_sessions(&dir, Some(&dir.join("a.jsonl")));
        assert_eq!(list.len(), 1, "a exclue, empty ignorée → reste b");
        assert_eq!(list[0].id, "b.jsonl");
        assert_eq!(list[0].summary, "session B");
        assert_eq!(list[0].message_count, 2);
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
        let b = JsonlSession::create_at(&dir.join("b.jsonl")).unwrap();
        b.sync(&[Message::user("b1")]).await.unwrap();
        let cur = JsonlSession::create_at(&dir.join("cur.jsonl")).unwrap();
        cur.sync(&[Message::user("courant")]).await.unwrap();

        let prompts = workspace_prompts(&dir, Some(&dir.join("cur.jsonl")), 100);
        let pos = |x: &str| prompts.iter().position(|p| p == x);
        assert!(pos("a1").unwrap() < pos("a2").unwrap(), "ordre intra-session");
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
}
