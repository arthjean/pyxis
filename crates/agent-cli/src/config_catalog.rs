//! The configuration catalog (US-100 -> US-102): the fifteen keys the loader
//! accepts, declared once with their flag, their variable and their layer.
//!
//! The triplet key / flag / environment variable exists nowhere as a link
//! today: `settings.rs` knows the keys, `main.rs` rewires five of them call by
//! call, and the `README.md` republished a list of security keys by hand until
//! it named five where the code refuses seven. The table below is that link,
//! and the document derives from it.
//!
//! Three properties make the verdict worth trusting, and each has its own half
//! of the gate:
//!
//! - The render is pure. [`render_config_catalog`] takes the declared rows and
//!   the declared layers and returns a `String`; it reads no file, writes none
//!   and consults no clock. Every order is an explicit sort and no `HashMap`
//!   reaches it.
//! - Freshness alone is blind. A byte comparison accepts a document rendered
//!   from zero keys, so [`check_coverage`] confronts the table with `KNOWN_KEYS`
//!   and `SECURITY_KEYS` in BOTH directions, [`check_wiring`] confronts the
//!   declared flags and variables with the wiring that reads them, and
//!   [`check_variables`] forbids silence: every `PYXIS_*` name a source of
//!   `crates/*/src` reads is either a key of the catalog or classified out of
//!   configuration. [`check_layers`] closes the last source the render reads,
//!   the layers themselves.
//! - The whole thing reads text and constants. It launches no process, opens no
//!   socket and reads one environment variable, the write switch, which is what
//!   lets it live inside `cargo test --workspace`.
//!
//! The generator lives under `#[cfg(test)]` inside the binary because
//! `agent-cli` has no `[lib]` target and `KNOWN_KEYS`, `SECURITY_KEYS` and
//! `ConfigLayer` are crate-private: an integration test could not read them,
//! and opening a library target to expose the internals of a binary for a
//! documentary need is a bigger change than this one deserves.
//!
//! `panic!` through `assert!` is the reporting mechanism of the gates below: a
//! stale document has to stop the suite with its path and the command that
//! fixes it, and the workspace denies `clippy::panic` everywhere else on
//! purpose.
#![allow(clippy::panic)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use agent_core::permission::PermissionMode;
use agent_core::sandbox::WORKSPACE_WRITE_ID;

use crate::settings::{
    COST_BUDGET_KEY, ConfigLayer, HOOKS_KEY, INPUT_COST_KEY, KNOWN_KEYS, MODEL_KEY,
    OUTPUT_COST_KEY, OVERLOAD_FALLBACK_KEY, PERMISSION_MODE_KEY, PROFILE_KEY, PROFILES_KEY,
    REASONING_EFFORT_KEY, SAFE_COMMANDS_KEY, SANDBOX_MODE_KEY, SECURITY_KEYS, TOKEN_BUDGET_KEY,
    WEB_SEARCH_KEY, WRITABLE_ROOTS_KEY,
};

/// The rendered document, relative to the repository root.
pub const CATALOG_DOC: &str = "docs/config-catalog.md";

/// The module rendering the document, cited in its header.
pub const GENERATOR: &str = "crates/agent-cli/src/config_catalog.rs";

/// The loader whose constants the table is confronted with.
pub const LOADER: &str = "crates/agent-cli/src/settings.rs";

/// The wiring that reads the flags and the variables of the table. Never
/// compiled by the guard: it is a text read, so the verdict is the same on any
/// machine.
pub const WIRING: &str = "crates/agent-cli/src/main.rs";

/// The environment variable that flips the freshness test into writing. Shared
/// with the crate graph and the tool catalog, so one switch regenerates every
/// catalog.
pub const UPDATE_VARIABLE: &str = "PYXIS_UPDATE_CATALOGS";

/// The exact command that rewrites the document: the head of the file, and the
/// remedy a failing comparison prints.
pub const REGENERATE_COMMAND: &str =
    "PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis config_catalog";

/// Where the variable scan looks: the sources compiled into the workspace. A
/// name read only by a test or an example is out of its reach on purpose, since
/// the claim the guard defends is about what the binary reads.
pub const SOURCES_ROOT: &str = "crates";

/// What an empty flag cell says. A blank cell reads as an omission; this reads
/// as a decision.
const NO_FLAG: &str = "aucun";

/// What an empty variable cell says.
const NO_VARIABLE: &str = "aucune";

/// What an unset default says: the key carries no value until a layer declares
/// one.
const NO_DEFAULT: &str = "aucun";

/// The value a key holds when no layer declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDefault {
    /// Nothing: the key is unset, and the behavior it drives has no value to
    /// read.
    Absent,
    /// A literal the code carries, rendered as a code span.
    Value(String),
    /// A behavior no literal states, rendered as prose.
    Described(String),
}

/// One row of the catalog: a key of `KNOWN_KEYS` with everything a reader needs
/// before opening `settings.rs`.
///
/// `security` is a CLAIM, not a copy: [`check_coverage`] confronts it with
/// `SECURITY_KEYS` in both directions, so a key promoted there without its row
/// following fails the suite instead of publishing a false `non`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigKey {
    /// The key, read from the constant `settings.rs` matches on.
    pub key: &'static str,
    /// Its TOML shape, as the loader parses it.
    pub kind: &'static str,
    /// What it holds when nothing declares it.
    pub default: KeyDefault,
    /// The LEAST trusted layer whose declaration is honored.
    pub lowest_layer: ConfigLayer,
    /// The typed flag that sets it, if one exists.
    pub flag: Option<&'static str>,
    /// The environment variable that sets it, if one exists.
    pub variable: Option<&'static str>,
    /// Whether it widens a security perimeter.
    pub security: bool,
}

