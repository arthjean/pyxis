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
mod observability;
mod prompt;
mod session;
mod settings;
mod skills;

use std::sync::Arc;

use agent_auth::{OAuthCredential, ProviderId, store};
use agent_core::clock::SystemClock;
use agent_core::guardrail::CostBudget;
use agent_core::message::{Message, recent_untrusted_content};
use agent_core::provider::Provider;
use agent_core::sandbox::SandboxPolicy;
use agent_core::{AgentContext, CancelToken, Deps, RunConfig};
use agent_provider::{KEYRING_ACCOUNT, OpenAiChatGptProvider};
use agent_sandbox::{ProxyPolicy, set_proxy_env};
use agent_tokenizer::HeuristicCounter;
use agent_tools::permission::{AutoDeny, PermissionMode, PermissionModeState};
use agent_tools::{
    ApplyPatch, Bash, DynTool, Edit, ExecCommand, ExecSessions, Glob, Grep, Read, Registry,
    UpdatePlan, ViewImage, Write, WriteStdin,
};

use crate::approver::TuiApprover;
use crate::interactive::InteractiveConfig;
use crate::session::SharedSession;

const RESUME_TAINT_SCAN_MESSAGES: usize = 8;

#[derive(Debug)]
struct Args {
    prompt: Option<String>,
    /// `-p` / `--print` seen, with or without its inline value (US-020). Kept
    /// apart from `prompt`, which stays `None` until standard input is resolved.
    print: bool,
    /// `--ephemeral` (US-020 AC4): nothing of this run is persisted.
    ephemeral: bool,
    /// `--output-last-message <path>` (US-020 AC3).
    output_last_message: Option<String>,
    resume: Option<String>,
    model: String,
    /// `--model` passed explicitly: distinguishes the compile-time default from a
    /// user choice, and therefore wins over the persisted model.
    model_from_cli: bool,
    allow_hosts: Vec<String>,
    yes: bool,
    sandbox: bool,
    /// `--profile <name>` (US-006): selects a `[profiles.<name>]` table.
    profile: Option<String>,
    /// `-c key=value` in command-line order (US-007).
    overrides: Vec<(String, String)>,
    /// `--permission-mode <mode>` (US-008), already validated.
    permission_mode: Option<PermissionMode>,
    /// `--sandbox <mode>` (US-008), already checked against `SandboxPolicy::IDS`.
    sandbox_mode: Option<String>,
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
  -p, --print [prompt]                 Mode headless one-shot. Without a value the
                                        prompt is read from standard input; with
                                        one, the argument wins and standard input
                                        is not read at all
      --output-last-message <path>      Write ONLY the last assistant message
                                        there, raw. The destination obeys the
                                        sandbox policy, which no flag widens
      --ephemeral                       Persist no session file. Headless only,
                                        and incompatible with --resume
      --resume [latest|<file.jsonl>]    Resume a session
      --model <slug>                    Model to use
      --allow <host>                    Allow a network host
  -y, --yes                             Accept edits in headless mode
      --no-sandbox                      Disable the filesystem sandbox
      --sandbox <mode>                  workspace-write | read-only | full-access
      --permission-mode <mode>          ask | accept-edits | auto | full-access |
                                        read-only
      --profile <name>                  Apply the [profiles.<name>] table
  -c, --config <key=value>              Override one configuration key for this
                                        run. Repeatable; the last occurrence of a
                                        key wins, and a security key is refused
      --token-budget <n>                Total token budget
      --cost-budget-micro-usd <n>       Total cost budget
      --input-cost-micro-per-ktok <n>   Input price
      --output-cost-micro-per-ktok <n>  Output price
      --overload-fallback-model <slug>  Fallback model on overload
      --output-format <text|json>       Headless output: final text (default) or
                                        one JSON event per line (docs/EVENT_SCHEMA.md)
  -h, --help                            Show this help

Configuration (TOML), by increasing precedence. `/status` names the layer every
non-default value comes from.
  global settings (10)   ~/.pyxis/settings.toml. User-owned, may set every key.
  profile (15)           The [profiles.<name>] table selected by --profile, or by
                         the `profile` key of the global file.
  project config (20)    <workspace>/.pyxis/config.toml. `permission_mode`,
                         `sandbox_mode`, `writable_roots`, `hooks` and `profile`
                         are ignored there with a warning: a workspace-controlled
                         file must never widen a security perimeter.
  environment (25)       PYXIS_* variables.
  command line (30)      -c key=value and the flags above. `-c` refuses a security
                         key: an argument can come from a script of the repository,
                         so it is no more trustworthy than a workspace file.

Sandbox:
  `--sandbox`, or `sandbox_mode` of ~/.pyxis/settings.toml, selects the policy:
    workspace-write (default)  Writes confined to the workspace, plus the
                               temporary directory and the extra roots listed
                               in `writable_roots`.
    read-only                  Nothing is writable; the network is closed and
                               any --allow list is ignored.
    full-access                No confinement. `--no-sandbox` is its alias.
  Under a confined policy, .git/ (hooks, config, and the worktree pointer file)
  and .pyxis/ — which holds the project config file — stay read-only, because
  their contents run or are read later outside the sandbox. That subtraction is
  evaluated BEFORE execution and covers `bash` too: Landlock rules are additive,
  so the kernel cannot take back a right it granted on the workspace, and the
  decision is therefore made at policy level. A command that is not proven
  side-effect free and NAMES one of those paths is refused, whether or not the
  write can be demonstrated. A path built at run time stays out of reach of any
  static analysis; see docs/CURRENT_STATUS.md.
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
        print: false,
        ephemeral: false,
        output_last_message: None,
        resume: None,
        model: agent_provider::DEFAULT_MODEL.to_string(),
        model_from_cli: false,
        allow_hosts: Vec::new(),
        yes: false,
        sandbox: true,
        profile: None,
        overrides: Vec::new(),
        permission_mode: None,
        sandbox_mode: None,
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
            // US-020 AC1: the value became optional. Without it the prompt is read
            // from standard input, which is what makes `-p` usable in a pipe.
            "-p" | "--print" => {
                args.print = true;
                if let Some(next) = it.peek()
                    && !next.starts_with('-')
                {
                    args.prompt = it.next();
                }
            }
            "--ephemeral" => args.ephemeral = true,
            "--output-last-message" => {
                args.output_last_message = Some(next_value(&mut it, "--output-last-message")?)
            }
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
            // US-008 AC3: an unknown value names what was received AND what is
            // accepted, and refuses to start rather than falling back on a
            // perimeter the user did not choose.
            "--sandbox" => {
                let raw = next_value(&mut it, "--sandbox")?;
                if !SandboxPolicy::IDS.contains(&raw.trim()) {
                    anyhow::bail!(
                        "--sandbox: unknown value `{raw}` (expected one of: {})",
                        SandboxPolicy::IDS.join(", ")
                    );
                }
                args.sandbox_mode = Some(raw.trim().to_string());
            }
            "--permission-mode" => {
                let raw = next_value(&mut it, "--permission-mode")?;
                let mode = settings::permission_mode_from_arg(&raw).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--permission-mode: unknown value `{raw}` (expected one of: {})",
                        settings::PERMISSION_MODE_IDS.join(", ")
                    )
                })?;
                args.permission_mode = Some(mode);
            }
            "--profile" => {
                let raw = next_value(&mut it, "--profile")?;
                if raw.trim().is_empty() {
                    anyhow::bail!("--profile: missing value");
                }
                args.profile = Some(raw.trim().to_string());
            }
            // US-007: the generic override. Split on the FIRST `=` only, so a
            // value may itself contain one.
            "-c" | "--config" => {
                let raw = next_value(&mut it, "-c")?;
                let (key, value) = raw
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("-c: expected key=value, got `{raw}`"))?;
                if key.trim().is_empty() {
                    anyhow::bail!("-c: expected key=value, got `{raw}`");
                }
                args.overrides
                    .push((key.trim().to_string(), value.to_string()));
            }
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
    if !args.help {
        validate_ephemeral(&args)?;
    }
    Ok(args)
}

