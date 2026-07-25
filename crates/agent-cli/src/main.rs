//! `pyxis` — binaire CLI. SEUL crate qui câble tout (ARCHITECTURE §2) : cœur +
//! provider abonnement ChatGPT + outils + session + sandbox + frontend TUI.
//!
//! Ordre critique : le **sandbox FS (Landlock) est appliqué sur le thread
//! principal AVANT la construction du runtime tokio** → les workers et les
//! sous-process Bash héritent du confinement (fork-safe, cf. `agent_sandbox::fs`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod approver;
mod context;
mod interactive;
mod jsonl;
mod prompt;
mod session;
mod settings;

use std::sync::Arc;

use agent_auth::{OAuthCredential, ProviderId, store};
use agent_core::clock::SystemClock;
use agent_core::guardrail::CostBudget;
use agent_core::message::{Message, recent_untrusted_content};
use agent_core::provider::Provider;
use agent_core::{AgentContext, CancelToken, Deps, RunConfig};
use agent_provider::{KEYRING_ACCOUNT, OpenAiChatGptProvider};
use agent_sandbox::{ProxyPolicy, set_proxy_env};
use agent_tokenizer::HeuristicCounter;
use agent_tools::permission::{AutoDeny, PermissionMode, PermissionModeState};
use agent_tools::{Bash, Edit, Glob, Grep, Read, Registry, Write};

use crate::approver::TuiApprover;
use crate::interactive::InteractiveConfig;
use crate::session::SharedSession;

const RESUME_TAINT_SCAN_MESSAGES: usize = 8;

#[derive(Debug)]
struct Args {
    prompt: Option<String>,
    resume: Option<String>,
    model: String,
    /// `--model` passé explicitement : distingue le défaut de compilation d'un
    /// choix utilisateur, et prime donc sur le modèle persisté.
    model_from_cli: bool,
    allow_hosts: Vec<String>,
    yes: bool,
    sandbox: bool,
    token_budget: Option<String>,
    cost_budget_micro_usd: Option<String>,
    input_cost_micro_per_ktok: Option<String>,
    output_cost_micro_per_ktok: Option<String>,
    overload_fallback_model: Option<String>,
    /// Format de sortie du mode headless (US-017). `Text` par défaut.
    output_format: jsonl::OutputFormat,
    help: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CliPermissionPolicy {
    mode: PermissionMode,
}

enum CredentialBootstrap {
    Connected(OAuthCredential),
    Missing,
    WrongProvider(ProviderId),
}

const HELP: &str = "\
Usage: pyxis [options] [prompt]

Options:
  -p, --print <prompt>                 Mode headless one-shot
      --resume [latest|<file.jsonl>]    Resume a session
      --model <slug>                    Model to use
      --allow <host>                    Allow a network host
  -y, --yes                             Accept edits in headless mode
      --no-sandbox                      Disable the filesystem sandbox
      --token-budget <n>                Total token budget
      --cost-budget-micro-usd <n>       Total cost budget
      --input-cost-micro-per-ktok <n>   Input price
      --output-cost-micro-per-ktok <n>  Output price
      --overload-fallback-model <slug>  Fallback model on overload
      --output-format <text|json>       Headless output: final text (default) or
                                        one JSON event per line (docs/EVENT_SCHEMA.md)
  -h, --help                            Show this help

Configuration (TOML), lowest precedence first:
  ~/.pyxis/settings.toml           Global, user-owned. May set every key.
  <workspace>/.pyxis/config.toml   Project. `permission_mode`, `writable_roots`
                                   and `hooks` are ignored there with a warning:
                                   a workspace-controlled file must never widen a
                                   security perimeter.
  Environment variables, then command-line arguments, override both.

Sandbox:
  Writes are confined to the workspace, plus the temporary directory and any
  extra roots listed in `writable_roots` of ~/.pyxis/settings.toml.
  The write and edit tools additionally refuse .git/ (hooks, config, and the
  worktree pointer file) and .pyxis/ — which holds the project config file —
  whose contents run or are read later outside the sandbox. That refusal does
  NOT cover `bash`: Landlock rules are additive, so a write right granted on the
  workspace cannot be subtracted for a subpath. A `bash` command can therefore
  still write .pyxis/config.toml; the blast radius is bounded by the project
  file never being allowed to set a security key.
";

fn parse_args() -> anyhow::Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(raw: I) -> anyhow::Result<Args>
where
    I: IntoIterator<Item = String>,
{
    let mut args = Args {
        prompt: None,
        resume: None,
        model: agent_provider::DEFAULT_MODEL.to_string(),
        model_from_cli: false,
        allow_hosts: Vec::new(),
        yes: false,
        sandbox: true,
        token_budget: None,
        cost_budget_micro_usd: None,
        input_cost_micro_per_ktok: None,
        output_cost_micro_per_ktok: None,
        overload_fallback_model: None,
        output_format: jsonl::OutputFormat::Text,
        help: false,
    };
    let mut it = raw.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => args.help = true,
            "-p" | "--print" => args.prompt = Some(next_value(&mut it, a.as_str())?),
            "--resume" => {
                args.resume = match it.peek() {
                    Some(next) if !next.starts_with('-') => it.next(),
                    _ => Some(String::new()),
                };
            }
            "--model" => {
                args.model = next_value(&mut it, "--model")?;
                args.model_from_cli = true;
            }
            "--allow" => args.allow_hosts.push(next_value(&mut it, "--allow")?),
            "--yes" | "-y" => args.yes = true,
            "--no-sandbox" => args.sandbox = false,
            "--token-budget" => args.token_budget = Some(next_value(&mut it, "--token-budget")?),
            "--cost-budget-micro-usd" => {
                args.cost_budget_micro_usd = Some(next_value(&mut it, "--cost-budget-micro-usd")?)
            }
            "--input-cost-micro-per-ktok" => {
                args.input_cost_micro_per_ktok =
                    Some(next_value(&mut it, "--input-cost-micro-per-ktok")?)
            }
            "--output-cost-micro-per-ktok" => {
                args.output_cost_micro_per_ktok =
                    Some(next_value(&mut it, "--output-cost-micro-per-ktok")?)
            }
            "--overload-fallback-model" => {
                args.overload_fallback_model =
                    Some(next_value(&mut it, "--overload-fallback-model")?)
            }
            "--output-format" => {
                let raw = next_value(&mut it, "--output-format")?;
                args.output_format = jsonl::output_format_from_arg(&raw).ok_or_else(|| {
                    anyhow::anyhow!("--output-format: expected `text` or `json`, got `{raw}`")
                })?;
            }
            other => {
                // A bare argument without -p is treated as the prompt.
                if other.starts_with('-') {
                    anyhow::bail!("unknown argument: {other}");
                }
                if args.prompt.is_none() {
                    args.prompt = Some(other.to_string());
                } else {
                    anyhow::bail!("unexpected positional argument: {other}");
                }
            }
        }
    }
    Ok(args)
}