/// A `PYXIS_*` name the sources read that is NOT a configuration key.
///
/// The classification is mandatory in both directions: a name nothing reads is
/// as much a defect as a name nothing classifies, because a stale row here is
/// how the table stops describing the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfConfig {
    /// The variable name.
    pub name: &'static str,
    /// Why it is not a setting.
    pub category: &'static str,
    /// What reads it, and for what.
    pub role: &'static str,
}

/// Every `PYXIS_*` name read under `crates/*/src` that carries no setting.
///
/// Sorted by name: the render publishes it as it stands.
pub const OUT_OF_CONFIG: &[OutOfConfig] = &[
    OutOfConfig {
        name: "PYXIS_A_VARIABLE_NOBODY_SETS",
        category: "tests",
        role: "nom volontairement absent de l'environnement, lu par le test qui prouve qu'une substitution non résolue le reste",
    },
    OutOfConfig {
        name: "PYXIS_CODEX_BASELINE",
        category: "parité",
        role: "chemin du clone Codex épinglé, lu en lecture seule par `agent-parity`",
    },
    OutOfConfig {
        name: "PYXIS_CODEX_CLIENT_VERSION",
        category: "protocole",
        role: "version du client annoncée à l'endpoint ChatGPT",
    },
    OutOfConfig {
        name: "PYXIS_DEBUG_TUI",
        category: "débogage",
        role: "journal de l'interface, écrit hors du souscripteur `tracing`",
    },
    OutOfConfig {
        name: "PYXIS_DEBUG_USAGE",
        category: "débogage",
        role: "sonde de calibration de l'usage rapporté par le fournisseur",
    },
    OutOfConfig {
        name: "PYXIS_HOME",
        category: "chemins",
        role: "racine de l'état utilisateur, `~/.pyxis` par défaut",
    },
    OutOfConfig {
        name: "PYXIS_IDLE_TIMEOUT_SECS",
        category: "transport",
        role: "délai d'inactivité du flux du fournisseur",
    },
    OutOfConfig {
        name: "PYXIS_LOG",
        category: "journalisation",
        role: "filtre du souscripteur `tracing` ; sans lui aucun souscripteur n'est installé",
    },
    OutOfConfig {
        name: "PYXIS_ORIGINATOR",
        category: "protocole",
        role: "originateur annoncé à l'endpoint ChatGPT",
    },
    OutOfConfig {
        name: "PYXIS_REDUCED_MOTION",
        category: "rendu",
        role: "coupe les animations de l'interface",
    },
    OutOfConfig {
        name: "PYXIS_TEST_ABSENT_VAR",
        category: "tests",
        role: "nom volontairement absent, lu par le test de substitution d'`agent-mcp`",
    },
    OutOfConfig {
        name: "PYXIS_UPDATE_CATALOGS",
        category: "génération",
        role: "bascule les portes de catalogue en écriture",
    },
    OutOfConfig {
        name: "PYXIS_UPDATE_SCHEMAS",
        category: "génération",
        role: "bascule la porte des schémas d'app-server en écriture",
    },
];

