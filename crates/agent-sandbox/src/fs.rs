//! Confinement FS kernel-level via Landlock (US-020 AC1). Politique : lecture
//! seule sur toute la hiérarchie, lecture+écriture uniquement sous le workspace.
//!
//! **Doit être appelé tôt, sur le thread principal, AVANT la construction du
//! runtime tokio** : un domaine Landlock est hérité par les threads créés
//! *après* la restriction et par les process enfants. Ainsi les workers tokio
//! ET les sous-process Bash héritent du confinement, sans le fragile `pre_exec`
//! post-fork (risque de deadlock malloc). `restrict_self` est irréversible.
//!
//! Landlock NE filtre PAS le réseau (cf. ADR-7 R3) ni les sockets D-Bus
//! → le keyring (Secret Service) et le provider (HTTPS direct) restent
//! fonctionnels ; le réseau des outils est filtré séparément par le proxy.

/// Résultat de l'application du sandbox FS, à présenter à l'utilisateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Confinement kernel effectif (politique FS complète supportée par le kernel).
    Enforced,
    /// Landlock actif mais kernel trop ancien pour garantir toute la politique.
    PartiallyEnforced,
    /// Kernel sans support Landlock effectif → confinement FS **non** garanti.
    NotEnforced,
    /// Plateforme non-Linux → sandbox FS désactivé (Linux-first, AC3).
    UnsupportedPlatform,
}

impl SandboxStatus {
    /// Message d'avertissement si le confinement n'est pas effectif (`None` si OK).
    pub fn warning(&self) -> Option<&'static str> {
        match self {
            SandboxStatus::Enforced => None,
            SandboxStatus::PartiallyEnforced => Some(
                "filesystem sandbox partially applied (incomplete Landlock support on this kernel): reduced guarantees",
            ),
            SandboxStatus::NotEnforced => Some(
                "filesystem sandbox NOT applied (kernel lacks effective Landlock support): writes are not confined",
            ),
            SandboxStatus::UnsupportedPlatform => Some(
                "filesystem sandbox disabled (non-Linux): Pyxis is Linux-first; writes are not confined",
            ),
        }
    }
}

/// Racine writable écartée à la résolution, avec la raison à tracer (US-012 AC2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredRoot {
    pub path: std::path::PathBuf,
    pub reason: IgnoreReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Chemin introuvable ou non résolvable (edge case #6).
    Missing,
    /// Chemin existant mais qui n'est pas un répertoire.
    NotADirectory,
    /// Racine si large que le confinement n'aurait plus de sens (`/`, le home).
    TooBroad,
}

impl IgnoreReason {
    pub fn message(&self) -> &'static str {
        match self {
            IgnoreReason::Missing => "path not found",
            IgnoreReason::NotADirectory => "not a directory",
            IgnoreReason::TooBroad => {
                "root too broad (system root or entire home): confinement would be meaningless"
            }
        }
    }
}

/// Résultat de la résolution des racines writables : ce qui sera accordé, et ce
/// qui a été écarté (à tracer par l'appelant).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WritableRoots {
    pub granted: Vec<std::path::PathBuf>,
    pub ignored: Vec<IgnoredRoot>,
}

impl WritableRoots {
    /// Vue empruntée, forme attendue par [`enforce_process`].
    pub fn as_paths(&self) -> Vec<&std::path::Path> {
        self.granted
            .iter()
            .map(std::path::PathBuf::as_path)
            .collect()
    }
}

/// Résout les racines writables accordées en plus du workspace (US-012).
///
/// Le répertoire temporaire (`$TMPDIR` puis `/tmp`) est toujours candidat : c'est
/// le défaut, et sans configuration le comportement se limite à lui. Chaque
/// chemin est canonicalisé (un `$TMPDIR` symlink est fréquent), dédupliqué, et
/// écarté avec une raison s'il est absent, s'il n'est pas un répertoire, ou s'il
/// est assez large pour vider le confinement de son sens.
///
/// `home` est passé explicitement (et non lu dans l'environnement) pour que la
/// politique soit testable sans muter l'environnement du process.
pub fn resolve_writable_roots(
    configured: &[std::path::PathBuf],
    home: Option<&std::path::Path>,
) -> WritableRoots {
    let mut candidates: Vec<std::path::PathBuf> = vec![std::env::temp_dir()];
    candidates.push(std::path::PathBuf::from("/tmp"));
    candidates.extend(configured.iter().cloned());

    let home = home.and_then(|h| std::fs::canonicalize(h).ok());
    let mut out = WritableRoots::default();
    for candidate in candidates {
        let Ok(real) = std::fs::canonicalize(&candidate) else {
            push_ignored(&mut out, candidate, IgnoreReason::Missing);
            continue;
        };
        if !real.is_dir() {
            push_ignored(&mut out, candidate, IgnoreReason::NotADirectory);
            continue;
        }
        // Trop large : la racine système, le home, ou n'importe quel ancêtre du
        // home (`/home`) — accorder l'un des trois revient à ne rien confiner.
        let too_broad =
            real.parent().is_none() || home.as_ref().is_some_and(|h| h.starts_with(&real));
        if too_broad {
            push_ignored(&mut out, candidate, IgnoreReason::TooBroad);
            continue;
        }
        if !out.granted.contains(&real) {
            out.granted.push(real);
        }
    }
    out
}

