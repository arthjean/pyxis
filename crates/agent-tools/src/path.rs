//! Confinement de chemins au workspace (défense applicative). Normalisation
//! LEXICALE (sans toucher le FS, donc valable même pour un fichier à créer) :
//! on résout `.`/`..` et on vérifie que le résultat reste sous la racine. Le
//! renforcement kernel anti-symlink/anti-évasion est délégué à Landlock (US-020,
//! ARCHITECTURE §4 / invariant sandbox) — ceci est la première ligne, pas la
//! seule.

use std::path::{Component, Path, PathBuf};

use crate::error::{ToolError, ValidationError};

/// Sous-chemins du workspace dont le contenu s'exécute PLUS TARD, hors sandbox et
/// hors proxy, ou qui pilotent Pyxis lui-même (US-013). Un agent détourné par
/// injection indirecte y déposerait du code que le prochain `git commit` de
/// l'utilisateur exécuterait sur sa machine (CVE-2026-26268) ; `.git/config`
/// suffit à rediriger `core.hooksPath` ou à définir un alias, et `.pyxis/` porte
/// la configuration et les sessions de l'agent (motif « configuration-based
/// sandbox escape »).
///
/// `.git` est protégé en entier, et pas seulement `hooks/` et `config` : dans un
/// worktree git, `.git` est un FICHIER `gitdir: …` dont la réécriture déplace la
/// configuration et les hooks vers un répertoire choisi par l'attaquant. Les deux
/// sous-chemins restent listés en tête pour que le refus nomme la zone exacte.
///
/// **Portée : outils d'édition uniquement.** Landlock étant additif, ce droit ne
/// peut pas être soustrait sous le workspace : une commande `bash` garde la
/// possibilité d'écrire dans ces chemins. La limite est documentée dans l'aide de
/// la CLI et dans `docs/CURRENT_STATUS.md` plutôt que passée sous silence.
pub const PROTECTED_SUBPATHS: &[&str] = &[".git/hooks", ".git/config", ".git", ".pyxis"];

/// Normalise lexicalement (résout `.` et `..` sans accès disque, ne suit pas les
/// symlinks). Un `..` qui remonte au-dessus de la racine est une évasion.
fn lexical_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                // Chemin absolu : on repart de zéro (sera re-vérifié contre base).
                out = PathBuf::from(comp.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    Some(out)
}

/// Résout `path` (absolu ou relatif au workspace) et vérifie le confinement.
/// Retourne le chemin normalisé absolu, ou `OutsideWorkspace`.
pub fn confine(workspace: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let requested = Path::new(path);
    let joined = if requested.is_absolute() {
        lexical_normalize(requested)
    } else {
        lexical_join(workspace, requested)
            .ok_or_else(|| ToolError::OutsideWorkspace(path.into()))?
    };
    let root = lexical_normalize(workspace);
    if joined.starts_with(&root) {
        Ok(joined)
    } else {
        Err(ToolError::OutsideWorkspace(path.into()))
    }
}

/// Garde-fou d'écriture appelé depuis `validate_input`, donc AVANT la décision de
/// permission : le refus d'une zone d'exécution différée (US-013) ne dépend
/// d'aucun mode et ne peut être levé ni par `DontAsk` ni par `BypassPermissions`.
/// Un chemin hors workspace n'est pas traité ici : il est refusé par `confine` au
/// moment de l'appel, avec son erreur dédiée.
pub fn guard_protected_path(workspace: &Path, path: &str) -> Result<(), ValidationError> {
    let Ok(target) = confine(workspace, path) else {
        return Ok(());
    };
    ensure_not_protected(workspace, &target, path).map_err(|e| ValidationError::new(e.to_string()))
}

/// Refuse un chemin qui atteint une zone protégée, directement ou par symlink.
pub fn ensure_not_protected(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    if let Some(zone) = protected_zone(&lexical_normalize(workspace), target) {
        return Err(protected_error(display_path, zone));
    }
    // AC2 : une cible qui n'atteint la zone qu'après résolution (symlink dans le
    // checkout, remontée relative déjà normalisée par `confine`) est refusée de la
    // même manière. La comparaison se fait sur les chemins RÉELS des deux côtés.
    let (Ok(real_root), Some(real_target)) = (
        std::fs::canonicalize(workspace),
        resolve_existing_prefix(target),
    ) else {
        return Ok(());
    };
    match protected_zone(&real_root, &real_target) {
        Some(zone) => Err(protected_error(display_path, zone)),
        None => Ok(()),
    }
}

fn protected_error(display_path: &str, zone: &str) -> ToolError {
    ToolError::Rejected(format!(
        "write refused: {display_path} is a protected path ({zone}); its contents run outside the sandbox"
    ))
}

fn protected_zone(root: &Path, path: &Path) -> Option<&'static str> {
    let rel = path.strip_prefix(root).ok()?;
    PROTECTED_SUBPATHS
        .iter()
        .copied()
        .find(|zone| rel.starts_with(Path::new(zone)))
}