fn next_value<I>(it: &mut std::iter::Peekable<I>, flag: &str) -> anyhow::Result<String>
where
    I: Iterator<Item = String>,
{
    let Some(value) = it.next() else {
        anyhow::bail!("{flag}: missing value");
    };
    if value.starts_with('-') {
        anyhow::bail!("{flag}: missing value");
    }
    Ok(value)
}

pub(crate) fn resolve_resume_path(
    sessions_dir: &std::path::Path,
    arg: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let arg = arg.trim();
    if arg.is_empty() || arg == "latest" {
        let latest = agent_session::list_sessions(sessions_dir, None)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("resume: no session available"))?;
        return Ok(sessions_dir.join(latest.id));
    }
    let path = crate::interactive::session_path_from_arg(sessions_dir, arg)
        .ok_or_else(|| anyhow::anyhow!("resume: invalid session id"))?;
    if !path.is_file() {
        anyhow::bail!("resume: session not found: {}", path.display());
    }
    Ok(path)
}

fn parse_positive_u64(raw: &str, name: &str) -> anyhow::Result<u64> {
    let value = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{name} must be a positive integer"))?;
    if value == 0 {
        anyhow::bail!("{name} must be > 0");
    }
    Ok(value)
}

/// Précédence d'un réglage numérique (US-016 AC1) : configuration < variable
/// d'environnement < argument. Le premier niveau non défini est simplement
/// traversé, ce qui rend la chaîne identique quel que soit le nombre de sources
/// réellement renseignées. L'environnement est passé en paramètre plutôt que lu
/// ici : la précédence devient testable sans muter l'environnement du processus.
fn precedence_u64(
    arg: Option<&str>,
    arg_name: &str,
    env: Option<&str>,
    env_name: &str,
    from_config: Option<u64>,
) -> anyhow::Result<Option<u64>> {
    if let Some(raw) = arg {
        return parse_positive_u64(raw, arg_name).map(Some);
    }
    match env {
        Some(raw) if !raw.trim().is_empty() => parse_positive_u64(raw, env_name).map(Some),
        _ => Ok(from_config),
    }
}

fn setting_u64(
    arg: &Option<String>,
    env: &str,
    name: &str,
    from_config: Option<u64>,
) -> anyhow::Result<Option<u64>> {
    precedence_u64(
        arg.as_deref(),
        name,
        std::env::var(env).ok().as_deref(),
        env,
        from_config,
    )
}

