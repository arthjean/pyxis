//! Contexte projet injecté comme messages `user` ÉPHÉMÈRES par tour (US-028) :
//! AGENTS.md (découvert cwd→`.git`) puis bloc `<environment>`. Lu AVANT le sandbox
//! (comme skills/mcp) car la remontée d'ancêtres devient inaccessible une fois
//! Landlock posé. Le contenu est ré-injecté à chaque requête mais jamais persisté
//! (cf. `agent_core::AgentContext::context_messages`).

use std::path::Path;

use agent_core::message::Message;

/// Budget d'octets du bloc AGENTS.md concaténé (borne le prompt). Aligné sur le
/// défaut historique de Codex (`project_doc_max_bytes`, 32 KiB).
const AGENTS_BUDGET: usize = 32_000;

/// Noms de fichiers d'instructions, par priorité. `CLAUDE.md` est un fallback
/// toléré (AC US-028) pour les dépôts encore en convention Claude Code.
const CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Profondeur max de remontée d'ancêtres (backstop quand aucun `.git` n'est trouvé).
const MAX_WALK_DEPTH: usize = 24;

/// Construit les messages de contexte éphémères : AGENTS.md (si présent) PUIS
/// environnement. Stable (AGENTS.md) avant volatil (date) → préfixe cacheable.
/// `date` est fourni par le harness (cf. [`today_utc`]).
pub fn messages(workspace: &Path, date: &str) -> Vec<Message> {
    let mut out = Vec::new();
    if let Some(agents) = discover_agents_md(workspace) {
        out.push(Message::user(agents));
    }
    out.push(Message::user(environment_block(workspace, date)));
    out
}