/// The declared table (US-100).
///
/// It names the keys through the constants of `settings.rs` rather than
/// respelling them, so a rename follows the compiler. What it adds is what no
/// constant carries: the flag, the variable, the default and the lowest layer.
///
/// Not a `const`: `PermissionMode::id` is not a `const fn`, and reading the
/// default from the enum is the point.
pub fn declared_keys() -> Vec<ConfigKey> {
    vec![
        ConfigKey {
            key: MODEL_KEY,
            kind: "chaîne",
            default: KeyDefault::Value(agent_provider::DEFAULT_MODEL.to_string()),
            lowest_layer: ConfigLayer::ProjectFile,
            flag: Some("--model"),
            variable: None,
            security: false,
        },
        ConfigKey {
            key: REASONING_EFFORT_KEY,
            kind: "chaîne",
            default: KeyDefault::Described(
                "celui que le modèle applique sans consigne".to_string(),
            ),
            lowest_layer: ConfigLayer::ProjectFile,
            flag: None,
            variable: None,
            security: false,
        },
        ConfigKey {
            key: PERMISSION_MODE_KEY,
            kind: "chaîne",
            default: KeyDefault::Described(format!(
                "`{}` ; `{}` en mode `-p` avec `--yes`",
                PermissionMode::Default.id(),
                PermissionMode::AcceptEdits.id()
            )),
            lowest_layer: ConfigLayer::GlobalFile,
            flag: Some("--permission-mode"),
            variable: None,
            security: true,
        },
        ConfigKey {
            key: SANDBOX_MODE_KEY,
            kind: "chaîne",
            default: KeyDefault::Value(WORKSPACE_WRITE_ID.to_string()),
            lowest_layer: ConfigLayer::GlobalFile,
            flag: Some("--sandbox"),
            variable: None,
            security: true,
        },
        ConfigKey {
            key: WRITABLE_ROOTS_KEY,
            kind: "tableau de chaînes",
            default: KeyDefault::Described("la racine de l'espace de travail seule".to_string()),
            lowest_layer: ConfigLayer::GlobalFile,
            flag: None,
            variable: None,
            security: true,
        },
        ConfigKey {
            key: HOOKS_KEY,
            kind: "tableau de tables",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::GlobalFile,
            flag: None,
            variable: None,
            security: true,
        },
        ConfigKey {
            key: PROFILE_KEY,
            kind: "chaîne",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::GlobalFile,
            flag: Some("--profile"),
            variable: None,
            security: true,
        },
        ConfigKey {
            key: PROFILES_KEY,
            kind: "table de tables",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::ProjectFile,
            flag: None,
            variable: None,
            security: false,
        },
        ConfigKey {
            key: TOKEN_BUDGET_KEY,
            kind: "entier positif",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::ProjectFile,
            flag: Some("--token-budget"),
            variable: Some("PYXIS_TOKEN_BUDGET"),
            security: false,
        },
        ConfigKey {
            key: COST_BUDGET_KEY,
            kind: "entier positif",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::ProjectFile,
            flag: Some("--cost-budget-micro-usd"),
            variable: Some("PYXIS_COST_BUDGET_MICRO_USD"),
            security: false,
        },
        ConfigKey {
            key: INPUT_COST_KEY,
            kind: "entier positif",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::ProjectFile,
            flag: Some("--input-cost-micro-per-ktok"),
            variable: Some("PYXIS_INPUT_COST_MICRO_PER_KTOK"),
            security: false,
        },
        ConfigKey {
            key: OUTPUT_COST_KEY,
            kind: "entier positif",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::ProjectFile,
            flag: Some("--output-cost-micro-per-ktok"),
            variable: Some("PYXIS_OUTPUT_COST_MICRO_PER_KTOK"),
            security: false,
        },
        ConfigKey {
            key: OVERLOAD_FALLBACK_KEY,
            kind: "chaîne",
            default: KeyDefault::Absent,
            lowest_layer: ConfigLayer::ProjectFile,
            flag: Some("--overload-fallback-model"),
            variable: Some("PYXIS_OVERLOAD_FALLBACK_MODEL"),
            security: false,
        },
        ConfigKey {
            key: WEB_SEARCH_KEY,
            kind: "booléen",
            default: KeyDefault::Value("false".to_string()),
            lowest_layer: ConfigLayer::GlobalFile,
            flag: None,
            variable: None,
            security: true,
        },
        ConfigKey {
            key: SAFE_COMMANDS_KEY,
            kind: "tableau de tables",
            default: KeyDefault::Described("la table intégrée seule".to_string()),
            lowest_layer: ConfigLayer::GlobalFile,
            flag: None,
            variable: None,
            security: true,
        },
    ]
}

// ─────────────────────── guards (US-101) ───────────────────────

/// Confront the table with the constants the loader really matches on.
///
/// Both directions, on purpose: a key `KNOWN_KEYS` accepts and the table
/// ignores would publish a catalog documenting less than what the loader
/// honors, and a row no key backs would publish a setting nobody can set.
pub fn check_coverage(rows: &[ConfigKey], known: &[&str], security: &[&str]) -> Vec<String> {
    let mut violations = Vec::new();
    if rows.is_empty() {
        violations.push(format!(
            "catalogue de configuration: la table de {GENERATOR} est vide ; un catalogue vide n'est pas un catalogue"
        ));
        return violations;
    }
    let declared: BTreeSet<&str> = rows.iter().map(|row| row.key).collect();
    if declared.len() != rows.len() {
        violations.push(format!(
            "catalogue de configuration: la table de {GENERATOR} déclare deux fois la même clé"
        ));
    }
    for key in known {
        if !declared.contains(key) {
            violations.push(format!(
                "catalogue de configuration: la clé `{key}` est acceptée par {LOADER} et absente de la table de {GENERATOR} ; le catalogue documenterait moins que ce que le loader honore"
            ));
        }
    }
    for row in rows {
        if !known.contains(&row.key) {
            violations.push(format!(
                "catalogue de configuration: la table de {GENERATOR} déclare `{}`, que `KNOWN_KEYS` de {LOADER} n'accepte pas",
                row.key
            ));
        }
        let is_security = security.contains(&row.key);
        if is_security && !row.security {
            violations.push(format!(
                "catalogue de configuration: `{}` est dans `SECURITY_KEYS` de {LOADER} et la table ne la marque pas comme clé de sécurité",
                row.key
            ));
        }
        if !is_security && row.security {
            violations.push(format!(
                "catalogue de configuration: la table marque `{}` comme clé de sécurité, or `SECURITY_KEYS` de {LOADER} ne la porte pas",
                row.key
            ));
        }
        // The rule of `settings.rs:445` rendered as a claim: a security key is
        // refused from a workspace-controlled file, so its lowest admitted
        // layer cannot be the project file.
        let admits_workspace = row.lowest_layer == ConfigLayer::ProjectFile;
        if is_security && admits_workspace {
            violations.push(format!(
                "catalogue de configuration: `{}` est une clé de sécurité et la table lui admet un fichier d'espace de travail, que {LOADER} refuse",
                row.key
            ));
        }
        if !is_security && !admits_workspace {
            violations.push(format!(
                "catalogue de configuration: `{}` n'est pas une clé de sécurité et la table lui refuse le fichier d'espace de travail, que {LOADER} accepte",
                row.key
            ));
        }
    }
    violations
}