/// US-020 AC6: `--ephemeral` contradicts a resume, and outside the headless mode
/// it would promise something the interactive commands cannot hold — `/new`,
/// `/fork` and `/resume` all move a file that would not exist. Refused rather
/// than silently degraded, and both flags are named.
fn validate_ephemeral(args: &Args) -> anyhow::Result<()> {
    if !args.ephemeral {
        return Ok(());
    }
    if args.resume.is_some() {
        anyhow::bail!("--ephemeral and --resume are incompatible");
    }
    if !args.print && args.prompt.is_none() {
        anyhow::bail!("--ephemeral requires the headless mode (-p / --print)");
    }
    Ok(())
}

/// Resolves the headless prompt (US-020 AC1/AC2/AC5). The ARGUMENT wins over
/// standard input, which is then not read at all: a `-p` that already carries
/// its prompt must not drain a pipe the caller left open for something else.
/// `read_stdin` is injected so the precedence stays testable without a process.
fn resolve_prompt(
    print: bool,
    prompt: Option<String>,
    read_stdin: impl FnOnce() -> anyhow::Result<Option<String>>,
) -> anyhow::Result<Option<String>> {
    if prompt.is_some() || !print {
        return Ok(prompt);
    }
    read_stdin()?
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(Some)
        // AC5: an empty standard input is an error carrying its name, never a
        // wait for something that will not come.
        .ok_or_else(|| anyhow::anyhow!("no prompt provided (argument or standard input)"))
}

