//! Confinement seen from the tools (US-004): what the sandbox blocked during a
//! call, how it is named to the model, and what a one-call escalation can
//! actually widen.
//!
//! The perimeter itself is `agent_core::sandbox::SandboxPolicy` (pure data).
//! This module only adds the runtime side: observing a block, classifying a
//! failure, and holding the RAII token of a widening. `agent-tools` still knows
//! nothing about `agent-sandbox`: both the observer and the escalator are traits
//! and closures injected by the binary.

/// Why a tool call failed, when the confinement is the cause. Producing this
/// variant is a claim: an ambiguous failure must stay `None` (US-004 AC6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDenial {
    /// The network allow-list refused a host. The ONE perimeter Pyxis can widen
    /// in place, because it lives in the proxy and not in the kernel.
    Network { host: String, allowed: String },
    /// The kernel FS confinement (Landlock) refused a write. A Landlock domain
    /// is inherited and IRREVERSIBLE: no in-process escalation can lift it, so
    /// this variant is named to the user and never offered for escalation.
    Filesystem,
}

impl SandboxDenial {
    /// Can this cause be lifted for a single call? Only the network can.
    pub fn is_escalatable(&self) -> bool {
        matches!(self, Self::Network { .. })
    }

    /// Line appended to the tool result, so the MODEL reads the cause instead of
    /// retrying variants of the same command (US-004 AC1).
    pub fn explain(&self) -> String {
        match self {
            Self::Network { host, allowed } => format!(
                "[sandbox] blocked by the network allow-list: {host} (allowed: {allowed}). \
                 This is a confinement refusal, not a failure of the command itself."
            ),
            Self::Filesystem => {
                ("[sandbox] the failure is attributable to the filesystem confinement. \
                 The Landlock perimeter is process-wide and irreversible: it cannot be \
                 widened for one call, only chosen again at startup. This is a confinement \
                 refusal, not a failure of the command itself.")
                    .to_string()
            }
        }
    }

    /// Question put to the user when an escalation is possible.
    pub fn escalation_reason(&self) -> String {
        match self {
            Self::Network { host, allowed } => format!(
                "the sandbox blocked {host} (allowed: {allowed}); re-run this one call with {host} allowed?"
            ),
            Self::Filesystem => "the sandbox blocked a write".to_string(),
        }
    }
}

/// What the confinement did while a call was running. Implemented by the binary
/// over the proxy handle; absent in tests and in any build without a proxy.
pub trait SandboxObserver: Send + Sync {
    /// Opaque position in the block log, taken BEFORE the call.
    fn mark(&self) -> usize;
    /// Hosts the network allow-list blocked since `mark`.
    fn blocked_since(&self, mark: usize) -> Vec<String>;
    /// Hosts the policy allows, rendered for a message.
    fn allowed(&self) -> String;
}

/// Opaque RAII token of a one-call widening. Dropping it revokes the grant
/// (US-004 AC3): the escalation cannot outlive the call by construction, not by
/// discipline.
pub type EscalationGuard = Box<dyn Send + Sync>;

/// Turns an approved escalation into an actual widening.
///
/// Two methods, in this order, and that order is the point: `can_lift` answers
/// WITHOUT touching any perimeter, so the user is never asked to approve a
/// widening that would not happen; `lift` is only reached after consent, so no
/// perimeter opens before it is granted.
pub trait SandboxEscalator: Send + Sync {
    /// Is this cause liftable HERE? Must have no effect.
    fn can_lift(&self, denial: &SandboxDenial) -> bool;
    /// Widens for exactly as long as the returned guard lives.
    fn lift(&self, denial: &SandboxDenial) -> Option<EscalationGuard>;
}

/// Signals a shell failure caused by the kernel FS confinement. Deliberately
/// narrow: each pattern is a message the kernel or libc produces for a denied
/// access, and a broader net would classify ordinary permission problems as
/// sandbox refusals.
const FILESYSTEM_DENIAL_SIGNALS: &[&str] = &[
    "permission denied",
    "read-only file system",
    "operation not permitted",
];

/// Classifies a failed shell call (US-004 AC1, AC6).
///
/// Fail-closed means fail-closed on the CLAIM, not on the command: an
/// unrecognized failure returns `None` and no escalation is offered. In
/// particular a filesystem signal is only attributed to the sandbox when the
/// kernel confinement is really in force; otherwise it is an ordinary
/// permission problem the user must fix themselves.
pub fn classify_failure(
    enforced: bool,
    blocked_hosts: &[String],
    allowed: &str,
    body: &str,
) -> Option<SandboxDenial> {
    // The proxy is authoritative: it recorded the block itself, so there is
    // nothing to infer. It also outranks a filesystem signal, since a blocked
    // download often fails while writing its output too.
    if let Some(host) = blocked_hosts.last() {
        return Some(SandboxDenial::Network {
            host: host.clone(),
            allowed: allowed.to_string(),
        });
    }
    if !enforced {
        return None;
    }
    let lowered = body.to_ascii_lowercase();
    FILESYSTEM_DENIAL_SIGNALS
        .iter()
        .any(|signal| lowered.contains(signal))
        .then_some(SandboxDenial::Filesystem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_block_is_a_network_denial() {
        let denial = classify_failure(
            true,
            &["evil-github.com".to_string()],
            "github.com",
            "curl: (56) Received HTTP code 403 from proxy after CONNECT",
        )
        .expect("a recorded block is unambiguous");
        assert_eq!(
            denial,
            SandboxDenial::Network {
                host: "evil-github.com".to_string(),
                allowed: "github.com".to_string(),
            }
        );
        assert!(denial.is_escalatable());
        assert!(denial.explain().contains("evil-github.com"));
    }

    #[test]
    fn a_write_refusal_under_landlock_is_a_filesystem_denial() {
        let denial = classify_failure(true, &[], "none", "touch: /etc/x: Permission denied")
            .expect("an enforced sandbox explains the refusal");
        assert_eq!(denial, SandboxDenial::Filesystem);
        // US-004: named, never offered for escalation, because the kernel
        // domain cannot be lifted in place.
        assert!(!denial.is_escalatable());
        assert!(denial.explain().contains("irreversible"));
    }

    #[test]
    fn the_same_message_without_a_sandbox_is_not_the_sandboxs_doing() {
        // US-004 AC6: no sandbox in force -> the claim would be false, so no claim.
        assert!(classify_failure(false, &[], "none", "touch: /etc/x: Permission denied").is_none());
    }

    #[test]
    fn an_ordinary_failure_is_never_attributed_to_the_sandbox() {
        for body in [
            "error[E0308]: mismatched types\n[exit code 101]",
            "fatal: not a git repository",
            "command not found: cargo",
        ] {
            assert!(
                classify_failure(true, &[], "none", body).is_none(),
                "{body} must not be read as a confinement refusal"
            );
        }
    }

    #[test]
    fn a_network_block_outranks_a_filesystem_signal() {
        let denial = classify_failure(
            true,
            &["registry.test".to_string()],
            "none",
            "error: failed to write cache: Permission denied",
        )
        .expect("classified");
        assert!(matches!(denial, SandboxDenial::Network { .. }));
    }
}