/// Découvre et concatène les AGENTS.md de `start` jusqu'au répertoire contenant
/// `.git` (inclus). Ordre de sortie parent→cwd (le plus proche en dernier → prime
/// à la lecture), priorité au plus proche sous budget. `None` si rien trouvé.
fn discover_agents_md(start: &Path) -> Option<String> {
    let mut dirs: Vec<&Path> = Vec::new();
    let mut cur: Option<&Path> = Some(start);
    let mut depth = 0usize;
    while let Some(d) = cur {
        dirs.push(d);
        // S'arrête à la racine du dépôt (`.git`) OU à un cap de profondeur : hors
        // d'un repo, la remontée grimperait sinon jusqu'à `/`, ramassant un AGENTS.md
        // planté en ancêtre (surface d'injection, OWASP LLM01).
        if d.join(".git").exists() || depth >= MAX_WALK_DEPTH {
            break;
        }
        depth += 1;
        cur = d.parent();
    }

    // Collecte du plus PROCHE au plus loin (priorité au proche sous budget).
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for d in dirs.iter().copied() {
        if let Some(content) = read_instructions(d) {
            let section = format!("## {}\n{}", d.display(), content);
            if total + section.len() > AGENTS_BUDGET && !kept.is_empty() {
                break;
            }
            total += section.len();
            kept.push(section);
        }
    }
    if kept.is_empty() {
        return None;
    }
    kept.reverse(); // → parent→cwd (le plus proche en dernier)
    let mut body = kept.join("\n\n");
    // backstop dur (char-safe) si une seule section dépasse le budget.
    if body.len() > AGENTS_BUDGET {
        let mut cut = AGENTS_BUDGET;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    Some(format!(
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>cwd: {}\n\n{}\n</INSTRUCTIONS>",
        start.display(),
        body
    ))
}

/// Lit le premier fichier d'instructions non vide d'un répertoire (AGENTS.md, puis
/// fallback CLAUDE.md). `None` si aucun n'existe ou tous vides. Durci : rejette les
/// symlinks et non-fichiers (symlink → secret/device) et lit AU PLUS `AGENTS_BUDGET`
/// octets (un fichier géant ne sature pas la RAM avant la borne — DoS au démarrage).
fn read_instructions(dir: &Path) -> Option<String> {
    for name in CANDIDATES {
        let path = dir.join(name);
        // `symlink_metadata` ne suit PAS le lien : un symlink a `is_file() == false`.
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => {}
            _ => continue,
        }
        if let Some(s) = read_capped(&path, AGENTS_BUDGET + 1) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Lit au plus `cap` octets d'un fichier (borne mémoire). UTF-8 lossy (un AGENTS.md
/// non-UTF8 ne fait pas échouer la lecture).
fn read_capped(path: &Path, cap: usize) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(cap as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Bloc environnement (US-028) : cwd, shell, date, fuseau. Message `user` injecté
/// chaque tour. Shell/fuseau best-effort depuis l'env (défauts `sh`/`UTC`).
fn environment_block(workspace: &Path, date: &str) -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let timezone = std::env::var("TZ").unwrap_or_else(|_| "UTC".to_string());
    format!(
        "<environment>\n<cwd>{}</cwd>\n<shell>{}</shell>\n<current_date>{}</current_date>\n<timezone>{}</timezone>\n</environment>",
        workspace.display(),
        shell,
        date,
        timezone
    )
}

/// Date UTC `YYYY-MM-DD` (fournie au bloc environnement). Calculée sans dépendance
/// externe via l'algorithme de Howard Hinnant.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `(année, mois, jour)` civils depuis un nombre de jours epoch (inverse de
/// `days_from_civil`, Howard Hinnant, domaine public).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("numen-ctx-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // marqueur de racine pour borner la remontée d'ancêtres.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn agents_md_discovered_and_wrapped() {
        let ws = tmp("agents");
        std::fs::write(ws.join("AGENTS.md"), "Use bun, never npm.").unwrap();
        let msgs = messages(&ws, "2026-06-17");
        // 2 messages : AGENTS.md puis environnement.
        assert_eq!(msgs.len(), 2);
        let agents = msgs[0].text();
        assert!(agents.contains("# AGENTS.md instructions"));
        assert!(agents.contains("<INSTRUCTIONS>cwd: "));
        assert!(agents.contains("Use bun, never npm."));
    }

    #[test]
    fn no_agents_md_yields_only_env_no_error() {
        let ws = tmp("noagents");
        let msgs = messages(&ws, "2026-06-17");
        assert_eq!(msgs.len(), 1, "seul le bloc env est injecté");
        assert!(msgs[0].text().contains("<environment>"));
    }

    #[test]
    fn claude_md_is_tolerated_fallback() {
        let ws = tmp("claude");
        std::fs::write(ws.join("CLAUDE.md"), "Projet en Rust.").unwrap();
        let msgs = messages(&ws, "2026-06-17");
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].text().contains("Projet en Rust."));
    }

    #[test]
    fn multi_level_concatenated_parent_to_cwd() {
        let root = tmp("multi");
        std::fs::write(root.join("AGENTS.md"), "ROOT_RULES").unwrap();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "SUB_RULES").unwrap();
        let msgs = messages(&sub, "2026-06-17");
        let agents = msgs[0].text();
        let root_at = agents.find("ROOT_RULES").expect("root présent");
        let sub_at = agents.find("SUB_RULES").expect("sub présent");
        assert!(
            root_at < sub_at,
            "ordre parent→cwd (le plus proche en dernier)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_agents_md_is_rejected() {
        // durcissement : un AGENTS.md symlink vers un secret ne doit PAS être lu.
        let ws = tmp("symlink");
        std::fs::write(ws.join("secret.txt"), "SECRET_CONTENT").unwrap();
        std::os::unix::fs::symlink(ws.join("secret.txt"), ws.join("AGENTS.md")).unwrap();
        let msgs = messages(&ws, "2026-06-17");
        assert_eq!(msgs.len(), 1, "symlink ignoré → bloc env seul");
        assert!(!msgs[0].text().contains("SECRET_CONTENT"));
    }

    #[test]
    fn env_block_has_required_quartet() {
        let ws = tmp("env");
        let block = environment_block(&ws, "2026-06-17");
        assert!(block.contains("<cwd>"));
        assert!(block.contains("<shell>"));
        assert!(block.contains("<current_date>2026-06-17</current_date>"));
        assert!(block.contains("<timezone>"));
    }

    #[test]
    fn civil_from_days_epoch_anchors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        // today_utc renvoie un format YYYY-MM-DD plausible.
        let today = today_utc();
        assert_eq!(today.len(), 10);
        assert_eq!(today.as_bytes()[4], b'-');
    }
}