fn push_ignored(out: &mut WritableRoots, path: std::path::PathBuf, reason: IgnoreReason) {
    if out.ignored.iter().any(|i| i.path == path) {
        return;
    }
    out.ignored.push(IgnoredRoot { path, reason });
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("landlock: {0}")]
    Landlock(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Applique le confinement FS process-wide : RW sous `workspace`, read-only
/// ailleurs. À appeler sur le thread principal avant le runtime async.
#[cfg(target_os = "linux")]
/// Devices dont l'usage reste autorisé sous confinement : voir la justification à
/// leur ajout dans `enforce_process`. Écrire dans `/dev/null` est sans effet, et
/// `/dev/tty` est déjà le terminal de l'utilisateur, hérité via stdout.
#[cfg(target_os = "linux")]
const STANDARD_DEVICES: &[&str] = &["/dev/tty", "/dev/null"];

/// `writable_files` : fichiers EXISTANTS hors workspace auxquels le process garde
/// un droit d'écriture (`~/.pyxis/settings.toml`). Portée volontairement réduite
/// au fichier, jamais à son dossier : le confinement reste vrai pour tout le reste
/// du home. Un chemin absent est ignoré (la règle Landlock exige un fd ouvrable).
///
/// `writable_roots` : répertoires EXISTANTS accordés en écriture complète, comme
/// le workspace (US-012). Le répertoire temporaire en fait partie par défaut :
/// sans lui, tout outillage passant par `mktemp` échoue et pousse l'utilisateur
/// à désactiver le confinement entier. La liste est résolue et filtrée en amont
/// par [`resolve_writable_roots`] ; ici, un chemin non ouvrable est ignoré.
pub fn enforce_process(
    workspace: &std::path::Path,
    writable_files: &[&std::path::Path],
    writable_roots: &[&std::path::Path],
) -> Result<SandboxStatus, SandboxError> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };

    let abi = ABI::V7;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| SandboxError::Landlock(e.to_string()))?
        .create()
        .map_err(|e| SandboxError::Landlock(e.to_string()))?
        // Lecture + exécution globales : le provider, le keyring D-Bus et la
        // résolution de chemins restent fonctionnels. La confidentialité FS n'est
        // pas l'objectif de cette politique, seulement le confinement en écriture.
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| SandboxError::Landlock(e.to_string()))?,
            AccessFs::from_read(abi),
        ))
        .map_err(|e| SandboxError::Landlock(e.to_string()))?
        // Accès complet uniquement sous le workspace. ABI V7 couvre les droits de
        // write modernes (`truncate`, `ioctl_dev`) quand le kernel les supporte.
        .add_rule(PathBeneath::new(
            PathFd::new(workspace).map_err(|e| SandboxError::Landlock(e.to_string()))?,
            AccessFs::from_all(abi),
        ))
        .map_err(|e| SandboxError::Landlock(e.to_string()))?;

    // Racines writables supplémentaires (US-012) : même politique que le workspace.
    // `from_all` accorde les droits de répertoire (création, suppression, rename),
    // ce qu'exige `mktemp -d` et tout outillage qui écrit sous `$TMPDIR`.
    for root in writable_roots {
        let Ok(fd) = PathFd::new(root) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .map_err(|e| SandboxError::Landlock(e.to_string()))?;
    }

    // Devices standard : sans eux, le confinement casse des usages qu'il n'a jamais
    // visés. `/dev/tty` porte l'ioctl `TIOCGWINSZ` que crossterm interroge pour la
    // taille du terminal — refusé, il retombe sur `tput` et lit 80x24, ce qui fige
    // le TUI dans un coin de l'écran. `/dev/null` est la poubelle d'écriture qu'une
    // partie de l'outillage (git en tête) ouvre systématiquement. Le droit
    // `IoctlDev` ne peut être accordé qu'ici : il est attaché au descripteur à son
    // ouverture, donc un fichier ouvert après l'enforcement ne l'obtient jamais.
    for device in STANDARD_DEVICES {
        let Ok(fd) = PathFd::new(device) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_file(abi)))
            .map_err(|e| SandboxError::Landlock(e.to_string()))?;
    }

    // Écriture au fichier près : `from_file` est le sous-ensemble applicable à un
    // fichier régulier (`WriteFile`, `Truncate`…). `from_all` y ajouterait des
    // droits de répertoire, que le kernel refuse sur un fichier — la ruleset
    // retomberait alors en `PartiallyEnforced` et déclencherait un faux
    // avertissement de confinement dégradé. Les droits de création vivant sur le
    // dossier parent, un fichier supprimé après l'enforcement n'est plus
    // recréable : la sauvegarde échoue alors explicitement.
    for file in writable_files {
        let Ok(fd) = PathFd::new(file) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_file(abi)))
            .map_err(|e| SandboxError::Landlock(e.to_string()))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::Landlock(e.to_string()))?;

    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => SandboxStatus::Enforced,
        RulesetStatus::PartiallyEnforced => SandboxStatus::PartiallyEnforced,
        RulesetStatus::NotEnforced => SandboxStatus::NotEnforced,
    })
}