/// Confront the published layers with the variants the enum declares.
///
/// `ConfigLayer::ALL` is a hand-written list, and US-102 promoted it to the
/// source of a published table. A variant added to the enum compiles as soon as
/// `precedence()` and `label()` gain their arms, and nothing then requires it to
/// join that list: the catalog would publish five layers where six resolve, and
/// the byte comparison would stay green, which is the blindness
/// [`check_coverage`] exists to remove for the keys.
///
/// The `match` below carries no arm body on purpose. It is exhaustive, so a
/// layer added to `ConfigLayer` stops the build here, against the array it also
/// has to join, rather than three files away in a document nobody rereads.
pub fn check_layers(layers: &[ConfigLayer]) -> Vec<String> {
    /// The variants the catalog owes its reader.
    const EVERY_LAYER: &[ConfigLayer] = &[
        ConfigLayer::GlobalFile,
        ConfigLayer::Profile,
        ConfigLayer::ProjectFile,
        ConfigLayer::Environment,
        ConfigLayer::SessionFlags,
    ];
    let mut violations = Vec::new();
    let published: BTreeSet<i16> = layers.iter().map(|layer| layer.precedence()).collect();
    for layer in EVERY_LAYER.iter().copied() {
        match layer {
            ConfigLayer::GlobalFile
            | ConfigLayer::Profile
            | ConfigLayer::ProjectFile
            | ConfigLayer::Environment
            | ConfigLayer::SessionFlags => {}
        }
        if !published.contains(&layer.precedence()) {
            violations.push(format!(
                "catalogue de configuration: la couche `{}` est déclarée par `ConfigLayer` de {LOADER} et absente des couches publiées ; le tableau des couches en documenterait moins que la résolution n'en applique",
                layer.label()
            ));
        }
    }
    violations
}

/// Confront the declared triplet with the wiring that reads it.
///
/// One direction only: `main.rs` carries flags no key backs (`--resume`,
/// `--listen`), and the reverse direction for variables is covered by
/// [`check_variables`], which reads every source rather than one file.
pub fn check_wiring(rows: &[ConfigKey], wiring: &str) -> Vec<String> {
    let mut violations = Vec::new();
    if wiring.trim().is_empty() {
        violations.push(format!(
            "catalogue de configuration: {WIRING} est vide ; la table serait validée contre rien"
        ));
        return violations;
    }
    for row in rows {
        if let Some(flag) = row.flag
            && !wiring.contains(&format!("\"{flag}\""))
        {
            violations.push(format!(
                "catalogue de configuration: la table donne le drapeau `{flag}` à `{}`, et {WIRING} ne l'analyse pas",
                row.key
            ));
        }
        if let Some(variable) = row.variable
            && !wiring.contains(&format!("\"{variable}\""))
        {
            violations.push(format!(
                "catalogue de configuration: la table donne la variable `{variable}` à `{}`, et {WIRING} ne la lit pas",
                row.key
            ));
        }
    }
    violations
}

/// The `PYXIS_*` names the given sources read.
///
/// A name is a quoted literal: that is what an environment read needs, whether
/// it is passed inline or held by a constant. An identifier spelled like one
/// (`agent_tools::spill::PYXIS_DIR`, a directory name) is deliberately out of
/// reach, because it reads nothing.
pub fn scan_variables(sources: &[(String, String)]) -> BTreeSet<String> {
    // Assembled rather than written, so the scan of this very file does not
    // read its own needle as a variable.
    let needle = format!("\"{}_", "PYXIS");
    let mut found = BTreeSet::new();
    for (_, content) in sources {
        let mut rest = content.as_str();
        while let Some(index) = rest.find(&needle) {
            let after = &rest[index + needle.len()..];
            let name: String = after
                .chars()
                .take_while(|character| {
                    character.is_ascii_uppercase()
                        || character.is_ascii_digit()
                        || *character == '_'
                })
                .collect();
            // A name is a whole literal, closed by its own quote: never the
            // head of a command line such as a regeneration recipe, where the
            // name is followed by an equals sign.
            if !name.is_empty() && after[name.len()..].starts_with('"') {
                found.insert(format!("{}_{name}", "PYXIS"));
            }
            rest = &rest[index + needle.len()..];
        }
    }
    found
}

/// Forbid silence: every name read is a key or a classification, and every
/// classification is read.
pub fn check_variables(
    found: &BTreeSet<String>,
    rows: &[ConfigKey],
    classified: &[OutOfConfig],
) -> Vec<String> {
    let mut violations = Vec::new();
    let keyed: BTreeSet<&str> = rows.iter().filter_map(|row| row.variable).collect();
    let out: BTreeSet<&str> = classified.iter().map(|entry| entry.name).collect();
    for name in found {
        let name = name.as_str();
        if keyed.contains(name) && out.contains(name) {
            violations.push(format!(
                "catalogue de configuration: `{name}` est à la fois une clé du catalogue et classée hors configuration"
            ));
        }
        if !keyed.contains(name) && !out.contains(name) {
            violations.push(format!(
                "catalogue de configuration: la variable `{name}` est lue sous {SOURCES_ROOT}/*/src et n'est ni rattachée à une clé du catalogue ni classée hors configuration ; toute variable se classe"
            ));
        }
    }
    for name in keyed.iter().chain(out.iter()) {
        if !found.contains(*name) {
            violations.push(format!(
                "catalogue de configuration: `{name}` est déclarée et aucune source de {SOURCES_ROOT}/*/src ne la lit"
            ));
        }
    }
    violations
}

