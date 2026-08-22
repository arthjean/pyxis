//! `pyxis`: the CLI binary. The ONLY crate that wires everything (ARCHITECTURE 2):
//! core + ChatGPT subscription provider + tools + session + sandbox + TUI frontend.
//!
//! Critical order: the **FS sandbox (Landlock) is applied on the main thread
//! BEFORE the tokio runtime is built** -> the workers and the Bash
//! subprocesses inherit the confinement (fork-safe, see `agent_sandbox::fs`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod app_server;
mod approver;
mod code_mode;
// The configuration catalog (EP-033) reads the key constants of `settings.rs`
// and the flag wiring of this file, then renders `docs/config-catalog.md`.
// Gated on `test` for the same reason as `tool_catalog`: it is a documentation
// gate, and the constants it confronts are crate-private.
#[cfg(test)]
mod config_catalog;
mod context;
mod failure_line;
mod headless;
mod interactive;
mod jobs;
mod jsonl;
mod observability;
mod prompt;
mod runtime;
mod session;
mod settings;
mod skills;
mod subagents;
// The tool catalog (EP-032) reads the wiring of this file and renders
// `docs/tool-catalog.md`. Gated on `test` because it is a documentation
// gate, and `agent-cli` has no `[lib]` target an integration test could
// import the wiring from.
#[cfg(test)]
mod tool_catalog;
// The transcript harness (EP-038) drives `headless::run` with a scripted
// provider, a frozen clock and a seeded id generator, and compares the bytes it
// renders. Gated on `test` for the same reason as `tool_catalog` and
// `config_catalog`: `agent-cli` has no `[lib]` target an integration test could
// import the published headless path from, so the only place that can call it
// is a module of the binary itself.
#[cfg(test)]
mod transcript;

use std::sync::Arc;

use agent_auth::{OAuthCredential, ProviderId, store};
use agent_core::RunConfig;
use agent_core::clock::SystemClock;
use agent_core::guardrail::CostBudget;
use agent_core::message::{Message, recent_untrusted_content};
use agent_core::provider::Provider;
use agent_core::sandbox::SandboxPolicy;
use agent_provider::{KEYRING_ACCOUNT, OpenAiChatGptProvider};
use agent_sandbox::{ProxyPolicy, set_proxy_env};
use agent_tokenizer::HeuristicCounter;
use agent_tools::permission::{AutoDeny, PermissionMode, PermissionModeState};
use agent_tools::{
    ApplyPatch, Bash, DynTool, Edit, ExecCommand, ExecSessions, Glob, Grep, Read, Registry,
    SpillStore, UpdatePlan, ViewImage, Write, WriteStdin,
};

use crate::approver::{CliPermissionBroker, TuiApprover, TuiNetworkApprover};
use crate::interactive::InteractiveConfig;

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
    /// `--app-server` (EP-005): serve the JSON-RPC client contract instead of
    /// opening a TUI or running one prompt.
    app_server: bool,
    /// `--listen <addr>`: serve the same contract over WebSocket as well.
    app_server_listen: Option<String>,
    /// `--emit-schemas <dir>`: write the published protocol schemas and exit.
    emit_schemas: Option<String>,
    help: bool,
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
      --app-server                      Serve the JSON-RPC client contract on
                                        standard input/output instead of opening
                                        a TUI (docs/app-server/README.md)
      --listen <addr>                   Also serve that contract over WebSocket.
                                        Loopback addresses only; the bearer token
                                        is printed on stderr at startup
      --emit-schemas <dir>              Write the app-server JSON Schema and
                                        TypeScript declarations there, then exit
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
        app_server: false,
        app_server_listen: None,
        emit_schemas: None,
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
            // EP-005: the third client. `app-server` is accepted as a bare word
            // too, since that is how Codex names the same mode.
            "--app-server" | "app-server" => args.app_server = true,
            "--listen" => args.app_server_listen = Some(next_value(&mut it, "--listen")?),
            "--emit-schemas" => args.emit_schemas = Some(next_value(&mut it, "--emit-schemas")?),
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
        validate_app_server(&args)?;
    }
    Ok(args)
}

/// EP-005: the app-server drives threads on behalf of a client, so it cannot
/// also be a one-shot run or an ephemeral one. Refused rather than silently
/// ignoring whichever flag came second.
fn validate_app_server(args: &Args) -> anyhow::Result<()> {
    if !args.app_server {
        if args.app_server_listen.is_some() {
            anyhow::bail!("--listen requires --app-server");
        }
        return Ok(());
    }
    if args.print || args.prompt.is_some() {
        anyhow::bail!("--app-server and -p/--print are incompatible");
    }
    if args.ephemeral {
        anyhow::bail!("--app-server and --ephemeral are incompatible");
    }
    if args.resume.is_some() {
        anyhow::bail!(
            "--app-server and --resume are incompatible: a client resumes with `thread/resume`"
        );
    }
    Ok(())
}

