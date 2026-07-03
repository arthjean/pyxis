//! Modèle de permissions (5 modes, ARCHITECTURE §4.4) + défense taint (§4.6,
//! OWASP LLM01). La décision finale combine : le mode courant, la décision
//! *baseline* propre à l'outil, sa nature (read-only / sensible), et la présence
//! de **taint récent**. Une action mutante ou sensible déclenchée en présence de
//! taint récent **force `Ask`** dans tous les modes sauf
//! `BypassPermissions` (invariant 3).
//!
//! La frontière interactive est le trait `Approver` : le pipeline ne sait pas
//! *comment* on demande (TUI, `-p`, auto) — il appelle `approve()`. Testable
//! headless via un approbateur scripté.

use async_trait::async_trait;

/// Les 5 modes de permission (ARCHITECTURE §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Demande sur action sensible.
    #[default]
    Default,
    /// Auto-accepte les éditions de fichiers, demande le reste.
    AcceptEdits,
    /// N'interrompt jamais (automatisations contrôlées).
    DontAsk,
    /// Court-circuite tous les checks (usage avancé / sous sandbox).
    BypassPermissions,
    /// Lecture seule : aucune mutation autorisée (phase de planification).
    Plan,
}

/// Décision *baseline* d'un outil pour une entrée donnée, avant application des
/// règles globales (mode + taint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Autorisé sans confirmation.
    Allow,
    /// Confirmation humaine requise.
    Ask,
    /// Interdit (l'outil ne s'exécutera pas).
    Deny,
}

/// Contexte passé à `Tool::permission` pour décider la baseline.
#[derive(Debug, Clone, Copy)]
pub struct PermCtx {
    pub mode: PermissionMode,
    /// Du taint untrusted a-t-il été produit récemment ? (défense injection.)
    pub taint_recent: bool,
}

/// Issue de la résolution finale (ce que le Registry applique).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// Exécuter directement.
    Allow,
    /// Demander confirmation à l'`Approver` avant d'exécuter.
    Ask,
    /// Refuser (ne pas exécuter, renvoyer une erreur à l'agent).
    Deny,
}

/// Résout la décision finale. PURE (sans I/O) → testable unitairement.
///
/// Priorité :
/// 1. `BypassPermissions` → toujours `Allow` (court-circuit total).
/// 2. `Plan` → `Allow` si l'outil est read-only, sinon `Deny` (aucune mutation).
/// 3. Sinon : on part de la baseline outil, on la met en forme selon le mode,
///    puis le **taint** force `Ask` pour une action mutante/sensible (sauf Bypass, déjà
///    traité) — invariant 3 / §4.6.
pub fn resolve_permission(
    mode: PermissionMode,
    baseline: PermissionDecision,
    is_read_only: bool,
    is_sensitive: bool,
    is_taint_sensitive: bool,
    taint_recent: bool,
) -> Resolved {
    // 1. Bypass : court-circuit (même le taint ne s'applique pas).
    if mode == PermissionMode::BypassPermissions {
        return Resolved::Allow;
    }
    // 2. Plan : lecture seule stricte.
    if mode == PermissionMode::Plan {
        return if is_read_only {
            Resolved::Allow
        } else {
            Resolved::Deny
        };
    }

    // 3. Mise en forme de la baseline selon le mode.
    let shaped = match baseline {
        PermissionDecision::Deny => Resolved::Deny,
        PermissionDecision::Allow => Resolved::Allow,
        PermissionDecision::Ask => match mode {
            // Default : on respecte la demande.
            PermissionMode::Default => Resolved::Ask,
            // AcceptEdits : auto-accepte les éditions (non sensibles) ; garde la
            // demande sur les actions sensibles (destructive/réseau).
            PermissionMode::AcceptEdits => {
                if is_sensitive {
                    Resolved::Ask
                } else {
                    Resolved::Allow
                }
            }
            // DontAsk : n'interrompt jamais (sous réserve du taint, ci-dessous).
            PermissionMode::DontAsk => Resolved::Allow,
            // Plan / Bypass déjà traités.
            PermissionMode::Plan | PermissionMode::BypassPermissions => Resolved::Allow,
        },
    };

    // Taint : une action mutante/sensible en contexte taché force la confirmation, quel
    // que soit le mode (hors Bypass, déjà retourné). C'est la mitigation directe
    // de l'injection indirecte (§4.6).
    if taint_recent && is_taint_sensitive && shaped == Resolved::Allow {
        return Resolved::Ask;
    }
    shaped
}

/// Demande de confirmation présentée à l'utilisateur (via l'`Approver`).
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: String,
    pub reason: String,
    /// Résumé court de l'entrée (ex. la commande Bash, le chemin écrit).
    pub input_summary: String,
    /// Entrée structurée brute — permet au frontend de rendre un aperçu riche
    /// (diff pour `edit`, commande pour `bash`) dans le dialog de permission.
    pub input: serde_json::Value,
}

