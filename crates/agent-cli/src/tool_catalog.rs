//! The tool catalog (US-097 -> US-099): what a session really exposes, rendered
//! from the tools the binary registers.
//!
//! The invariant this document serves is stated in `AGENTS.md` and inspectable
//! nowhere: tool output is untrusted by default, and ten implementations of
//! `agent-tools` lower `returns_untrusted` to `false`. Each of those ten was a
//! local decision, reviewed on its own line, in its own pull request, against
//! nothing. An eleventh would pass the same way. Rendered here, they become a
//! population: the summary table counts them, and a flag that flips shows up in
//! the diff of the change that flipped it, next to the others.
//!
//! Three properties make the verdict worth trusting, and each has its own half
//! of the gate:
//!
//! - The render is pure. [`render_tool_catalog`] takes harvested entries and
//!   returns a `String`; it reads no file, writes none and consults no clock, so
//!   the same wiring renders the same bytes on any machine. Every order is an
//!   explicit sort and no `HashMap` reaches it.
//! - Freshness alone is blind. A byte comparison accepts a document a generator
//!   rendered from zero tools, so [`check_wiring`] confronts the manifest with
//!   the `.register(` sites of `main.rs` in BOTH directions, [`harvest`] fails
//!   an entry that instantiates nothing, and the sections are asserted non
//!   empty.
//! - The whole thing reads text and metadata. It launches no process, opens no
//!   socket and reads one environment variable, the write switch, which is what
//!   lets it live inside `cargo test --workspace`.
//!
//! The generator lives under `#[cfg(test)]` inside the binary because
//! `agent-cli` has no `[lib]` target: `crates/agent-cli/tests/` could not import
//! the wiring it has to read, and opening a library target to expose the
//! internals of a binary for a documentary need is a bigger change than this
//! one deserves.
//!
//! `panic!` through `assert!` is the reporting mechanism of the gates below: a
//! stale document has to stop the suite with its path and the command that fixes
//! it, and the workspace denies `clippy::panic` everywhere else on purpose.
#![allow(clippy::panic)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_code_mode::{CodeModeSession, NestedToolBinding, SessionId};
use agent_core::permission::PermissionMode;
use agent_core::provider::ToolKind;
use agent_core::sandbox::SandboxPolicy;
use agent_tools::{
    ApplyPatch, Bash, CodeModeHandle, CodeModeSessionFactory, DynTool, Edit, ExecTool, Glob, Grep,
    Read, Registry, Tool, ToolPolicy, UpdatePlan, ViewImage, WaitTool, Write, WriteStdin,
};
use async_trait::async_trait;

/// The rendered document, relative to the repository root.
pub const CATALOG_DOC: &str = "docs/tool-catalog.md";

/// The module rendering the document, cited in its header.
pub const GENERATOR: &str = "crates/agent-cli/src/tool_catalog.rs";

/// The wiring the completeness guard reads. Never compiled by it: the guard is a
/// text read, so it returns the same verdict on any machine.
pub const WIRING: &str = "crates/agent-cli/src/main.rs";

/// The environment variable that flips the freshness test into writing. Shared
/// with the crate graph, so one switch regenerates every catalog.
pub const UPDATE_VARIABLE: &str = "PYXIS_UPDATE_CATALOGS";

/// The exact command that rewrites the document: the head of the file, and the
/// remedy a failing comparison prints.
pub const REGENERATE_COMMAND: &str =
    "PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis tool_catalog";

/// Ceiling on the rendered document. The schemas are published in full, which is
/// what makes the file useful and what makes it grow; past this the diff stops
/// being readable and the choice of rendering them whole has to be revisited
/// rather than silently paid for.
pub const MAX_BYTES: usize = 500 * 1024;

/// The workspace the render's registry is anchored on. A fixed placeholder: no
/// path of the machine reaches the document, and a test asserts it.
const RENDER_WORKSPACE: &str = "/pyxis";

/// The permission mode the document is rendered under. Nothing in the metadata
/// depends on it today, and declaring it is what makes that falsifiable rather
/// than tacit: a tool whose schema started varying with the mode would make the
/// declared value the thing to check.
const RENDER_MODE: PermissionMode = PermissionMode::Default;

/// Does the rendering provider declare vision (US-011)?
const RENDER_VISION: bool = true;

/// Can the rendering provider encode `ToolKind::Namespace`? False, so every
/// native tool is read at the top level, which is where they all sit.
const RENDER_NAMESPACE_TOOLS: bool = false;

/// What the render declares about the nested catalog `exec` publishes.
///
/// The only tool of the manifest whose description is NOT a function of itself:
/// `ExecTool::description` calls `exec_tool_spec(&self.handle.catalog(), ...)`,
/// and that catalog is filled per step by `CodeModeHandle::bind_step`. A render
/// that opens no session therefore reads the empty-catalog branch, which the
/// document has to say, because the alternative is a description a live session
/// never sends passing for the one it does. The rest of the section is the same
/// text either way: what varies is the tail, and the tail is what this declares.
const RENDER_NESTED_CATALOG: &str = "vide : le rendu ne lie aucune étape, donc `exec` publie sa branche « aucun outil imbriqué » \
     et non le bloc `ts` qu'une session vivante lui accroche (`CodeModeHandle::bind_step`)";

/// The `.register_dyn(` arguments `main.rs` is expected to carry. Exactly one
/// exists: the loop variable over `mcp_startup.tools`, whose count depends on
/// the servers connected at startup and therefore has no place in a document
/// compared byte for byte. A NEW `register_dyn` site is named by the guard
/// rather than quietly joining that exemption.
const DYNAMIC_REGISTRATIONS: &[&str] = &["tool"];

/// Tools no `.register(` site of `main.rs` carries: `Registry::build` adds them
/// itself. They belong in the catalog because a session exposes them, and they
/// are declared here rather than in the manifest so the completeness guard,
/// which reads `main.rs`, keeps comparing like with like.
const IMPLICIT_TOOLS: &[(&str, &str)] = &[(
    "tool_search",
    "enregistré par `Registry::build`, jamais par `main.rs` ; exposé au modèle seulement quand un outil est réellement différé",
)];