fn run_config_from_args(args: &Args, config: &settings::Config) -> anyhow::Result<RunConfig> {
    let token_budget = setting_u64(
        &args.token_budget,
        "PYXIS_TOKEN_BUDGET",
        "--token-budget",
        config.token_budget,
    )?;
    let cost_limit = setting_u64(
        &args.cost_budget_micro_usd,
        "PYXIS_COST_BUDGET_MICRO_USD",
        "--cost-budget-micro-usd",
        config.cost_budget_micro_usd,
    )?;
    let input_price = setting_u64(
        &args.input_cost_micro_per_ktok,
        "PYXIS_INPUT_COST_MICRO_PER_KTOK",
        "--input-cost-micro-per-ktok",
        config.input_cost_micro_per_ktok,
    )?;
    let output_price = setting_u64(
        &args.output_cost_micro_per_ktok,
        "PYXIS_OUTPUT_COST_MICRO_PER_KTOK",
        "--output-cost-micro-per-ktok",
        config.output_cost_micro_per_ktok,
    )?;

    let cost_budget = match (cost_limit, input_price, output_price) {
        (None, None, None) => None,
        (Some(limit_micro_usd), Some(input_micro_per_ktok), Some(output_micro_per_ktok)) => {
            Some(CostBudget {
                limit_micro_usd,
                input_micro_per_ktok,
                output_micro_per_ktok,
            })
        }
        _ => anyhow::bail!(
            "incomplete cost budget: provide --cost-budget-micro-usd, --input-cost-micro-per-ktok, and --output-cost-micro-per-ktok"
        ),
    };
    let overload_fallback_model = args
        .overload_fallback_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("PYXIS_OVERLOAD_FALLBACK_MODEL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| config.overload_fallback_model.clone());
    if let Some(fallback) = &overload_fallback_model
        && prompt::uses_codex_finetuned_prompt(&args.model)
            != prompt::uses_codex_finetuned_prompt(fallback)
    {
        anyhow::bail!(
            "fallback model is incompatible with the system prompt: primary={} fallback={}",
            args.model,
            fallback
        );
    }

    Ok(RunConfig {
        token_budget,
        cost_budget,
        overload_fallback_model,
        ..RunConfig::default()
    })
}

fn permission_policy(headless: bool, yes: bool, _sandbox_enforced: bool) -> CliPermissionPolicy {
    if !headless {
        return CliPermissionPolicy {
            mode: PermissionMode::Default,
        };
    }
    if !yes {
        return CliPermissionPolicy {
            mode: PermissionMode::Default,
        };
    }
    CliPermissionPolicy {
        mode: PermissionMode::AcceptEdits,
    }
}

/// Mode de permission effectif, et si la configuration a remplacé le défaut du
/// mode headless (US-016 AC6). Ce remplacement peut ÉLARGIR ce qu'un `-p`
/// s'autorise : il est donc annoncé plutôt que subi, et seule la configuration
/// globale peut porter la clé (`settings::SECURITY_KEYS`), jamais celle du
/// projet.
fn resolve_permission_mode(
    from_config: Option<PermissionMode>,
    policy: CliPermissionPolicy,
    headless: bool,
) -> (PermissionMode, bool) {
    let mode = from_config.unwrap_or(policy.mode);
    (mode, headless && mode != policy.mode)
}

fn sandbox_enforced_from_args(
    args: &Args,
    workspace: &std::path::Path,
    settings_path: Option<&std::path::Path>,
    writable_roots: &agent_sandbox::WritableRoots,
) -> bool {
    if !args.sandbox {
        if args.yes {
            eprintln!(
                "[sandbox] disabled by --no-sandbox: --yes may accept edits without filesystem confinement"
            );
        } else {
            eprintln!("[sandbox] disabled by --no-sandbox");
        }
        return false;
    }
    // US-012 AC2 : une racine écartée est tracée, jamais silencieuse.
    for ignored in &writable_roots.ignored {
        eprintln!(
            "[sandbox] writable root ignored: {} ({})",
            ignored.path.display(),
            ignored.reason.message()
        );
    }
    let writable: Vec<&std::path::Path> = settings_path.into_iter().collect();
    match agent_sandbox::enforce_process(workspace, &writable, &writable_roots.as_paths()) {
        Ok(status) => {
            if let Some(w) = status.warning() {
                eprintln!("[sandbox] {w}");
            }
            status == agent_sandbox::fs::SandboxStatus::Enforced
        }
        Err(e) => {
            if args.yes {
                eprintln!(
                    "[sandbox] enforcement failed: {e}; --yes may accept edits without filesystem confinement"
                );
            } else {
                eprintln!("[sandbox] enforcement failed: {e}");
            }
            false
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    if args.help {
        print!("{HELP}");
        return Ok(());
    }
    let workspace = std::env::current_dir()?;

    // Skills lus AVANT le sandbox : `~/.agents/skills` est hors workspace, donc
    // inaccessible une fois Landlock appliqué.
    let skills = read_skills();

    // Config MCP lue AVANT le sandbox : `~/.claude.json` (serveurs Claude Code
    // réutilisés) est hors workspace, donc inaccessible une fois Landlock posé. En
    // mode -p (headless) le menu /mcp n'existe pas → on ne lit rien (latence).
    let mcp_config = if args.prompt.is_none() {
        read_mcp_config(&workspace)
    } else {
        agent_mcp::McpConfigFile::default()
    };

    // Contexte projet (AGENTS.md + env) lu AVANT le sandbox : la remontée
    // d'ancêtres jusqu'au `.git` devient inaccessible une fois Landlock posé
    // (US-028). Injecté ensuite comme messages éphémères par tour.
    let context_msgs = context::messages(&workspace, &context::today_utc());

    let credential = prepare_credential_before_sandbox(&args)?;

    // Configuration lue AVANT le sandbox et dans LES DEUX modes (US-016 AC6) : le
    // fichier global est hors workspace, donc inaccessible une fois Landlock posé,
    // et le mode headless a autant besoin de ses réglages que l'interactif. Lire
    // n'est pas persister : `-p` ne réécrit jamais le fichier (voir plus bas).
    let config = settings::load(
        settings::default_settings_path().as_deref(),
        Some(&settings::project_config_path(&workspace)),
    );
    for warning in &config.warnings {
        eprintln!("[config] {warning}");
    }

    // Réglages persistants (`~/.pyxis/settings.toml`) : le fichier doit exister
    // AVANT Landlock pour recevoir sa règle d'écriture. En headless (-p) rien
    // n'est persisté : la session est pilotée par la configuration et les flags.
    let settings_path = if args.prompt.is_none() {
        settings::default_settings_path().filter(|path| match settings::ensure_file(path) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("[settings] {}: {err}", path.display());
                false
            }
        })
    } else {
        None
    };

    // Racines writables résolues AVANT le runtime (US-012 AC3) : `restrict_self`
    // est irréversible et précède tokio, donc la liste doit être connue ici.
    let writable_roots = agent_sandbox::resolve_writable_roots(
        &config.writable_roots,
        settings::home_dir().as_deref(),
    );

    // Sandbox FS AVANT le runtime (thread principal → hérité par les workers).
    let sandbox_enforced =
        sandbox_enforced_from_args(&args, &workspace, settings_path.as_deref(), &writable_roots);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(
        args,
        workspace,
        PreSandbox {
            skills,
            mcp_config,
            context_msgs,
            cred: credential,
            settings_path,
            config,
        },
        sandbox_enforced,
    ))
}

