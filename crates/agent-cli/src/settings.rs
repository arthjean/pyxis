use std::io;
use std::path::{Path, PathBuf};

use agent_tools::PermissionMode;

const SETTINGS_FILE: &str = "settings.toml";
const PERMISSION_MODE_KEY: &str = "permission_mode";
const REASONING_EFFORT_KEY: &str = "reasoning_effort";
const MODEL_KEY: &str = "model";

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

pub fn load_permission_mode(path: &Path) -> io::Result<Option<PermissionMode>> {
    Ok(load_string_key(path, PERMISSION_MODE_KEY)?
        .as_deref()
        .and_then(permission_mode_from_arg))
}

pub fn save_permission_mode(path: &Path, mode: PermissionMode) -> io::Result<()> {
    save_string_key(path, PERMISSION_MODE_KEY, Some(permission_mode_id(mode)))
}

pub fn load_reasoning_effort(path: &Path) -> io::Result<Option<String>> {
    Ok(load_string_key(path, REASONING_EFFORT_KEY)?.filter(|value| !value.trim().is_empty()))
}

pub fn save_reasoning_effort(path: &Path, effort: Option<&str>) -> io::Result<()> {
    save_string_key(
        path,
        REASONING_EFFORT_KEY,
        effort.map(str::trim).filter(|value| !value.is_empty()),
    )
}

pub fn load_model(path: &Path) -> io::Result<Option<String>> {
    Ok(load_string_key(path, MODEL_KEY)?.filter(|value| !value.trim().is_empty()))
}

pub fn save_model(path: &Path, model: &str) -> io::Result<()> {
    save_string_key(
        path,
        MODEL_KEY,
        Some(model.trim()).filter(|v| !v.is_empty()),
    )
}

/// Crée le fichier (vide) et son dossier s'ils manquent. À appeler AVANT le
/// sandbox : Landlock ne sait accorder un droit d'écriture qu'à un chemin déjà
/// ouvrable, et le dossier parent reste, lui, en lecture seule.
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

fn load_string_key(path: &Path, expected_key: &str) -> io::Result<Option<String>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    Ok(contents.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != expected_key {
            return None;
        }
        parse_tomlish_string(value.trim()).map(str::to_string)
    }))
}

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

fn parse_tomlish_string(value: &str) -> Option<&str> {
    if let Some(rest) = value.strip_prefix('"') {
        return rest.split_once('"').map(|(value, _)| value);
    }
    value
        .split('#')
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn home_dir() -> Option<PathBuf> {
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

    #[test]
    fn load_permission_mode_reads_saved_value() {
        let path = temp_path("load");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "permission_mode = \"accept-edits\" # keep\n").unwrap();

        let mode = load_permission_mode(&path).unwrap();

        assert_eq!(mode, Some(PermissionMode::AcceptEdits));
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
    fn load_reasoning_effort_reads_saved_value() {
        let path = temp_path("effort-load");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "reasoning_effort = \"high\" # keep\n").unwrap();

        let effort = load_reasoning_effort(&path).unwrap();

        assert_eq!(effort.as_deref(), Some("high"));
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

        assert_eq!(load_model(&path).unwrap().as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            load_reasoning_effort(&path).unwrap().as_deref(),
            Some("xhigh")
        );
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
        assert_eq!(load_model(&path).unwrap().as_deref(), Some("gpt-5.5"));
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
}