/// One line of the manifest: a `.register(` site of `main.rs`, and how to build
/// what that site registers.
struct CatalogEntry {
    /// The path exactly as `main.rs` writes it at its registration site, minus a
    /// trailing `::new`. This string is what the completeness guard compares, so
    /// it is not a label: renaming the type in `main.rs` has to fail here.
    registration: &'static str,
    /// The condition under which `main.rs` reaches that site, when there is one.
    /// Rendered in the catalog, because a tool registered only when V8 starts is
    /// not a tool every session has.
    condition: Option<&'static str>,
    /// Instantiates exactly what the site registers.
    build: fn() -> Vec<Box<dyn DynTool>>,
}

/// One tool of the catalog: what it declares, and the condition of the site that
/// registers it.
pub struct CatalogTool {
    /// Read from the `DynTool` through `Registry::tool_policies` (US-096).
    pub policy: ToolPolicy,
    /// `None` when every session exposes the tool.
    pub condition: Option<String>,
}

/// Boxes one tool for the harvest.
fn one<T: Tool + 'static>(tool: T) -> Vec<Box<dyn DynTool>> {
    vec![agent_tools::into_dyn(tool)]
}

/// A Code Mode handle that opens no session.
///
/// Building the real one runs `code_mode::build`, which initializes V8 to prove
/// it works: a rendering that did that would make the document depend on
/// whether the machine can start a JavaScript engine. `wait` declares its name,
/// description and schema from itself and costs the substitution nothing;
/// `exec` does NOT, and that is what [`RENDER_NESTED_CATALOG`] exists to say:
/// its description carries the nested catalog the handle holds, empty here
/// because no step is bound, so the document declares the branch it published
/// rather than letting a reader take it for the text a live session sends.
struct NoCodeModeSession;

impl CodeModeSessionFactory for NoCodeModeSession {
    fn open(&self, _id: SessionId) -> Result<Arc<CodeModeSession>, String> {
        Err("le rendu du catalogue n'ouvre aucune session Code Mode".to_string())
    }
}

fn code_mode_handle() -> Arc<CodeModeHandle> {
    Arc::new(CodeModeHandle::new(
        Arc::new(NoCodeModeSession),
        NestedToolBinding::default(),
    ))
}

/// A permission broker that grants nothing. `request_permissions` reads its
/// metadata from itself; the broker is only there to build it.
struct NoBroker;

#[async_trait]
impl agent_tools::PermissionBroker for NoBroker {
    async fn request(
        &self,
        _ask: &agent_tools::PermissionAsk,
        _reason: &str,
    ) -> agent_tools::GrantOutcome {
        agent_tools::GrantOutcome::Refused(
            "le rendu du catalogue n'élargit aucun périmètre".to_string(),
        )
    }
}

/// The tools `main.rs` registers, one entry per `.register(` site, in the order
/// the file wires them. The order is documentation only: the render sorts.
const MANIFEST: &[CatalogEntry] = &[
    CatalogEntry {
        registration: "Read",
        condition: None,
        build: || one(Read),
    },
    CatalogEntry {
        registration: "Glob",
        condition: None,
        build: || one(Glob),
    },
    CatalogEntry {
        registration: "Grep",
        condition: None,
        build: || one(Grep),
    },
    CatalogEntry {
        registration: "Write",
        condition: None,
        build: || one(Write),
    },
    CatalogEntry {
        registration: "Edit",
        condition: None,
        build: || one(Edit),
    },
    CatalogEntry {
        registration: "Bash",
        condition: None,
        build: || one(Bash),
    },
    CatalogEntry {
        registration: "UpdatePlan",
        condition: None,
        build: || one(UpdatePlan),
    },
    CatalogEntry {
        registration: "ApplyPatch",
        condition: None,
        build: || one(ApplyPatch),
    },
    CatalogEntry {
        registration: "ViewImage",
        condition: None,
        build: || one(ViewImage),
    },
    CatalogEntry {
        registration: "ExecCommand",
        condition: None,
        build: || one(agent_tools::ExecCommand),
    },
    CatalogEntry {
        registration: "WriteStdin",
        condition: None,
        build: || one(WriteStdin),
    },
    CatalogEntry {
        registration: "agent_tools::CurrentTime",
        condition: None,
        build: || one(agent_tools::CurrentTime),
    },
    CatalogEntry {
        registration: "agent_tools::Sleep",
        condition: None,
        build: || one(agent_tools::Sleep),
    },
    CatalogEntry {
        registration: "agent_tools::GetContextRemaining",
        condition: None,
        build: || one(agent_tools::GetContextRemaining),
    },
    CatalogEntry {
        registration: "agent_tools::NewContextWindow",
        condition: None,
        build: || one(agent_tools::NewContextWindow),
    },
    CatalogEntry {
        registration: "agent_tools::RequestUserInput",
        condition: None,
        build: || one(agent_tools::RequestUserInput),
    },
    CatalogEntry {
        registration: "agent_mcp::ListMcpResources",
        condition: None,
        build: || one(agent_mcp::ListMcpResources::new(catalog())),
    },
    CatalogEntry {
        registration: "agent_mcp::ListMcpResourceTemplates",
        condition: None,
        build: || one(agent_mcp::ListMcpResourceTemplates::new(catalog())),
    },
    CatalogEntry {
        registration: "agent_mcp::ReadMcpResource",
        condition: None,
        build: || one(agent_mcp::ReadMcpResource::new(catalog())),
    },
    CatalogEntry {
        registration: "agent_tools::RequestPermissions",
        condition: None,
        build: || one(agent_tools::RequestPermissions::new(Arc::new(NoBroker))),
    },
    CatalogEntry {
        registration: "agent_tools::ExecTool",
        condition: Some("seulement quand le runtime Code Mode démarre (`code_mode::build`)"),
        build: || one(ExecTool::new(code_mode_handle())),
    },
    CatalogEntry {
        registration: "agent_tools::WaitTool",
        condition: Some("seulement quand le runtime Code Mode démarre (`code_mode::build`)"),
        build: || one(WaitTool::new(code_mode_handle())),
    },
    CatalogEntry {
        registration: "agent_tools::SpawnAgent",
        condition: Some(MULTI_AGENT_CONDITION),
        build: || one(agent_tools::SpawnAgent::new(agents())),
    },
    CatalogEntry {
        registration: "agent_tools::SendMessage",
        condition: Some(MULTI_AGENT_CONDITION),
        build: || one(agent_tools::SendMessage::new(agents())),
    },
    CatalogEntry {
        registration: "agent_tools::FollowupTask",
        condition: Some(MULTI_AGENT_CONDITION),
        build: || one(agent_tools::FollowupTask::new(agents())),
    },
    CatalogEntry {
        registration: "agent_tools::ListAgents",
        condition: Some(MULTI_AGENT_CONDITION),
        build: || one(agent_tools::ListAgents::new(agents())),
    },
    CatalogEntry {
        registration: "agent_tools::WaitAgent",
        condition: Some(MULTI_AGENT_CONDITION),
        build: || one(agent_tools::WaitAgent::new(agents())),
    },
    CatalogEntry {
        registration: "agent_tools::InterruptAgent",
        condition: Some(MULTI_AGENT_CONDITION),
        build: || one(agent_tools::InterruptAgent::new(agents())),
    },
    CatalogEntry {
        registration: "agent_tools::ListJobs",
        condition: None,
        build: || one(agent_tools::ListJobs::new(jobs())),
    },
];