/// Frontière interactive : le pipeline délègue la confirmation ici. La CLI/TUI
/// fournit une implémentation réelle (prompt) ; les tests un double scripté.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Retourne `true` si l'action est autorisée.
    async fn approve(&self, req: &PermissionRequest) -> bool;
}

/// Approbateur qui accepte tout (mode `-p --yes`, ou tests du chemin passant).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoApprove;

#[async_trait]
impl Approver for AutoApprove {
    async fn approve(&self, _req: &PermissionRequest) -> bool {
        true
    }
}

/// Approbateur qui refuse tout (fail-closed : défaut sûr en headless sans
/// interlocuteur, ou tests du chemin refus).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    async fn approve(&self, _req: &PermissionRequest) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bash-like : sensible, mutant. Edit-like : non sensible, mutant mais protégée
    // par le taint. Read-like : read-only, non sensible.
    const SENSITIVE: (bool, bool, bool) = (
        /*read_only*/ false, /*sensitive*/ true, /*taint*/ true,
    );
    const EDIT: (bool, bool, bool) = (false, false, true);
    const READ: (bool, bool, bool) = (true, false, false);

    fn res(
        mode: PermissionMode,
        base: PermissionDecision,
        kind: (bool, bool, bool),
        taint: bool,
    ) -> Resolved {
        resolve_permission(mode, base, kind.0, kind.1, kind.2, taint)
    }

    #[test]
    fn bypass_short_circuits_everything() {
        // Même une action sensible tachée passe en Bypass.
        assert_eq!(
            res(
                PermissionMode::BypassPermissions,
                PermissionDecision::Ask,
                SENSITIVE,
                true
            ),
            Resolved::Allow
        );
    }

    #[test]
    fn plan_is_read_only() {
        assert_eq!(
            res(PermissionMode::Plan, PermissionDecision::Allow, READ, false),
            Resolved::Allow
        );
        // Toute mutation est refusée en Plan, même baseline Allow.
        assert_eq!(
            res(PermissionMode::Plan, PermissionDecision::Allow, EDIT, false),
            Resolved::Deny
        );
        assert_eq!(
            res(
                PermissionMode::Plan,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Deny
        );
    }

    #[test]
    fn default_mode_asks_on_sensitive_allows_reads() {
        // US-013 AC1 : Default → demande sur action mutante/réseau.
        assert_eq!(
            res(
                PermissionMode::Default,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Ask
        );
        assert_eq!(
            res(
                PermissionMode::Default,
                PermissionDecision::Allow,
                READ,
                false
            ),
            Resolved::Allow
        );
    }

    #[test]
    fn accept_edits_auto_accepts_edits_keeps_ask_on_sensitive() {
        // Édition (non sensible, baseline Ask) → auto-acceptée.
        assert_eq!(
            res(
                PermissionMode::AcceptEdits,
                PermissionDecision::Ask,
                EDIT,
                false
            ),
            Resolved::Allow
        );
        // Action sensible → reste Ask.
        assert_eq!(
            res(
                PermissionMode::AcceptEdits,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Ask
        );
    }

    #[test]
    fn taint_forces_ask_overriding_dontask() {
        // US-013 AC3 / §4.6 : DontAsk autoriserait, mais taint récent + sensible
        // → confirmation forcée (override du mode, hors Bypass).
        assert_eq!(
            res(
                PermissionMode::DontAsk,
                PermissionDecision::Ask,
                SENSITIVE,
                false
            ),
            Resolved::Allow,
            "sans taint, DontAsk n'interrompt pas"
        );
        assert_eq!(
            res(
                PermissionMode::DontAsk,
                PermissionDecision::Ask,
                SENSITIVE,
                true
            ),
            Resolved::Ask,
            "avec taint récent, l'action sensible force la confirmation"
        );
    }

    #[test]
    fn taint_forces_ask_on_edits_without_breaking_accept_edits() {
        // Une édition reste auto-acceptée sans taint.
        assert_eq!(
            res(
                PermissionMode::AcceptEdits,
                PermissionDecision::Ask,
                EDIT,
                false
            ),
            Resolved::Allow
        );
        // Mais le taint protège aussi les mutations non sensibles au sens normal.
        assert_eq!(
            res(PermissionMode::DontAsk, PermissionDecision::Ask, EDIT, true),
            Resolved::Ask
        );
    }

    #[test]
    fn taint_does_not_force_ask_on_read_only_tools() {
        assert_eq!(
            res(PermissionMode::DontAsk, PermissionDecision::Ask, READ, true),
            Resolved::Allow
        );
    }

    #[test]
    fn explicit_deny_is_terminal() {
        assert_eq!(
            res(
                PermissionMode::Default,
                PermissionDecision::Deny,
                READ,
                false
            ),
            Resolved::Deny
        );
    }
}