/// Découvre les serveurs MCP avant le sandbox : `<workspace>/.mcp.json` (priorité
/// haute) fusionné sous les `mcpServers` user-scope de `~/.claude.json`. Si la
/// config workspace existe mais est invalide, on n'active pas le fallback user.
fn read_mcp_config(workspace: &std::path::Path) -> agent_mcp::McpConfigFile {
    let workspace_file = workspace.join(".mcp.json");
    let workspace_cfg = match agent_mcp::McpConfigFile::load(workspace) {
        Ok(cfg) => cfg,
        Err(e) if workspace_file.exists() => {
            eprintln!("[mcp] invalid workspace config: {e}; ignoring user MCP");
            return agent_mcp::McpConfigFile::default();
        }
        Err(e) => {
            eprintln!("[mcp] {e}");
            agent_mcp::McpConfigFile::default()
        }
    };
    let claude_cfg = home_dir()
        .map(|home| {
            let path = home.join(".claude.json");
            agent_mcp::McpConfigFile::load_claude(&path).unwrap_or_else(|e| {
                eprintln!("[mcp] ~/.claude.json: {e}");
                agent_mcp::McpConfigFile::default()
            })
        })
        .unwrap_or_default();
    workspace_cfg.merge_under(claude_cfg)
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

fn load_chatgpt_credential() -> anyhow::Result<CredentialBootstrap> {
    match store::load(KEYRING_ACCOUNT)? {
        Some(agent_auth::Credential::Oauth(o)) if o.provider == ProviderId::OpenAiChatGpt => {
            Ok(CredentialBootstrap::Connected(o))
        }
        Some(agent_auth::Credential::Oauth(o)) => {
            Ok(CredentialBootstrap::WrongProvider(o.provider))
        }
        _ => Ok(CredentialBootstrap::Missing),
    }
}

fn headless_auth_error(bootstrap: &CredentialBootstrap) -> String {
    match bootstrap {
        CredentialBootstrap::Connected(_) => String::new(),
        CredentialBootstrap::Missing => {
            "Pyxis is not connected to ChatGPT. Run `pyxis` without -p to open onboarding.".into()
        }
        CredentialBootstrap::WrongProvider(provider) => format!(
            "Invalid ChatGPT credential in the keyring ({provider:?}). Run `pyxis` without -p to reconnect ChatGPT."
        ),
    }
}

fn prepare_credential_before_sandbox(args: &Args) -> anyhow::Result<OAuthCredential> {
    let bootstrap = load_chatgpt_credential()?;
    match bootstrap {
        CredentialBootstrap::Connected(cred) => Ok(cred),
        missing_or_invalid if args.prompt.is_some() => {
            anyhow::bail!("{}", headless_auth_error(&missing_or_invalid))
        }
        CredentialBootstrap::Missing => run_auth_onboarding(),
        CredentialBootstrap::WrongProvider(provider) => {
            eprintln!(
                "Invalid ChatGPT credential in the keyring ({provider:?}). Reconnection required."
            );
            run_auth_onboarding()
        }
    }
}

fn save_chatgpt_credential(cred: OAuthCredential) -> anyhow::Result<()> {
    store::save(KEYRING_ACCOUNT, &agent_auth::Credential::Oauth(cred))?;
    match load_chatgpt_credential()? {
        CredentialBootstrap::Connected(_) => Ok(()),
        CredentialBootstrap::Missing => anyhow::bail!(
            "ChatGPT credential not found after keyring write. The Windows secret store did not persist the entry."
        ),
        CredentialBootstrap::WrongProvider(provider) => anyhow::bail!(
            "ChatGPT credential was read back with the wrong provider ({provider:?}) after keyring write."
        ),
    }
}

fn run_auth_onboarding() -> anyhow::Result<OAuthCredential> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        eprintln!();
        eprintln!("Welcome to Pyxis");
        eprintln!("ChatGPT connection required to use the agent.");
        eprintln!();

        let client = reqwest::Client::new();
        let cred =
            agent_auth::oauth::openai_chatgpt::login_browser_with_notice(&client, |url, opened| {
                if opened {
                    eprintln!("Browser opened. Finish the ChatGPT login, then return here.");
                    eprintln!("If nothing appears, open this URL:");
                    eprintln!("{url}");
                } else {
                    eprintln!("Open this URL to authorize Pyxis:");
                    eprintln!("{url}");
                }
            })
            .await?;

        let stored = cred.clone();
        tokio::task::spawn_blocking(move || save_chatgpt_credential(stored))
            .await
            .map_err(|e| anyhow::anyhow!("keyring: {e}"))??;

        eprintln!("Connected. Starting Pyxis...");
        eprintln!();
        Ok(cred)
    })
}