/// The six multi-agent tools are registered unconditionally and EXPOSED per
/// step, from the catalog's `multi_agent_version`: registered is not the same as
/// visible, and the catalog says which one it is describing.
const MULTI_AGENT_CONDITION: &str =
    "toujours enregistré, exposé au modèle selon le `multi_agent_version` du catalogue";

/// A resource catalog with no server connected: the three MCP resource tools
/// read their metadata from themselves.
fn catalog() -> agent_mcp::McpResourceCatalog {
    agent_mcp::McpResourceCatalog::new()
}

/// A supervisor handle bound to nothing: the six multi-agent tools declare their
/// name, description and schema without one.
fn agents() -> Arc<agent_tools::AgentHandle> {
    Arc::new(agent_tools::AgentHandle::new())
}

/// Same late binding as `agents`: the catalog renders a DESCRIPTION, and a
/// description does not need a thread behind the handle.
fn jobs() -> Arc<agent_tools::JobHandle> {
    Arc::new(agent_tools::JobHandle::new())
}

// ─────────────────────────── harvest (US-097, US-098) ───────────────────────────

/// The registry the document is rendered from, built from the harvested tools
/// under the declared configuration.
fn render_registry(tools: Vec<Box<dyn DynTool>>) -> Registry {
    let mut builder = Registry::builder(RENDER_WORKSPACE)
        .mode(RENDER_MODE)
        .vision(RENDER_VISION)
        .namespace_tools(RENDER_NAMESPACE_TOOLS)
        // The render executes nothing, so the perimeter it declares is the one
        // that can do nothing, and it is declared as unenforced because no
        // kernel is carrying it here.
        .sandbox(
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            false,
        );
    for tool in tools {
        builder = builder.register_dyn(tool);
    }
    builder.build()
}