/// Every Rust source compiled into the workspace, as `(path relative to the
/// root, content)`, sorted by path.
pub fn source_files(root: &Path) -> Result<Vec<(String, String)>, String> {
    let crates = root.join(SOURCES_ROOT);
    let mut directories = read_sorted(&crates)?;
    directories.retain(|path| path.is_dir());
    let mut sources = Vec::new();
    for directory in directories {
        let src = directory.join("src");
        if src.is_dir() {
            collect_rust_sources(&src, root, &mut sources)?;
        }
    }
    if sources.is_empty() {
        return Err(format!(
            "catalogue de configuration: aucune source lue sous {}, la classification serait validée contre rien",
            crates.display()
        ));
    }
    sources.sort();
    Ok(sources)
}

/// The entries of a directory, sorted: two machines walk it in the same order.
fn read_sorted(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "catalogue de configuration: {} illisible : {error}",
                directory.display()
            )
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    Ok(entries)
}

fn collect_rust_sources(
    directory: &Path,
    root: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    for path in read_sorted(directory)? {
        if path.is_dir() {
            collect_rust_sources(&path, root, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            let content = fs::read_to_string(&path).map_err(|error| {
                format!(
                    "catalogue de configuration: {} illisible : {error}",
                    path.display()
                )
            })?;
            out.push((relative, content));
        }
    }
    Ok(())
}

// ─────────────────────── render (US-102) ───────────────────────

/// Render the whole document, header included, from the declared rows and the
/// declared layers.
///
/// Pure: no file, no clock, no environment. Every order is an explicit sort, so
/// two runs on the same table produce the same bytes. The five layers arrive as
/// values and are rendered through `label()` and `precedence()`: a layer added
/// to the enum is rendered here without a line of this function changing.
pub fn render_config_catalog(
    rows: &[ConfigKey],
    layers: &[ConfigLayer],
) -> Result<String, Vec<String>> {
    let violations = check_coverage(rows, KNOWN_KEYS, SECURITY_KEYS);
    if !violations.is_empty() {
        return Err(violations);
    }
    let missing = check_layers(layers);
    if !missing.is_empty() {
        return Err(missing);
    }

    let mut sorted: Vec<&ConfigKey> = rows.iter().collect();
    sorted.sort_by(|left, right| left.key.cmp(right.key));
    let mut ordered = layers.to_vec();
    ordered.sort_by_key(|layer| layer.precedence());

    let secured = sorted.iter().filter(|row| row.security).count();
    let with_variable = sorted.iter().filter(|row| row.variable.is_some()).count();
    let with_flag = sorted.iter().filter(|row| row.flag.is_some()).count();
    let total = sorted.len();

    let mut out = String::new();
    out.push_str(&format!(
        "<!-- Généré par {GENERATOR} ; ne pas éditer à la main. -->\n"
    ));
    out.push_str(&format!("<!-- Régénérer : {REGENERATE_COMMAND} -->\n"));
    out.push('\n');
    out.push_str("# Catalogue de configuration\n\n");
    out.push_str(&format!(
        "Les {total} clés que `pyxis` accepte, avec leur type, leur défaut, la couche la plus\n\
         basse qui peut les déclarer, leur drapeau, leur variable d'environnement et leur\n\
         caractère de sécurité. Elles sont déclarées dans\n\
         [`{GENERATOR}`](../{GENERATOR}) et confrontées à `KNOWN_KEYS` et `SECURITY_KEYS` de\n\
         [`{LOADER}`](../{LOADER}) dans les deux sens : une clé que le loader accepte et que ce\n\
         document ignore fait échouer la suite, et l'inverse aussi.\n\n"
    ));

    out.push_str("## Couches\n\n");
    out.push_str(
        "Une valeur effective vient d'une couche nommée, et chaque couche porte une précédence\n\
         déclarée. La résolution compare ces nombres, jamais l'ordre d'application : une couche\n\
         plus forte qui a déjà réclamé une clé la garde, quelle que soit la couche appliquée\n\
         ensuite.\n\n",
    );
    out.push_str("| Couche | Précédence |\n|---|---|\n");
    for layer in &ordered {
        out.push_str(&format!(
            "| {} | {} |\n",
            code_cell(layer.label()),
            layer.precedence()
        ));
    }
    out.push('\n');

    out.push_str("## Clés de sécurité\n\n");
    out.push_str(&format!(
        "Sur {total} clés, **{secured} élargissent un périmètre de sécurité**. Une clé de\n\
         sécurité est refusée depuis un fichier que l'espace de travail contrôle,\n\
         `<workspace>/.pyxis/config.toml`, avec un avertissement nommant la couche qui a essayé,\n\
         et elle est refusée depuis `-c clé=valeur`, un argument pouvant venir d'un script du\n\
         dépôt. Les drapeaux typés restent la façon de choisir un périmètre pour une session :\n\
         l'utilisateur les a frappés. Un profil déclaré par un fichier du dépôt ne contourne pas\n\
         la règle, la portée du fichier d'origine voyageant avec lui.\n\n"
    ));

    out.push_str("## Clés\n\n");
    out.push_str(&format!(
        "La colonne « couche la plus basse admise » nomme la couche la MOINS fiable dont une\n\
         déclaration est honorée, du fichier que le dépôt écrit au drapeau que l'utilisateur\n\
         frappe. Un défaut « {NO_DEFAULT} » veut dire que la clé n'a pas de valeur tant qu'aucune\n\
         couche ne la déclare. {with_flag} clés ont un drapeau et {with_variable} ont une variable\n\
         d'environnement ; les autres portent un marqueur d'absence, jamais une cellule vide.\n\n"
    ));
    out.push_str(
        "| Clé | Type | Défaut | Couche la plus basse admise | Drapeau | Variable d'environnement | Clé de sécurité |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|\n");
    for row in &sorted {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            code_cell(row.key),
            escape_cell(row.kind),
            render_default(&row.default),
            code_cell(row.lowest_layer.label()),
            row.flag.map_or_else(|| NO_FLAG.to_string(), code_cell),
            row.variable
                .map_or_else(|| NO_VARIABLE.to_string(), code_cell),
            yes_no(row.security),
        ));
    }
    out.push('\n');

    out.push_str("## Variables d'environnement hors configuration\n\n");
    out.push_str(&format!(
        "Toute variable `{}_*` lue sous `{SOURCES_ROOT}/*/src` se classe : soit elle porte une clé\n\
         du tableau ci-dessus, soit elle figure ici. Une variable qu'aucune des deux tables ne\n\
         nomme fait échouer la suite, et une ligne d'ici que plus aucune source ne lit aussi :\n\
         il n'y a pas de silence.\n\n",
        "PYXIS"
    ));
    out.push_str("| Variable | Catégorie | Rôle |\n|---|---|---|\n");
    let mut classified: Vec<&OutOfConfig> = OUT_OF_CONFIG.iter().collect();
    classified.sort_by(|left, right| left.name.cmp(right.name));
    for entry in classified {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            code_cell(entry.name),
            escape_cell(entry.category),
            escape_cell(entry.role),
        ));
    }

    while out.ends_with("\n\n") {
        out.pop();
    }
    Ok(out)
}