/// Liste les skills disponibles dans `~/.agents/skills` (un dossier = un skill,
/// nom = nom du dossier), triés. Symlink partagé entre CLIs ; lecture best-effort.
fn read_skills() -> Vec<String> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let dir = home.join(".agents").join("skills");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut skills: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    skills.sort();
    skills
}

/// Tout ce qui touche un chemin hors workspace, donc chargé ou créé AVANT
/// l'enforcement Landlock : au-delà, seul le workspace (et le fichier de réglages)
/// reste accessible en écriture.
struct PreSandbox {
    skills: Vec<String>,
    mcp_config: agent_mcp::McpConfigFile,
    context_msgs: Vec<Message>,
    cred: OAuthCredential,
    /// Fichier de réglages écrivable (interactif seulement). `None` en headless.
    settings_path: Option<std::path::PathBuf>,
    /// Configuration effective (global + projet), lue dans les deux modes.
    config: settings::Config,
}

async fn run(
    mut args: Args,
    workspace: std::path::PathBuf,
    pre: PreSandbox,
    sandbox_enforced: bool,
) -> anyhow::Result<()> {
    let PreSandbox {
        skills,
        mcp_config,
        context_msgs,
        cred,
        settings_path,
        config,
    } = pre;
    // `--model` explicite prime ; sinon on reprend le modèle de la configuration
    // (dernier choix de `/models`, ou réglage de projet). Résolu AVANT tout le
    // reste : la validation du fallback et l'effort initial se calculent sur le
    // modèle réellement utilisé.
    if !args.model_from_cli
        && let Some(model) = config.model.clone()
    {
        args.model = model;
    }
    let run_config = run_config_from_args(&args, &config)?;
    let headless = args.prompt.is_some();
    let initial_reasoning_effort = config
        .reasoning_effort
        .as_deref()
        .and_then(|effort| agent_tui::normalize_reasoning_effort_for_model(&args.model, effort))
        .or_else(|| agent_tui::default_reasoning_effort_for_model(&args.model).map(str::to_string));
    // 1. Credential abonnement ChatGPT chargée avant le sandbox. Si elle manque en
    // interactif, l'onboarding OAuth a déjà tourné avant d'arriver ici.
    let mut chatgpt = OpenAiChatGptProvider::new(
        cred,
        agent_provider::DEFAULT_MAX_CONTEXT,
        initial_reasoning_effort.clone(),
    );
    // US-022 : idle timeout SSE configurable par session (défaut 60 s). Une valeur
    // env invalide/0 est ignorée → garde le défaut (watchdog jamais désactivé).
    if let Some(secs) = std::env::var("PYXIS_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        chatgpt = chatgpt.with_idle_timeout(std::time::Duration::from_secs(secs));
    }
    let chatgpt = Arc::new(chatgpt);
    // Catalogue `/models` découvert sur le compte connecté, hors chemin critique :
    // la session démarre sur le catalogue embarqué et bascule dès la réponse. Un
    // échec (hors ligne, token expiré) laisse simplement le catalogue embarqué.
    if !headless {
        let catalog_source = Arc::clone(&chatgpt);
        tokio::spawn(async move {
            // Erreur volontairement muette : le TUI occupe le terminal, et le
            // catalogue embarqué reste un fallback correct.
            if let Ok(models) = catalog_source.list_models().await {
                agent_tui::set_models(
                    models
                        .into_iter()
                        .map(|model| agent_tui::ModelCatalogEntry {
                            slug: model.slug,
                            default_reasoning_effort: model.default_reasoning_effort,
                            supported_reasoning_efforts: model.supported_reasoning_efforts,
                        })
                        .collect(),
                );
            }
        });
    }
    let provider: Arc<dyn Provider> = chatgpt;

    // 2. Proxy réseau allow-list (fail-closed). Durcit les commandes Bash.
    let proxy = agent_sandbox::spawn_proxy(ProxyPolicy::new(args.allow_hosts.clone())).await?;
    let proxy_addr = proxy.addr.clone();
    let harden: agent_tools::CommandHardener =
        Arc::new(move |cmd: &mut tokio::process::Command| set_proxy_env(cmd, &proxy_addr));
    let mcp_harden = Arc::clone(&harden);

    // 3. Session persistante : un fichier JSONL par conversation (horodaté) sous
    // <workspace>/.pyxis/sessions/, listable/reprenable via `/resume`.
    let sessions_dir = workspace.join(".pyxis").join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let (current_session, initial_messages) = if let Some(resume_arg) = &args.resume {
        let path = resolve_resume_path(&sessions_dir, resume_arg)?;
        let resumed =
            agent_session::resume_file(&path).map_err(|e| anyhow::anyhow!("resume: {e}"))?;
        (path, resumed.messages)
    } else {
        (interactive::new_session_path(&sessions_dir), Vec::new())
    };
    provider.set_prompt_cache_key(&interactive::prompt_cache_key_for_session(&current_session));
    let jsonl = agent_session::JsonlSession::create_at(&current_session)
        .map_err(|e| anyhow::anyhow!("session: {e}"))?;
    let (shared_session, conversation) = SharedSession::new(jsonl);
    if !initial_messages.is_empty() {
        *conversation
            .lock()
            .map_err(|_| anyhow::anyhow!("session: poisoned snapshot"))? = initial_messages;
    }
    let initial_taint_recent = recent_untrusted_content(
        &conversation.lock().map(|g| g.clone()).unwrap_or_default(),
        RESUME_TAINT_SCAN_MESSAGES,
    );

    // Objectif persistant par session (`/goal`) : uniquement en interactif.
    let goal = if headless {
        None
    } else {
        interactive::read_goal(&interactive::goal_path_for_session(&current_session))
    };

    // 4. Registry d'outils + approbateur (TUI en interactif, auto en headless).
    let (perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let policy = permission_policy(headless, args.yes, sandbox_enforced);
    let (initial_permission_mode, announce_override) =
        resolve_permission_mode(config.permission_mode, policy, headless);
    if announce_override {
        eprintln!(
            "[config] permission mode from configuration: {} (default for -p would be {})",
            settings::permission_mode_id(initial_permission_mode),
            settings::permission_mode_id(policy.mode)
        );
    }
    let permission_mode = PermissionModeState::new(initial_permission_mode);
    let approver: Arc<dyn agent_tools::permission::Approver> = if headless {
        Arc::new(AutoDeny)
    } else {
        Arc::new(TuiApprover::new(perm_tx))
    };

    let registry = Registry::builder(&workspace)
        .mode_state(permission_mode.clone())
        .approver(approver)
        .initial_taint_recent(initial_taint_recent)
        .command_hardener(harden)
        .register(Read)
        .register(Glob)
        .register(Grep)
        .register(Write)
        .register(Edit)
        .register(Bash)
        .build();
    let tool_specs = registry.tool_specs();
    // US-026/US-027 : guidelines comportementales des outils, collectées AVANT que
    // `registry` ne soit déplacé dans `Deps`. Le system prompt de base est désormais
    // sélectionné PAR SLUG (US-027) au moment de composer (headless ici, par tour en
    // interactif), pas figé : un `/models` doit pouvoir changer le template.
    let tool_guidelines = registry.behavioral_guidelines();

    // 5. Deps injectées dans la boucle.
    let deps = Deps {
        provider,
        session: shared_session.clone(),
        tokenizer: Arc::new(HeuristicCounter),
        clock: Arc::new(SystemClock),
        tools: Arc::new(registry),
        // US-001 : token de base jamais signalé. La boucle interactive substitue un
        // token PAR TOUR (`launch_turn`) ; le mode headless garde celui-ci.
        cancel: CancelToken::new(),
    };

    // 6. Dispatch headless (-p) vs interactif.
    if let Some(prompt) = args.prompt {
        // Headless one-shot : slug fixe (`args.model`) → template sélectionné une fois.
        let base = interactive::with_tool_guidelines(
            prompt::select_system_prompt(&args.model),
            &tool_guidelines,
        );
        let mut messages = conversation.lock().map(|g| g.clone()).unwrap_or_default();
        messages.push(Message::user(prompt));
        let ctx = AgentContext {
            model: args.model,
            reasoning_effort: initial_reasoning_effort.clone(),
            system: Some(interactive::compose_system(&base, goal.as_deref())),
            messages,
            tools: tool_specs,
            config: run_config,
            context_messages: context_msgs,
            ephemeral_messages: Vec::new(),
        };
        let mut events = jsonl::EventWriter::new(args.output_format);
        let result =
            agent_core::run_headless_observed(ctx, deps, |event| events.event(event)).await;

        let session_id = current_session
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        events.run_summary(&session_id, &result.ended);

        match result.ended {
            agent_core::HeadlessEnd::Error(e) => anyhow::bail!("{e}"),
            agent_core::HeadlessEnd::Exhausted(reason) => anyhow::bail!("stopped: {reason:?}"),
            agent_core::HeadlessEnd::EndTurn => {}
        }
        // En mode JSON, le texte vit déjà dans les événements `text` : l'écrire à
        // nouveau injecterait des lignes non JSON dans le flux.
        if !events.is_json() {
            // En one-shot, pas de boucle d'objectif : on retire juste le marqueur.
            let text = result
                .text
                .replace(interactive::GOAL_DONE_MARKER, "")
                .trim_end()
                .to_string();
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
    } else {
        // Registre MCP construit depuis la config découverte avant le sandbox
        // (workspace + ~/.claude.json). Tous les serveurs démarrent déconnectés ;
        // la connexion se fait à la demande via `/mcp`.
        let mcp = Arc::new(std::sync::Mutex::new(agent_mcp::McpRegistry::from_config(
            mcp_config,
        )));

        let cfg = InteractiveConfig {
            model: args.model,
            reasoning_effort: initial_reasoning_effort,
            tool_guidelines,
            context_messages: context_msgs,
            run_config,
            tool_specs,
            truecolor: agent_tui::supports_truecolor(),
            // Reduced-motion : spinner dégradé en point pulsé (US-044).
            reduced_motion: std::env::var_os("NO_COLOR").is_some()
                || std::env::var_os("PYXIS_REDUCED_MOTION").is_some(),
            // credential chargée plus haut (sinon on a bail) → connecté.
            connected: true,
            skills,
            goal,
            command_hardener: mcp_harden,
            permission_mode,
            settings_path,
        };
        interactive::run(
            deps,
            conversation,
            perm_rx,
            cfg,
            shared_session,
            sessions_dir,
            current_session,
            mcp,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Args, jsonl, parse_args_from, permission_policy, precedence_u64, resolve_permission_mode,
        resolve_resume_path, run_config_from_args, settings,
    };
    use agent_tools::permission::PermissionMode;

    fn args() -> Args {
        Args {
            model: "mock".into(),
            model_from_cli: true,
            prompt: None,
            resume: None,
            allow_hosts: Vec::new(),
            yes: false,
            sandbox: true,
            token_budget: None,
            cost_budget_micro_usd: None,
            input_cost_micro_per_ktok: None,
            output_cost_micro_per_ktok: None,
            overload_fallback_model: None,
            output_format: jsonl::OutputFormat::Text,
            help: false,
        }
    }

    /// US-016 AC6 : la configuration globale pilote aussi le mode de permission
    /// du headless. Le remplacement est SIGNALÉ, parce qu'il peut élargir ce
    /// qu'un `-p` s'autorise par rapport au défaut fail-closed.
    #[test]
    fn configuration_replaces_the_headless_permission_default_and_says_so() {
        let headless_default = permission_policy(true, false, true);
        assert_eq!(headless_default.mode, PermissionMode::Default);

        let (mode, announced) = resolve_permission_mode(
            Some(PermissionMode::BypassPermissions),
            headless_default,
            true,
        );
        assert_eq!(mode, PermissionMode::BypassPermissions);
        assert!(announced, "un elargissement en headless doit etre annonce");

        // Sans configuration, le défaut fail-closed est conservé, sans bruit.
        let (mode, announced) = resolve_permission_mode(None, headless_default, true);
        assert_eq!(mode, PermissionMode::Default);
        assert!(!announced);

        // Une configuration qui redit le défaut n'annonce rien non plus.
        let (_, announced) =
            resolve_permission_mode(Some(PermissionMode::Default), headless_default, true);
        assert!(!announced);

        // En interactif, la substitution est le comportement normal depuis
        // US-012 : rien à annoncer.
        let interactive = permission_policy(false, false, true);
        let (mode, announced) =
            resolve_permission_mode(Some(PermissionMode::AcceptEdits), interactive, false);
        assert_eq!(mode, PermissionMode::AcceptEdits);
        assert!(!announced);
    }

    /// US-016 AC1, les quatre niveaux que `main` superpose : défaut (aucune
    /// source) < configuration < environnement < argument.
    #[test]
    fn precedence_runs_config_then_env_then_argument() {
        let name = "--token-budget";
        let env = "PYXIS_TOKEN_BUDGET";

        assert_eq!(precedence_u64(None, name, None, env, None).unwrap(), None);
        assert_eq!(
            precedence_u64(None, name, None, env, Some(10)).unwrap(),
            Some(10)
        );
        assert_eq!(
            precedence_u64(None, name, Some("20"), env, Some(10)).unwrap(),
            Some(20)
        );
        assert_eq!(
            precedence_u64(Some("30"), name, Some("20"), env, Some(10)).unwrap(),
            Some(30)
        );
        // Une variable vide n'est pas une définition : elle laisse passer la
        // configuration au lieu d'écraser avec un zéro.
        assert_eq!(
            precedence_u64(None, name, Some("  "), env, Some(10)).unwrap(),
            Some(10)
        );
    }

    /// AC1 côté `RunConfig` : sans argument ni environnement, la configuration
    /// pilote réellement le budget passé au cœur.
    #[test]
    fn run_config_falls_back_to_the_configuration_file() {
        let config = settings::Config {
            token_budget: Some(4242),
            overload_fallback_model: Some("gpt-5.5".into()),
            ..settings::Config::default()
        };
        let mut args = args();
        args.model = "gpt-5.5".into();

        let cfg = run_config_from_args(&args, &config).unwrap();

        assert_eq!(cfg.token_budget, Some(4242));
        assert_eq!(cfg.overload_fallback_model.as_deref(), Some("gpt-5.5"));
    }

    /// L'argument prime sur la configuration, dans le sens attendu.
    #[test]
    fn cli_argument_overrides_the_configuration_file() {
        let config = settings::Config {
            token_budget: Some(4242),
            ..settings::Config::default()
        };
        let mut args = args();
        args.token_budget = Some("7".into());

        let cfg = run_config_from_args(&args, &config).unwrap();

        assert_eq!(cfg.token_budget, Some(7));
    }

    #[test]
    fn run_config_reads_token_budget_flag() {
        let mut args = args();
        args.token_budget = Some("1234".into());
        let cfg = run_config_from_args(&args, &settings::Config::default()).unwrap();
        assert_eq!(cfg.token_budget, Some(1234));
    }

    #[test]
    fn run_config_reads_complete_cost_budget() {
        let mut args = args();
        args.cost_budget_micro_usd = Some("10".into());
        args.input_cost_micro_per_ktok = Some("2".into());
        args.output_cost_micro_per_ktok = Some("4".into());
        let cfg = run_config_from_args(&args, &settings::Config::default()).unwrap();
        let cost = cfg.cost_budget.unwrap();
        assert_eq!(cost.limit_micro_usd, 10);
        assert_eq!(cost.input_micro_per_ktok, 2);
        assert_eq!(cost.output_micro_per_ktok, 4);
    }

    #[test]
    fn run_config_rejects_incomplete_cost_budget() {
        let mut args = args();
        args.cost_budget_micro_usd = Some("10".into());
        let err = run_config_from_args(&args, &settings::Config::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("incomplete cost budget"));
    }

    #[test]
    fn run_config_rejects_zero_budget() {
        let mut args = args();
        args.token_budget = Some("0".into());
        let err = run_config_from_args(&args, &settings::Config::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be > 0"));
    }

    #[test]
    fn run_config_reads_overload_fallback_model() {
        let mut args = args();
        args.overload_fallback_model = Some(" fallback ".into());
        let cfg = run_config_from_args(&args, &settings::Config::default()).unwrap();
        assert_eq!(cfg.overload_fallback_model.as_deref(), Some("fallback"));
    }

    #[test]
    fn run_config_rejects_cross_prompt_family_fallback() {
        let mut args = args();
        args.model = "gpt-5-codex".into();
        args.overload_fallback_model = Some("gpt-5.5".into());
        let err = run_config_from_args(&args, &settings::Config::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback model is incompatible"));
    }

    #[test]
    fn parse_args_reads_resume_latest() {
        let args = parse_args_from(vec!["--resume".to_string()]).unwrap();
        assert_eq!(args.resume.as_deref(), Some(""));
        assert!(args.prompt.is_none());
    }

    #[test]
    fn parse_args_reads_resume_id_and_headless_prompt() {
        let args = parse_args_from(vec![
            "--resume".to_string(),
            "123.jsonl".to_string(),
            "-p".to_string(),
            "continue".to_string(),
        ])
        .unwrap();
        assert_eq!(args.resume.as_deref(), Some("123.jsonl"));
        assert_eq!(args.prompt.as_deref(), Some("continue"));
    }

    #[test]
    fn parse_args_resume_without_id_does_not_swallow_next_flag() {
        let args = parse_args_from(vec![
            "--resume".to_string(),
            "-p".to_string(),
            "continue".to_string(),
        ])
        .unwrap();
        assert_eq!(args.resume.as_deref(), Some(""));
        assert_eq!(args.prompt.as_deref(), Some("continue"));
    }

    #[test]
    fn parse_args_rejects_missing_print_value() {
        let err = parse_args_from(vec!["-p".to_string(), "--resume".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("-p: missing value"));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args_from(vec!["--wat".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn parse_args_rejects_model_flag_without_value() {
        let err = parse_args_from(vec!["--model".to_string(), "--resume".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--model: missing value"));
    }

    #[test]
    fn parse_args_rejects_extra_positional() {
        let err = parse_args_from(vec!["one".to_string(), "two".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unexpected positional argument"));
    }

    #[test]
    fn parse_args_reads_help() {
        let args = parse_args_from(vec!["--help".to_string()]).unwrap();
        assert!(args.help);
    }

    #[test]
    fn resolve_resume_path_rejects_missing_explicit_session() {
        let dir = std::env::temp_dir().join(format!("pyxis-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = resolve_resume_path(&dir, "missing.jsonl")
            .unwrap_err()
            .to_string();
        assert!(err.contains("session not found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headless_without_yes_is_fail_closed_default() {
        let p = permission_policy(true, false, true);
        assert_eq!(p.mode, agent_tools::permission::PermissionMode::Default);
    }

    #[test]
    fn headless_yes_accepts_edits_but_not_sensitive_actions() {
        let p = permission_policy(true, true, true);
        assert_eq!(p.mode, agent_tools::permission::PermissionMode::AcceptEdits);
    }

    #[test]
    fn headless_yes_accepts_edits_even_without_sandbox() {
        let p = permission_policy(true, true, false);
        assert_eq!(p.mode, agent_tools::permission::PermissionMode::AcceptEdits);
    }
}