/// Instantiate every manifest entry and read back what a session would expose.
///
/// An entry that yields no tool is the failure a byte comparison cannot see: the
/// document renders, one section is simply missing, and the comparison calls the
/// result fresh. It is reported here, with what to compare.
pub fn harvest() -> Result<Vec<CatalogTool>, Vec<String>> {
    let mut errors = Vec::new();
    let mut tools: Vec<Box<dyn DynTool>> = Vec::new();
    let mut conditions: Vec<(String, Option<&'static str>)> = Vec::new();

    for entry in MANIFEST {
        let produced = (entry.build)();
        if produced.is_empty() {
            errors.push(format!(
                "catalogue: l'entrée « {} » du manifeste n'a produit aucun outil ; comparer sa fermeture `build` au site `.register(` de {WIRING}",
                entry.registration
            ));
            continue;
        }
        for tool in produced {
            let name = tool.name().to_string();
            if conditions.iter().any(|(known, _)| *known == name) {
                errors.push(format!(
                    "catalogue: l'outil « {name} » est produit deux fois ; le registre garde le premier et le second serait absent du document"
                ));
                continue;
            }
            conditions.push((name, entry.condition));
            tools.push(tool);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let policies = render_registry(tools).tool_policies();
    let mut catalog = Vec::new();
    for policy in policies {
        let condition = match conditions
            .iter()
            .find(|(name, _)| *name == policy.name)
            .map(|(_, condition)| condition.map(str::to_string))
        {
            Some(condition) => condition,
            None => match IMPLICIT_TOOLS.iter().find(|(name, _)| *name == policy.name) {
                Some((_, condition)) => Some((*condition).to_string()),
                None => {
                    errors.push(format!(
                        "catalogue: l'outil « {} » est exposé par le registre sans entrée de manifeste ni ligne dans la table des implicites",
                        policy.name
                    ));
                    continue;
                }
            },
        };
        catalog.push(CatalogTool { policy, condition });
    }
    if errors.is_empty() {
        Ok(catalog)
    } else {
        Err(errors)
    }
}

// ─────────────────────── the wiring guard (US-098) ───────────────────────

/// One `.register(...)` or `.register_dyn(...)` site of the wiring.
#[derive(Debug, PartialEq, Eq)]
pub struct RegistrationSite {
    /// 1-indexed line of the file, so a violation names where to look.
    pub line: usize,
    /// The path of the registered type, minus a trailing `::new`.
    pub path: String,
    /// `.register_dyn(`, which registers tools the catalog does not cover.
    pub dynamic: bool,
}

/// Every registration site of the wiring, read as text.
///
/// A file that holds none is an error and not an empty result: a manifest
/// validated against zero site is validated against nothing, which is precisely
/// the shape of a guard that stopped guarding.
pub fn registration_sites(content: &str) -> Result<Vec<RegistrationSite>, Vec<String>> {
    let mut sites = Vec::new();
    let mut errors = Vec::new();
    // Scanned over the whole file rather than line by line: `main.rs` wraps at
    // least one site across two lines, and a per-line reader would call that
    // one unreadable while the compiler reads it perfectly well.
    for (marker, dynamic) in [(".register(", false), (".register_dyn(", true)] {
        let mut cursor = 0;
        while let Some(found) = content.get(cursor..).and_then(|rest| rest.find(marker)) {
            let after = cursor + found + marker.len();
            cursor = after;
            let line = content.get(..after).unwrap_or_default().lines().count();
            match leading_path(content.get(after..).unwrap_or_default()) {
                Some(path) => sites.push(RegistrationSite {
                    line,
                    path,
                    dynamic,
                }),
                None => errors.push(format!(
                    "catalogue: {WIRING}:{line} : site d'enregistrement illisible"
                )),
            }
        }
    }
    if !errors.is_empty() {
        errors.sort();
        return Err(errors);
    }
    if sites.is_empty() {
        return Err(vec![format!(
            "catalogue: {WIRING} illisible ou sans site `.register(` ; un manifeste confronté à zéro site est confronté à rien"
        )]);
    }
    sites.sort_by_key(|site| (site.line, site.dynamic));
    Ok(sites)
}

/// The path a registration site names: dotted-free identifiers joined by `::`,
/// with a trailing `::new` dropped so `Read` and `agent_tools::SpawnAgent::new`
/// reduce to what the manifest declares.
fn leading_path(rest: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    let mut remaining = rest.trim_start();
    loop {
        let end = remaining
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(remaining.len());
        let segment = remaining.get(..end)?;
        if segment.is_empty() {
            return None;
        }
        segments.push(segment);
        remaining = remaining.get(end..)?;
        match remaining.strip_prefix("::") {
            Some(next) => remaining = next,
            None => break,
        }
    }
    if segments.last() == Some(&"new") && segments.len() > 1 {
        segments.pop();
    }
    Some(segments.join("::"))
}

/// Confront the manifest with the wiring, in both directions.
///
/// One direction catches the tool somebody added to `main.rs` and forgot here,
/// which is the failure this epic exists for. The other catches the entry left
/// behind by a tool that was removed, which would document a tool no session
/// holds. Neither is visible to a byte comparison.
pub fn check_wiring(sites: &[RegistrationSite]) -> Vec<String> {
    let mut violations = Vec::new();
    let declared: BTreeSet<&str> = MANIFEST.iter().map(|entry| entry.registration).collect();

    for site in sites {
        if site.dynamic {
            if !DYNAMIC_REGISTRATIONS.contains(&site.path.as_str()) {
                violations.push(format!(
                    "catalogue: {WIRING}:{} : `.register_dyn({})` est un enregistrement dynamique que la garde ne connaît pas ; l'inscrire dans DYNAMIC_REGISTRATIONS ou lui donner une entrée de manifeste",
                    site.line, site.path
                ));
            }
            continue;
        }
        if !declared.contains(site.path.as_str()) {
            violations.push(format!(
                "catalogue: {WIRING}:{} : `{}` est enregistré par le binaire et absent du manifeste de {GENERATOR}",
                site.line, site.path
            ));
        }
    }

    let wired: BTreeSet<&str> = sites
        .iter()
        .filter(|site| !site.dynamic)
        .map(|site| site.path.as_str())
        .collect();
    for entry in MANIFEST {
        if !wired.contains(entry.registration) {
            violations.push(format!(
                "catalogue: l'entrée « {} » du manifeste ne correspond à aucun site `.register(` de {WIRING}",
                entry.registration
            ));
        }
    }
    violations.sort();
    violations.dedup();
    violations
}

// ─────────────────────────── the render (US-097) ───────────────────────────

/// Render the whole document, header included, from the harvested tools.
///
/// Pure: no file, no clock, no environment. Every order is an explicit sort, so
/// two runs on the same wiring produce the same bytes.
pub fn render_tool_catalog(tools: &[CatalogTool]) -> Result<String, Vec<String>> {
    if tools.is_empty() {
        return Err(vec![format!(
            "catalogue: aucun outil récolté ; un catalogue vide n'est pas un catalogue, comparer le manifeste de {GENERATOR} aux sites `.register(` de {WIRING}"
        )]);
    }
    let mut sorted: Vec<&CatalogTool> = tools.iter().collect();
    sorted.sort_by(|left, right| left.policy.name.cmp(&right.policy.name));

    let untrusted = sorted
        .iter()
        .filter(|tool| tool.policy.returns_untrusted)
        .count();
    let trusted = sorted.len() - untrusted;

    let mut out = String::new();
    out.push_str(&format!(
        "<!-- Généré par {GENERATOR} ; ne pas éditer à la main. -->\n"
    ));
    out.push_str(&format!("<!-- Régénérer : {REGENERATE_COMMAND} -->\n"));
    out.push('\n');
    out.push_str("# Catalogue d'outils\n\n");
    out.push_str(&format!(
        "Les {} outils qu'une session de `pyxis` expose, avec leurs propriétés de politique.\n",
        sorted.len()
    ));
    out.push_str(&format!(
        "Ils sont instanciés depuis les sites `.register(` de [`{WIRING}`](../{WIRING}) et lus\n\
         sur les `DynTool` eux-mêmes : une propriété se corrige dans l'outil qui la déclare,\n\
         jamais ici, et ce document est réécrit par la commande de son en-tête.\n\n"
    ));
    out.push_str(
        "La souillure est le sujet de ce document. `AGENTS.md` pose que la sortie d'un outil est\n\
         non fiable par défaut et que les défauts du trait `Tool` sont fermés ; chaque\n\
         désarmement est une décision locale, et rendus ensemble ils deviennent une population\n\
         qu'un relecteur peut compter. Le diff de ce fichier est l'artefact de revue : il n'est\n\
         donc pas marqué `linguist-generated`, ce qui le ferait replier par GitHub.\n\n",
    );
    out.push_str(
        "Les outils MCP dynamiques sont hors périmètre : leur nombre dépend des serveurs\n\
         connectés au démarrage, ils entrent par `.register_dyn(` et aucun document comparé\n\
         octet pour octet ne peut les contenir.\n\n",
    );

    out.push_str("## Configuration de rendu\n\n");
    out.push_str(
        "Les colonnes ci-dessous sont lues sous cette configuration. Aucune propriété rendue\n\
         n'en dépend aujourd'hui : la déclarer est ce qui rend l'hypothèse réfutable plutôt que\n\
         tacite, le jour où un outil ferait varier son schéma avec les capacités du fournisseur.\n\n",
    );
    out.push_str("| Paramètre | Valeur |\n|---|---|\n");
    out.push_str(&format!(
        "| Mode de permission | `{}` |\n",
        RENDER_MODE.id()
    ));
    out.push_str(
        "| Mode de bac à sable | `read-only`, sans accès réseau, non appliqué par le noyau |\n",
    );
    out.push_str(&format!(
        "| Capacité vision du fournisseur | {} |\n",
        yes_no(RENDER_VISION)
    ));
    out.push_str(&format!(
        "| Espaces de noms encodables par le fournisseur | {} |\n",
        yes_no(RENDER_NAMESPACE_TOOLS)
    ));
    out.push_str("| Code Mode | présent, avec une fabrique de sessions qui n'en ouvre aucune |\n");
    out.push_str(&format!(
        "| Catalogue imbriqué de `exec` | {} |\n\n",
        escape_cell(RENDER_NESTED_CATALOG)
    ));

    out.push_str("## Synthèse\n\n");
    out.push_str(&format!(
        "Sur {} outils, **{untrusted} rendent une sortie non fiable** et {trusted} ne le font pas.\n\n",
        sorted.len()
    ));
    out.push_str(
        "| Outil | Espace de noms | Nature | Lecture seule | Concurrence | Sensible | Sensible à la souillure | Sortie non fiable | Différable | Condition d'enregistrement |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for tool in &sorted {
        let policy = &tool.policy;
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            code_cell(&policy.name),
            policy
                .namespace
                .as_deref()
                .map_or_else(|| "aucun".to_string(), code_cell),
            code_cell(kind_label(&policy.kind)),
            yes_no(policy.is_read_only),
            yes_no(policy.is_concurrency_safe),
            yes_no(policy.is_sensitive),
            yes_no(policy.is_taint_sensitive),
            yes_no(policy.returns_untrusted),
            yes_no(policy.is_deferrable),
            tool.condition
                .as_deref()
                .map_or_else(|| "aucune".to_string(), escape_cell),
        ));
    }
    out.push('\n');

    out.push_str("## Outils\n\n");
    out.push_str(
        "La description est celle que le modèle reçoit, non tronquée, et le schéma celui que\n\
         l'outil publie. Les deux restent en anglais, verbatim : traduire une `description()`\n\
         en ferait une copie qui diverge du texte réellement envoyé.\n\n",
    );
    for tool in &sorted {
        let policy = &tool.policy;
        out.push_str(&format!("### {}\n\n", code_cell(&policy.name)));
        if let Some(condition) = &tool.condition {
            out.push_str(&format!("Condition : {}\n\n", escape_cell(condition)));
        }
        out.push_str("Description :\n\n");
        out.push_str(&fenced(&policy.description, "text"));
        out.push_str("\nSchéma d'entrée :\n\n");
        out.push_str(&fenced(&pretty_json(&policy.input_schema), "json"));
        out.push('\n');
    }
    // The trailing blank line of the last section is the document's final
    // newline, and nothing else follows it.
    while out.ends_with("\n\n") {
        out.pop();
    }
    Ok(out)
}

/// The whole document, from the wiring on disk.
pub fn tool_catalog_document() -> Result<String, Vec<String>> {
    render_tool_catalog(&harvest()?)
}

/// `true` and `false` as a reader of French prose reads them.
fn yes_no(value: bool) -> &'static str {
    if value { "oui" } else { "non" }
}