/// Résout la partie EXISTANTE de `target` (canonicalisée, donc symlinks suivis) et
/// lui rattache les composants encore absents. `None` si rien ne résout.
fn resolve_existing_prefix(target: &Path) -> Option<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = target.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut out = real;
            out.extend(missing.iter().rev());
            return Some(out);
        }
        missing.push(probe.file_name()?.to_os_string());
        if !probe.pop() {
            return None;
        }
    }
}

/// Vérifie que le plus profond ancêtre existant de `target` résout réellement
/// sous le workspace. À appeler avant `create_dir_all` pour ne pas créer de
/// répertoire via un symlink/junction hors workspace.
pub fn ensure_existing_ancestor_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    let root =
        std::fs::canonicalize(workspace).map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
    let mut probe = target;
    loop {
        match std::fs::symlink_metadata(probe) {
            Ok(_) => {
                let real = std::fs::canonicalize(probe)
                    .map_err(|e| ToolError::Io(format!("{}: {e}", probe.display())))?;
                return if real.starts_with(&root) {
                    Ok(())
                } else {
                    Err(ToolError::OutsideWorkspace(display_path.into()))
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                probe = probe
                    .parent()
                    .ok_or_else(|| ToolError::OutsideWorkspace(display_path.into()))?;
            }
            Err(e) => return Err(ToolError::Io(format!("{}: {e}", probe.display()))),
        }
    }
}

/// Vérifie que `target` lui-même, s'il existe, ne résout pas hors workspace.
/// Pour un fichier nouveau, vérifie son parent après création des dossiers.
pub fn ensure_real_path_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    ensure_existing_ancestor_confined(workspace, target, display_path)?;
    if std::fs::symlink_metadata(target).is_ok() {
        let root = std::fs::canonicalize(workspace)
            .map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
        let real = std::fs::canonicalize(target)
            .map_err(|e| ToolError::Io(format!("{}: {e}", target.display())))?;
        if !real.starts_with(&root) {
            return Err(ToolError::OutsideWorkspace(display_path.into()));
        }
    }
    Ok(())
}

/// Vérifie que tous les composants existants de `target` sont des chemins réels
/// sous le workspace, sans symlink ni reparse point. Les outils natifs refusent
/// volontairement les liens pour éviter qu'un checkout contrôle l'accès à des
/// fichiers hors workspace.
pub fn ensure_existing_path_no_links(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    walk_existing_components(workspace, target, display_path, false)?;
    ensure_real_path_confined(workspace, target, display_path)
}

/// Vérifie un chemin à créer ou remplacer. Les parents existants ne doivent pas
/// contenir de liens ; la cible est vérifiée aussi si elle existe déjà.
pub fn ensure_creatable_path_no_links(
    workspace: &Path,
    target: &Path,
    display_path: &str,
) -> Result<(), ToolError> {
    walk_existing_components(workspace, target, display_path, true)?;
    ensure_real_path_confined(workspace, target, display_path)
}

/// Remplace un fichier par un contenu borné sans écrire à travers un symlink
/// final. Le fichier temporaire est créé dans le même parent avec `create_new`,
/// puis renommé sur la cible après une dernière vérification.
pub async fn replace_file_confined(
    workspace: &Path,
    target: &Path,
    display_path: &str,
    bytes: &[u8],
) -> Result<(), ToolError> {
    if let Some(parent) = target.parent() {
        ensure_existing_ancestor_confined(workspace, parent, display_path)?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::Io(format!("creating parent directory: {e}")))?;
        ensure_existing_path_no_links(workspace, parent, display_path)?;
    }
    ensure_creatable_path_no_links(workspace, target, display_path)?;

    let parent = target
        .parent()
        .ok_or_else(|| ToolError::OutsideWorkspace(display_path.into()))?;
    let stem = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{stem}.pyxis-tmp-{}-{nonce}", std::process::id()));

    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", tmp.display())))?;
        file.write_all(bytes)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", tmp.display())))?;
        file.flush()
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", tmp.display())))?;
    }

    ensure_creatable_path_no_links(workspace, target, display_path)?;
    match std::fs::symlink_metadata(target) {
        Ok(meta) => {
            if is_link_like(&meta) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(ToolError::OutsideWorkspace(display_path.into()));
            }
            if meta.is_dir() {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(ToolError::Rejected(format!(
                    "{display_path} is a directory, not a file"
                )));
            }
            tokio::fs::remove_file(target)
                .await
                .map_err(|e| ToolError::Io(format!("{}: {e}", target.display())))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(ToolError::Io(format!("{}: {e}", target.display())));
        }
    }

    tokio::fs::rename(&tmp, target).await.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::Io(format!("{}: {e}", display_path))
    })?;
    Ok(())
}