/// Hors Linux : dégradation explicite (AC3). Le sandbox FS est désactivé ;
/// l'appelant DOIT avertir l'utilisateur via `SandboxStatus::warning`.
#[cfg(not(target_os = "linux"))]
pub fn enforce_process(
    _workspace: &std::path::Path,
    _writable_files: &[&std::path::Path],
    _writable_roots: &[&std::path::Path],
) -> Result<SandboxStatus, SandboxError> {
    Ok(SandboxStatus::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_present_when_not_fully_enforced() {
        assert!(SandboxStatus::Enforced.warning().is_none());
        assert!(SandboxStatus::PartiallyEnforced.warning().is_some());
        assert!(SandboxStatus::NotEnforced.warning().is_some());
        assert!(SandboxStatus::UnsupportedPlatform.warning().is_some());
    }

    // Sur Linux avec kernel Landlock, le confinement réel est prouvé par le spike
    // s5 (process isolé : restrict_self est irréversible). Ici on vérifie juste que
    // l'appel ne panique pas et retourne un statut cohérent, SANS restreindre le
    // process de test (qui doit pouvoir continuer à écrire).
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_degrades() {
        let st = enforce_process(std::path::Path::new("/tmp"), &[], &[]).unwrap();
        assert_eq!(st, SandboxStatus::UnsupportedPlatform);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyxis-roots-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn temp_dir_is_granted_by_default() {
        let roots = resolve_writable_roots(&[], None);
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            roots.granted.contains(&tmp),
            "le répertoire temporaire doit être accordé sans configuration: {roots:?}"
        );
    }

    #[test]
    fn configured_root_is_granted_and_deduplicated() {
        let dir = scratch("granted");
        let roots = resolve_writable_roots(&[dir.clone(), dir.clone()], None);
        assert_eq!(
            roots.granted.iter().filter(|p| **p == dir).count(),
            1,
            "une racine répétée n'est accordée qu'une fois: {roots:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_root_is_ignored_with_a_reason() {
        let missing = std::env::temp_dir().join(format!("pyxis-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let roots = resolve_writable_roots(std::slice::from_ref(&missing), None);
        assert!(!roots.granted.contains(&missing));
        let ignored = roots
            .ignored
            .iter()
            .find(|i| i.path == missing)
            .expect("racine absente tracée");
        assert_eq!(ignored.reason, IgnoreReason::Missing);
    }

    #[test]
    fn file_root_is_ignored_as_not_a_directory() {
        let dir = scratch("file-root");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let roots = resolve_writable_roots(std::slice::from_ref(&file), None);
        assert!(!roots.granted.contains(&file));
        assert!(
            roots
                .ignored
                .iter()
                .any(|i| i.path == file && i.reason == IgnoreReason::NotADirectory)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_root_and_whole_home_are_refused() {
        let home = scratch("home");
        let roots = resolve_writable_roots(
            &[
                std::path::PathBuf::from("/"),
                home.clone(),
                // ancêtre du home : accorder `/home` revient à accorder le home.
                home.parent().unwrap().to_path_buf(),
            ],
            Some(&home),
        );
        for refused in [
            std::path::PathBuf::from("/"),
            home.clone(),
            home.parent().unwrap().to_path_buf(),
        ] {
            assert!(
                !roots.granted.contains(&refused),
                "{} ne doit jamais être accordé: {roots:?}",
                refused.display()
            );
            assert!(
                roots
                    .ignored
                    .iter()
                    .any(|i| i.path == refused && i.reason == IgnoreReason::TooBroad),
                "{} doit être refusé comme trop large: {roots:?}",
                refused.display()
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