/// The wire shape of a tool, named by the tag its serialization carries.
fn kind_label(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Function { .. } => "function",
        ToolKind::Freeform { .. } => "freeform",
        ToolKind::Namespace { .. } => "namespace",
        ToolKind::ToolSearch { .. } => "tool_search",
        ToolKind::WebSearch { .. } => "web_search",
    }
}

/// A table cell holds one line and no unescaped pipe, whatever the tool wrote.
/// Backticks are left alone: in a cell they open an inline code span, which is
/// exactly what the conditions of this catalog use them for.
fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// A cell rendered as a code span when the value can survive one. A backslash
/// escapes nothing inside a code span, so a value carrying a backtick is
/// rendered as escaped text instead: the table stays valid either way.
fn code_cell(value: &str) -> String {
    if value.contains('`') || value.contains('\n') || value.contains('\r') {
        escape_cell(value).replace('`', "\\`")
    } else {
        format!("`{}`", value.replace('|', "\\|"))
    }
}

/// A fenced block whose fence is longer than the longest backtick run inside it,
/// so a description carrying its own fence cannot end the block early.
fn fenced(body: &str, language: &str) -> String {
    let longest = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let body = body.trim_end_matches('\n');
    format!("{fence}{language}\n{body}\n{fence}\n")
}

/// The schema, pretty-printed. `serde_json` orders object keys by itself, so two
/// runs on the same schema produce the same text.
fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|error| format!("<schéma non sérialisable : {error}>"))
}

// ─────────────────────── freshness (US-099) ───────────────────────