fn walk_existing_components(
    workspace: &Path,
    target: &Path,
    display_path: &str,
    allow_missing_leaf: bool,
) -> Result<(), ToolError> {
    let root_lex = lexical_normalize(workspace);
    let root_real =
        std::fs::canonicalize(workspace).map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
    let rel = target
        .strip_prefix(&root_lex)
        .map_err(|_| ToolError::OutsideWorkspace(display_path.into()))?;
    let mut probe = root_real.clone();
    let components: Vec<_> = rel.components().collect();
    for (idx, comp) in components.iter().enumerate() {
        probe.push(comp.as_os_str());
        match std::fs::symlink_metadata(&probe) {
            Ok(meta) => {
                if is_link_like(&meta) {
                    return Err(ToolError::OutsideWorkspace(display_path.into()));
                }
                let real = std::fs::canonicalize(&probe)
                    .map_err(|e| ToolError::Io(format!("{}: {e}", probe.display())))?;
                if !real.starts_with(&root_real) {
                    return Err(ToolError::OutsideWorkspace(display_path.into()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let missing_leaf = idx + 1 == components.len();
                if allow_missing_leaf && missing_leaf {
                    return Ok(());
                }
                return Err(ToolError::Io(format!("{}: {e}", probe.display())));
            }
            Err(e) => return Err(ToolError::Io(format!("{}: {e}", probe.display()))),
        }
    }
    Ok(())
}

fn is_link_like(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Normalise un chemin absolu lexicalement (résout `.`/`..`).
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_stays_in_workspace() {
        let ws = Path::new("/work/repo");
        let p = confine(ws, "src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/work/repo/src/main.rs"));
    }

    #[test]
    fn dotdot_escape_is_rejected() {
        let ws = Path::new("/work/repo");
        assert!(matches!(
            confine(ws, "../secret.txt"),
            Err(ToolError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            confine(ws, "src/../../etc/passwd"),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn absolute_path_outside_is_rejected() {
        let ws = Path::new("/work/repo");
        assert!(matches!(
            confine(ws, "/etc/passwd"),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn absolute_path_inside_is_accepted() {
        let ws = Path::new("/work/repo");
        let p = confine(ws, "/work/repo/src/lib.rs").unwrap();
        assert_eq!(p, PathBuf::from("/work/repo/src/lib.rs"));
    }

    #[test]
    fn interior_dotdot_that_stays_inside_is_ok() {
        let ws = Path::new("/work/repo");
        let p = confine(ws, "src/foo/../bar.rs").unwrap();
        assert_eq!(p, PathBuf::from("/work/repo/src/bar.rs"));
    }

    fn protected_ws(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pyxis-protected-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
        match std::fs::canonicalize(&dir) {
            Ok(real) => real,
            Err(_) => dir,
        }
    }

    #[test]
    fn protected_subpaths_are_refused_for_writes() {
        let ws = protected_ws("direct");
        for path in [
            ".git/hooks/pre-commit",
            ".git/hooks",
            ".git/config",
            ".pyxis/settings.toml",
            // remontée relative : `confine` normalise, la zone reste atteinte.
            "src/../.git/hooks/post-merge",
            // worktree git : `.git` est un fichier `gitdir: …`, le réécrire déplace
            // hooks et config vers un répertoire choisi par l'attaquant.
            ".git",
            ".git/info/exclude",
        ] {
            let err = guard_protected_path(&ws, path).unwrap_err().to_string();
            assert!(
                err.contains("protected path"),
                "{path} doit être refusé comme zone protégée: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn ordinary_workspace_paths_stay_writable() {
        let ws = protected_ws("ordinary");
        for path in [
            "src/main.rs",
            ".github/workflows/ci.yml",
            // proches mais distincts : le préfixe est comparé par composants, donc
            // `.gitignore` n'est pas `.git`.
            ".gitignore",
            ".gitmodules",
            ".pyxis-notes.md",
        ] {
            assert!(
                guard_protected_path(&ws, path).is_ok(),
                "{path} ne doit pas être refusé"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn missing_git_directory_is_not_an_error() {
        // AC5 : un projet sans dépôt git n'est pas un cas d'erreur.
        let ws = std::env::temp_dir().join(format!("pyxis-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        assert!(guard_protected_path(&ws, "src/main.rs").is_ok());
        assert!(guard_protected_path(&ws, ".git/hooks/pre-commit").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_protected_zone_is_refused_like_the_direct_path() {
        let ws = protected_ws("symlink");
        let link = ws.join("hooks-link");
        if std::os::unix::fs::symlink(ws.join(".git/hooks"), &link).is_err() {
            let _ = std::fs::remove_dir_all(&ws);
            return;
        }
        let err = guard_protected_path(&ws, "hooks-link/pre-commit")
            .unwrap_err()
            .to_string();
        assert!(err.contains("protected path"), "{err}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn real_path_rejects_symlink_escape_when_platform_allows_symlink() {
        let root = std::env::temp_dir().join(format!("pyxis-path-root-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("pyxis-path-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join("link");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();
        if !linked {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&outside);
            return;
        }
        let err = ensure_existing_ancestor_confined(&root, &link.join("file.txt"), "link/file.txt")
            .unwrap_err();
        assert!(matches!(err, ToolError::OutsideWorkspace(_)));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
