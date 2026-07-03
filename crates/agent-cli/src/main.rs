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
mod prompt;
mod session;

use std::sync::Arc;

use agent_auth::store;
use agent_core::clock::SystemClock;
use agent_core::guardrail::CostBudget;
use agent_core::message::Message;
use agent_core::provider::Provider;
use agent_core::{AgentContext, Deps, RunConfig};
use agent_provider::{KEYRING_ACCOUNT, OpenAiChatGptProvider};
use agent_sandbox::{ProxyPolicy, set_proxy_env};
use agent_tokenizer::HeuristicCounter;
use agent_tools::permission::{AutoApprove, AutoDeny, PermissionMode};
use agent_tools::{Bash, Edit, Glob, Grep, Read, Registry, Write};

use crate::approver::TuiApprover;
use crate::interactive::InteractiveConfig;
use crate::session::SharedSession;

struct Args {
    prompt: Option<String>,
    resume: Option<String>,
    model: String,
    allow_hosts: Vec<String>,
    yes: bool,
    sandbox: bool,
    token_budget: Option<String>,
    cost_budget_micro_usd: Option<String>,
    input_cost_micro_per_ktok: Option<String>,
    output_cost_micro_per_ktok: Option<String>,
    overload_fallback_model: Option<String>,
}

fn parse_args() -> Args {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(raw: I) -> Args
where
    I: IntoIterator<Item = String>,
{
    let mut args = Args {
        prompt: None,
        resume: None,
        model: agent_provider::DEFAULT_MODEL.to_string(),
        allow_hosts: Vec::new(),
        yes: false,
        sandbox: true,
        token_budget: None,
        cost_budget_micro_usd: None,
        input_cost_micro_per_ktok: None,
        output_cost_micro_per_ktok: None,
        overload_fallback_model: None,
    };
    let mut it = raw.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" | "--print" => args.prompt = it.next(),
            "--resume" => {
                args.resume = match it.peek() {
                    Some(next) if !next.starts_with('-') => it.next(),
                    _ => Some(String::new()),
                };
            }
            "--model" => {
                if let Some(m) = it.next() {
                    args.model = m;
                }
            }
            "--allow" => {
                if let Some(h) = it.next() {
                    args.allow_hosts.push(h);
                }
            }
            "--yes" | "-y" => args.yes = true,
            "--no-sandbox" => args.sandbox = false,
            "--token-budget" => args.token_budget = it.next(),
            "--cost-budget-micro-usd" => args.cost_budget_micro_usd = it.next(),
            "--input-cost-micro-per-ktok" => args.input_cost_micro_per_ktok = it.next(),
            "--output-cost-micro-per-ktok" => args.output_cost_micro_per_ktok = it.next(),
            "--overload-fallback-model" => args.overload_fallback_model = it.next(),
            other => {
                // un argument nu sans -p est traité comme le prompt (mode -p implicite).
                if args.prompt.is_none() && !other.starts_with('-') {
                    args.prompt = Some(other.to_string());
                }
            }
        }
    }
    args
}

fn resolve_resume_path(
    sessions_dir: &std::path::Path,
    arg: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let arg = arg.trim();
    if arg.is_empty() || arg == "latest" {
        let latest = agent_session::list_sessions(sessions_dir, None)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("resume : aucune session disponible"))?;
        return Ok(sessions_dir.join(latest.id));
    }
    crate::interactive::session_path_from_arg(sessions_dir, arg)
        .ok_or_else(|| anyhow::anyhow!("resume : identifiant de session invalide"))
}

fn parse_positive_u64(raw: &str, name: &str) -> anyhow::Result<u64> {
    let value = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{name} doit être un entier positif"))?;
    if value == 0 {
        anyhow::bail!("{name} doit être > 0");
    }
    Ok(value)
}

fn setting_u64(arg: &Option<String>, env: &str, name: &str) -> anyhow::Result<Option<u64>> {
    match arg {
        Some(raw) => parse_positive_u64(raw, name).map(Some),
        None => match std::env::var(env) {
            Ok(raw) if !raw.trim().is_empty() => parse_positive_u64(&raw, env).map(Some),
            _ => Ok(None),
        },
    }
}