/// The repository root, from the manifest directory of this crate. Resolved at
/// COMPILE time: the gate reads no environment variable but the write switch.
pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Compare a published document to the render. An absent file arrives here as an
/// empty string and is reported as a stale one: the remedy is the same command,
/// so there is no reason to say it differently.
pub fn freshness_violation(published: &str, rendered: &str) -> Option<String> {
    (published != rendered).then(|| {
        format!("catalogue: {CATALOG_DOC} est périmé ; régénérer avec {REGENERATE_COMMAND}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::ToolKind;

    fn wiring() -> String {
        fs::read_to_string(repository_root().join(WIRING)).expect("le câblage est lisible")
    }

    fn published() -> String {
        fs::read_to_string(repository_root().join(CATALOG_DOC)).unwrap_or_default()
    }

    fn policy(name: &str) -> ToolPolicy {
        ToolPolicy {
            name: name.to_string(),
            description: format!("What {name} does."),
            input_schema: serde_json::json!({"type": "object"}),
            kind: ToolKind::Function {
                input_schema: serde_json::json!({"type": "object"}),
                strict: true,
                defer_loading: false,
                output_schema: None,
            },
            is_concurrency_safe: false,
            is_read_only: false,
            is_sensitive: true,
            is_taint_sensitive: true,
            returns_untrusted: true,
            is_deferrable: false,
            namespace: None,
        }
    }

    fn entry(name: &str) -> CatalogTool {
        CatalogTool {
            policy: policy(name),
            condition: None,
        }
    }

    // ─────────── US-099: the published document ───────────

    #[test]
    fn tool_catalog_is_what_the_binary_registers() {
        let rendered = match tool_catalog_document() {
            Ok(rendered) => rendered,
            Err(errors) => panic!("{}", errors.join("\n")),
        };
        let path = repository_root().join(CATALOG_DOC);
        if std::env::var_os(UPDATE_VARIABLE).is_some() {
            fs::write(&path, &rendered).expect("le catalogue publié est écrivable");
            return;
        }
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            rendered,
            "{} est périmé ; régénérer avec {REGENERATE_COMMAND}",
            path.display()
        );
    }

    #[test]
    fn tool_catalog_an_absent_document_is_reported_like_a_stale_one() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        let reported = freshness_violation("", &rendered).expect("un fichier absent est périmé");
        assert!(reported.contains(CATALOG_DOC), "{reported}");
        assert!(reported.contains(REGENERATE_COMMAND), "{reported}");
        assert_eq!(freshness_violation(&rendered, &rendered), None);
    }

    #[test]
    fn tool_catalog_flipping_returns_untrusted_moves_one_row_and_one_section() {
        let before = render_tool_catalog(&[entry("read"), entry("write")])
            .expect("le catalogue se rend avant");
        let mut flipped = entry("write");
        flipped.policy.returns_untrusted = false;
        let after =
            render_tool_catalog(&[entry("read"), flipped]).expect("le catalogue se rend après");

        let moved: Vec<(&str, &str)> = before
            .lines()
            .zip(after.lines())
            .filter(|(left, right)| left != right)
            .collect();
        assert_eq!(before.lines().count(), after.lines().count());
        assert_eq!(moved.len(), 2, "{moved:?}");
        assert!(
            moved
                .iter()
                .any(|(_, right)| right.starts_with("| `write`")),
            "{moved:?}"
        );
        assert!(
            moved
                .iter()
                .any(|(_, right)| right.contains("**1 rendent une sortie non fiable** et 1")),
            "{moved:?}"
        );
    }

    #[test]
    fn tool_catalog_summary_states_both_counts_in_one_reading() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        let tools = harvest().expect("les outils sont récoltés");
        let untrusted = tools
            .iter()
            .filter(|tool| tool.policy.returns_untrusted)
            .count();
        assert!(
            rendered.contains(&format!(
                "Sur {} outils, **{untrusted} rendent une sortie non fiable** et {} ne le font pas.",
                tools.len(),
                tools.len() - untrusted
            )),
            "la synthèse ne dit pas les deux comptes"
        );
        assert!(untrusted > 0 && untrusted < tools.len(), "{untrusted}");
    }

    #[test]
    fn tool_catalog_is_not_folded_by_github_as_a_generated_file() {
        let attributes =
            fs::read_to_string(repository_root().join(".gitattributes")).unwrap_or_default();
        for line in attributes.lines() {
            let line = line.trim();
            if line.starts_with('#') || !line.contains("linguist-generated") {
                continue;
            }
            for catalog in [CATALOG_DOC, "docs/crate-graph.md", "docs/config-catalog.md"] {
                let stem = catalog.trim_start_matches("docs/");
                assert!(
                    !line.contains(catalog) && !line.contains(stem),
                    "{catalog} serait replié par GitHub, or son diff est l'artefact de revue : {line}"
                );
            }
        }
    }

    #[test]
    fn tool_catalog_stays_under_the_bound_that_keeps_its_diff_readable() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        assert!(
            rendered.len() <= MAX_BYTES,
            "le catalogue pèse {} octets pour une borne de {MAX_BYTES}",
            rendered.len()
        );
    }

    #[test]
    fn tool_catalog_gate_launches_no_process_and_opens_no_socket() {
        let file =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tool_catalog.rs"))
                .expect("le source du générateur est lisible");
        // Only the generator is scanned, not this module: a test naming the
        // forbidden strings would otherwise flag itself.
        let generator = file
            .split_once("mod tests {")
            .map(|(before, _)| before)
            .expect("le module de test délimite le générateur");
        for forbidden in [
            "std::pro",
            "Command::new",
            "TcpStream",
            "reqwest",
            "tokio::net",
        ] {
            assert!(
                !generator.contains(forbidden),
                "le générateur du catalogue doit rester une lecture de métadonnées : {forbidden}"
            );
        }
        // The only environment variable read at run time is the write switch,
        // and it is read in the freshness test rather than in the generator.
        assert!(!generator.contains("std::env::var"), "{UPDATE_VARIABLE}");
        // Assembled rather than written, so this assertion is not its own match.
        let switch = format!("std::env::{}", "var_os");
        assert_eq!(file.matches(&switch).count(), 1, "{UPDATE_VARIABLE}");
    }

    // ─────────── US-097: what the render produces ───────────

    #[test]
    fn tool_catalog_render_is_byte_identical_across_two_consecutive_runs() {
        let first = tool_catalog_document().expect("le catalogue se rend");
        let second = tool_catalog_document().expect("le catalogue se rend encore");
        assert_eq!(first, second);
        assert!(
            first.ends_with('\n'),
            "le document finit par un saut de ligne"
        );
        assert!(!first.ends_with("\n\n"), "et par un seul");
        assert!(
            !first.contains('\r'),
            "le document porte des fins de ligne LF"
        );
        assert!(
            !first.contains(repository_root().to_string_lossy().as_ref()),
            "aucun chemin absolu n'atteint le rendu"
        );
    }

    #[test]
    fn tool_catalog_header_names_its_generator_and_the_command_that_rewrites_it() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        let mut lines = rendered.lines();
        let first = lines.next().expect("une première ligne");
        let second = lines.next().expect("une deuxième ligne");
        assert!(
            first.starts_with("<!--") && first.contains(GENERATOR),
            "{first}"
        );
        assert!(
            second.starts_with("<!--") && second.contains(REGENERATE_COMMAND),
            "{second}"
        );
    }

    #[test]
    fn tool_catalog_declares_the_configuration_it_was_rendered_under() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        assert!(rendered.contains("## Configuration de rendu"), "{rendered}");
        for declared in [
            "Mode de permission",
            "Mode de bac à sable",
            "Capacité vision du fournisseur",
            "Espaces de noms encodables par le fournisseur",
            "Code Mode",
            "Catalogue imbriqué de `exec`",
        ] {
            assert!(rendered.contains(declared), "{declared} n'est pas déclaré");
        }
        assert!(
            rendered.contains(RENDER_MODE.id()),
            "le mode n'est pas nommé"
        );
    }

    /// The one description that is not a function of its own tool. If the render
    /// ever binds a step, `exec` publishes the other branch and this fails,
    /// which is the point: the declaration and the published text move together
    /// or the document lies about what the model receives.
    #[test]
    fn tool_catalog_declares_the_nested_catalog_the_exec_description_was_rendered_with() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        assert!(
            rendered.contains(&escape_cell(RENDER_NESTED_CATALOG)),
            "le catalogue imbriqué de `exec` n'est pas déclaré"
        );
        assert!(
            rendered.contains("No nested tool is available in this cell."),
            "la description publiée n'est pas la branche à catalogue vide que la configuration déclare"
        );
        assert!(
            !rendered.contains("Nested tools, on the global `tools` object"),
            "la description publiée porte un catalogue imbriqué que le rendu ne lie pas"
        );
    }

    #[test]
    fn tool_catalog_says_in_a_sentence_that_dynamic_mcp_tools_are_out_of_scope() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        assert!(
            rendered.contains("Les outils MCP dynamiques sont hors périmètre"),
            "l'omission des outils MCP doit être écrite, pas silencieuse"
        );
    }

    #[test]
    fn tool_catalog_gives_every_exposed_tool_a_row_and_a_section() {
        let tools = harvest().expect("les outils sont récoltés");
        let rendered = render_tool_catalog(&tools).expect("le catalogue se rend");
        for tool in &tools {
            assert!(
                rendered.contains(&format!("\n| `{}` |", tool.policy.name)),
                "« {} » n'a pas de ligne de synthèse",
                tool.policy.name
            );
            assert!(
                rendered.contains(&format!("\n### `{}`\n", tool.policy.name)),
                "« {} » n'a pas de section",
                tool.policy.name
            );
            assert!(
                rendered.contains(&tool.policy.description),
                "la description de « {} » est absente ou tronquée",
                tool.policy.name
            );
        }
    }

    #[test]
    fn tool_catalog_every_section_is_asserted_non_empty() {
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        let rows = rendered
            .lines()
            .filter(|line| line.starts_with("| `") && line.matches(" | ").count() >= 9)
            .count();
        let sections = rendered
            .lines()
            .filter(|line| line.starts_with("### "))
            .count();
        let harvested = harvest().expect("les outils sont récoltés").len();
        assert!(harvested > 0);
        assert_eq!(rows, harvested);
        assert_eq!(sections, harvested);
        assert!(rendered.contains("## Synthèse"));
        assert!(rendered.contains("## Outils"));
    }

    #[test]
    fn tool_catalog_an_empty_harvest_fails_instead_of_publishing_an_empty_catalog() {
        let errors = render_tool_catalog(&[]).expect_err("un catalogue vide est refusé");
        let reported = errors.first().expect("une erreur");
        assert!(reported.contains(WIRING), "{reported}");
    }

    #[test]
    fn tool_catalog_escapes_a_pipe_a_backtick_and_a_newline_so_the_table_survives() {
        let mut hostile = entry("read");
        hostile.policy.name = "read|write".to_string();
        hostile.condition = Some("une\ncondition | avec `des` pièges".to_string());
        let rendered = render_tool_catalog(&[hostile]).expect("le catalogue se rend");
        let row = rendered
            .lines()
            .find(|line| line.starts_with("| `read"))
            .expect("la ligne est rendue");
        assert_eq!(row.matches(" | ").count(), 9, "{row}");
        assert!(row.contains("`read\\|write`"), "{row}");
        assert!(row.contains("une condition \\| avec `des` pièges"), "{row}");
        assert!(!row.contains('\n'));
    }

    #[test]
    fn tool_catalog_a_description_carrying_a_fence_does_not_end_its_block_early() {
        let mut fenced_description = entry("bash");
        fenced_description.policy.description =
            "Run this:\n```sh\necho hi\n```\nAnd stop.".to_string();
        let rendered = render_tool_catalog(&[fenced_description]).expect("le catalogue se rend");
        assert!(rendered.contains("````text\nRun this:"), "{rendered}");
        assert!(
            rendered.contains("echo hi\n```\nAnd stop.\n````"),
            "{rendered}"
        );
    }

    #[test]
    fn tool_catalog_a_backtick_in_a_name_leaves_the_code_span_behind() {
        assert_eq!(code_cell("read"), "`read`");
        assert_eq!(code_cell("re`ad"), "re\\`ad");
        assert_eq!(code_cell("read|write"), "`read\\|write`");
    }

    // ─────────── US-098: the completeness guard ───────────

    #[test]
    fn tool_catalog_manifest_names_exactly_what_the_binary_registers() {
        let sites = match registration_sites(&wiring()) {
            Ok(sites) => sites,
            Err(errors) => panic!("{}", errors.join("\n")),
        };
        let violations = check_wiring(&sites);
        assert!(violations.is_empty(), "{}", violations.join("\n"));
        assert_eq!(
            sites.iter().filter(|site| !site.dynamic).count(),
            MANIFEST.len()
        );
    }

    #[test]
    fn tool_catalog_a_registered_tool_absent_from_the_manifest_is_named() {
        let sites = registration_sites(
            "let builder = Registry::builder(w)\n    .register(Read)\n    .register(agent_tools::Nouveau::new(handle));\n",
        )
        .expect("les sites sont lus");
        let violations = check_wiring(&sites);
        let reported = violations
            .iter()
            .find(|violation| violation.contains("agent_tools::Nouveau"))
            .expect("l'outil non déclaré est nommé");
        assert!(reported.contains("main.rs:3"), "{reported}");
        assert!(reported.contains(GENERATOR), "{reported}");
    }

    #[test]
    fn tool_catalog_a_manifest_entry_without_a_registration_site_is_named() {
        let sites = registration_sites(".register(Read)\n").expect("les sites sont lus");
        let violations = check_wiring(&sites);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("« Bash »") && violation.contains(WIRING)),
            "{violations:?}"
        );
    }

    #[test]
    fn tool_catalog_an_unknown_register_dyn_site_is_named_rather_than_exempted() {
        let sites = registration_sites(".register(Read)\n.register_dyn(surprise)\n")
            .expect("les sites sont lus");
        assert!(
            sites
                .iter()
                .any(|site| site.dynamic && site.path == "surprise")
        );
        let violations = check_wiring(&sites);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("surprise")
                    && violation.contains("DYNAMIC_REGISTRATIONS")),
            "{violations:?}"
        );
    }

    #[test]
    fn tool_catalog_the_known_mcp_register_dyn_site_is_not_a_violation() {
        let sites = registration_sites(".register(Read)\n.register_dyn(tool)\n")
            .expect("les sites sont lus");
        assert!(
            !check_wiring(&sites)
                .iter()
                .any(|violation| violation.contains("register_dyn")),
            "la boucle MCP connue ne doit pas être signalée"
        );
    }

    #[test]
    fn tool_catalog_an_empty_wiring_fails_instead_of_validating_a_manifest_against_nothing() {
        for content in ["", "fn main() {}\n"] {
            let errors = registration_sites(content).expect_err("un câblage sans site est refusé");
            let reported = errors.first().expect("une erreur");
            assert!(reported.contains(WIRING), "{reported}");
            assert!(reported.contains("confronté à rien"), "{reported}");
        }
    }

    #[test]
    fn tool_catalog_an_unreadable_registration_site_names_its_line() {
        let errors = registration_sites(".register(Read)\n    .register(\n")
            .expect_err("un site illisible est refusé");
        let reported = errors.first().expect("une erreur");
        assert!(reported.contains("main.rs:2"), "{reported}");
        assert!(reported.contains("illisible"), "{reported}");
    }

    #[test]
    fn tool_catalog_a_conditional_site_is_counted_and_its_condition_is_published() {
        let sites = registration_sites(&wiring()).expect("les sites sont lus");
        for conditional in ["agent_tools::ExecTool", "agent_tools::SpawnAgent"] {
            assert!(
                sites.iter().any(|site| site.path == conditional),
                "{conditional} n'est pas compté comme un site d'enregistrement"
            );
        }
        let rendered = tool_catalog_document().expect("le catalogue se rend");
        assert!(rendered.contains("seulement quand le runtime Code Mode démarre"));
        assert!(rendered.contains("multi_agent_version"));
    }

    #[test]
    fn tool_catalog_a_registration_path_reduces_to_what_the_manifest_declares() {
        assert_eq!(leading_path("Read)").as_deref(), Some("Read"));
        assert_eq!(
            leading_path("agent_tools::SpawnAgent::new(Arc::clone(&h)))").as_deref(),
            Some("agent_tools::SpawnAgent")
        );
        assert_eq!(
            leading_path("agent_mcp::ReadMcpResource::new(").as_deref(),
            Some("agent_mcp::ReadMcpResource")
        );
        assert_eq!(leading_path("new)").as_deref(), Some("new"));
        assert_eq!(leading_path(""), None);
        assert_eq!(leading_path("  ").as_deref(), None);
    }

    #[test]
    fn tool_catalog_an_entry_that_harvests_nothing_says_what_to_compare() {
        // The message is what the guard would print; the guard itself is proved
        // on the real manifest, where every entry does harvest a tool.
        let empty = CatalogEntry {
            registration: "agent_tools::Fantome",
            condition: None,
            build: Vec::new,
        };
        assert!((empty.build)().is_empty());
        let reported = format!(
            "catalogue: l'entrée « {} » du manifeste n'a produit aucun outil ; comparer sa fermeture `build` au site `.register(` de {WIRING}",
            empty.registration
        );
        assert!(reported.contains("agent_tools::Fantome"), "{reported}");
        assert!(reported.contains(WIRING), "{reported}");
    }

    #[test]
    fn tool_catalog_every_manifest_entry_harvests_at_least_one_tool() {
        for entry in MANIFEST {
            assert!(
                !(entry.build)().is_empty(),
                "l'entrée « {} » n'a produit aucun outil",
                entry.registration
            );
        }
    }

    #[test]
    fn tool_catalog_an_implicit_tool_is_declared_rather_than_discovered() {
        let tools = harvest().expect("les outils sont récoltés");
        let search = tools
            .iter()
            .find(|tool| tool.policy.name == "tool_search")
            .expect("`tool_search` est exposé par toute session");
        assert_eq!(
            search.condition.as_deref(),
            Some(IMPLICIT_TOOLS[0].1),
            "un outil que `Registry::build` ajoute doit porter sa condition"
        );
        assert_eq!(tools.len(), MANIFEST.len() + IMPLICIT_TOOLS.len());
    }

    #[test]
    fn tool_catalog_the_published_document_and_the_render_agree_on_the_tool_count() {
        let published = published();
        if published.is_empty() {
            return;
        }
        let harvested = harvest().expect("les outils sont récoltés").len();
        assert!(
            published.contains(&format!("Les {harvested} outils")),
            "le document publié n'annonce pas {harvested} outils"
        );
    }
}
