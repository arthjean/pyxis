//! `pyxis`: the CLI binary. The ONLY crate that wires everything (ARCHITECTURE 2):
//! core + ChatGPT subscription provider + tools + session + sandbox + TUI frontend.
//!
//! Critical order: the **FS sandbox (Landlock) is applied on the main thread
//! BEFORE the tokio runtime is built** -> the workers and the Bash
//! subprocesses inherit the confinement (fork-safe, see `agent_sandbox::fs`).
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
    /// `--model` passed explicitly: distinguishes the compile-time default from a
    /// user choice, and therefore wins over the persisted model.
    model_from_cli: bool,
    allow_hosts: Vec<String>,
    yes: bool,
    sandbox: bool,
    token_budget: Option<String>,
    cost_budget_micro_usd: Option<String>,
    input_cost_micro_per_ktok: Option<String>,
    output_cost_micro_per_ktok: Option<String>,
    overload_fallback_model: Option<String>,
    /// Output format of the headless mode (US-017). `Text` by default.
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

/// Precedence of a numeric setting (US-016 AC1): configuration < environment
/// variable < argument. The first undefined level is simply
/// traversed, which makes the chain identical whatever the number of sources
/// actually filled in. The environment is passed as a parameter rather than read
/// here: precedence becomes testable without mutating the process environment.
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
        // US-002: the calibration probe is decided by the binary, and it is the
        // binary that writes its output. The core only computes the estimate.
        usage_probe: std::env::var_os("PYXIS_DEBUG_USAGE").is_some(),
        ..RunConfig::default()
    })
}

