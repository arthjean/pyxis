//! Aperçu du redesign de la zone de saisie (US-019, itération UI) :
//! `cargo run -p agent-tui --example input`. Rend le composer + status line dans
//! plusieurs états via `TestBackend`, sans terminal réel, pour eyeball
//! l'esthétique avant de la voir en live.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_core::AgentEvent;
use agent_tui::{AppState, Block, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn dump(state: &AppState, w: u16, h: u16, label: &str) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    println!(
        "\n── {label} {}",
        "─".repeat((w as usize).saturating_sub(label.len() + 4))
    );
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}

fn base() -> AppState {
    let mut s = AppState::new("gpt-5.5", true);
    s.workspace = "pyxis".into();
    s.provider_connected = true;
    s.reasoning_effort = Some("medium".into());
    s.skills = vec![
        "frontend-design".into(),
        "meta-code".into(),
        "ui-ux-pro-max".into(),
        "impeccable".into(),
    ];
    s
}

fn main() {
    let w = 84;

    // 1. Au repos, prompt vide (état de démarrage).
    let mut a = base();
    a.blocks.push(Block::Notice(
        "Pyxis — tape ta demande, ⌃C pour quitter".into(),
    ));
    a.context_pct = Some(15);
    dump(&a, w, 8, "repos · prompt vide");

    // 2. En cours de saisie.
    let mut b = base();
    b.input = "revois l'UI, commençons par l'input avec une status line en dessous".into();
    b.context_pct = Some(38);
    dump(&b, w, 8, "saisie en cours");

    // 3. L'agent réfléchit : une ligne Codex-like s'affiche au-dessus.
    let mut c = base();
    c.push_user("refactore le parsing dans src/lexer.rs");
    c.apply(&AgentEvent::Reasoning("je lis le fichier d'abord".into()));
    c.apply(&AgentEvent::Text(
        "J'ouvre `src/lexer.rs` pour repérer le `todo!()`.".into(),
    ));
    c.context_pct = Some(64);
    dump(&c, w, 10, "agent en réflexion · status au-dessus");

    // 4. Menu de commandes slash (saisie « / », 2e ligne sélectionnée).
    let mut d = base();
    d.context_pct = Some(15);
    d.input = "/".into();
    d.completion_index = 1;
    dump(&d, w, 12, "menu slash (popup commandes)");

    // 5. Sous-menu de sélection de modèle (« /models », gpt-5.4 sélectionné).
    let mut e = base();
    e.context_pct = Some(15);
    e.input = "/models ".into();
    e.completion_index = 1;
    dump(&e, w, 12, "sous-menu /models");

    // 6. Sous-menu de sélection d'effort.
    let mut e2 = base();
    e2.context_pct = Some(15);
    e2.input = "/effort ".into();
    e2.completion_index = 3;
    dump(&e2, w, 12, "sous-menu /effort");

    // 7. Sous-menu /resume : conversations passées du workspace.
    let mut f = base();
    f.context_pct = Some(15);
    f.input = "/resume ".into();
    f.sessions = vec![
        agent_tui::SessionMeta {
            id: "1718.jsonl".into(),
            label: "Explique le projet stp.".into(),
            hint: "14 msg · il y a 12 min".into(),
        },
        agent_tui::SessionMeta {
            id: "1717.jsonl".into(),
            label: "Refactore le parsing dans src/lexer.rs".into(),
            hint: "31 msg · il y a 2 h".into(),
        },
        agent_tui::SessionMeta {
            id: "1716.jsonl".into(),
            label: "Mets en place le sandbox Landlock".into(),
            hint: "52 msg · il y a 1 j".into(),
        },
    ];
    dump(&f, w, 12, "sous-menu /resume (conversations passées)");

    // 8. /providers niveau 1 : type d'authentification.
    let mut g = base();
    g.context_pct = Some(15);
    g.input = "/providers ".into();
    dump(&g, w, 9, "/providers — niveau 1 (auth)");

    // 9. /providers niveau 2 : choix du fournisseur (badge connecté sur Codex).
    let mut h = base();
    h.context_pct = Some(15);
    h.input = "/providers subscription ".into();
    dump(&h, w, 9, "/providers subscription — niveau 2 (badge)");

    // 10. /providers niveau 3 : actions (connecté → Connect grisé, Disconnect actif).
    let mut k = base();
    k.context_pct = Some(15);
    k.input = "/providers subscription codex ".into();
    dump(
        &k,
        w,
        9,
        "/providers subscription codex — niveau 3 (actions)",
    );

    // 11. Sous-menu /skills (liste des skills du dossier ~/.agents/skills).
    let mut l = base();
    l.context_pct = Some(15);
    l.input = "/skills ".into();
    dump(&l, w, 9, "/skills (sous-menu des skills)");

    // 12. Skill inséré dans le message → `/frontend-design` en surbrillance.
    let mut m = base();
    m.context_pct = Some(15);
    m.input = "/frontend-design refais l'input".into();
    m.cursor = m.input.len();
    dump(&m, w, 8, "skill inséré (surbrillance en vrai terminal)");

    // 13. Commande /goal → le `/goal` est surligné (comme un skill).
    let mut n = base();
    n.context_pct = Some(15);
    n.input = "/goal vivre de mes produits".into();
    n.cursor = n.input.len();
    dump(&n, w, 8, "/goal (commande surlignée)");
}
