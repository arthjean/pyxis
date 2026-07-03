//! Confinement de chemins au workspace (défense applicative). Normalisation
//! LEXICALE (sans toucher le FS, donc valable même pour un fichier à créer) :
//! on résout `.`/`..` et on vérifie que le résultat reste sous la racine. Le
//! renforcement kernel anti-symlink/anti-évasion est délégué à Landlock (US-020,
//! ARCHITECTURE §4 / invariant sandbox) — ceci est la première ligne, pas la
//! seule.

use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

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
            .map_err(|e| ToolError::Io(format!("création du dossier parent: {e}")))?;
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
                    "{display_path} est un répertoire, pas un fichier"
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