/// US-005: sandbox scope in one line, as `/status` shows it. Resolved here
/// because enforcement happens before the interactive loop exists, and the loop
/// has no way to observe it afterwards.
fn sandbox_scope_label(enforced: bool, extra_roots: &[std::path::PathBuf]) -> String {
    if !enforced {
        return "off (writes not restricted)".to_string();
    }
    match extra_roots.len() {
        0 => "enforced (workspace)".to_string(),
        1 => "enforced (workspace + 1 extra root)".to_string(),
        n => format!("enforced (workspace + {n} extra roots)"),
    }
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

/// Effective permission mode, and whether the configuration replaced the default of
/// the headless mode (US-016 AC6). That replacement can WIDEN what a `-p`
/// allows itself: it is therefore announced rather than silently applied, and only the global
/// configuration can carry the key (`settings::SECURITY_KEYS`), never the
/// project one.
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
    // US-012 AC2: a discarded root is logged, never silent.
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

    // Skills read BEFORE the sandbox: `~/.agents/skills` is outside the workspace, hence
    // inaccessible once Landlock is applied.
    let skills = read_skills();

    // MCP config read BEFORE the sandbox: `~/.claude.json` (reused Claude Code
    // servers) is outside the workspace, hence inaccessible once Landlock is in place. In
    // -p (headless) mode the /mcp menu does not exist -> we read nothing (latency).
    let mcp_config = if args.prompt.is_none() {
        read_mcp_config(&workspace)
    } else {
        agent_mcp::McpConfigFile::default()
    };

    // Project context (AGENTS.md + env) read BEFORE the sandbox: walking up the
    // ancestors to the `.git` becomes inaccessible once Landlock is in place
    // (US-028). Injected afterwards as ephemeral messages per turn.
    let context_msgs = context::messages(&workspace, &context::today_utc());

    let credential = prepare_credential_before_sandbox(&args)?;

    // Configuration read BEFORE the sandbox and in BOTH modes (US-016 AC6): the
    // global file is outside the workspace, hence inaccessible once Landlock is in place,
    // and the headless mode needs its settings as much as the interactive one. Reading
    // is not persisting: `-p` never rewrites the file (see below).
    let config = settings::load(
        settings::default_settings_path().as_deref(),
        Some(&settings::project_config_path(&workspace)),
    );
    for warning in &config.warnings {
        eprintln!("[config] {warning}");
    }

    // Persistent settings (`~/.pyxis/settings.toml`): the file must exist
    // BEFORE Landlock to receive its write rule. In headless (-p) nothing
    // is persisted: the session is driven by the configuration and the flags.
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

    // Writable roots resolved BEFORE the runtime (US-012 AC3): `restrict_self`
    // is irreversible and precedes tokio, so the list must be known here.
    let writable_roots = agent_sandbox::resolve_writable_roots(
        &config.writable_roots,
        settings::home_dir().as_deref(),
    );

    // FS sandbox BEFORE the runtime (main thread -> inherited by the workers).
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

/// Discovers the MCP servers before the sandbox: `<workspace>/.mcp.json` (high
/// priority) merged over the user-scope `mcpServers` of `~/.claude.json`. When the
/// workspace config exists but is invalid, we do not enable the user fallback.
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

/// Per-server bound of the startup connection (spawn + handshake + `tools/list`).
/// Every server is dialed concurrently, so this is also the ceiling this step adds
/// to the total launch time (US-012 AC3).
const MCP_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Connects the configured MCP servers and wraps their tools as `DynTool`
/// (US-012). Returns the tools to register and the notices the session must show.
///
/// A server declared by the workspace, shadowing a user entry, or carrying a
/// sensitive env key is deliberately NOT connected here: it stays behind the
/// explicit `/mcp <server> trust` gate (US-013 AC5). Opening a repository must
/// never be enough to obtain a process spawn.
///
/// A server that fails or exceeds its bound is left out: the healthy servers keep
/// their tools and the session starts (US-012 AC2).
async fn connect_mcp_at_startup(
    mcp: &Arc<std::sync::Mutex<agent_mcp::McpRegistry>>,
    harden: &agent_tools::CommandHardener,
) -> (Vec<Box<dyn agent_tools::DynTool>>, Vec<String>) {
    let mut notices: Vec<String> = Vec::new();
    let mut candidates: Vec<String> = Vec::new();
    match mcp.lock() {
        Ok(reg) => {
            for (name, server) in reg.iter() {
                if interactive::mcp_requires_trust(server.config()) {
                    notices.push(format!(
                        "MCP \"{name}\" not connected: explicit trust required (/mcp {name} trust)."
                    ));
                } else {
                    candidates.push(name.clone());
                }
            }
        }
        Err(_) => {
            return (
                Vec::new(),
                vec!["MCP: registry unavailable at startup.".to_string()],
            );
        }
    }
    if candidates.is_empty() {
        return (Vec::new(), notices);
    }

    let mut pending = tokio::task::JoinSet::new();
    for name in candidates {
        let begin = match mcp.lock() {
            Ok(mut reg) => reg.begin_connect(&name),
            Err(_) => continue,
        };
        let (cfg, old) = match begin {
            Ok(pair) => pair,
            Err(err) => {
                notices.push(format!("MCP: {err}"));
                continue;
            }
        };
        if let Some(old) = old {
            tokio::spawn(async move { old.cancel().await });
        }
        let harden = Arc::clone(harden);
        pending.spawn(async move {
            let attempt = async {
                let conn =
                    agent_mcp::McpConnection::connect_hardened(&name, &cfg, Some(&harden)).await?;
                match conn.list_tools(&name).await {
                    Ok(tools) => Ok((conn, tools)),
                    Err(err) => {
                        conn.cancel().await;
                        Err(err)
                    }
                }
            };
            // On expiry the future is dropped: the transport `Drop` kills the
            // subprocess, so a slow server leaves nothing behind.
            match tokio::time::timeout(MCP_STARTUP_TIMEOUT, attempt).await {
                Ok(Ok(connected)) => (name, Ok(connected)),
                Ok(Err(err)) => (name, Err(err.to_string())),
                Err(_) => (
                    name,
                    Err(format!(
                        "connection timeout after {}s",
                        MCP_STARTUP_TIMEOUT.as_secs()
                    )),
                ),
            }
        });
    }

    let mut tools: Vec<Box<dyn agent_tools::DynTool>> = Vec::new();
    // Shared across every server: uniqueness of the exposed names is a property of
    // the whole set (US-011 AC1).
    let mut taken = std::collections::BTreeSet::new();
    while let Some(joined) = pending.join_next().await {
        let (name, outcome) = match joined {
            Ok(pair) => pair,
            Err(err) => {
                notices.push(format!("MCP: connection task failed: {err}"));
                continue;
            }
        };
        let (conn, listed) = match outcome {
            Ok(connected) => connected,
            Err(err) => {
                if let Ok(mut reg) = mcp.lock() {
                    reg.fail(&name, err.clone());
                }
                notices.push(format!(
                    "MCP \"{name}\" unavailable: {err} (its tools are absent)."
                ));
                continue;
            }
        };
        let client = conn.client(&name);
        let (mut exposed, skipped) = agent_mcp::dyn_tools(&name, &listed, &client, &mut taken);
        for skip in skipped {
            notices.push(skip.summary());
        }
        tools.append(&mut exposed);
        match mcp.lock() {
            Ok(mut reg) => {
                if let Some(orphan) = reg.finish_connect(&name, conn, listed) {
                    tokio::spawn(async move { orphan.cancel().await });
                }
            }
            Err(_) => {
                tokio::spawn(async move { conn.cancel().await });
                notices.push(format!(
                    "MCP \"{name}\": registry unavailable, connection closed."
                ));
            }
        }
    }
    (tools, notices)
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

/// Lists the skills available in `~/.agents/skills` (one directory = one skill,
/// name = directory name), sorted. Symlink shared between CLIs; best-effort read.
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

/// Everything that touches a path outside the workspace, hence loaded or created BEFORE
/// the Landlock enforcement: past it, only the workspace (and the settings file)
/// stays writable.
struct PreSandbox {
    skills: Vec<String>,
    mcp_config: agent_mcp::McpConfigFile,
    context_msgs: Vec<Message>,
    cred: OAuthCredential,
    /// Writable settings file (interactive only). `None` in headless mode.
    settings_path: Option<std::path::PathBuf>,
    /// Effective configuration (global + project), read in both modes.
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
    // An explicit `--model` wins; otherwise we take the model of the configuration
    // (last `/models` choice, or project setting). Resolved BEFORE everything
    // else: the fallback validation and the initial effort are computed on the
    // model actually used.
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
    // 1. ChatGPT subscription credential loaded before the sandbox. When it is missing in
    // interactive mode, the OAuth onboarding has already run before we get here.
    let mut chatgpt = OpenAiChatGptProvider::new(
        cred,
        agent_provider::DEFAULT_MAX_CONTEXT,
        initial_reasoning_effort.clone(),
    );
    // US-022: SSE idle timeout configurable per session (default 60 s). An invalid/0
    // env value is ignored -> keeps the default (watchdog never disabled).
    if let Some(secs) = std::env::var("PYXIS_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        chatgpt = chatgpt.with_idle_timeout(std::time::Duration::from_secs(secs));
    }
    let chatgpt = Arc::new(chatgpt);
    // `/models` catalog discovered on the connected account, off the critical path:
    // the session starts on the bundled catalog and switches as soon as the answer arrives. A
    // failure (offline, expired token) simply leaves the bundled catalog.
    if !headless {
        let catalog_source = Arc::clone(&chatgpt);
        tokio::spawn(async move {
            // Error deliberately silent: the TUI owns the terminal, and the
            // bundled catalog stays a correct fallback.
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

    // 2. Network allow-list proxy (fail-closed). Hardens the Bash commands.
    let proxy = agent_sandbox::spawn_proxy(ProxyPolicy::new(args.allow_hosts.clone())).await?;
    let proxy_addr = proxy.addr.clone();
    let harden: agent_tools::CommandHardener =
        Arc::new(move |cmd: &mut tokio::process::Command| set_proxy_env(cmd, &proxy_addr));
    let mcp_harden = Arc::clone(&harden);

    // 3. Persistent session: one JSONL file per conversation (timestamped) under
    // <workspace>/.pyxis/sessions/, listable/resumable through `/resume`.
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

    // Persistent per-session goal (`/goal`): interactive mode only.
    let goal = if headless {
        None
    } else {
        interactive::read_goal(&interactive::goal_path_for_session(&current_session))
    };

    // 4. Tool registry + approver (TUI in interactive mode, auto in headless).
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

    // US-008: session approval memory, shared with the frontend like the
    // permission mode. In memory only: nothing is written to disk.
    let approvals = agent_tools::permission::ApprovalMemory::new();

    // US-012: MCP servers connected BEFORE the tool registry is built, because the
    // tools they expose enter it as `DynTool` and the specs are composed from it.
    // Headless mode reads no MCP config (see `main`), hence connects nothing and
    // keeps its output byte-for-byte identical.
    let mcp = Arc::new(std::sync::Mutex::new(agent_mcp::McpRegistry::from_config(
        mcp_config,
    )));
    let (mcp_tools, mcp_notices) = if headless {
        (Vec::new(), Vec::new())
    } else {
        connect_mcp_at_startup(&mcp, &mcp_harden).await
    };

    // US-017: hooks come from the GLOBAL configuration alone (`settings.rs` drops
    // the key from a workspace file). Without a declaration the registry keeps
    // `NoHooks`: no process, no clone, no added latency.
    let (hook_notice_tx, hook_notice_rx) = tokio::sync::mpsc::channel::<String>(16);
    let hooks: Arc<dyn agent_tools::hooks::Hooks> = if config.hooks.is_empty() {
        Arc::new(agent_tools::hooks::NoHooks)
    } else {
        // A later hook failing is reported to the human, never to the model: in
        // the TUI as a notice, in headless mode on stderr, where the diagnostics
        // of this mode already go (stdout stays byte-for-byte identical).
        let notice: agent_tools::hooks::HookNotice = if headless {
            Arc::new(|message: String| eprintln!("[hook] {message}"))
        } else {
            Arc::new(move |message: String| {
                let _ = hook_notice_tx.try_send(message);
            })
        };
        Arc::new(
            agent_tools::hooks::CommandHooks::new(config.hooks.clone(), &workspace)
                .with_hardener(Arc::clone(&mcp_harden))
                .with_notice(notice),
        )
    };

    let mut builder = Registry::builder(&workspace)
        .mode_state(permission_mode.clone())
        .approver(approver)
        .approvals(approvals.clone())
        .initial_taint_recent(initial_taint_recent)
        .hooks(hooks)
        .command_hardener(harden)
        .register(Read)
        .register(Glob)
        .register(Grep)
        .register(Write)
        .register(Edit)
        .register(Bash);
    for tool in mcp_tools {
        builder = builder.register_dyn(tool);
    }
    let registry = builder.build();
    let tool_specs = registry.tool_specs();
    // US-026/US-027: behavioral guidelines of the tools, collected BEFORE
    // `registry` is moved into `Deps`. The base system prompt is now
    // selected PER SLUG (US-027) when composing (headless here, per turn in
    // interactive mode), not frozen: a `/models` must be able to change the template.
    let tool_guidelines = registry.behavioral_guidelines();

    // 5. Deps injected into the loop.
    let deps = Deps {
        provider,
        session: shared_session.clone(),
        tokenizer: Arc::new(HeuristicCounter),
        clock: Arc::new(SystemClock),
        tools: Arc::new(registry),
        // US-001: base token never signalled. The interactive loop substitutes a
        // PER-TURN token (`launch_turn`); the headless mode keeps this one.
        cancel: CancelToken::new(),
    };

    // 6. Headless (-p) vs interactive dispatch.
    if let Some(prompt) = args.prompt {
        // Headless one-shot: fixed slug (`args.model`) -> template selected once.
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
        // US-018: reference taken BEFORE the turn, on the workspace as it is.
        // Machine output only: the text format must stay identical to the
        // character (US-017 AC4), so it has no consumer for this diff
        // and must not pay for a `git status` per run.
        let mut diff_tracker = if events.is_json() {
            Some(agent_tools::turn_diff::TurnDiffTracker::begin(&workspace).await)
        } else {
            None
        };
        let result =
            agent_core::run_headless_observed(ctx, deps, |event| events.event(event)).await;

        // Aggregated diff after the end of the turn, hence after the last tool write
        // (US-018 AC6: including when the turn was interrupted).
        if let Some(tracker) = diff_tracker.as_mut() {
            match tracker.turn_diff().await {
                Ok(diff) if !diff.is_empty() => {
                    events.event(&agent_core::AgentEvent::TurnDiff(diff))
                }
                Ok(_) => {}
                Err(err) => eprintln!("[diff] {err}"),
            }
        }
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
        // In JSON mode, the text already lives in the `text` events: writing it
        // again would inject non-JSON lines into the stream.
        if !events.is_json() {
            // In one-shot mode, no goal loop: we simply remove the marker.
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
        let cfg = InteractiveConfig {
            model: args.model,
            reasoning_effort: initial_reasoning_effort,
            tool_guidelines,
            context_messages: context_msgs,
            run_config,
            tool_specs,
            truecolor: agent_tui::supports_truecolor(),
            // Reduced motion: spinner degraded to a pulsing dot (US-044).
            reduced_motion: std::env::var_os("NO_COLOR").is_some()
                || std::env::var_os("PYXIS_REDUCED_MOTION").is_some(),
            // credential loaded above (otherwise we bail) -> connected.
            connected: true,
            skills,
            goal,
            command_hardener: Arc::clone(&mcp_harden),
            mcp_notices,
            permission_mode,
            approvals,
            settings_path,
            workspace: workspace.clone(),
            sandbox_scope: sandbox_scope_label(sandbox_enforced, &config.writable_roots),
        };
        interactive::run(
            deps,
            conversation,
            perm_rx,
            hook_notice_rx,
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

    /// US-016 AC6: the global configuration also drives the permission mode
    /// of the headless mode. The replacement is REPORTED, because it can widen what
    /// a `-p` allows itself compared to the fail-closed default.
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

        // Without a configuration, the fail-closed default is kept, silently.
        let (mode, announced) = resolve_permission_mode(None, headless_default, true);
        assert_eq!(mode, PermissionMode::Default);
        assert!(!announced);

        // A configuration that restates the default announces nothing either.
        let (_, announced) =
            resolve_permission_mode(Some(PermissionMode::Default), headless_default, true);
        assert!(!announced);

        // In interactive mode, the substitution is the normal behavior since
        // US-012: nothing to announce.
        let interactive = permission_policy(false, false, true);
        let (mode, announced) =
            resolve_permission_mode(Some(PermissionMode::AcceptEdits), interactive, false);
        assert_eq!(mode, PermissionMode::AcceptEdits);
        assert!(!announced);
    }

    /// US-016 AC1, the four levels that `main` layers: default (no
    /// source) < configuration < environment < argument.
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
        // An empty variable is not a definition: it lets the configuration
        // through instead of overwriting with a zero.
        assert_eq!(
            precedence_u64(None, name, Some("  "), env, Some(10)).unwrap(),
            Some(10)
        );
    }

    /// AC1 on the `RunConfig` side: without an argument nor an environment variable, the
    /// configuration really drives the budget passed to the core.
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

    /// The argument wins over the configuration, in the expected direction.
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
