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
/// Streaming de sortie (US-015) : taille et délai de coalescence des fragments,
/// et plafond d'un fragment publié.
const STREAM_FLUSH_BYTES: usize = 4_096;
const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const STREAM_CHUNK_MAX: usize = 8_192;

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
        #[cfg(windows)]
        {
            "Run a PowerShell command (powershell.exe -NoProfile -NonInteractive -Command) in the workspace and return \
             stdout/stderr plus the exit code. The command runs under a timeout. \
             Parameter: command."
                .to_string()
        }
        // US-014 : la description nomme le shell RÉELLEMENT utilisé, le même que
        // celui annoncé dans le bloc `<environment>`.
        #[cfg(not(windows))]
        {
            format!(
                "Run a shell command ({} -c) in the workspace and return \
                 stdout/stderr plus the exit code. The command runs under a timeout. \
                 Parameter: command.",
                crate::shell::resolve().label
            )
        }
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute." }
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
    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.command.trim().is_empty() {
            return Err(ValidationError::new("empty command"));
        }
        let bytes = input.command.len();
        if bytes > MAX_COMMAND_BYTES {
            return Err(ValidationError::new(format!(
                "command too large: {bytes} bytes > {MAX_COMMAND_BYTES}"
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
        let shell = crate::shell::resolve();
        let mut child = match build_command(&shell, &input.command, ctx).spawn() {
            Ok(child) => child,
            // AC4 : un shell de connexion introuvable ou qui refuse de démarrer ne
            // fait pas échouer le tour. On retombe sur `sh` pour cette commande, et
            // le drapeau process-wide aligne l'annonce faite au modèle dès le tour
            // suivant.
            Err(first) if !shell.is_fallback() => {
                crate::shell::mark_login_shell_unusable();
                let fallback = crate::shell::resolve();
                build_command(&fallback, &input.command, ctx)
                    .spawn()
                    .map_err(|e| {
                        ToolError::Io(format!(
                            "shell launch: {} unusable ({first}), fallback {} failed: {e}",
                            shell.label, fallback.label
                        ))
                    })?
            }
            Err(e) => return Err(ToolError::Io(format!("shell launch: {e}"))),
        };
        let pid = child.id();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // US-015 : les deux flux sont streamés au fil de l'eau vers le client, en
        // plus d'être capturés pour le résultat final.
        let stdout_sink = ctx.output.clone();
        let stderr_sink = ctx.output.clone();
        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(out) => read_tail(out, stdout_sink).await,
                None => Capture::default(),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(err) => read_tail(err, stderr_sink).await,
                None => Capture::default(),
            }
        });

        let mut cleanup_timed_out = false;
        let (status, timed_out) = match tokio::time::timeout(ctx.timeout, child.wait()).await {
            Ok(res) => (
                Some(res.map_err(|e| ToolError::Io(format!("shell wait: {e}")))?),
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
                .map_err(|e| ToolError::Io(format!("stdout read: {e}")))?;
            let stderr = stderr_task
                .await
                .map_err(|e| ToolError::Io(format!("stderr read: {e}")))?;
            (stdout, stderr)
        };

        let mut body = String::new();
        let stdout_text = String::from_utf8_lossy(&stdout.bytes);
        let stderr_text = String::from_utf8_lossy(&stderr.bytes);
        if stdout.omitted > 0 {
            body.push_str(&format!(
                "[... stdout truncated, {} bytes, beginning omitted]\n",
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
                    "[... stderr truncated, {} bytes, beginning omitted]\n",
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
            body.push_str("[tool timeout exceeded]");
            if cleanup_timed_out {
                body.push_str("\n[process-tree cleanup incomplete after timeout]");
            }
            return Ok(ToolOutput::error(body));
        }

        let code = status.and_then(|s| s.code());
        match code {
            Some(0) => {
                if body.is_empty() {
                    body.push_str("(no output, success)");
                }
                Ok(ToolOutput::text(body))
            }
            Some(n) => {
                body.push_str(&format!("\n[exit code {n}]"));
                Ok(ToolOutput::error(body))
            }
            None => {
                body.push_str("\n[terminated by signal]");
                Ok(ToolOutput::error(body))
            }
        }
    }
}

/// Construit la commande shell (mêmes options qu'avant US-014, seul le programme
/// exécuté devient variable).
fn build_command(
    shell: &crate::shell::ShellChoice,
    command: &str,
    ctx: &ToolCtx,
) -> tokio::process::Command {
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(command);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(&shell.program);
        // `-c` en mode non interactif : aucun fichier d'initialisation interactif
        // n'est lu, le comportement reste celui d'un shell de script.
        cmd.arg("-c").arg(command);
        cmd.process_group(0);
        cmd
    };

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
    cmd
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

/// Lit un flux jusqu'à EOF : capture la QUEUE pour le résultat final (politique
/// de troncature inchangée) et, si un consommateur écoute, publie la sortie au
/// fil de l'eau (US-015).
async fn read_tail(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    sink: Option<crate::tool::OutputSink>,
) -> Capture {
    let mut out = Capture::default();
    let mut buf = [0_u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut last_flush = tokio::time::Instant::now();
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        out.bytes.extend_from_slice(&buf[..n]);
        if sink.is_some() {
            pending.extend_from_slice(&buf[..n]);
            // Coalescence : au plus un fragment par `STREAM_FLUSH_INTERVAL`, ce qui
            // borne le trafic d'événements sur une sortie bavarde tout en gardant la
            // latence d'affichage très sous la seconde.
            if pending.len() >= STREAM_FLUSH_BYTES || last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
            {
                flush_stream(&mut pending, sink.as_ref());
                last_flush = tokio::time::Instant::now();
            }
        }
        if out.bytes.len() > MAX_OUTPUT {
            let overflow = out.bytes.len() - MAX_OUTPUT;
            out.bytes.drain(0..overflow);
            out.omitted = out.omitted.saturating_add(overflow);
        }
    }
    flush_stream(&mut pending, sink.as_ref());
    out
}

/// Publie la partie UTF-8 complète de `pending` et conserve le reliquat : un
/// caractère multi-octets coupé par une frontière de lecture ne doit pas devenir
/// un `U+FFFD` dans l'affichage.
fn flush_stream(pending: &mut Vec<u8>, sink: Option<&crate::tool::OutputSink>) {
    let Some(sink) = sink else {
        pending.clear();
        return;
    };
    if pending.is_empty() {
        return;
    }
    let valid_up_to = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) => e.valid_up_to(),
    };
    if valid_up_to == 0 {
        // Reliquat plus long qu'un caractère UTF-8 : il ne sera jamais complété,
        // on le rend en lossy plutôt que de le laisser croître.
        if pending.len() > 4 {
            sink(String::from_utf8_lossy(pending).into_owned());
            pending.clear();
        }
        return;
    }
    let rest = pending.split_off(valid_up_to);
    let mut text = String::from_utf8_lossy(pending).into_owned();
    // Backstop : un fragment géant n'apporte rien à un affichage live borné.
    if text.len() > STREAM_CHUNK_MAX {
        text = truncate_tail(&text, STREAM_CHUNK_MAX);
    }
    *pending = rest;
    sink(text);
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
        "[... output truncated, {cut} bytes, beginning omitted]\n{}",
        &body[cut..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_truncation_keeps_the_end_and_marks_omission() {
        let body: String = (0..10).map(|i| format!("line{i}\n")).collect();
        let out = truncate_tail(&body, 20);
        assert!(out.starts_with("[... output truncated, "));
        assert!(out.contains("bytes, beginning omitted]"));
        assert!(out.contains("line9"), "the end should be preserved: {out}");
        assert!(
            !out.contains("line0"),
            "the beginning should be omitted: {out}"
        );
    }

    #[test]
    fn tail_truncation_is_char_boundary_safe() {
        let body = "¢".repeat(100);
        let out = truncate_tail(&body, 51);
        assert!(out.contains("beginning omitted]"));
        assert!(out.ends_with('¢'));
    }

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(truncate_tail("short", 30_000), "short");
    }
}
