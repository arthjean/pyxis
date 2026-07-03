//! Outil `bash` — exécute une commande shell dans le workspace. Action SENSIBLE
//! (destructive/réseau possible) → cible de la défense taint (§4.6) et `Ask` par
//! défaut. Sortie untrusted (stdout/stderr = contenu externe). Le Registry
//! enveloppe l'appel dans un `timeout` ; `kill_on_drop` tue le process si le
//! timeout expire (US-012 AC2 / unhappy path US-003). US-012.

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{MAX_COMMAND_BYTES, Tool, ToolCtx, ToolOutput};

/// Borne de capture (évite un flood de prompt sur une sortie géante).
const MAX_OUTPUT: usize = 30_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    pub command: String,
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    type Input = BashInput;

    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> String {
        "Exécute une commande shell (sh -c) dans le workspace et retourne \
         stdout/stderr et le code de sortie. La commande tourne sous timeout. \
         Paramètre : command."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Commande shell à exécuter." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    // Defaults fail-closed conservés : non read-only, non concurrent, SENSIBLE,
    // untrusted. On les rend explicites pour la lisibilité.
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        true
    }
    fn returns_untrusted(&self) -> bool {
        true
    }
    fn validate_input(&self, input: &Self::Input) -> Result<(), ValidationError> {
        if input.command.trim().is_empty() {
            return Err(ValidationError::new("commande vide"));
        }
        let bytes = input.command.len();
        if bytes > MAX_COMMAND_BYTES {
            return Err(ValidationError::new(format!(
                "commande trop longue: {bytes} octets > {MAX_COMMAND_BYTES}"
            )));
        }
        Ok(())
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }
    fn timeout(&self, ctx: &ToolCtx) -> std::time::Duration {
        ctx.timeout
            .checked_add(ctx.cleanup_grace)
            .unwrap_or(ctx.timeout)
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = tokio::process::Command::new("powershell.exe");
            cmd.arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(&input.command);
            cmd
        };
        #[cfg(not(windows))]
        let mut cmd = tokio::process::Command::new("sh");

        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            cmd.arg("-c").arg(&input.command);
            cmd.process_group(0);
        }

        cmd.current_dir(&ctx.workspace)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        // Durcissement sandbox (réseau via HTTP_PROXY) injecté par l'agent-cli.
        // Le confinement FS Landlock est process-wide → hérité par ce sous-process.
        if let Some(harden) = &ctx.harden {
            harden(&mut cmd);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Io(format!("lancement du shell: {e}")))?;
        let pid = child.id();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(out) => read_tail(out).await,
                None => Capture::default(),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(err) => read_tail(err).await,
                None => Capture::default(),
            }
        });

        let mut cleanup_timed_out = false;
        let (status, timed_out) = match tokio::time::timeout(ctx.timeout, child.wait()).await {
            Ok(res) => (
                Some(res.map_err(|e| ToolError::Io(format!("attente du shell: {e}")))?),
                false,
            ),
            Err(_) => {
                let cleanup = async {
                    if let Some(pid) = pid {
                        kill_process_tree(pid).await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                };
                cleanup_timed_out = tokio::time::timeout(ctx.cleanup_grace, cleanup)
                    .await
                    .is_err();
                (None, true)
            }
        };

        let (stdout, stderr) = if cleanup_timed_out {
            stdout_task.abort();
            stderr_task.abort();
            (Capture::default(), Capture::default())
        } else {
            let stdout = stdout_task
                .await
                .map_err(|e| ToolError::Io(format!("lecture stdout: {e}")))?;
            let stderr = stderr_task
                .await
                .map_err(|e| ToolError::Io(format!("lecture stderr: {e}")))?;
            (stdout, stderr)
        };

        let mut body = String::new();
        let stdout_text = String::from_utf8_lossy(&stdout.bytes);
        let stderr_text = String::from_utf8_lossy(&stderr.bytes);
        if stdout.omitted > 0 {
            body.push_str(&format!(
                "[... stdout tronqué, {} octets, début omis]\n",
                stdout.omitted
            ));
        }
        if !stdout.is_empty() {
            body.push_str(&stdout_text);
        }
        if !stderr_text.is_empty() || stderr.omitted > 0 {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            if stderr.omitted > 0 {
                body.push_str(&format!(
                    "[... stderr tronqué, {} octets, début omis]\n",
                    stderr.omitted
                ));
            }
            body.push_str(&stderr_text);
        }
        if body.len() > MAX_OUTPUT {
            body = truncate_tail(&body, MAX_OUTPUT);
        }

        if timed_out {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str("[timeout outil dépassé]");
            if cleanup_timed_out {
                body.push_str("\n[cleanup process-tree incomplet après timeout]");
            }
            return Ok(ToolOutput::error(body));
        }

        let code = status.and_then(|s| s.code());
        match code {
            Some(0) => {
                if body.is_empty() {
                    body.push_str("(aucune sortie, succès)");
                }
                Ok(ToolOutput::text(body))
            }
            Some(n) => {
                body.push_str(&format!("\n[code de sortie {n}]"));
                Ok(ToolOutput::error(body))
            }
            None => {
                body.push_str("\n[terminé par signal]");
                Ok(ToolOutput::error(body))
            }
        }
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    omitted: usize,
}

impl Capture {
    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

async fn read_tail(mut reader: impl tokio::io::AsyncRead + Unpin) -> Capture {
    let mut out = Capture::default();
    let mut buf = [0_u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        out.bytes.extend_from_slice(&buf[..n]);
        if out.bytes.len() > MAX_OUTPUT {
            let overflow = out.bytes.len() - MAX_OUTPUT;
            out.bytes.drain(0..overflow);
            out.omitted = out.omitted.saturating_add(overflow);
        }
    }
    out
}

async fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(windows))]
    {
        let group = format!("-{pid}");
        let _ = tokio::process::Command::new("kill")
            .arg("-TERM")
            .arg(&group)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = tokio::process::Command::new("kill")
            .arg("-KILL")
            .arg(&group)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
}

/// Tronque `body` en gardant la QUEUE (tail) sur `max` octets (US-026) : sur une
/// sortie longue (compilation : warnings en tête, erreurs + exit code en queue),
/// le tail préserve l'information critique. Le point de coupe est aligné sur une
/// frontière de caractère UTF-8 (jamais de panic d'indexation).
fn truncate_tail(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let mut cut = body.len() - max;
    while cut < body.len() && !body.is_char_boundary(cut) {
        cut += 1;
    }
    format!(
        "[... sortie tronquée, {cut} octets, début omis]\n{}",
        &body[cut..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_truncation_keeps_the_end_and_marks_omission() {
        // 10 lignes ; on tronque pour ne garder que la fin (où vivent erreurs/exit).
        let body: String = (0..10).map(|i| format!("ligne{i}\n")).collect();
        let out = truncate_tail(&body, 20);
        assert!(out.starts_with("[... sortie tronquée, "));
        assert!(out.contains("octets, début omis]"));
        assert!(out.contains("ligne9"), "la fin doit être conservée: {out}");
        assert!(!out.contains("ligne0"), "le début doit être omis: {out}");
    }

    #[test]
    fn tail_truncation_is_char_boundary_safe() {
        // coupe au milieu d'un flux multi-octets → pas de panic, frontière respectée.
        let body = "é".repeat(100); // 200 octets
        let out = truncate_tail(&body, 51);
        assert!(out.contains("début omis]"));
        // le suffixe conservé est de l'UTF-8 valide (aucune coupe mid-codepoint).
        assert!(out.ends_with('é'));
    }

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(truncate_tail("court", 30_000), "court");
    }
}