fn run_config_from_args(args: &Args) -> anyhow::Result<RunConfig> {
    let token_budget = setting_u64(&args.token_budget, "PYXIS_TOKEN_BUDGET", "--token-budget")?;
    let cost_limit = setting_u64(
        &args.cost_budget_micro_usd,
        "PYXIS_COST_BUDGET_MICRO_USD",
        "--cost-budget-micro-usd",
    )?;
    let input_price = setting_u64(
        &args.input_cost_micro_per_ktok,
        "PYXIS_INPUT_COST_MICRO_PER_KTOK",
        "--input-cost-micro-per-ktok",
    )?;
    let output_price = setting_u64(
        &args.output_cost_micro_per_ktok,
        "PYXIS_OUTPUT_COST_MICRO_PER_KTOK",
        "--output-cost-micro-per-ktok",
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
            "budget coût incomplet : fournir --cost-budget-micro-usd, --input-cost-micro-per-ktok et --output-cost-micro-per-ktok"
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
        });

    Ok(RunConfig {
        token_budget,
        cost_budget,
        overload_fallback_model,
        ..RunConfig::default()
    })
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();
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

    // Sandbox FS AVANT le runtime (thread principal → hérité par les workers).
    if args.sandbox {
        match agent_sandbox::enforce_process(&workspace) {
            Ok(status) => {
                if let Some(w) = status.warning() {
                    eprintln!("[sandbox] {w}");
                }
            }
            Err(e) => eprintln!("[sandbox] échec d'application : {e} — écritures non confinées"),
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(args, workspace, skills, mcp_config, context_msgs))
}

/// Découvre les serveurs MCP avant le sandbox : `<workspace>/.mcp.json` (priorité
/// haute) fusionné sous les `mcpServers` user-scope de `~/.claude.json` (réutilise
/// les serveurs déjà installés pour Claude Code). Best-effort : un fichier
/// illisible ou invalide est signalé puis ignoré.
fn read_mcp_config(workspace: &std::path::Path) -> agent_mcp::McpConfigFile {
    let workspace_cfg = agent_mcp::McpConfigFile::load(workspace).unwrap_or_else(|e| {
        eprintln!("[mcp] {e}");
        agent_mcp::McpConfigFile::default()
    });
    let claude_cfg = std::env::var_os("HOME")
        .map(|home| {
            let path = std::path::Path::new(&home).join(".claude.json");
            agent_mcp::McpConfigFile::load_claude(&path).unwrap_or_else(|e| {
                eprintln!("[mcp] ~/.claude.json : {e}");
                agent_mcp::McpConfigFile::default()
            })
        })
        .unwrap_or_default();
    workspace_cfg.merge_under(claude_cfg)
}

/// Liste les skills disponibles dans `~/.agents/skills` (un dossier = un skill,
/// nom = nom du dossier), triés. Symlink partagé entre CLIs ; lecture best-effort.
fn read_skills() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = std::path::Path::new(&home).join(".agents").join("skills");
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

