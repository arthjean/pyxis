//! Diagnostics of a config load: why an entry is not enabled as is, or what was
//! altered on the way in.
//!
//! Kept apart from the parser on purpose. These are the only strings the human
//! ever sees about their MCP configuration, so an entry is never dropped or
//! narrowed without a reason that can be read back.

use super::McpConfigSource;

/// Reason why an MCP entry is not enabled as is, or was altered on load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpConfigIssueKind {
    Disabled,
    UnsupportedTransport,
    InvalidEntry(String),
    EmptyCommand,
    Shadowed {
        kept_source: McpConfigSource,
    },
    /// Remote URL refused before any connection (US-013 AC6).
    InsecureUrl(String),
    /// An approval level declared by a workspace-controlled file: dropped
    /// (US-015 AC4).
    ApprovalIgnored,
    /// A parallel-calls declaration by a workspace-controlled file: dropped.
    ParallelIgnored,
    /// A bound was clamped to the ceiling the harness accepts.
    TimeoutClamped {
        key: &'static str,
    },
    /// `required` on a server that is not dialed at startup: honoring it would
    /// make the session unstartable behind its own trust gate.
    RequiredIgnored,
    /// A shadowing entry was narrowed down to what the shadowed one allowed
    /// (US-014 AC4).
    FilterNarrowed {
        kept_source: McpConfigSource,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigIssue {
    pub server: String,
    pub source: McpConfigSource,
    pub kind: McpConfigIssueKind,
}

impl McpConfigIssue {
    pub fn summary(&self) -> String {
        let server = &self.server;
        let origin = self.source.short_label();
        match &self.kind {
            McpConfigIssueKind::Disabled => format!("{server} ({origin}) disabled"),
            McpConfigIssueKind::UnsupportedTransport => {
                format!("{server} ({origin}) unsupported transport ignored")
            }
            McpConfigIssueKind::InvalidEntry(err) => {
                format!("{server} ({origin}) invalid entry: {err}")
            }
            McpConfigIssueKind::EmptyCommand => {
                format!("{server} ({origin}) empty command ignored")
            }
            McpConfigIssueKind::Shadowed { kept_source } => format!(
                "{server} ({origin}) shadowed by {}",
                kept_source.short_label()
            ),
            McpConfigIssueKind::InsecureUrl(url) => format!(
                "{server} ({origin}) refused: {url} is not https (only a loopback host may be plain http)"
            ),
            McpConfigIssueKind::ApprovalIgnored => format!(
                "{server} ({origin}) tool approval ignored: a workspace file cannot auto-approve"
            ),
            McpConfigIssueKind::ParallelIgnored => format!(
                "{server} ({origin}) supportsParallelToolCalls ignored: a workspace file cannot lift the serialization"
            ),
            McpConfigIssueKind::TimeoutClamped { key } => {
                format!("{server} ({origin}) {key} clamped to the accepted ceiling")
            }
            McpConfigIssueKind::RequiredIgnored => format!(
                "{server} ({origin}) required ignored: this server is only dialed after an explicit trust"
            ),
            McpConfigIssueKind::FilterNarrowed { kept_source } => format!(
                "{server} ({origin}) tool policy narrowed down to the {} one: a shadowing entry cannot widen",
                kept_source.short_label()
            ),
        }
    }
}
