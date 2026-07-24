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

pub fn enforce_process(workspace: &std::path::Path) -> Result<SandboxStatus, SandboxError> {
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
pub fn enforce_process(_workspace: &std::path::Path) -> Result<SandboxStatus, SandboxError> {
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
        let st = enforce_process(std::path::Path::new("/tmp")).unwrap();
        assert_eq!(st, SandboxStatus::UnsupportedPlatform);
    }
}
