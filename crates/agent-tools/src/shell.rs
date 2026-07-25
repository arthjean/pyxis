//! Shell d'exécution des commandes (US-014). Source UNIQUE pour l'outil `bash`
//! et pour le bloc `<environment>` annoncé au modèle : ce que Pyxis annonce doit
//! être ce qu'il exécute, sinon le modèle produit des constructions qui échouent.
//!
//! Le shell de connexion n'est retenu que s'il est exécutable ET connu comme
//! compatible POSIX : `fish`, `nu` ou `xonsh` acceptent `-c` mais pas la
//! syntaxe que le modèle produit (`&&`, `2>&1`, `$(...)`, `export`), donc les
//! annoncer reviendrait au même mensonge dans l'autre sens. Repli : `sh`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Shells dont `-c` interprète la syntaxe POSIX que le modèle génère.
const POSIX_SHELLS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "ksh", "ksh93", "mksh", "pdksh", "ash", "busybox",
];

/// Repli universel : présent sur tout système POSIX, sémantique de référence.
pub const FALLBACK: &str = "sh";

/// Un shell de connexion qui refuse de démarrer est constaté à l'exécution
/// (AC4). Le drapeau est process-wide : le tour suivant annonce donc `sh` au
/// modèle, au lieu de répéter une promesse déjà démentie.
static LOGIN_SHELL_UNUSABLE: AtomicBool = AtomicBool::new(false);

/// Shell retenu : `program` est exécuté, `label` est annoncé au modèle. Les deux
/// désignent toujours la même chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellChoice {
    pub program: PathBuf,
    pub label: String,
}

impl ShellChoice {
    fn fallback() -> Self {
        Self {
            program: PathBuf::from(FALLBACK),
            label: FALLBACK.to_string(),
        }
    }

    /// Vrai si ce choix EST déjà le repli (aucun second essai possible).
    pub fn is_fallback(&self) -> bool {
        self.program == Path::new(FALLBACK)
    }
}

/// Shell effectivement utilisé pour exécuter et annoncer.
pub fn resolve() -> ShellChoice {
    #[cfg(windows)]
    {
        ShellChoice {
            program: PathBuf::from("powershell.exe"),
            label: "powershell.exe".to_string(),
        }
    }
    #[cfg(not(windows))]
    {
        if LOGIN_SHELL_UNUSABLE.load(Ordering::Relaxed) {
            return ShellChoice::fallback();
        }
        resolve_from(std::env::var_os("SHELL").as_deref())
    }
}

/// Décision pure (testable sans muter l'environnement du process).
#[cfg(not(windows))]
pub fn resolve_from(login_shell: Option<&std::ffi::OsStr>) -> ShellChoice {
    let Some(raw) = login_shell
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return ShellChoice::fallback();
    };
    let known_posix = raw
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| POSIX_SHELLS.contains(&name));
    if !known_posix || !is_executable(&raw) {
        return ShellChoice::fallback();
    }
    let label = raw.to_string_lossy().into_owned();
    ShellChoice {
        program: raw,
        label,
    }
}

/// Signale que le shell de connexion a refusé de démarrer : les appels suivants,
/// y compris l'annonce faite au modèle, retombent sur `sh`.
pub fn mark_login_shell_unusable() {
    LOGIN_SHELL_UNUSABLE.store(true, Ordering::Relaxed);
}

#[cfg(not(windows))]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn absent_login_shell_falls_back_to_sh() {
        let choice = resolve_from(None);
        assert_eq!(choice.label, "sh");
        assert!(choice.is_fallback());
        assert_eq!(resolve_from(Some(OsStr::new(""))).label, "sh");
    }

    #[test]
    fn missing_binary_falls_back_to_sh() {
        assert_eq!(
            resolve_from(Some(OsStr::new("/nonexistent/bin/bash"))).label,
            "sh"
        );
    }

    #[test]
    fn non_posix_login_shell_falls_back_to_sh() {
        // fish accepte `-c` mais pas `export VAR=1` ni `&&` de la même façon :
        // l'annoncer au modèle produirait des commandes qui échouent.
        assert_eq!(resolve_from(Some(OsStr::new("/usr/bin/fish"))).label, "sh");
        assert_eq!(resolve_from(Some(OsStr::new("/usr/bin/nu"))).label, "sh");
    }

    #[test]
    fn executable_posix_login_shell_is_kept_and_announced_verbatim() {
        // `/bin/sh` existe partout où ce test tourne ; le label doit être le
        // chemin exécuté, pas un alias.
        let choice = resolve_from(Some(OsStr::new("/bin/sh")));
        assert_eq!(choice.program, PathBuf::from("/bin/sh"));
        assert_eq!(choice.label, "/bin/sh");
    }

    #[test]
    fn announced_shell_is_the_executed_one() {
        let choice = resolve();
        assert_eq!(choice.label, choice.program.to_string_lossy());
    }
}