/// Standard input as a prompt. `None` when it is a terminal: reading would block
/// on a human who was never asked to type, which is exactly the indefinite wait
/// AC5 refuses.
fn stdin_prompt() -> anyhow::Result<Option<String>> {
    use std::io::{IsTerminal, Read};

    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    stdin.read_to_string(&mut buf)?;
    Ok(Some(buf))
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

/// Precedence of a numeric setting above the file layers (US-005): configuration
/// < environment variable < argument. Returns the value AND the layer that
/// carries it when one of the two upper levels won, so `/status` can name it;
/// `None` means the files (or the default) still own the key. The environment is
/// passed as a parameter rather than read here: precedence becomes testable
/// without mutating the process environment.
fn precedence_u64(
    arg: Option<&str>,
    arg_name: &str,
    env: Option<&str>,
    env_name: &str,
    from_config: Option<u64>,
) -> anyhow::Result<(Option<u64>, Option<settings::ConfigLayer>)> {
    if let Some(raw) = arg {
        return parse_positive_u64(raw, arg_name)
            .map(|value| (Some(value), Some(settings::ConfigLayer::SessionFlags)));
    }
    match env {
        Some(raw) if !raw.trim().is_empty() => parse_positive_u64(raw, env_name)
            .map(|value| (Some(value), Some(settings::ConfigLayer::Environment))),
        _ => Ok((from_config, None)),
    }
}

/// Same chain for a string setting. An empty value is not a definition: it lets
/// the level below through instead of overwriting it with nothing.
fn precedence_string(
    arg: Option<&str>,
    env: Option<&str>,
    from_config: Option<String>,
) -> (Option<String>, Option<settings::ConfigLayer>) {
    let defined = |raw: &str| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    if let Some(value) = arg.and_then(defined) {
        return (Some(value), Some(settings::ConfigLayer::SessionFlags));
    }
    if let Some(value) = env.and_then(defined) {
        return (Some(value), Some(settings::ConfigLayer::Environment));
    }
    (from_config, None)
}

/// Resolves one numeric setting and records which layer won (US-005 AC2).
fn setting_u64(
    arg: &Option<String>,
    env: &str,
    name: &str,
    from_config: Option<u64>,
    key: &'static str,
    sources: &mut settings::Provenance,
) -> anyhow::Result<Option<u64>> {
    let (value, layer) = precedence_u64(
        arg.as_deref(),
        name,
        std::env::var(env).ok().as_deref(),
        env,
        from_config,
    )?;
    if let Some(layer) = layer {
        sources.claim(key, layer);
    }
    Ok(value)
}

fn run_config_from_args(args: &Args, config: &mut settings::Config) -> anyhow::Result<RunConfig> {
    let token_budget = setting_u64(
        &args.token_budget,
        "PYXIS_TOKEN_BUDGET",
        "--token-budget",
        config.token_budget,
        settings::TOKEN_BUDGET_KEY,
        &mut config.sources,
    )?;
    let cost_limit = setting_u64(
        &args.cost_budget_micro_usd,
        "PYXIS_COST_BUDGET_MICRO_USD",
        "--cost-budget-micro-usd",
        config.cost_budget_micro_usd,
        settings::COST_BUDGET_KEY,
        &mut config.sources,
    )?;
    let input_price = setting_u64(
        &args.input_cost_micro_per_ktok,
        "PYXIS_INPUT_COST_MICRO_PER_KTOK",
        "--input-cost-micro-per-ktok",
        config.input_cost_micro_per_ktok,
        settings::INPUT_COST_KEY,
        &mut config.sources,
    )?;
    let output_price = setting_u64(
        &args.output_cost_micro_per_ktok,
        "PYXIS_OUTPUT_COST_MICRO_PER_KTOK",
        "--output-cost-micro-per-ktok",
        config.output_cost_micro_per_ktok,
        settings::OUTPUT_COST_KEY,
        &mut config.sources,
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
    let (overload_fallback_model, fallback_layer) = precedence_string(
        args.overload_fallback_model.as_deref(),
        std::env::var("PYXIS_OVERLOAD_FALLBACK_MODEL")
            .ok()
            .as_deref(),
        config.overload_fallback_model.clone(),
    );
    if let Some(layer) = fallback_layer {
        config.sources.claim(settings::OVERLOAD_FALLBACK_KEY, layer);
    }
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
///
/// US-001 AC4/AC5: the line always names the policy REALLY in force. A confined
/// variant the kernel did not apply is shown as what it is, never under the
/// name that was asked for.
fn sandbox_scope_label(policy: &SandboxPolicy, enforced: bool) -> String {
    if !policy.confines_writes() {
        return "off (writes not restricted)".to_string();
    }
    if !enforced {
        return format!(
            "{} requested, NOT applied (writes not restricted)",
            policy.id()
        );
    }
    match policy {
        SandboxPolicy::DangerFullAccess => "off (writes not restricted)".to_string(),
        SandboxPolicy::ReadOnly { .. } => "enforced (read-only)".to_string(),
        SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
            // The workspace is the first root: the rest is what was added to it.
            match writable_roots.len().saturating_sub(1) {
                0 => "enforced (workspace)".to_string(),
                1 => "enforced (workspace + 1 extra root)".to_string(),
                n => format!("enforced (workspace + {n} extra roots)"),
            }
        }
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

/// What the confinement resolved to at startup: the policy asked for, and
/// whether the kernel really carries it. Threaded into the runtime because
/// enforcement happens before it exists and cannot be observed afterwards.
struct SandboxSetup {
    policy: SandboxPolicy,
    enforced: bool,
}

/// Reads the network confinement on behalf of the tools (US-004). The glue is
/// here, in the only crate that knows both `agent-tools` and `agent-sandbox`.
struct ProxyObserver {
    proxy: agent_sandbox::ProxyHandle,
}

impl agent_tools::SandboxObserver for ProxyObserver {
    fn mark(&self) -> usize {
        self.proxy.mark()
    }
    fn blocked_since(&self, mark: usize) -> Vec<String> {
        self.proxy.blocked_since(mark)
    }
    fn allowed(&self) -> String {
        agent_sandbox::describe_allowed(&self.proxy.allowed)
    }
}

/// Applies an approved one-call widening (US-004).
///
/// The network is the ONLY perimeter that widens in place: it lives in the
/// proxy, which is ours. The filesystem perimeter is a Landlock domain, which
/// is inherited by every child and irreversible for the process, so no
/// escalation can lift it and none is offered. Returning `None` there is what
/// keeps the refusal in place instead of promising a retry that would fail
/// identically.
struct ProxyEscalator {
    grants: agent_sandbox::NetworkGrants,
}

impl agent_tools::SandboxEscalator for ProxyEscalator {
    fn can_lift(&self, denial: &agent_tools::SandboxDenial) -> bool {
        denial.is_escalatable()
    }
    fn lift(&self, denial: &agent_tools::SandboxDenial) -> Option<agent_tools::EscalationGuard> {
        match denial {
            agent_tools::SandboxDenial::Network { host, .. } => {
                Some(Box::new(self.grants.grant(host)))
            }
            agent_tools::SandboxDenial::Filesystem => None,
        }
    }
}

/// Resolves the policy in force (US-001). `--no-sandbox` is a documented ALIAS
/// of full access, with no third semantics of its own (US-008 AC5); the named
/// variants come from `sandbox_mode`, a security key no workspace file can set.
fn sandbox_policy_from_args(
    args: &Args,
    workspace: &std::path::Path,
    config: &settings::Config,
    writable_roots: &agent_sandbox::WritableRoots,
) -> SandboxPolicy {
    if !args.sandbox {
        return SandboxPolicy::DangerFullAccess;
    }
    let subpaths = agent_tools::path::PROTECTED_SUBPATHS;
    let default = || {
        SandboxPolicy::workspace_write(
            workspace,
            writable_roots.granted.clone(),
            subpaths.iter().map(std::path::PathBuf::from),
        )
    };
    let Some(id) = config.sandbox_mode.as_deref() else {
        return default();
    };
    // `settings.rs` already refused an unknown identifier: reaching the fallback
    // would mean the two lists diverged, which must degrade to the safe side
    // rather than to an unconfined session.
    SandboxPolicy::from_id(id, workspace, writable_roots.granted.clone(), subpaths).unwrap_or_else(
        || {
            eprintln!("[sandbox] unknown policy `{id}`, falling back on the default");
            default()
        },
    )
}

fn enforce_sandbox(
    args: &Args,
    policy: &SandboxPolicy,
    settings_path: Option<&std::path::Path>,
    diagnostics: &observability::Diagnostics,
    writable_roots: &agent_sandbox::WritableRoots,
    agent_state_dirs: &[&std::path::Path],
) -> bool {
    if !policy.confines_writes() {
        let source = if args.sandbox {
            "by the `full-access` policy"
        } else {
            "by --no-sandbox"
        };
        if args.yes {
            eprintln!(
                "[sandbox] disabled {source}: --yes may accept edits without filesystem confinement"
            );
        } else {
            eprintln!("[sandbox] disabled {source}");
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
    // Settings + diagnostic files (US-020): the only paths outside the workspace
    // that stay writable, each granted individually.
    let mut writable: Vec<&std::path::Path> = settings_path.into_iter().collect();
    writable.extend(diagnostics.writable_files());
    match agent_sandbox::enforce_policy(policy, &writable, agent_state_dirs) {
        Ok(status) => {
            // US-001 AC5: a degradation names the requested variant AND the one
            // really applied, never silently keeps the first.
            if let Some(message) = status.degradation(policy) {
                eprintln!("[sandbox] {message}");
            }
            status.confines()
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
    let mut args = parse_args()?;
    if args.help {
        print!("{HELP}");
        return Ok(());
    }
    // US-020: standard input resolved FIRST, because `args.prompt.is_some()` is
    // what tells the rest of the startup it is running headless.
    args.prompt = resolve_prompt(args.print, args.prompt.take(), stdin_prompt)?;

    // US-020: diagnostics prepared FIRST. The panic net must cover the whole
    // startup, and its file must exist before Landlock, which cannot grant a write
    // right on a path that does not open yet.
    let diagnostics = observability::prepare(settings::pyxis_home().as_deref());
    for warning in &diagnostics.warnings {
        eprintln!("[trace] {warning}");
    }
    observability::install_panic_hook(diagnostics.panic_log.clone());
    if let Some(path) = observability::install_tracing(&diagnostics) {
        eprintln!("[trace] {}", path.display());
    }

    let workspace = std::env::current_dir()?;

    // Configuration resolved BEFORE the sandbox and in BOTH modes (US-016 AC6):
    // the global file is outside the workspace, hence inaccessible once Landlock
    // is in place, and the headless mode needs its settings as much as the
    // interactive one. Reading is not persisting: `-p` never rewrites the file
    // (see below). Resolved FIRST of everything that can fail, so an unknown
    // profile (US-006 AC4) or an unusable `-c` value (US-007 AC4) stops the
    // startup before an OAuth browser or an MCP subprocess is launched.
    let config = settings::resolve(settings::Request {
        global: settings::default_settings_path().as_deref(),
        project: Some(&settings::project_config_path(&workspace)),
        profile: args.profile.as_deref(),
        overrides: &args.overrides,
        flags: settings::Flags {
            model: args.model_from_cli.then_some(args.model.as_str()),
            permission_mode: args.permission_mode,
            // US-008 AC5: `--no-sandbox` is a documented ALIAS of full access.
            // Expressed as the same key rather than as a separate path, so it has
            // no second semantics and `/status` names its provenance like any
            // other value.
            sandbox_mode: args
                .sandbox_mode
                .as_deref()
                .or((!args.sandbox).then_some("full-access")),
        },
    })
    .map_err(|err| anyhow::anyhow!("config: {err}"))?;
    for warning in &config.warnings {
        eprintln!("[config] {warning}");
    }

    // Skills scanned BEFORE the sandbox, like the rest of the out-of-workspace
    // configuration (US-014). Only `name` and `description` are preloaded, as the
    // spec requires; a body is read at invocation time, which the read-only-global
    // Landlock policy still allows.
    // US-018: two scopes, the user root and the project root. A project skill
    // masks a user skill of the same name, and its content stays what it already
    // was: injected text framed as untrusted, never a declared capability.
    let skills = skills::load_all(
        skills_root().as_deref(),
        Some(&project_skills_root(&workspace)),
    );
    for issue in &skills.issues {
        eprintln!("[skills] {issue}");
    }

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
    // US-015: the skill catalog travels with the project context, framed the same
    // way, and `project_messages` is the single place that decides that order
    // (shared with the `/init` refresh, US-019).
    let context_msgs = context::project_messages(&workspace, &context::today_utc(), &skills);

    let credential = prepare_credential_before_sandbox(&args)?;

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

    // Session directory created BEFORE the sandbox (US-001): under `read-only`
    // nothing in the workspace is writable afterwards, and a Landlock rule needs
    // a path that already opens. A session Pyxis cannot record is not a session,
    // so this directory is granted whatever the policy.
    // US-020 AC4: `--ephemeral` neither creates it nor asks for a write right on
    // it. Creating the directory "just in case" would already be a trace left in
    // the workspace, which is the whole thing the flag promises not to do.
    let sessions_dir = workspace.join(".pyxis").join("sessions");
    if !args.ephemeral
        && let Err(err) = std::fs::create_dir_all(&sessions_dir)
    {
        eprintln!("[session] {}: {err}", sessions_dir.display());
    }
    let session_dirs: &[&std::path::Path] = if args.ephemeral {
        &[]
    } else {
        &[sessions_dir.as_path()]
    };

    // FS sandbox BEFORE the runtime (main thread -> inherited by the workers).
    let policy = sandbox_policy_from_args(&args, &workspace, &config, &writable_roots);
    let enforced = enforce_sandbox(
        &args,
        &policy,
        settings_path.as_deref(),
        &diagnostics,
        &writable_roots,
        session_dirs,
    );

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
        SandboxSetup { policy, enforced },
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

/// What the startup connection produced: the tools to register, the notices to
/// show, and the model-facing names exposed per server. The last one is what lets
/// a session-time disconnect take exactly its own tools back out (US-016).
#[derive(Default)]
struct McpStartup {
    tools: Vec<Box<dyn DynTool>>,
    notices: Vec<String>,
    names: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
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
) -> McpStartup {
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
            return McpStartup {
                notices: vec!["MCP: registry unavailable at startup.".to_string()],
                ..McpStartup::default()
            };
        }
    }
    if candidates.is_empty() {
        return McpStartup {
            notices,
            ..McpStartup::default()
        };
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
        // The policy travels with the task: the registry lock is not held while
        // the connection is in flight.
        let policy = cfg.tools.clone();
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
            // subprocess (or the HTTP session), so a slow server leaves nothing behind.
            match tokio::time::timeout(MCP_STARTUP_TIMEOUT, attempt).await {
                Ok(Ok(connected)) => (name, policy, Ok(connected)),
                Ok(Err(err)) => (name, policy, Err(err.to_string())),
                Err(_) => (
                    name,
                    policy,
                    Err(format!(
                        "connection timeout after {}s",
                        MCP_STARTUP_TIMEOUT.as_secs()
                    )),
                ),
            }
        });
    }

    let mut startup = McpStartup::default();
    // Shared across every server: uniqueness of the exposed names is a property of
    // the whole set (US-011 AC1).
    let mut taken = std::collections::BTreeSet::new();
    while let Some(joined) = pending.join_next().await {
        let (name, policy, outcome) = match joined {
            Ok(triple) => triple,
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
        // US-014: the policy shapes the exposed surface BEFORE anything reaches the
        // registry, and the filtered list is what `/mcp` shows.
        let (listed, filter_notices) = agent_mcp::filter_tools(&name, &listed, &policy);
        notices.extend(filter_notices);
        let (mut exposed, skipped) =
            agent_mcp::dyn_tools(&name, &listed, &policy, &client, &mut taken);
        for skip in skipped {
            notices.push(skip.summary());
        }
        // The tools are only registered once the connection is actually held by the
        // registry: a connection closed on the way would otherwise leave the model
        // with tools that can no longer reach anything.
        let held = match mcp.lock() {
            Ok(mut reg) => match reg.finish_connect(&name, conn, listed) {
                Some(orphan) => {
                    tokio::spawn(async move { orphan.cancel().await });
                    false
                }
                None => true,
            },
            Err(_) => {
                tokio::spawn(async move { conn.cancel().await });
                notices.push(format!(
                    "MCP \"{name}\": registry unavailable, connection closed."
                ));
                false
            }
        };
        if held {
            startup.names.insert(
                name.clone(),
                exposed.iter().map(|tool| tool.name().to_string()).collect(),
            );
            startup.tools.append(&mut exposed);
        }
    }
    startup.notices = notices;
    startup
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

/// Root of the installed skills, shared with the other CLIs through
/// `~/.agents/skills`. `None` when the home cannot be resolved: falling back on a
/// relative path would make `.agents/skills` of the current workspace loadable,
/// and a skill root is a security key that a repository never gets to declare
/// (FR-13).
fn skills_root() -> Option<std::path::PathBuf> {
    Some(home_dir()?.join(".agents").join("skills"))
}

/// Root of the project skills (US-018). The path is FIXED here: the repository
/// supplies skill content, never the location it is read from, which is what keeps
/// the root itself out of a workspace file's reach (FR-13, FR-18).
fn project_skills_root(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join(".agents").join("skills")
}

/// Everything that touches a path outside the workspace, hence loaded or created BEFORE
/// the Landlock enforcement: past it, only the workspace (and the settings file)
/// stays writable.
struct PreSandbox {
    skills: skills::Catalog,
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
    sandbox: SandboxSetup,
) -> anyhow::Result<()> {
    let PreSandbox {
        skills,
        mcp_config,
        context_msgs,
        cred,
        settings_path,
        mut config,
    } = pre;
    // The resolved model already accounts for `--model`, which enters the
    // configuration as the strongest layer (US-005): there is nothing left to
    // arbitrate here. Resolved BEFORE everything else, because the fallback
    // validation and the initial effort are computed on the model actually used.
    if let Some(model) = config.model.clone() {
        args.model = model;
    }
    let run_config = run_config_from_args(&args, &mut config)?;
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

    // Notices addressed to the HUMAN (hooks, blocked hosts): TUI history in
    // interactive mode, stderr in headless mode where the diagnostics of this
    // mode already go (stdout stays byte-for-byte identical).
    let (notice_tx, hook_notice_rx) = tokio::sync::mpsc::channel::<String>(16);

    // 2. Network allow-list proxy (fail-closed). Hardens the Bash commands.
    // US-003 AC1: whether the network is reachable at all is a property of the
    // sandbox policy; the allow-list only narrows it further.
    let (proxy_policy, network_conflict) =
        ProxyPolicy::from_sandbox(&sandbox.policy, args.allow_hosts.clone());
    if let Some(conflict) = network_conflict {
        // AC5: the conflict resolution is announced, never silent.
        eprintln!("[sandbox] {conflict}");
    }
    // US-003 AC6: a refusal is restituted to the user, not merely logged.
    let proxy_notice: agent_sandbox::ProxyNotice = if headless {
        Arc::new(|message: String| eprintln!("[sandbox] {message}"))
    } else {
        let tx = notice_tx.clone();
        Arc::new(move |message: String| {
            let _ = tx.try_send(format!("sandbox: {message}"));
        })
    };
    let proxy = agent_sandbox::spawn_proxy(proxy_policy, Some(proxy_notice)).await?;
    let proxy_addr = proxy.addr.clone();
    let harden: agent_tools::CommandHardener =
        Arc::new(move |cmd: &mut tokio::process::Command| set_proxy_env(cmd, &proxy_addr));
    let mcp_harden = Arc::clone(&harden);

    // 3. Persistent session: one JSONL file per conversation (timestamped) under
    // <workspace>/.pyxis/sessions/, listable/resumable through `/resume`.
    let sessions_dir = workspace.join(".pyxis").join("sessions");
    if !args.ephemeral {
        std::fs::create_dir_all(&sessions_dir)?;
    }
    let (current_session, initial_messages) = if let Some(resume_arg) = &args.resume {
        let path = resolve_resume_path(&sessions_dir, resume_arg)?;
        let resumed =
            agent_session::resume_file(&path).map_err(|e| anyhow::anyhow!("resume: {e}"))?;
        (path, resumed.messages)
    } else {
        (interactive::new_session_path(&sessions_dir), Vec::new())
    };
    provider.set_prompt_cache_key(&interactive::prompt_cache_key_for_session(&current_session));
    // US-020 AC4: under `--ephemeral` the file is never opened, so nothing can be
    // written to it. The identifier keeps naming the run in the summary line.
    let (shared_session, conversation) = if args.ephemeral {
        SharedSession::ephemeral()
    } else {
        let jsonl = agent_session::JsonlSession::create_at(&current_session)
            .map_err(|e| anyhow::anyhow!("session: {e}"))?;
        SharedSession::new(jsonl)
    };
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
    let policy = permission_policy(headless, args.yes, sandbox.enforced);
    let (initial_permission_mode, announce_override) =
        resolve_permission_mode(config.permission_mode, policy, headless);
    if announce_override {
        // US-008 AC6: the widening is announced, and it names the layer that
        // carries it: a flag and a global file do not call for the same reaction.
        let source = config
            .sources
            .layer(settings::PERMISSION_MODE_KEY)
            .map(settings::ConfigLayer::label)
            .unwrap_or("configuration");
        eprintln!(
            "[config] permission mode from {source}: {} (default for -p would be {})",
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
    let mcp_startup = if headless {
        McpStartup::default()
    } else {
        connect_mcp_at_startup(&mcp, &mcp_harden).await
    };

    // US-017: hooks come from the GLOBAL configuration alone (`settings.rs` drops
    // the key from a workspace file). Without a declaration the registry keeps
    // `NoHooks`: no process, no clone, no added latency.
    let session_id = current_session
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let hooks: Arc<dyn agent_tools::hooks::Hooks> = if config.hooks.is_empty() {
        Arc::new(agent_tools::hooks::NoHooks)
    } else {
        // A later hook failing is reported to the human, never to the model.
        let notice: agent_tools::hooks::HookNotice = if headless {
            Arc::new(|message: String| eprintln!("[hook] {message}"))
        } else {
            let tx = notice_tx.clone();
            Arc::new(move |message: String| {
                // Once the TUI is gone (a `SessionEnd` hook runs after it) the
                // channel is closed: the human still gets the message, on stderr.
                if let Err(err) = tx.try_send(message) {
                    eprintln!("[hook] {}", err.into_inner());
                }
            })
        };
        Arc::new(
            agent_tools::hooks::CommandHooks::new(config.hooks.clone(), &workspace)
                .with_hardener(Arc::clone(&mcp_harden))
                .with_notice(notice)
                .with_session_id(session_id.clone()),
        )
    };

    // US-017 AC2: the session lifecycle gates on its own hooks. `SessionStart`
    // fires before the first turn and before the TUI takes the terminal, so a
    // refusal is readable; it stops the session instead of starting one the user
    // declared they did not want.
    if let agent_tools::hooks::HookDecision::Deny(reason) = hooks
        .lifecycle(agent_tools::hooks::Lifecycle::SessionStart {
            source: if args.resume.is_some() {
                "resume"
            } else {
                "startup"
            },
        })
        .await
    {
        anyhow::bail!("hook: {reason}");
    }

    // US-012: the persistent shell sessions of this run. Kept here so the end of
    // the run can close them, which is what guarantees no orphan child survives
    // Pyxis (AC4).
    let exec_sessions = ExecSessions::new();

    let mut builder = Registry::builder(&workspace)
        .mode_state(permission_mode.clone())
        .approver(approver)
        .approvals(approvals.clone())
        .initial_taint_recent(initial_taint_recent)
        .hooks(Arc::clone(&hooks))
        .command_hardener(harden)
        // US-011: whether the active model reads images at all. A provider that
        // does not declare vision makes `view_image` refuse before any read.
        .vision(provider.capabilities().vision)
        .exec_sessions(exec_sessions.clone())
        // US-001/US-002: the perimeter the tools evaluate BEFORE execution, and
        // whether the kernel really carries it (US-004 classification).
        .sandbox(sandbox.policy.clone(), sandbox.enforced)
        .sandbox_observer(Arc::new(ProxyObserver {
            proxy: proxy.clone(),
        }))
        .sandbox_escalator(Arc::new(ProxyEscalator {
            grants: proxy.grants.clone(),
        }))
        .register(Read)
        .register(Glob)
        .register(Grep)
        .register(Write)
        .register(Edit)
        .register(Bash)
        // EP-003: the four ported families.
        .register(UpdatePlan)
        .register(ApplyPatch)
        .register(ViewImage)
        .register(ExecCommand)
        .register(WriteStdin);
    for tool in mcp_startup.tools {
        builder = builder.register_dyn(tool);
    }
    // US-016: shared between the loop (which dispatches through `ToolDispatch`) and
    // the frontend (which stages the tools of a server connected in session and
    // recomposes the specs at each turn boundary).
    let registry = Arc::new(builder.build());
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
        tools: Arc::clone(&registry) as Arc<dyn agent_core::tools::ToolDispatch>,
        // US-001: base token never signalled. The interactive loop substitutes a
        // PER-TURN token (`launch_turn`); the headless mode keeps this one.
        cancel: CancelToken::new(),
    };

    // 6. Headless (-p) vs interactive dispatch. Wrapped so that `SessionEnd`
    // fires on every way out, including the failures each branch bails on.
    let output_last_message = args.output_last_message.take();
    let outcome: anyhow::Result<()> = async {
        if let Some(prompt) = args.prompt {
            // Headless one-shot: fixed slug (`args.model`) -> template selected once.
            let base = interactive::with_tool_guidelines(
                prompt::select_system_prompt(&args.model),
                &tool_guidelines,
            );
            // US-017 AC2: the prompt is submitted to the hooks before it becomes a
            // turn. A refusal names the reason and no turn starts.
            if let agent_tools::hooks::HookDecision::Deny(reason) = hooks
                .lifecycle(agent_tools::hooks::Lifecycle::UserPromptSubmit { prompt: &prompt })
                .await
            {
                anyhow::bail!("hook: {reason}");
            }
            // US-016: a prompt opening on `/<skill>` injects that skill's instructions
            // for this turn. An unreadable skill fails the run instead of sending a
            // turn that would only carry its name.
            let skill_messages = match skills::invocation(&skills, &prompt) {
                Some(Ok(injection)) => vec![Message::user(injection.block)],
                Some(Err(err)) => anyhow::bail!("skill: {err}"),
                None => Vec::new(),
            };
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
                ephemeral_messages: skill_messages,
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
            // US-012 AC4: a one-shot run leaves no shell session behind, whatever
            // the outcome below (several paths bail from here).
            exec_sessions.shutdown();

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
            events.run_summary(&session_id, &result.ended);

            // US-017: end of turn. Nothing follows it here, so the hook observes and
            // a failure of its own is reported, never fatal.
            if matches!(result.ended, agent_core::HeadlessEnd::EndTurn) {
                hooks.lifecycle(agent_tools::hooks::Lifecycle::Stop).await;
            }

            // US-020 AC3: the last assistant message alone, raw, at the requested
            // destination. Written whatever the outcome, because the exit code is
            // what tells a pipeline whether the run succeeded; the failure of the
            // WRITE is reported after the outcome, which owns the exit code.
            let last_message_write = output_last_message.map(|path| {
                let text = result
                    .text
                    .replace(interactive::GOAL_DONE_MARKER, "")
                    .trim_end()
                    .to_string();
                // A destination outside the writable perimeter is refused by the
                // sandbox, not widened for it: an argument may come from a script
                // of the repository (same reasoning as `-c`).
                std::fs::write(&path, text)
                    .map_err(|e| anyhow::anyhow!("--output-last-message {path}: {e}"))
            });

            match result.ended {
                agent_core::HeadlessEnd::Error(e) => anyhow::bail!("{e}"),
                agent_core::HeadlessEnd::Exhausted(reason) => anyhow::bail!("stopped: {reason:?}"),
                agent_core::HeadlessEnd::EndTurn => {}
            }
            if let Some(written) = last_message_write {
                written?;
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
                truecolor: agent_tui::supports_truecolor(),
                // Reduced motion: spinner degraded to a pulsing dot (US-044).
                reduced_motion: std::env::var_os("NO_COLOR").is_some()
                    || std::env::var_os("PYXIS_REDUCED_MOTION").is_some(),
                // credential loaded above (otherwise we bail) -> connected.
                connected: true,
                skills,
                goal,
                hooks: Arc::clone(&hooks),
                hook_specs: config.hooks.clone(),
                command_hardener: Arc::clone(&mcp_harden),
                mcp_notices: mcp_startup.notices,
                mcp_tool_names: mcp_startup.names,
                registry: Arc::clone(&registry),
                permission_mode,
                approvals,
                settings_path,
                workspace: workspace.clone(),
                sandbox_scope: sandbox_scope_label(&sandbox.policy, sandbox.enforced),
                // US-005 AC2: computed here, where the layers were resolved. The loop
                // only filters out the values it changed in session.
                config_sources: settings::status_sources(&config),
                profile: config.profile.clone(),
            };
            let outcome = interactive::run(
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
            .await;
            // US-012 AC4: the session ending closes every shell session and kills
            // its process tree, on the error path too.
            exec_sessions.shutdown();
            outcome?;
        }
        Ok(())
    }
    .await;

    // US-017: the session is over whatever happened; the hook observes it and
    // cannot refuse an end that already took place.
    if hooks.watches(agent_tools::hooks::HookEvent::SessionEnd) {
        hooks
            .lifecycle(agent_tools::hooks::Lifecycle::SessionEnd {
                reason: if outcome.is_ok() { "exit" } else { "error" },
            })
            .await;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::{
        Args, SandboxPolicy, jsonl, parse_args_from, permission_policy, precedence_string,
        precedence_u64, resolve_permission_mode, resolve_prompt, resolve_resume_path,
        run_config_from_args, sandbox_policy_from_args, sandbox_scope_label, settings,
        validate_ephemeral,
    };
    use agent_tools::permission::PermissionMode;

    fn argv(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_string()).collect()
    }

    fn args() -> Args {
        Args {
            model: "mock".into(),
            model_from_cli: true,
            prompt: None,
            print: false,
            ephemeral: false,
            output_last_message: None,
            resume: None,
            allow_hosts: Vec::new(),
            yes: false,
            sandbox: true,
            profile: None,
            overrides: Vec::new(),
            permission_mode: None,
            sandbox_mode: None,
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

    /// US-016 AC1 / US-005: the levels `main` layers above the files, and the
    /// layer each winning value is attributed to.
    #[test]
    fn precedence_runs_config_then_env_then_argument() {
        let name = "--token-budget";
        let env = "PYXIS_TOKEN_BUDGET";
        let env_layer = settings::ConfigLayer::Environment;
        let flag_layer = settings::ConfigLayer::SessionFlags;

        assert_eq!(
            precedence_u64(None, name, None, env, None).unwrap(),
            (None, None)
        );
        // Nothing above the files won: the key keeps the layer they gave it.
        assert_eq!(
            precedence_u64(None, name, None, env, Some(10)).unwrap(),
            (Some(10), None)
        );
        assert_eq!(
            precedence_u64(None, name, Some("20"), env, Some(10)).unwrap(),
            (Some(20), Some(env_layer))
        );
        assert_eq!(
            precedence_u64(Some("30"), name, Some("20"), env, Some(10)).unwrap(),
            (Some(30), Some(flag_layer))
        );
        // An empty variable is not a definition: it lets the configuration
        // through instead of overwriting with a zero.
        assert_eq!(
            precedence_u64(None, name, Some("  "), env, Some(10)).unwrap(),
            (Some(10), None)
        );

        // Same chain for a string setting.
        assert_eq!(
            precedence_string(Some(" flag "), Some("env"), Some("file".into())),
            (Some("flag".to_string()), Some(flag_layer))
        );
        assert_eq!(
            precedence_string(None, Some(" env "), Some("file".into())),
            (Some("env".to_string()), Some(env_layer))
        );
        assert_eq!(
            precedence_string(Some(" "), Some(""), Some("file".into())),
            (Some("file".to_string()), None)
        );
    }

    /// AC1 on the `RunConfig` side: without an argument nor an environment variable, the
    /// configuration really drives the budget passed to the core.
    #[test]
    fn run_config_falls_back_to_the_configuration_file() {
        let mut config = settings::Config {
            token_budget: Some(4242),
            overload_fallback_model: Some("gpt-5.5".into()),
            ..settings::Config::default()
        };
        let mut args = args();
        args.model = "gpt-5.5".into();

        let cfg = run_config_from_args(&args, &mut config).unwrap();

        assert_eq!(cfg.token_budget, Some(4242));
        assert_eq!(cfg.overload_fallback_model.as_deref(), Some("gpt-5.5"));
    }

    /// The argument wins over the configuration, in the expected direction.
    #[test]
    fn cli_argument_overrides_the_configuration_file() {
        let mut config = settings::Config {
            token_budget: Some(4242),
            ..settings::Config::default()
        };
        let mut args = args();
        args.token_budget = Some("7".into());

        let cfg = run_config_from_args(&args, &mut config).unwrap();

        assert_eq!(cfg.token_budget, Some(7));
    }

    #[test]
    fn run_config_reads_token_budget_flag() {
        let mut args = args();
        args.token_budget = Some("1234".into());
        let cfg = run_config_from_args(&args, &mut settings::Config::default()).unwrap();
        assert_eq!(cfg.token_budget, Some(1234));
    }

    #[test]
    fn run_config_reads_complete_cost_budget() {
        let mut args = args();
        args.cost_budget_micro_usd = Some("10".into());
        args.input_cost_micro_per_ktok = Some("2".into());
        args.output_cost_micro_per_ktok = Some("4".into());
        let cfg = run_config_from_args(&args, &mut settings::Config::default()).unwrap();
        let cost = cfg.cost_budget.unwrap();
        assert_eq!(cost.limit_micro_usd, 10);
        assert_eq!(cost.input_micro_per_ktok, 2);
        assert_eq!(cost.output_micro_per_ktok, 4);
    }

    #[test]
    fn run_config_rejects_incomplete_cost_budget() {
        let mut args = args();
        args.cost_budget_micro_usd = Some("10".into());
        let err = run_config_from_args(&args, &mut settings::Config::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("incomplete cost budget"));
    }

    #[test]
    fn run_config_rejects_zero_budget() {
        let mut args = args();
        args.token_budget = Some("0".into());
        let err = run_config_from_args(&args, &mut settings::Config::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be > 0"));
    }

    #[test]
    fn run_config_reads_overload_fallback_model() {
        let mut args = args();
        args.overload_fallback_model = Some(" fallback ".into());
        let cfg = run_config_from_args(&args, &mut settings::Config::default()).unwrap();
        assert_eq!(cfg.overload_fallback_model.as_deref(), Some("fallback"));
    }

    #[test]
    fn run_config_rejects_cross_prompt_family_fallback() {
        let mut args = args();
        args.model = "gpt-5-codex".into();
        args.overload_fallback_model = Some("gpt-5.5".into());
        let err = run_config_from_args(&args, &mut settings::Config::default())
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

    /// US-020 AC1 changed this contract: `-p` no longer requires a value, so a
    /// flag right after it is parsed as a flag instead of aborting the startup.
    /// The prompt then comes from standard input, resolved outside the parser.
    #[test]
    fn print_does_not_swallow_the_flag_that_follows_it() {
        let args = parse_args_from(vec!["-p".to_string(), "--resume".to_string()]).unwrap();
        assert!(args.print);
        assert_eq!(args.prompt, None);
        assert_eq!(args.resume.as_deref(), Some(""));
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

    // ───────────────────────── EP-001 ─────────────────────────

    fn roots(granted: &[&str]) -> agent_sandbox::WritableRoots {
        agent_sandbox::WritableRoots {
            granted: granted.iter().map(std::path::PathBuf::from).collect(),
            ignored: Vec::new(),
        }
    }

    /// US-008 AC5: `--no-sandbox` is an ALIAS of full access, with no third
    /// semantics of its own.
    #[test]
    fn no_sandbox_is_an_alias_of_full_access() {
        let mut a = args();
        a.sandbox = false;
        let policy = sandbox_policy_from_args(
            &a,
            std::path::Path::new("/work/repo"),
            &settings::Config::default(),
            &roots(&[]),
        );
        assert_eq!(policy, SandboxPolicy::DangerFullAccess);

        // And it outranks a confined policy from the configuration: a flag the
        // user typed is never overridden by a file.
        let config = settings::Config {
            sandbox_mode: Some("read-only".to_string()),
            ..settings::Config::default()
        };
        let policy =
            sandbox_policy_from_args(&a, std::path::Path::new("/work/repo"), &config, &roots(&[]));
        assert_eq!(policy, SandboxPolicy::DangerFullAccess);
    }

    /// US-001 AC1/AC3: without configuration the perimeter is what it was
    /// before the policy type existed: the workspace, its extra roots, and the
    /// deferred-execution subpaths subtracted.
    #[test]
    fn the_default_policy_reproduces_the_previous_perimeter() {
        let policy = sandbox_policy_from_args(
            &args(),
            std::path::Path::new("/work/repo"),
            &settings::Config::default(),
            &roots(&["/tmp/scratch"]),
        );
        let SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
        } = &policy
        else {
            unreachable!("le défaut doit rester workspace-write: {policy:?}");
        };
        assert!(
            network_access,
            "l'allow-list reste seule maîtresse du réseau"
        );
        assert_eq!(writable_roots.len(), 2);
        assert_eq!(
            writable_roots[0].root,
            std::path::PathBuf::from("/work/repo")
        );
        assert_eq!(
            writable_roots[0].read_only_subpaths,
            agent_tools::path::PROTECTED_SUBPATHS
                .iter()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>()
        );
        // The extra roots carry no subtraction: `.git` only means something
        // under the workspace.
        assert!(writable_roots[1].read_only_subpaths.is_empty());
    }

    #[test]
    fn a_named_policy_from_the_global_configuration_is_applied() {
        let config = settings::Config {
            sandbox_mode: Some("read-only".to_string()),
            ..settings::Config::default()
        };
        let policy = sandbox_policy_from_args(
            &args(),
            std::path::Path::new("/work/repo"),
            &config,
            &roots(&["/tmp/scratch"]),
        );
        assert_eq!(
            policy,
            SandboxPolicy::ReadOnly {
                network_access: false
            }
        );
        // Read-only grants no root, so `writable_roots` has nothing to add.
        assert!(policy.writable_roots().is_empty());
    }

    /// US-001 AC4/AC5: the status line always names the policy REALLY in force.
    #[test]
    fn the_status_line_names_the_policy_really_in_force() {
        let full = SandboxPolicy::DangerFullAccess;
        assert_eq!(
            sandbox_scope_label(&full, false),
            "off (writes not restricted)"
        );

        let ws = SandboxPolicy::workspace_write("/work/repo", Vec::new(), [".git"]);
        assert_eq!(sandbox_scope_label(&ws, true), "enforced (workspace)");
        let ws_extra = SandboxPolicy::workspace_write(
            "/work/repo",
            vec![std::path::PathBuf::from("/tmp/a")],
            [".git"],
        );
        assert_eq!(
            sandbox_scope_label(&ws_extra, true),
            "enforced (workspace + 1 extra root)"
        );

        let read_only = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        assert_eq!(
            sandbox_scope_label(&read_only, true),
            "enforced (read-only)"
        );

        // Requested but not carried by the kernel: shown as what it is.
        let degraded = sandbox_scope_label(&read_only, false);
        assert!(degraded.contains("read-only"), "{degraded}");
        assert!(degraded.contains("NOT applied"), "{degraded}");
    }

    // ───────────────────────── EP-002 ─────────────────────────

    /// US-007 / US-008: the flags that drive the configuration layers are read,
    /// and `-c` splits on the FIRST `=` so a value may contain one.
    #[test]
    fn parse_args_reads_the_configuration_flags() {
        let args = parse_args_from(
            [
                "--profile",
                "review",
                "--permission-mode",
                "read-only",
                "--sandbox",
                "read-only",
                "-c",
                "model=gpt-5.6-sol",
                "--config",
                "overload_fallback_model=a=b",
            ]
            .map(String::from)
            .to_vec(),
        )
        .unwrap();

        assert_eq!(args.profile.as_deref(), Some("review"));
        assert_eq!(args.permission_mode, Some(PermissionMode::Plan));
        assert_eq!(args.sandbox_mode.as_deref(), Some("read-only"));
        assert_eq!(
            args.overrides,
            vec![
                ("model".to_string(), "gpt-5.6-sol".to_string()),
                ("overload_fallback_model".to_string(), "a=b".to_string()),
            ]
        );
    }

    /// US-008 AC3: an unknown value on either flag refuses to start, naming the
    /// value received AND the ones accepted. Never a silent fallback: that would
    /// run a session under a perimeter the user did not choose.
    #[test]
    fn parse_args_refuses_an_unknown_mode_and_names_the_accepted_values() {
        let err = parse_args_from(vec!["--sandbox".to_string(), "paranoid".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("paranoid"), "{err}");
        assert!(err.contains("workspace-write"), "{err}");

        let err = parse_args_from(vec!["--permission-mode".to_string(), "yolo".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("yolo"), "{err}");
        assert!(err.contains("accept-edits"), "{err}");

        let err = parse_args_from(vec!["-c".to_string(), "model".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected key=value"), "{err}");

        let err = parse_args_from(vec!["-c".to_string(), "=value".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected key=value"), "{err}");
    }

    /// US-008 AC2: the sandbox flag selects the variant of US-001, and it outranks
    /// the global file because it enters the configuration as the strongest layer.
    #[test]
    fn the_sandbox_flag_selects_the_variant_over_the_file() {
        let dir = std::env::temp_dir().join(format!("pyxis-ep002-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let global = dir.join("settings.toml");
        std::fs::write(&global, "sandbox_mode = \"read-only\"\n").unwrap();

        let config = settings::resolve(settings::Request {
            global: Some(&global),
            flags: settings::Flags {
                sandbox_mode: Some("workspace-write"),
                ..settings::Flags::default()
            },
            ..settings::Request::default()
        })
        .unwrap();

        let policy = sandbox_policy_from_args(
            &args(),
            std::path::Path::new("/work/repo"),
            &config,
            &roots(&[]),
        );
        assert!(
            matches!(policy, SandboxPolicy::WorkspaceWrite { .. }),
            "{policy:?}"
        );

        // AC5: `--no-sandbox` reaches the same key, as an alias of full access.
        let config = settings::resolve(settings::Request {
            global: Some(&global),
            flags: settings::Flags {
                sandbox_mode: Some("full-access"),
                ..settings::Flags::default()
            },
            ..settings::Request::default()
        })
        .unwrap();
        assert_eq!(config.sandbox_mode.as_deref(), Some("full-access"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// US-008 AC4/AC6: a flag applies in headless mode as in interactive mode, and
    /// a mode more permissive than the `-p` default is announced.
    #[test]
    fn a_permission_flag_applies_in_headless_mode_and_is_announced() {
        let config = settings::resolve(settings::Request {
            flags: settings::Flags {
                permission_mode: Some(PermissionMode::BypassPermissions),
                ..settings::Flags::default()
            },
            ..settings::Request::default()
        })
        .unwrap();

        assert_eq!(
            config.sources.layer(settings::PERMISSION_MODE_KEY),
            Some(settings::ConfigLayer::SessionFlags)
        );
        let (mode, announced) = resolve_permission_mode(
            config.permission_mode,
            permission_policy(true, false, true),
            true,
        );
        assert_eq!(mode, PermissionMode::BypassPermissions);
        assert!(announced, "un elargissement en headless doit etre annonce");
    }

    /// US-020 AC1: `-p` no longer demands its value. Without one the mode is
    /// still headless, and the prompt is left to standard input.
    #[test]
    fn print_without_value_is_headless_and_leaves_the_prompt_to_stdin() {
        let args = parse_args_from(argv(&["-p"])).unwrap();
        assert!(args.print);
        assert_eq!(args.prompt, None);

        // A following flag is NOT swallowed as the prompt.
        let args = parse_args_from(argv(&["-p", "--output-format", "json"])).unwrap();
        assert!(args.print);
        assert_eq!(args.prompt, None);
        assert_eq!(args.output_format, jsonl::OutputFormat::Json);

        // With a value, the old behavior is unchanged.
        let args = parse_args_from(argv(&["-p", "hello"])).unwrap();
        assert!(args.print);
        assert_eq!(args.prompt.as_deref(), Some("hello"));
    }

    /// US-020 AC2: the argument wins, and standard input is then not read AT ALL
    /// (the injected reader must never run).
    #[test]
    fn an_argument_prompt_wins_over_stdin_which_stays_unread() {
        let mut read = false;
        let resolved = resolve_prompt(true, Some("from arg".into()), || {
            read = true;
            Ok(Some("from stdin".into()))
        })
        .unwrap();
        assert_eq!(resolved.as_deref(), Some("from arg"));
        assert!(
            !read,
            "l'entrée standard ne doit pas être lue quand l'argument porte le prompt"
        );
    }

    /// US-020 AC1: piped, the prompt comes from standard input, trailing newline
    /// of `echo` included.
    #[test]
    fn stdin_becomes_the_prompt_when_the_argument_is_absent() {
        let resolved = resolve_prompt(true, None, || Ok(Some("  do the thing\n".into()))).unwrap();
        assert_eq!(resolved.as_deref(), Some("do the thing"));
    }

    /// US-020 AC5: nothing on either side is a NAMED error, never a wait. Both
    /// shapes count as nothing: a terminal (`None`) and an empty pipe.
    #[test]
    fn no_prompt_at_all_fails_by_name_instead_of_waiting() {
        for stdin in [None, Some(String::new()), Some("  \n".into())] {
            let err = resolve_prompt(true, None, || Ok(stdin.clone())).unwrap_err();
            assert!(
                err.to_string()
                    .contains("no prompt provided (argument or standard input)"),
                "message: {err}"
            );
        }
        // Without `-p` there is no headless run to feed: interactive mode stays.
        assert_eq!(resolve_prompt(false, None, || Ok(None)).unwrap(), None);
    }

    /// US-020 AC6: the two flags contradict each other and the refusal names both.
    #[test]
    fn ephemeral_and_resume_are_refused_by_name() {
        let err = parse_args_from(argv(&["-p", "x", "--ephemeral", "--resume", "latest"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--ephemeral"), "message: {err}");
        assert!(err.contains("--resume"), "message: {err}");
    }

    /// US-020: `--ephemeral` promises "no session file", which only the headless
    /// mode can hold: `/new`, `/fork` and `/resume` all move a file.
    #[test]
    fn ephemeral_requires_the_headless_mode() {
        let err = parse_args_from(argv(&["--ephemeral"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--ephemeral"), "message: {err}");
        assert!(err.contains("-p"), "message: {err}");

        assert!(parse_args_from(argv(&["-p", "x", "--ephemeral"])).is_ok());
        // A bare positional argument is already a headless run.
        assert!(parse_args_from(argv(&["--ephemeral", "do it"])).is_ok());
        // `--help` still prints instead of failing on an incoherent combination.
        assert!(parse_args_from(argv(&["--ephemeral", "--help"])).is_ok());
    }

    /// US-020 AC4/AC6: the validation is on the resolved flags, not on the order
    /// they were typed in.
    #[test]
    fn ephemeral_validation_is_order_independent() {
        let mut a = args();
        a.ephemeral = true;
        a.print = true;
        assert!(validate_ephemeral(&a).is_ok());
        a.resume = Some(String::new());
        assert!(validate_ephemeral(&a).is_err());
    }

    /// US-020 AC3: the destination is taken as given; no default, no extension.
    #[test]
    fn output_last_message_takes_a_path() {
        let args = parse_args_from(argv(&["-p", "x", "--output-last-message", "out.txt"])).unwrap();
        assert_eq!(args.output_last_message.as_deref(), Some("out.txt"));
        let err = parse_args_from(argv(&["-p", "x", "--output-last-message"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--output-last-message"), "message: {err}");
    }
}