async fn run(
    args: Args,
    workspace: std::path::PathBuf,
    skills: Vec<String>,
    mcp_config: agent_mcp::McpConfigFile,
    context_msgs: Vec<Message>,
) -> anyhow::Result<()> {
    let run_config = run_config_from_args(&args)?;
    // 1. Credential abonnement ChatGPT (keyring). Absente → on guide vers le login.
    let cred = match store::load(KEYRING_ACCOUNT)? {
        Some(agent_auth::Credential::Oauth(o)) => o,
        _ => {
            anyhow::bail!(
                "Pas de credential ChatGPT. Connecte-toi d'abord :\n  \
                 cargo run -p agent-auth --example login"
            );
        }
    };
    let mut chatgpt = OpenAiChatGptProvider::new(
        cred,
        agent_provider::DEFAULT_MAX_CONTEXT,
        Some(agent_provider::DEFAULT_REASONING_EFFORT.to_string()),
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
    let provider: Arc<dyn Provider> = Arc::new(chatgpt);

    // 2. Proxy réseau allow-list (fail-closed). Durcit les commandes Bash.
    let proxy = agent_sandbox::spawn_proxy(ProxyPolicy::new(args.allow_hosts.clone())).await?;
    let proxy_addr = proxy.addr.clone();
    let harden: agent_tools::CommandHardener =
        Arc::new(move |cmd: &mut tokio::process::Command| set_proxy_env(cmd, &proxy_addr));

    // 3. Session persistante : un fichier JSONL par conversation (horodaté) sous
    // <workspace>/.pyxis/sessions/, listable/reprenable via `/resume`.
    let sessions_dir = workspace.join(".pyxis").join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let (current_session, initial_messages) = if let Some(resume_arg) = &args.resume {
        let path = resolve_resume_path(&sessions_dir, resume_arg)?;
        let resumed =
            agent_session::resume_file(&path).map_err(|e| anyhow::anyhow!("resume : {e}"))?;
        (path, resumed.messages)
    } else {
        (interactive::new_session_path(&sessions_dir), Vec::new())
    };
    let jsonl = agent_session::JsonlSession::create_at(&current_session)
        .map_err(|e| anyhow::anyhow!("session : {e}"))?;
    let (shared_session, conversation) = SharedSession::new(jsonl);
    if !initial_messages.is_empty() {
        *conversation
            .lock()
            .map_err(|_| anyhow::anyhow!("session : snapshot empoisonné"))? = initial_messages;
    }

    // Objectif de session persistant (`/goal`) : chargé du sidecar `.pyxis/goal`
    // (survit au redémarrage), composé dans le system prompt à chaque tour.
    let goal = std::fs::read_to_string(workspace.join(".pyxis").join("goal"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // 4. Registry d'outils + approbateur (TUI en interactif, auto en headless).
    let headless = args.prompt.is_some();
    let (perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (mode, approver): (PermissionMode, Arc<dyn agent_tools::permission::Approver>) = if headless
    {
        // -p : pas d'interlocuteur. --yes auto-accepte ; sinon refuse le sensible.
        let appr: Arc<dyn agent_tools::permission::Approver> = if args.yes {
            Arc::new(AutoApprove)
        } else {
            Arc::new(AutoDeny)
        };
        (PermissionMode::AcceptEdits, appr)
    } else {
        (PermissionMode::Default, Arc::new(TuiApprover::new(perm_tx)))
    };

    let registry = Registry::builder(&workspace)
        .mode(mode)
        .approver(approver)
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
            system: Some(interactive::compose_system(&base, goal.as_deref())),
            messages,
            tools: tool_specs,
            config: run_config,
            context_messages: context_msgs,
        };
        let result = agent_core::run_headless(ctx, deps).await;
        match result.ended {
            agent_core::HeadlessEnd::Error(e) => anyhow::bail!("{e}"),
            agent_core::HeadlessEnd::Exhausted(reason) => anyhow::bail!("arrêt: {reason:?}"),
            agent_core::HeadlessEnd::EndTurn => {}
        }
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
    } else {
        // Registre MCP construit depuis la config découverte avant le sandbox
        // (workspace + ~/.claude.json). Tous les serveurs démarrent déconnectés ;
        // la connexion se fait à la demande via `/mcp`.
        let mcp = Arc::new(std::sync::Mutex::new(agent_mcp::McpRegistry::from_config(
            mcp_config,
        )));

        let cfg = InteractiveConfig {
            model: args.model,
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
    use super::{Args, parse_args_from, run_config_from_args};

    fn args() -> Args {
        Args {
            model: "mock".into(),
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
        }
    }

    #[test]
    fn run_config_reads_token_budget_flag() {
        let mut args = args();
        args.token_budget = Some("1234".into());
        let cfg = run_config_from_args(&args).unwrap();
        assert_eq!(cfg.token_budget, Some(1234));
    }

    #[test]
    fn run_config_reads_complete_cost_budget() {
        let mut args = args();
        args.cost_budget_micro_usd = Some("10".into());
        args.input_cost_micro_per_ktok = Some("2".into());
        args.output_cost_micro_per_ktok = Some("4".into());
        let cfg = run_config_from_args(&args).unwrap();
        let cost = cfg.cost_budget.unwrap();
        assert_eq!(cost.limit_micro_usd, 10);
        assert_eq!(cost.input_micro_per_ktok, 2);
        assert_eq!(cost.output_micro_per_ktok, 4);
    }

    #[test]
    fn run_config_rejects_incomplete_cost_budget() {
        let mut args = args();
        args.cost_budget_micro_usd = Some("10".into());
        let err = run_config_from_args(&args).unwrap_err().to_string();
        assert!(err.contains("budget coût incomplet"));
    }

    #[test]
    fn run_config_rejects_zero_budget() {
        let mut args = args();
        args.token_budget = Some("0".into());
        let err = run_config_from_args(&args).unwrap_err().to_string();
        assert!(err.contains("doit être > 0"));
    }

    #[test]
    fn run_config_reads_overload_fallback_model() {
        let mut args = args();
        args.overload_fallback_model = Some(" fallback ".into());
        let cfg = run_config_from_args(&args).unwrap();
        assert_eq!(cfg.overload_fallback_model.as_deref(), Some("fallback"));
    }

    #[test]
    fn parse_args_reads_resume_latest() {
        let args = parse_args_from(vec!["--resume".to_string()]);
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
        ]);
        assert_eq!(args.resume.as_deref(), Some("123.jsonl"));
        assert_eq!(args.prompt.as_deref(), Some("continue"));
    }

    #[test]
    fn parse_args_resume_without_id_does_not_swallow_next_flag() {
        let args = parse_args_from(vec![
            "--resume".to_string(),
            "-p".to_string(),
            "continue".to_string(),
        ]);
        assert_eq!(args.resume.as_deref(), Some(""));
        assert_eq!(args.prompt.as_deref(), Some("continue"));
    }
}