/// The whole document, from the declared table and the declared layers.
pub fn config_catalog_document() -> Result<String, Vec<String>> {
    render_config_catalog(&declared_keys(), ConfigLayer::ALL)
}

/// A default cell. An absent value gets the marker, never a blank.
fn render_default(default: &KeyDefault) -> String {
    match default {
        KeyDefault::Absent => NO_DEFAULT.to_string(),
        KeyDefault::Value(value) => code_cell(value),
        KeyDefault::Described(text) => escape_cell(text),
    }
}

/// `true` and `false` as a reader of French prose reads them.
fn yes_no(value: bool) -> &'static str {
    if value { "oui" } else { "non" }
}

/// A table cell holds one line and no unescaped pipe. Backticks are left alone:
/// in a cell they open an inline code span, which is what the prose cells of
/// this catalog use them for.
fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// A cell rendered as a code span when the value can survive one.
fn code_cell(value: &str) -> String {
    if value.contains('`') || value.contains('\n') || value.contains('\r') {
        escape_cell(value).replace('`', "\\`")
    } else {
        format!("`{}`", value.replace('|', "\\|"))
    }
}

/// The repository root, from the manifest directory of this crate. Resolved at
/// COMPILE time: the gate reads no environment variable but the write switch.
pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Compare a published document to the render. An absent file arrives here as
/// an empty string and is reported as a stale one: the remedy is the same
/// command, so there is no reason to say it differently.
pub fn freshness_violation(published: &str, rendered: &str) -> Option<String> {
    (published != rendered).then(|| {
        format!("catalogue: {CATALOG_DOC} est périmé ; régénérer avec {REGENERATE_COMMAND}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<ConfigKey> {
        declared_keys()
    }

    fn wiring() -> String {
        fs::read_to_string(repository_root().join(WIRING)).expect("le câblage est lisible")
    }

    fn document() -> String {
        match config_catalog_document() {
            Ok(rendered) => rendered,
            Err(errors) => panic!("{}", errors.join("\n")),
        }
    }

    // ─────────── US-101: nothing the loader accepts can be missing ───────────

    #[test]
    fn config_catalog_declares_every_key_the_loader_accepts() {
        assert_eq!(
            check_coverage(&rows(), KNOWN_KEYS, SECURITY_KEYS),
            Vec::<String>::new()
        );
        assert_eq!(rows().len(), KNOWN_KEYS.len());
    }

    #[test]
    fn config_catalog_a_key_added_to_known_keys_without_a_row_is_named() {
        let known: Vec<&str> = KNOWN_KEYS.iter().copied().chain(["retry_budget"]).collect();
        let violations = check_coverage(&rows(), &known, SECURITY_KEYS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("retry_budget")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_a_row_absent_from_known_keys_is_named() {
        let known: Vec<&str> = KNOWN_KEYS
            .iter()
            .copied()
            .filter(|key| *key != MODEL_KEY)
            .collect();
        let violations = check_coverage(&rows(), &known, SECURITY_KEYS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(MODEL_KEY)
                    && violation.contains("n'accepte pas")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_a_security_key_the_table_does_not_mark_is_named() {
        let security: Vec<&str> = SECURITY_KEYS.iter().copied().chain([MODEL_KEY]).collect();
        let violations = check_coverage(&rows(), KNOWN_KEYS, &security);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(MODEL_KEY)
                    && violation.contains("ne la marque pas")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_a_security_key_admitting_a_workspace_file_is_named() {
        let mut rows = rows();
        for row in &mut rows {
            if row.key == HOOKS_KEY {
                row.lowest_layer = ConfigLayer::ProjectFile;
            }
        }
        let violations = check_coverage(&rows, KNOWN_KEYS, SECURITY_KEYS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(HOOKS_KEY)
                    && violation.contains("fichier d'espace de travail")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_an_empty_table_fails_instead_of_publishing_an_empty_catalog() {
        let violations = check_coverage(&[], KNOWN_KEYS, SECURITY_KEYS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("vide")),
            "{violations:?}"
        );
        assert!(render_config_catalog(&[], ConfigLayer::ALL).is_err());
    }

    #[test]
    fn config_catalog_publishes_every_layer_the_resolution_applies() {
        assert_eq!(check_layers(ConfigLayer::ALL), Vec::<String>::new());
    }

    #[test]
    fn config_catalog_a_layer_the_published_list_forgets_is_named() {
        let amputated: Vec<ConfigLayer> = ConfigLayer::ALL
            .iter()
            .copied()
            .filter(|layer| *layer != ConfigLayer::Profile)
            .collect();
        let violations = check_layers(&amputated);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(ConfigLayer::Profile.label())),
            "{violations:?}"
        );
        assert!(
            render_config_catalog(&rows(), &amputated).is_err(),
            "un tableau des couches troué ne se publie pas"
        );
    }

    #[test]
    fn config_catalog_an_empty_layer_list_names_every_layer_it_owes() {
        let violations = check_layers(&[]);
        assert_eq!(violations.len(), ConfigLayer::ALL.len());
        assert!(render_config_catalog(&rows(), &[]).is_err());
    }

    #[test]
    fn config_catalog_declares_only_flags_and_variables_the_wiring_reads() {
        assert_eq!(check_wiring(&rows(), &wiring()), Vec::<String>::new());
    }

    #[test]
    fn config_catalog_a_flag_the_wiring_does_not_carry_is_named() {
        let mut rows = rows();
        for row in &mut rows {
            if row.key == MODEL_KEY {
                row.flag = Some("--modele");
            }
        }
        let violations = check_wiring(&rows, &wiring());
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("--modele")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_an_empty_wiring_fails_instead_of_validating_the_table_against_nothing() {
        assert!(!check_wiring(&rows(), "   ").is_empty());
    }

    #[test]
    fn config_catalog_every_pyxis_variable_the_sources_read_is_classified() {
        let sources = source_files(&repository_root()).expect("les sources sont lisibles");
        let found = scan_variables(&sources);
        assert!(
            found.len() >= OUT_OF_CONFIG.len(),
            "le balayage n'a lu que {} noms",
            found.len()
        );
        assert_eq!(
            check_variables(&found, &rows(), OUT_OF_CONFIG),
            Vec::<String>::new()
        );
    }

    #[test]
    fn config_catalog_a_variable_nothing_classifies_is_named() {
        // Assembled so the scan of this very file does not read the fixture as
        // a variable the workspace really uses.
        let name = concat!("PYXIS", "_NEUVE");
        let content = format!("let raw = std::env::var(\"{name}\");");
        let found = scan_variables(&[("fixture.rs".to_string(), content)]);
        assert!(found.contains(name), "{found:?}");
        let violations = check_variables(&found, &rows(), &[]);
        assert!(
            violations.iter().any(|violation| violation.contains(name)
                && violation.contains("ni classée hors configuration")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_a_classified_variable_nothing_reads_is_named() {
        let violations = check_variables(&BTreeSet::new(), &rows(), OUT_OF_CONFIG);
        let orphan = OUT_OF_CONFIG[0].name;
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(orphan) && violation.contains("aucune source")),
            "{violations:?}"
        );
    }

    #[test]
    fn config_catalog_reads_a_quoted_name_and_not_an_identifier_spelled_like_one() {
        let quoted = concat!("PYXIS", "_LUE");
        let identifier = concat!("PYXIS", "_CONSTANTE");
        let content = format!(
            "pub const {identifier}: &str = \".pyxis\";\nlet raw = std::env::var(\"{quoted}\");\n"
        );
        let found = scan_variables(&[("fixture.rs".to_string(), content)]);
        assert!(found.contains(quoted), "{found:?}");
        assert!(!found.contains(identifier), "{found:?}");
    }

    #[test]
    fn config_catalog_reads_a_whole_literal_and_not_the_head_of_a_command_line() {
        let name = concat!("PYXIS", "_RECETTE");
        let content = format!("const COMMAND: &str = \"{name}=1 cargo test\";");
        let found = scan_variables(&[("fixture.rs".to_string(), content)]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn config_catalog_gate_launches_no_process_and_opens_no_socket() {
        let file =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config_catalog.rs"))
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
                "le générateur du catalogue doit rester une lecture de texte : {forbidden}"
            );
        }
        // The only environment variable read at run time is the write switch,
        // and it is read in the freshness test rather than in the generator.
        assert!(!generator.contains("std::env::var"), "{UPDATE_VARIABLE}");
        // Assembled rather than written, so this assertion is not its own match.
        let switch = format!("std::env::{}", "var_os");
        assert_eq!(file.matches(&switch).count(), 1, "{UPDATE_VARIABLE}");
    }

    // ─────────── US-102: the rendered document ───────────

    #[test]
    fn config_catalog_is_what_settings_accepts() {
        let rendered = document();
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
    fn config_catalog_an_absent_document_is_reported_like_a_stale_one() {
        let rendered = document();
        let reported = freshness_violation("", &rendered).expect("un fichier absent est périmé");
        assert!(reported.contains(CATALOG_DOC), "{reported}");
        assert!(reported.contains(REGENERATE_COMMAND), "{reported}");
        assert_eq!(freshness_violation(&rendered, &rendered), None);
    }

    #[test]
    fn config_catalog_render_is_byte_identical_across_two_consecutive_runs() {
        let first = document();
        let second = document();
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
    fn config_catalog_header_names_its_generator_and_the_command_that_rewrites_it() {
        let rendered = document();
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
    fn config_catalog_renders_every_layer_from_its_declared_precedence() {
        let rendered = document();
        for layer in ConfigLayer::ALL {
            assert!(
                rendered.contains(&format!("| `{}` | {} |", layer.label(), layer.precedence())),
                "la couche {} manque au tableau",
                layer.label()
            );
        }
        // No layer is named by hand in the generator: a new variant is rendered
        // by `label()` and `precedence()` without a line of it changing.
        let file =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config_catalog.rs"))
                .expect("le source du générateur est lisible");
        let generator = file
            .split_once("mod tests {")
            .map(|(before, _)| before)
            .expect("le module de test délimite le générateur");
        for layer in ConfigLayer::ALL {
            let literal = format!("\"{}\"", layer.label());
            assert!(
                !generator.contains(&literal),
                "le générateur recopie le libellé de {}",
                layer.label()
            );
        }
    }

    #[test]
    fn config_catalog_gives_every_known_key_a_row_with_its_type_and_its_layer() {
        let rendered = document();
        for row in rows() {
            // Seven columns, so a layer row spelled like a key (`profile`) is
            // not mistaken for one.
            let line = rendered
                .lines()
                .find(|line| {
                    line.starts_with(&format!("| `{}` |", row.key))
                        && line.matches('|').count() == 8
                })
                .unwrap_or_else(|| panic!("la clé {} n'a pas de ligne", row.key));
            assert!(line.contains(row.kind), "{line}");
            assert!(line.contains(row.lowest_layer.label()), "{line}");
            assert!(
                line.ends_with(&format!("| {} |", yes_no(row.security))),
                "{line}"
            );
        }
    }

    #[test]
    fn config_catalog_marks_an_absent_flag_and_an_absent_variable_rather_than_leaving_a_blank() {
        let rendered = document();
        let line = rendered
            .lines()
            .find(|line| line.starts_with(&format!("| `{HOOKS_KEY}` |")))
            .expect("hooks a une ligne");
        assert!(line.contains(NO_FLAG), "{line}");
        assert!(line.contains(NO_VARIABLE), "{line}");
        assert!(!line.contains("|  |"), "une cellule est vide : {line}");
    }

    #[test]
    fn config_catalog_states_the_rule_that_refuses_a_security_key_from_a_workspace_file() {
        let rendered = document();
        assert!(
            rendered.contains("<workspace>/.pyxis/config.toml"),
            "le document ne nomme pas le fichier refusé"
        );
        assert!(
            rendered.contains("`-c clé=valeur`"),
            "le document ne dit pas que `-c` est refusé"
        );
        let secured = rows().iter().filter(|row| row.security).count();
        assert!(
            rendered.contains(&format!(
                "**{secured} élargissent un périmètre de sécurité**"
            )),
            "le document ne compte pas les clés de sécurité"
        );
    }

    #[test]
    fn config_catalog_publishes_the_classification_of_every_variable_it_scanned() {
        let rendered = document();
        for entry in OUT_OF_CONFIG {
            assert!(
                rendered.contains(&format!("| `{}` | {} |", entry.name, entry.category)),
                "{} manque au tableau hors configuration",
                entry.name
            );
        }
        for row in rows() {
            if let Some(variable) = row.variable {
                assert!(
                    rendered.contains(variable),
                    "{variable} manque au catalogue"
                );
            }
        }
    }

    #[test]
    fn config_catalog_is_not_folded_by_github_as_a_generated_file() {
        let attributes =
            fs::read_to_string(repository_root().join(".gitattributes")).unwrap_or_default();
        for line in attributes.lines() {
            let line = line.trim();
            if line.starts_with('#') || !line.contains("linguist-generated") {
                continue;
            }
            assert!(
                !line.contains(CATALOG_DOC) && !line.contains("config-catalog.md"),
                "{CATALOG_DOC} serait replié par GitHub, or son diff est l'artefact de revue : {line}"
            );
        }
    }
}
