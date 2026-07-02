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
    model: String,
    allow_hosts: Vec<String>,
    yes: bool,
    sandbox: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        prompt: None,
        model: agent_provider::DEFAULT_MODEL.to_string(),
        allow_hosts: Vec::new(),
        yes: false,
        sandbox: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" | "--print" => args.prompt = it.next(),
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
    let session_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let current_session = sessions_dir.join(format!("{session_millis}.jsonl"));
    let jsonl = agent_session::JsonlSession::create_at(&current_session)
        .map_err(|e| anyhow::anyhow!("session : {e}"))?;
    let (shared_session, conversation) = SharedSession::new(jsonl);

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
        let ctx = AgentContext {
            model: args.model,
            system: Some(interactive::compose_system(&base, goal.as_deref())),
            messages: vec![Message::user(prompt)],
            tools: tool_specs,
            config: RunConfig::default(),
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
            run_config: RunConfig::default(),
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