/// Serves the app-server on stdio, plus WebSocket when an address was given.
///
/// The two transports share ONE protocol actor, so the ownership of a thread is
/// arbitrated across them: a WebSocket client asking for the thread the stdio
/// client drives gets the same typed conflict a second stdio client would.
async fn serve_app_server(
    server: Arc<agent_app_server::AppServer>,
    listen: Option<String>,
) -> anyhow::Result<()> {
    let websocket = match listen {
        None => None,
        Some(raw) => {
            let addr: std::net::SocketAddr = raw
                .parse()
                .map_err(|err| anyhow::anyhow!("--listen `{raw}`: {err}"))?;
            let token = Arc::new(agent_app_server::mint_token());
            // On stderr, never on stdout: stdout is the protocol stream.
            eprintln!("[app-server] ws://{addr} token={token}");
            let server = Arc::clone(&server);
            Some(tokio::spawn(async move {
                agent_app_server::serve_websocket(server, addr, token).await
            }))
        }
    };
    let outcome = agent_app_server::serve_stdio(server).await;
    if let Some(websocket) = websocket {
        websocket.abort();
    }
    outcome.map_err(|err| anyhow::anyhow!("{err}"))
}

/// Writes the published app-server schemas (US-018 AC3).
///
/// Deterministic by construction: the generator sorts every map and walks the
/// method tables in declaration order, so re-running it on one commit rewrites
/// byte-identical files and the repository diff is the real drift signal.
fn emit_schemas(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let schema = dir.join(agent_app_server::schema::JSON_SCHEMA_FILE);
    let types = dir.join(agent_app_server::schema::TYPESCRIPT_FILE);
    let mut document =
        serde_json::to_string_pretty(&agent_app_server::schema::json_schema_document())?;
    document.push('\n');
    std::fs::write(&schema, document)?;
    std::fs::write(&types, agent_app_server::schema::typescript()?)?;
    println!("{}", schema.display());
    println!("{}", types.display());
    Ok(())
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

/// Mode a run starts in when nothing declares one. Interactive always starts on
/// `ask`, so only the headless mode has a choice to make, and `--yes` is the
/// whole of it. Whether the kernel really confines the filesystem does NOT enter
/// here: `--yes` states what the user accepts, and a policy that read the
/// sandbox back would silently mean two different things on two machines. The
/// warning belongs to `enforce_sandbox`, which is where the degradation is known.
fn default_permission_mode(headless: bool, yes: bool) -> PermissionMode {
    if headless && yes {
        PermissionMode::AcceptEdits
    } else {
        PermissionMode::Default
    }
}

/// Effective permission mode, and whether the configuration replaced the default of
/// the headless mode (US-016 AC6). That replacement can WIDEN what a `-p`
/// allows itself: it is therefore announced rather than silently applied, and only the global
/// configuration can carry the key (`settings::SECURITY_KEYS`), never the
/// project one.
fn resolve_permission_mode(
    from_config: Option<PermissionMode>,
    default: PermissionMode,
    headless: bool,
) -> (PermissionMode, bool) {
    let mode = from_config.unwrap_or(default);
    (mode, headless && mode != default)
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

/// Spill storage of the run (US-072), created HERE for the same reason as the
/// session directory: under a confining policy nothing in the workspace stays
/// writable afterwards, and a Landlock rule needs a path that already opens.
///
/// Unlike a session, a spill root Pyxis cannot create is NOT fatal. The absence
/// means no spill at all, which is exactly the behavior before EP-022, so the
/// failure is named and the binary starts. `--ephemeral` skips it for the same
/// reason it skips the session directory: the flag promises no trace left in
/// the workspace, and creating the root "just in case" would already be one.
fn open_spill_store(workspace: &std::path::Path, ephemeral: bool) -> Option<Arc<SpillStore>> {
    if ephemeral {
        return None;
    }
    match SpillStore::create(workspace, &agent_tools::spill::run_id()) {
        Ok(store) => Some(Arc::new(store)),
        Err(err) => {
            eprintln!("[spill] {err}; large tool outputs stay truncated");
            None
        }
    }
}

/// The directories Pyxis writes for ITSELF whatever the policy: the session
/// directory, and the spill root when one exists. Assembled apart from `main`
/// so what reaches `writable_dirs` is provable without restricting the test
/// process.
fn agent_state_dirs<'a>(
    session_dirs: &[&'a std::path::Path],
    spill: Option<&'a SpillStore>,
) -> Vec<&'a std::path::Path> {
    let mut dirs = session_dirs.to_vec();
    if let Some(store) = spill {
        dirs.push(store.root());
    }
    dirs
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

/// Selects the process-wide provider before any TLS client or Tokio worker can
/// ask rustls to infer one from the unified dependency features. The binary
/// contains both `ring` (reqwest) and `aws-lc-rs` (AWS SDK), so inference is
/// deliberately ambiguous.
fn install_tls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn main() -> anyhow::Result<()> {
    install_tls_crypto_provider();
    let mut args = parse_args()?;
    if args.help {
        print!("{HELP}");
        return Ok(());
    }
    // EP-005: pure generation, so it runs before the credential, the sandbox and
    // the runtime. Writing the published contract must never depend on being
    // able to connect to anything.
    if let Some(dir) = args.emit_schemas.as_deref() {
        return emit_schemas(std::path::Path::new(dir));
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
    // Only the disk-reading half here: the environment block is closed in
    // `run`, where the sandbox policy and the permission state that it announces
    // finally exist (see `context::with_environment`).
    let context_docs = context::project_documents(&workspace, &skills);

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
    //
    // Created ONCE, here, and fatal: a session Pyxis cannot record is not a
    // session. The runtime used to try again on its own and bail, which meant
    // the same failure was reported twice with two different verdicts.
    let sessions_dir = workspace.join(".pyxis").join("sessions");
    if !args.ephemeral {
        std::fs::create_dir_all(&sessions_dir)
            .map_err(|err| anyhow::anyhow!("session: {}: {err}", sessions_dir.display()))?;
    }
    let session_dirs: &[&std::path::Path] = if args.ephemeral {
        &[]
    } else {
        &[sessions_dir.as_path()]
    };

    let spill = open_spill_store(&workspace, args.ephemeral);
    let state_dirs = agent_state_dirs(session_dirs, spill.as_deref());

    // FS sandbox BEFORE the runtime (main thread -> inherited by the workers).
    let policy = sandbox_policy_from_args(&args, &workspace, &config, &writable_roots);
    let enforced = enforce_sandbox(
        &args,
        &policy,
        settings_path.as_deref(),
        &diagnostics,
        &writable_roots,
        &state_dirs,
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
            context_docs,
            cred: credential,
            settings_path,
            config,
            sessions_dir,
            spill,
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
    /// Servers the user marked `required` that did not come up. A non-empty list
    /// aborts the session rather than starting one silently short of a tool the
    /// user declared indispensable.
    required_failures: Vec<String>,
}

/// Bound of the keyring-side HTTP client used to refresh an MCP credential.
const MCP_OAUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Resolves the stored OAuth credential of a remote MCP server, refreshing it
/// when it is about to expire. `None` = connect anonymously and let the server
/// decide, which is right for a server that accepts it and produces an
/// actionable `401` for one that does not.
///
/// This is the caller's job rather than `agent-mcp`'s. Reading the OS secret
/// store is an authentication concern, and folding it into the transport would
/// make a broken keyring indistinguishable from a broken connection, on top of
/// pulling the whole auth crate into the protocol one.
pub(crate) async fn mcp_oauth_token(
    cfg: &agent_mcp::McpServerConfig,
    name: &str,
) -> Option<String> {
    // A stdio server has no authorization server, and an explicit
    // `bearerTokenEnvVar` is the auditable path the transport resolves itself.
    if !matches!(
        &cfg.transport,
        agent_mcp::McpTransport::Http {
            bearer_token_env_var: None,
            ..
        }
    ) {
        return None;
    }
    let client = reqwest::Client::builder()
        .connect_timeout(MCP_OAUTH_TIMEOUT)
        .build()
        .ok()?;
    match agent_auth::oauth::mcp::access_token(&client, name, agent_auth::oauth::now_ms()).await {
        // Point of use: the token stops being a `Secret` exactly where it is
        // handed to the transport that puts it on the wire.
        Ok(token) => token.map(|token| token.expose().to_string()),
        Err(err) => {
            // Not fatal: a stale or unreadable credential must not make an
            // anonymous-capable server unreachable. It is traced rather than
            // swallowed, so the cause is recoverable from the log.
            tracing::debug!(
                target: "pyxis::mcp",
                server = %name,
                error = %err,
                "stored MCP credential unusable, connecting anonymously"
            );
            None
        }
    }
}

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
    resources: &agent_mcp::McpResourceCatalog,
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
                    // `required` cannot be honored on a server that is not dialed
                    // here: it would make the session unstartable behind its own
                    // trust gate, which is exactly how a workspace file would turn
                    // a refused spawn into a refused session.
                    if server.config().policy.required {
                        notices.push(format!(
                            "MCP \"{name}\": required ignored, this server is only dialed after an explicit trust."
                        ));
                    }
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
        // The config travels with the task: the registry lock is not held while
        // the connection is in flight.
        let task_cfg = cfg.clone();
        // No outer bound here on purpose. `startupTimeoutMs` is ONE deadline taken
        // inside the connection, covering spawn, handshake, retries and
        // `tools/list` together; wrapping a second bound around it would cut the
        // remote retry policy off mid-attempt and make the declared value mean
        // something different depending on the transport. Every server is dialed
        // concurrently, so the slowest one is still the ceiling this step adds to
        // the launch, not the sum (US-012 AC3).
        pending.spawn(async move {
            let token = mcp_oauth_token(&cfg, &name).await;
            let attempt = async {
                let conn = agent_mcp::McpConnection::connect_with(
                    &name,
                    &cfg,
                    Some(&harden),
                    token.as_deref(),
                )
                .await?;
                match conn.list_tools(&name).await {
                    Ok(tools) => Ok((conn, tools)),
                    Err(err) => {
                        conn.cancel().await;
                        Err(err)
                    }
                }
            };
            match attempt.await {
                Ok(connected) => (name, task_cfg, Ok(connected)),
                Err(err) => (name, task_cfg, Err(err.to_string())),
            }
        });
    }

    let mut startup = McpStartup::default();
    // Shared across every server: uniqueness of the exposed names is a property of
    // the whole set (US-011 AC1).
    let mut taken = std::collections::BTreeSet::new();
    while let Some(joined) = pending.join_next().await {
        let (name, cfg, outcome) = match joined {
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
                // `required`: the user stated the session is not worth starting
                // without this server, so the failure is fatal instead of a notice.
                if cfg.policy.required {
                    startup
                        .required_failures
                        .push(format!("MCP \"{name}\" (required): {err}"));
                } else {
                    notices.push(format!(
                        "MCP \"{name}\" unavailable: {err} (its tools are absent)."
                    ));
                }
                continue;
            }
        };
        let client = conn.client(&name);
        // US-014: the policy shapes the exposed surface BEFORE anything reaches the
        // registry, and the filtered list is what `/mcp` shows.
        let (listed, filter_notices) = agent_mcp::filter_tools(&name, &listed, &cfg.policy.tools);
        notices.extend(filter_notices);
        let (mut exposed, skipped) =
            agent_mcp::dyn_tools(&name, &listed, &cfg.policy, &client, &mut taken);
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
            // US-012: the resource tools reach exactly the servers held by the
            // registry, so they are published at the same instant the tools are
            // and never one step ahead of a connection that failed to be kept.
            resources.connect(name.clone(), client);
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
    /// The disk-read half of the project context, in block order. The
    /// environment block is appended in `run`.
    context_docs: Vec<Message>,
    cred: OAuthCredential,
    /// Writable settings file (interactive only). `None` in headless mode.
    settings_path: Option<std::path::PathBuf>,
    /// Effective configuration (global + project), read in both modes.
    config: settings::Config,
    /// `<workspace>/.pyxis/sessions`, created (unless `--ephemeral`) and granted
    /// to the sandbox before Landlock. Resolved there and carried here so the
    /// runtime never rebuilds a path whose creation it did not witness.
    sessions_dir: std::path::PathBuf,
    /// Spill storage of the run (US-072), created and granted to the sandbox
    /// alongside the session directory. `None` = no spill, either because
    /// `--ephemeral` forbids the trace or because the root could not be created.
    spill: Option<Arc<SpillStore>>,
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
        context_docs,
        cred,
        settings_path,
        mut config,
        sessions_dir,
        spill,
    } = pre;
    // The resolved model already accounts for `--model`, which enters the
    // configuration as the strongest layer (US-005): there is nothing left to
    // arbitrate here. Resolved BEFORE everything else, because the fallback
    // validation and the initial effort are computed on the model actually used.
    if let Some(model) = config.model.clone() {
        args.model = model;
    }
    let run_config = run_config_from_args(&args, &mut config)?;
    // EP-002: the Code Mode runtime of this process. `None` leaves every direct
    // model untouched and keeps a code-mode model refused with its cause named.
    let code_mode = code_mode::build();
    let code_mode_available = code_mode.is_some();
    let headless = args.prompt.is_some();
    let initial_reasoning_effort = config
        .reasoning_effort
        .as_deref()
        .and_then(agent_tui::normalize_reasoning_effort);
    // 1. ChatGPT subscription credential loaded before the sandbox. When it is missing in
    // interactive mode, the OAuth onboarding has already run before we get here.
    let mut chatgpt = OpenAiChatGptProvider::new(
        cred,
        agent_provider::DEFAULT_MAX_CONTEXT,
        initial_reasoning_effort.clone(),
    )?;
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
    // The catalog only stops refusing a code-mode model once a runtime really
    // exists in this process. Set before any resolve, and before the model list
    // is published, so no surface ever shows a compatibility the run lacks.
    chatgpt.set_code_mode(code_mode_available);
    // Interactive and headless resolve through the same scoped refresh before
    // the first turn. A failure keeps the embedded descriptor for this scope.
    match chatgpt.list_models().await {
        Ok(models) => {
            if !headless {
                agent_tui::set_models(
                    models
                        .into_iter()
                        .map(|model| agent_tui::ModelCatalogEntry {
                            slug: model.slug,
                            default_reasoning_effort: model.default_reasoning_effort,
                            supported_reasoning_efforts: model.supported_reasoning_efforts,
                            incompatibility_reason: model.incompatibility_reason,
                        })
                        .collect(),
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                target: "pyxis::models",
                error = %error,
                "remote model catalog unavailable; using embedded descriptors"
            );
        }
    }
    let provider: Arc<dyn Provider> = chatgpt;

    // Notices addressed to the HUMAN (hooks, blocked hosts): TUI history in
    // interactive mode, stderr in headless mode where the diagnostics of this
    // mode already go (stdout stays byte-for-byte identical).
    let (notice_tx, hook_notice_rx) = tokio::sync::mpsc::channel::<String>(16);

    // Confirmation channel, opened before the proxy because the proxy asks on it
    // too (US-004): a host blocked at connect time is a question, not only a
    // refusal, and the answer has to reach the socket still waiting for it.
    let (perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);

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
    // US-004: only an interactive run has someone to ask. Headless and
    // app-server keep the historical behavior (immediate 403), because a
    // question nobody can answer is a socket held open for the bound and then
    // refused anyway.
    let network_approver: Option<Arc<dyn agent_sandbox::NetworkApprover>> = (!headless
        && !args.app_server)
        .then(|| Arc::new(TuiNetworkApprover::new(perm_tx.clone())) as Arc<_>);
    let proxy = agent_sandbox::spawn_proxy_with_approver(
        proxy_policy,
        Some(proxy_notice),
        network_approver,
    )
    .await?;
    let proxy_addr = proxy.addr.clone();
    let harden: agent_tools::CommandHardener =
        Arc::new(move |cmd: &mut tokio::process::Command| set_proxy_env(cmd, &proxy_addr));
    let mcp_harden = Arc::clone(&harden);

    // 3. Persistent session: one JSONL file per conversation (timestamped) under
    // <workspace>/.pyxis/sessions/, created before the sandbox (see `main`).
    let (current_session, initial_messages) = if let Some(resume_arg) = &args.resume {
        let path = resolve_resume_path(&sessions_dir, resume_arg)?;
        let resumed =
            agent_session::resume_file(&path).map_err(|e| anyhow::anyhow!("resume: {e}"))?;
        (path, resumed.messages)
    } else {
        (interactive::new_session_path(&sessions_dir), Vec::new())
    };
    provider.set_prompt_cache_key(&interactive::prompt_cache_key_for_session(&current_session));
    // US-018 AC4: under `--ephemeral` the durable log is an in-memory adapter, so
    // no file of any kind is opened. The identifier keeps naming the run in the
    // summary line. The transcript itself is opened by the thread runtime, which
    // is the single writer of that file since EP-005.
    let initial_taint_recent =
        recent_untrusted_content(&initial_messages, RESUME_TAINT_SCAN_MESSAGES);

    // Persistent per-session goal (`/goal`): interactive mode only.
    let goal = if headless {
        None
    } else {
        interactive::read_goal(&interactive::goal_path_for_session(&current_session))
    };

    // 4. Tool registry + approver (TUI in interactive mode, auto in headless).
    // The channel itself was opened above, with the proxy.
    let default_mode = default_permission_mode(headless, args.yes);
    let (initial_permission_mode, announce_override) =
        resolve_permission_mode(config.permission_mode, default_mode, headless);
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
            settings::permission_mode_id(default_mode)
        );
    }
    let permission_mode = PermissionModeState::new(initial_permission_mode);
    // What the `<environment>` block announces to the model. Holds the shared
    // permission state rather than a snapshot of it, so `/permissions` reaches
    // the next turn without recomposing the project context.
    let workspace_access = context::WorkspaceAccess::new(&sandbox.policy, permission_mode.clone());
    // EP-005: the slot the app-server connection binds itself into. Built here
    // because the registry is assembled once, before any thread exists, and
    // fail-closed while nothing is bound: an app-server with no client attached
    // denies rather than approves.
    let app_bridge = agent_app_server::ClientBridge::new();
    let approver: Arc<dyn agent_tools::permission::Approver> = if args.app_server {
        Arc::new(agent_app_server::BridgeApprover::new(Arc::clone(
            &app_bridge,
        )))
    } else if headless {
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
    // US-012: the connected servers, as the three resource tools see them.
    // Created before the startup connection so a server held by the registry is
    // published into it in the same step.
    let mcp_resources = agent_mcp::McpResourceCatalog::new();
    let mcp_startup = if headless {
        McpStartup::default()
    } else {
        connect_mcp_at_startup(&mcp, &mcp_harden, &mcp_resources).await
    };
    // A server the user marked `required` did not come up: refuse to start rather
    // than open a session that silently lacks a tool declared indispensable.
    if !mcp_startup.required_failures.is_empty() {
        anyhow::bail!(
            "required MCP server(s) failed to start:\n  {}",
            mcp_startup.required_failures.join("\n  ")
        );
    }

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

    // EP-003/US-011: the multi-agent v2 surface. The spawner is the binary's
    // half of the contract (it builds a child's registry from its authority);
    // the handle is what the six tools address, and `SessionRuntime` binds it to
    // the supervisor of whichever thread is open. Both exist before the registry
    // because the tools are registered into it.
    let subagent_spawner = subagents::SubAgentSpawner::new(
        Arc::clone(&provider),
        Arc::new(HeuristicCounter),
        Arc::new(SystemClock),
        workspace.clone(),
        sandbox.policy.clone(),
        sandbox.enforced,
        Arc::clone(&hooks),
        Arc::clone(&harden),
        (!args.ephemeral).then(|| current_session.clone()),
    );
    // EP-042: the launcher half of the background job registry, on the same
    // split. `agent-runtime` accounts for a job and `agent-tools` owns the
    // terminal; only this crate sees both, so this is where a recorded job
    // becomes a running shell. The handle is the one the terminal tools already
    // carry, rebound by `SessionRuntime` to the registry of the open thread.
    let jobs = runtime::JobWiring {
        launcher: Arc::new(jobs::TerminalJobLauncher::new(exec_sessions.clone()))
            as Arc<dyn agent_runtime::jobs::JobLauncher>,
        handle: exec_sessions.job_handle(),
    };

    let agent_handle = Arc::new(agent_tools::AgentHandle::new());
    let agents = runtime::AgentWiring {
        spawner: Arc::clone(&subagent_spawner) as Arc<dyn agent_runtime::supervisor::AgentSpawner>,
        handle: Arc::clone(&agent_handle),
        // The root thread is the user's: the tool pipeline bounds it, not this
        // type. A child never gets more than the intersection with it (FR-11).
        authority: agent_runtime::AgentAuthority::unrestricted(),
    };

    // Published by the loop, read by `get_context_remaining`. Created here
    // because the registry and the engine are assembled from the same scope and
    // must share ONE handle.
    let context_window = agent_core::budget::ContextWindowState::new();

    // Question addressed to the human (`request_user_input`), on the same
    // channel the sandbox and the hooks already use. Headless has no one to ask,
    // so no channel is installed and the tool says the question was not asked.
    let user_notice: Option<agent_tools::UserNotice> = (!headless).then(|| {
        let tx = notice_tx.clone();
        Arc::new(move |message: String| {
            if let Err(err) = tx.try_send(message) {
                eprintln!("[ask] {}", err.into_inner());
            }
        }) as agent_tools::UserNotice
    });

    // US-007: what the user declared side-effect free, on top of the built-in
    // table. Refusals are reported rather than swallowed: a line that does
    // nothing must not look like a line that worked.
    let (command_policy, policy_notices) =
        agent_tools::command::CommandPolicy::from_entries(config.safe_commands.clone());
    for notice in &policy_notices {
        eprintln!("[config] {notice}");
    }
    let command_policy = Arc::new(command_policy);

    let mut builder = Registry::builder(&workspace)
        .mode_state(permission_mode.clone())
        .command_policy(Arc::clone(&command_policy))
        .context_window(context_window.clone())
        .approver(Arc::clone(&approver))
        .approvals(approvals.clone())
        .initial_taint_recent(initial_taint_recent)
        .hooks(Arc::clone(&hooks))
        .command_hardener(harden)
        // US-011: whether the active model reads images at all. A provider that
        // does not declare vision makes `view_image` refuse before any read.
        .vision(provider.capabilities().vision)
        // Grouping the MCP surface per server is only exposable when the
        // adapter can encode it; otherwise every tool stays flat.
        .namespace_tools(provider.capabilities().tool_calling.namespace_tools)
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
        .register(WriteStdin)
        // Clock and context-window surface: what the model cannot observe from
        // inside a turn, and therefore invents when it is not exposed.
        .register(agent_tools::CurrentTime)
        .register(agent_tools::Sleep)
        .register(agent_tools::GetContextRemaining)
        .register(agent_tools::NewContextWindow)
        // Asking the human: a question that does not block the turn, and a
        // widening request that does.
        .register(agent_tools::RequestUserInput)
        // US-012: the read-only half of MCP, which servers are told to model as
        // resources rather than as tools.
        .register(agent_mcp::ListMcpResources::new(mcp_resources.clone()))
        .register(agent_mcp::ListMcpResourceTemplates::new(
            mcp_resources.clone(),
        ))
        .register(agent_mcp::ReadMcpResource::new(mcp_resources.clone()))
        .register(agent_tools::RequestPermissions::new(Arc::new(
            CliPermissionBroker::new(
                Arc::clone(&approver),
                proxy.grants.clone(),
                permission_mode.clone(),
            ),
        )));
    if let Some(notice) = user_notice {
        builder = builder.user_notice(notice);
    }
    // US-072: the root created before Landlock, handed to the tools. Absent
    // under `--ephemeral` or after a creation failure, and the absence keeps
    // every tool at its pre-EP-022 behavior.
    if let Some(store) = &spill {
        builder = builder.spill(Arc::clone(store));
    }
    if let Some(handle) = &code_mode {
        builder = builder
            .register(agent_tools::ExecTool::new(Arc::clone(handle)))
            .register(agent_tools::WaitTool::new(Arc::clone(handle)))
            // US-064: the run the nested dispatch counts on, so a steering
            // input breaks it at the same safe point it breaks the outer one.
            .nested_loop_guard(handle.loop_guard());
    }
    // US-011 AC1: the six baseline v2 tools, pointing at a real supervisor and a
    // real spawner. Which models actually SEE them is decided per step from the
    // catalog's `multi_agent_version`, not here.
    builder = builder
        .register(agent_tools::SpawnAgent::new(Arc::clone(&agent_handle)))
        .register(agent_tools::SendMessage::new(Arc::clone(&agent_handle)))
        .register(agent_tools::FollowupTask::new(Arc::clone(&agent_handle)))
        .register(agent_tools::ListAgents::new(Arc::clone(&agent_handle)))
        .register(agent_tools::WaitAgent::new(Arc::clone(&agent_handle)))
        .register(agent_tools::InterruptAgent::new(Arc::clone(&agent_handle)));
    // US-138: what the model sees of its own background jobs. The handle is the
    // one the terminal tools already carry, so the tool reads the registry of
    // whichever thread is open rather than a registry of its own.
    builder = builder.register(agent_tools::ListJobs::new(exec_sessions.job_handle()));
    for tool in mcp_startup.tools {
        builder = builder.register_dyn(tool);
    }
    // US-016: shared between the loop (which dispatches through `ToolDispatch`) and
    // the frontend (which stages the tools of a server connected in session and
    // recomposes the specs at each turn boundary).
    let registry = Arc::new(builder.build());
    // US-026/US-027: behavioral guidelines of the tools. The base system prompt
    // is selected PER SLUG (US-027) when composing, per turn, not frozen: a
    // `/models` must be able to change the template.
    let tool_guidelines = registry.behavioral_guidelines();

    // 5. What the thread runtime needs to run a turn. The session is NOT here:
    // it is the thread log, and `SessionRuntime` is what opens it (EP-005).
    let engine = runtime::EngineDeps {
        provider: Arc::clone(&provider),
        tokenizer: Arc::new(HeuristicCounter),
        clock: Arc::new(SystemClock),
        // The production generator. It is named here, once, rather than inside
        // `SessionRuntime::open`, so a caller can hand a seeded one instead.
        ids: Arc::new(agent_runtime::id::RandomIds),
        tools: Arc::clone(&registry) as Arc<dyn agent_core::tools::ToolDispatch>,
        // Same handle on both sides: the loop publishes its budget into it, the
        // registry hands it to `get_context_remaining`. Two handles here would
        // make the tool report a window that never moves.
        context_window: context_window.clone(),
    };
    let sandbox_scope = sandbox_scope_label(&sandbox.policy, sandbox.enforced);
    // What every turn is captured from, and what the model sees at each step.
    // Shared with both clients: TUI and headless drive the SAME runtime.
    let settings = runtime::SettingsCell::with_provider(
        runtime::TurnSettings {
            model: args.model.clone(),
            reasoning_effort: initial_reasoning_effort.clone(),
            tool_guidelines: tool_guidelines.clone(),
            goal: goal.clone(),
            run_config,
            permission_mode: settings::permission_mode_id(initial_permission_mode).to_string(),
            // Hosted search runs on the backend, so it reaches the network from
            // there: the local allow-list never sees it. Off unless the user
            // asked for it in their GLOBAL settings, which is why the key is a
            // security key.
            web_search: config.web_search,
            sandbox: sandbox_scope.clone(),
            workspace: workspace.clone(),
        },
        Arc::clone(&provider),
    );
    // A child inherits the model and effort in force, read at spawn time.
    subagent_spawner.bind_settings(Arc::clone(&settings));
    // The catalog only stops refusing a code-mode model once a runtime really
    // exists in this process.

    let steps = runtime::CliStepSource::with_code_mode(
        Arc::clone(&registry),
        context::with_environment(
            context_docs,
            &workspace,
            &context::today_utc(),
            &workspace_access,
        ),
        code_mode.clone(),
        Arc::clone(&settings),
    );

    // 6. Headless (-p) vs interactive dispatch. Wrapped so that `SessionEnd`
    // fires on every way out, including the failures each branch bails on.
    let output_last_message = args.output_last_message.take();
    let outcome: anyhow::Result<()> = async {
        if args.app_server {
            // EP-005: the third client. It opens no thread at startup: a client
            // decides which one it drives, and until then this process holds a
            // provider, a registry and a sandbox and nothing else.
            let host = app_server::CliHost::new(app_server::CliHostParts {
                sessions_dir: sessions_dir.clone(),
                engine,
                registry: Arc::clone(&registry),
                settings: Arc::clone(&settings),
                steps: Arc::clone(&steps),
                agents: Some(agents.clone()),
                jobs: Some(jobs.clone()),
                bridge: Arc::clone(&app_bridge),
                cancel: tokio_util::sync::CancellationToken::new(),
            });
            let server = agent_app_server::AppServer::new(
                Arc::new(app_server::SharedHost(host)) as Arc<dyn agent_app_server::RuntimeHost>,
                Arc::clone(&app_bridge),
            );
            let outcome = serve_app_server(server, args.app_server_listen.clone()).await;
            exec_sessions.shutdown().await;
            steps.shutdown_code_mode().await;
            outcome?;
        } else if let Some(prompt) = args.prompt {
            let outcome = headless::run(headless::HeadlessRun {
                agents: Some(agents.clone()),
                jobs: Some(jobs.clone()),
                prompt,
                session_path: (!args.ephemeral).then(|| current_session.clone()),
                session_id: session_id.clone(),
                workspace: workspace.clone(),
                output_format: args.output_format,
                output: Box::new(std::io::stdout()),
                output_last_message,
                hooks: Arc::clone(&hooks),
                skills: &skills,
                registry: Arc::clone(&registry),
                engine,
                settings: Arc::clone(&settings),
                steps: Arc::clone(&steps),
            })
            .await;
            // US-012 AC4: a one-shot run leaves no shell session behind, whatever
            // the outcome (several paths bail from here). Same reasoning for the
            // Code Mode session: a cell must not outlive the run that opened it.
            exec_sessions.shutdown().await;
            steps.shutdown_code_mode().await;
            outcome?;
        } else {
            let cfg = InteractiveConfig {
                agents: Some(agents.clone()),
                jobs: Some(jobs.clone()),
                model: args.model,
                reasoning_effort: initial_reasoning_effort,
                goal,
                truecolor: {
                    // History cells render without access to this state, so the
                    // terminal's colour depth is also recorded process-wide.
                    let truecolor = agent_tui::supports_truecolor();
                    agent_tui::theme::set_truecolor(truecolor);
                    truecolor
                },
                // Reduced motion: spinner degraded to a pulsing dot (US-044).
                reduced_motion: std::env::var_os("NO_COLOR").is_some()
                    || std::env::var_os("PYXIS_REDUCED_MOTION").is_some(),
                // credential loaded above (otherwise we bail) -> connected.
                connected: true,
                skills,
                hooks: Arc::clone(&hooks),
                hook_specs: config.hooks.clone(),
                command_hardener: Arc::clone(&mcp_harden),
                mcp_resources: mcp_resources.clone(),
                mcp_notices: mcp_startup.notices,
                mcp_tool_names: mcp_startup.names,
                registry: Arc::clone(&registry),
                permission_mode,
                approvals,
                settings_path,
                workspace: workspace.clone(),
                sandbox_scope,
                workspace_access,
                exec_sessions: exec_sessions.clone(),
                // US-005 AC2: computed here, where the layers were resolved. The loop
                // only filters out the values it changed in session.
                config_sources: settings::status_sources(&config),
                profile: config.profile.clone(),
                engine,
                settings: Arc::clone(&settings),
                steps: Arc::clone(&steps),
                provider: Arc::clone(&provider),
            };
            let outcome = interactive::run(
                perm_rx,
                hook_notice_rx,
                cfg,
                sessions_dir,
                current_session,
                mcp,
            )
            .await;
            // US-012 AC4: the session ending closes every shell session and kills
            // its process tree, on the error path too. The Code Mode session is
            // closed on the same reasoning: no cell outlives the run.
            exec_sessions.shutdown().await;
            steps.shutdown_code_mode().await;
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
        Args, SandboxPolicy, agent_state_dirs, default_permission_mode, jsonl, open_spill_store,
        parse_args_from, precedence_string, precedence_u64, resolve_permission_mode,
        resolve_prompt, resolve_resume_path, run_config_from_args, sandbox_policy_from_args,
        sandbox_scope_label, settings, validate_ephemeral,
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
            app_server: false,
            app_server_listen: None,
            emit_schemas: None,
            help: false,
        }
    }

    #[test]
    fn tls_builder_has_an_unambiguous_process_provider() {
        super::install_tls_crypto_provider();
        let _ = rustls::ClientConfig::builder();
    }

    /// US-016 AC6: the global configuration also drives the permission mode
    /// of the headless mode. The replacement is REPORTED, because it can widen what
    /// a `-p` allows itself compared to the fail-closed default.
    #[test]
    fn configuration_replaces_the_headless_permission_default_and_says_so() {
        let headless_default = default_permission_mode(true, false);
        assert_eq!(headless_default, PermissionMode::Default);

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
        let interactive = default_permission_mode(false, false);
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
    fn run_config_does_not_infer_prompt_compatibility_from_model_slugs() {
        let mut args = args();
        args.model = "gpt-5-codex".into();
        args.overload_fallback_model = Some("gpt-5.5".into());
        let cfg = run_config_from_args(&args, &mut settings::Config::default()).unwrap();
        assert_eq!(cfg.overload_fallback_model.as_deref(), Some("gpt-5.5"));
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

    /// `--yes` is the ONLY thing that moves the starting mode, and it only does
    /// so in headless: interactive always opens on `ask`. The kernel's sandbox
    /// verdict is deliberately not an input, so the same flags mean the same
    /// thing on a machine where Landlock degraded.
    #[test]
    fn only_headless_yes_moves_the_default_mode() {
        use agent_tools::permission::PermissionMode::{AcceptEdits, Default as Ask};

        assert_eq!(default_permission_mode(true, false), Ask);
        assert_eq!(default_permission_mode(true, true), AcceptEdits);
        assert_eq!(default_permission_mode(false, false), Ask);
        assert_eq!(
            default_permission_mode(false, true),
            Ask,
            "`--yes` is a headless flag: it must not widen an interactive session"
        );
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
            default_permission_mode(true, false),
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

    /// EP-005: the app-server drives threads for a client, so it is neither a
    /// one-shot run nor an ephemeral one, and each contradiction is NAMED
    /// rather than resolved by whichever flag came last.
    #[test]
    fn the_app_server_refuses_the_modes_it_contradicts() {
        let parsed = parse_args_from(argv(&["--app-server"])).expect("bare mode");
        assert!(parsed.app_server);
        assert!(
            parse_args_from(argv(&["app-server"]))
                .expect("bare word")
                .app_server
        );

        for (argv_case, expected) in [
            (vec!["--app-server", "-p", "x"], "-p/--print"),
            (vec!["--app-server", "--ephemeral"], "--ephemeral"),
            (vec!["--app-server", "--resume", "latest"], "--resume"),
            (vec!["--listen", "127.0.0.1:7777"], "--listen requires"),
        ] {
            let err = parse_args_from(argv(&argv_case))
                .expect_err("incoherent combination")
                .to_string();
            assert!(err.contains(expected), "{argv_case:?} -> {err}");
        }
    }

    /// The generated schemas are what the repository holds, and the flag that
    /// writes them takes a destination.
    #[test]
    fn emit_schemas_takes_a_destination() {
        let parsed =
            parse_args_from(argv(&["--emit-schemas", "docs/app-server"])).expect("a destination");
        assert_eq!(parsed.emit_schemas.as_deref(), Some("docs/app-server"));
        assert!(parse_args_from(argv(&["--emit-schemas"])).is_err());
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

    /// Scoped workspace for the spill wiring tests.
    fn spill_workspace() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pyxis-cli-spill-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// US-072 AC1/AC2: the root exists before the sandbox is enforced and joins
    /// the set `writable_dirs` grants, next to the session directory. Proved on
    /// the read-only policy, where the workspace itself grants nothing: the
    /// spill root would be unwritable if the wiring did not carry it.
    #[test]
    fn the_spill_root_is_created_and_granted_to_the_sandbox() {
        let workspace = spill_workspace();
        let sessions = workspace.join(".pyxis").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let store = open_spill_store(&workspace, false).expect("a spill root");

        assert!(store.root().is_dir(), "the root must open before Landlock");
        assert!(
            store
                .root()
                .starts_with(workspace.join(".pyxis").join("spill"))
        );

        let session_dirs: &[&std::path::Path] = &[sessions.as_path()];
        let dirs = agent_state_dirs(session_dirs, Some(store.as_ref()));
        let policy = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let writable = agent_sandbox::fs::writable_dirs(&policy, &dirs);
        assert!(writable.contains(&store.root()), "{writable:?}");
        assert!(writable.contains(&sessions.as_path()), "{writable:?}");

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// US-072 AC6, unhappy path: a root that cannot be created must not stop
    /// the binary. `.pyxis` occupied by a FILE is the cheapest real failure.
    #[test]
    fn a_spill_root_that_cannot_be_created_leaves_the_binary_starting() {
        let workspace = spill_workspace();
        std::fs::write(workspace.join(".pyxis"), "not a directory").unwrap();

        assert!(open_spill_store(&workspace, false).is_none());
        assert!(agent_state_dirs(&[], None).is_empty());

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// US-020 AC4 extended to the spill root: `--ephemeral` promises no trace
    /// left in the workspace, so nothing is created and nothing is granted.
    #[test]
    fn an_ephemeral_run_creates_no_spill_root() {
        let workspace = spill_workspace();
        assert!(open_spill_store(&workspace, true).is_none());
        assert!(!workspace.join(".pyxis").join("spill").exists());

        let _ = std::fs::remove_dir_all(&workspace);
    }
}
