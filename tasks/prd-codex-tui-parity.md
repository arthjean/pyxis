[PRD]
# PRD: Pyxis - Parite TUI Codex CLI

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-03 | Arthur Jean | Initial draft pour porter le systeme TUI Codex CLI dans Pyxis, avec conservation du nom Pyxis et des invariants headless. |

## Problem Statement

Pyxis a deja livre un coeur d'agent Rust solide et un TUI riche sur plusieurs surfaces, mais son architecture d'affichage reste plus simple que celle de Codex CLI. La demande produit a change: il ne s'agit plus seulement de copier le rendu des reponses, mais de reprendre le systeme TUI complet de Codex comme base robuste, puis d'innover dessus.

1. **Le transcript Pyxis reste un fil de `Block` plat.** `crates/agent-tui/src/state.rs` accumule des blocs typés, puis `render.rs` recompose la frame entiere dans Ratatui. Codex separe au contraire des cellules finalisees (`HistoryCell`), une cellule active mutable, une queue de stream, et une insertion durable dans le scrollback terminal. Sans ce modele, Pyxis peut imiter le style, mais pas le comportement.
2. **Le streaming Pyxis n'a pas le controle fin Codex.** Codex utilise `StreamController` pour separer prefixe stable et tail mutable, retenir les tables markdown incompletes, consolider ensuite en `AgentMarkdownCell`, et re-render proprement au resize. Pyxis stream actuellement dans le dernier bloc assistant.
3. **Les outils et approvals ne partagent pas encore le meme cycle UI que Codex.** Pyxis affiche deja outils, diffs et permissions, mais Codex possede des cellules specialisees (`ExecCell`, `McpToolCallCell`, `PatchHistoryCell`, `ApprovalOverlay`) reliees au cycle `ItemStarted` / `ItemCompleted`.
4. **Le terminal Pyxis utilise encore l'alt-screen classique.** Codex maintient une zone inline redimensionnable et insere les cellules finalisees au-dessus du viewport avec des sequences terminal, ce qui donne un scrollback natif plus robuste pour les sessions longues.
5. **Le bottom pane Codex est un systeme de vues empilees.** Composer, popups, approvals, list selection, footer, previews et status indicators partagent un contrat `BottomPaneView`. Pyxis a de bonnes briques, mais pas encore cette orchestration complete.

**Why now:** l'audit profond de `C:\dev\codex` a isole les points de greffe, les PRD anterieurs `prd-codex-orchestration` et `prd-response-rendering` sont marques DONE, et `docs/CURRENT_STATUS.md` confirme que le coeur headless, la TUI, les sessions, le resume, les permissions, MCP config et les outils existent deja. Le prochain saut de qualite n'est plus un patch visuel, c'est une migration de modele TUI.

## Overview

Ce PRD formalise une migration de Pyxis vers une parite TUI quasi egale a Codex CLI, avec une exception explicite: le nom public, les textes de marque et la commande restent `pyxis`. Le but est d'utiliser Codex comme base d'ergonomie eprouvee, pas de diluer l'identite du projet.

La solution porte les concepts structurants de Codex dans `agent-tui`: `HistoryCell`, `transcript_cells`, `active_cell`, `StreamController`, consolidation markdown finale, cellules outils specialisees, bottom pane a vues empilees, list selection, approval overlay, footer/status, et boucle terminal avec scrollback inline. Le coeur `agent-core` reste headless et n'emet jamais d'ANSI. Toute information supplementaire necessaire a la TUI passe par des evenements structures, additifs et testables.

L'ordre d'implementation est important. On commence par la compatibilite licence et l'inventaire de portage, puis le contrat d'evenements UI, puis le moteur transcript/scrollback, puis le streaming/rendering, puis les outils/approvals, et enfin le bottom pane/composer. Reprendre le composer Codex est inclus dans le scope, car la demande finale est une parite quasi totale. Si une difference Pyxis est conservee, elle doit etre documentee comme divergence produit explicite.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Parite transcript Codex | 100 % des tours user, agent, reasoning, exec, tool, diff et permission passent par des cellules `HistoryCell` | 0 regression sur resume et sessions > 500 cellules |
| Robustesse streaming | 100 % des deltas assistant passent par un controller stable-prefix/tail | Tables markdown incompletes sans flicker sur 50 sessions dogfood |
| Scrollback terminal | Zone inline + insertion de cellules finalisees disponible derriere feature flag | Mode inline scrollback actif par defaut sur Linux et Windows Terminal |
| Parite bottom pane | Composer, view stack, list selection, approvals, footer/status portes a 90 % des comportements Codex ciblés | 95 % de snapshots de parite sur flows critiques |
| Performance TUI | P95 frame < 16 ms sur 500 cellules, CPU idle < 5 % | P95 frame < 16 ms sur 1000 cellules |

## Target Users

### Arthur Jean, createur et dogfooder principal
- **Role:** Solo indie maker, mainteneur Pyxis et Paneflow, utilisateur quotidien de Codex.
- **Behaviors:** Sessions longues d'audit, refactor, edition, permissions et outils dans un terminal.
- **Pain points:** Une TUI moins solide que Codex force la bascule vers Codex CLI/App, meme si le coeur Pyxis est bon.
- **Current workaround:** Utiliser Codex directement pour beneficier de son transcript, de ses approvals et de son scrollback.
- **Success looks like:** Pyxis donne la meme sensation de solidite que Codex CLI, avec le nom et la trajectoire produit Pyxis.

### Developpeur Rust / systemes, early adopter OSS
- **Role:** Utilisateur qui juge Pyxis sur une session terminal reelle.
- **Behaviors:** Lance des commandes, accepte/refuse des permissions, lit diffs, sorties et markdown.
- **Pain points:** Les agents CLI semblent fragiles quand le scroll, le resize, les outils ou le streaming se comportent mal.
- **Current workaround:** Retour au Codex CLI officiel.
- **Success looks like:** Le transcript Pyxis tient les sessions longues et les approvals sans surprise.

### Futur client Paneflow
- **Role:** UI GPU ou desktop qui consommera le meme coeur d'evenements.
- **Behaviors:** Veut rendre les memes items que la TUI, mais dans une surface GPUI.
- **Pain points:** Si les decisions de rendu restent implicites dans un `Block` plat, Paneflow devra recoder la logique.
- **Current workaround:** Aucun, integration future.
- **Success looks like:** Un contrat d'items transcript clair que Paneflow peut reutiliser.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- **Codex CLI local (`C:\dev\codex`)**: reference directe. Le pipeline `ResponseEvent -> EventMsg -> ServerNotification -> ThreadItem` separe runtime et clients. La TUI consomme `ServerNotification`, rend des `HistoryCell`, stream via `StreamController`, consolide en `AgentMarkdownCell`, et insere les cellules finalisees dans le scrollback.
- **Codex TUI bottom pane**: `BottomPaneView`, `ApprovalOverlay`, `ListSelectionView`, `ChatComposer`, `TextArea`, footer et status partagent un modele empile. La parite visuelle depend de cet etat, pas seulement des styles.
- **Ratatui**: la documentation de Ratatui decrit un modele immediate rendering avec `Terminal::draw`, `Frame`, `Paragraph`, `Layout` et wrapping. Cela confirme que le scrollback durable doit etre gere explicitement au-dessus de Ratatui, comme Codex le fait.
- **Crossterm**: fournit events, paste, alternate screen et primitives terminal. Pyxis l'utilise deja, mais Codex va plus loin avec scroll regions et viewport inline.
- **pulldown-cmark et Syntect**: les choix actuels Pyxis restent compatibles avec la parite markdown/highlight. pulldown-cmark expose les options tables, footnotes et task lists ; Syntect cible le highlight Rust avec definitions Sublime.
- **Market gap:** une base TUI de qualite Codex dans un projet Rust GPL, headless, multi-provider et embarquable dans Paneflow.

### Best Practices Applied
- Porter les contrats d'etat avant les styles: cellules, lifecycle, consolidation, puis rendu.
- Garder `agent-core` sans ANSI et sans dependance TUI.
- Feature flag pour la migration scrollback: l'ancien renderer reste disponible jusqu'a parite snapshot.
- Sauvegarder les obligations Apache-2.0 si du code Codex est copie ou derive.
- Snapshot tests et tests terminal pour resize, paste, streaming, tables, diffs, approvals et permissions.

Sources utilisees:
- [Ratatui docs](https://docs.rs/ratatui/latest/ratatui/)
- [Crossterm docs](https://docs.rs/crossterm/)
- [pulldown-cmark docs](https://docs.rs/pulldown-cmark/latest/pulldown_cmark/)
- [Syntect repository](https://github.com/trishume/syntect/)
- `C:\dev\codex\codex-rs\tui\src\chatwidget.rs`
- `C:\dev\codex\codex-rs\tui\src\streaming\controller.rs`
- `C:\dev\codex\codex-rs\tui\src\history_cell\messages.rs`
- `C:\dev\codex\codex-rs\tui\src\bottom_pane\mod.rs`
- `C:\dev\codex\codex-rs\tui\src\tui.rs`

## Assumptions & Constraints

### Assumptions (to validate)
- Apache-2.0 code from Codex can be incorporated into GPL-3.0-or-later Pyxis if notices and license obligations are preserved.
- The Codex inline scrollback model works acceptably on Windows Terminal, not only on Unix-like terminals.
- Pyxis can introduce richer `AgentEvent` variants without breaking headless `-p` or session JSONL compatibility.
- The current markdown/diff/highlight modules can be adapted instead of replaced wholesale.
- Full composer parity is desirable even though Pyxis currently has a composer Arthur liked earlier. This PRD follows the latest request: quasi total parity.

### Hard Constraints
- Public command and product name remain `pyxis`.
- `agent-core` must remain headless: no Ratatui, no Crossterm, no ANSI.
- Existing sessions must resume. Any event schema migration must be backward-compatible.
- Existing dirty worktree changes must not be reverted by implementation agents.
- GPL-3.0-or-later remains the workspace license. Any copied Codex source requires Apache-2.0 preservation.
- No Codex attribution in commits, PR titles or generated public comments. License/notice files are allowed and required when copying source.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all --check` - formatage Rust.
- `cargo check --workspace --all-targets` - compilation de tout le workspace.
- `cargo clippy --workspace --all-targets` - lints workspace, avec revue manuelle des warnings `unwrap` et `expect`.
- `cargo test --workspace` - suite complete.
- `git diff --check` - aucun whitespace conflictuel.

For UI stories, additional gates:
- Run focused snapshot tests for `agent-tui`.
- Manual terminal verification on wide, narrow and resized terminal.
- Manual verification with truecolor enabled and with `COLORTERM` absent.
- For scrollback stories, verify at least one Windows Terminal run or document the exact blocker.

## Epics & User Stories

### EP-001: Cadre de portage, licence et contrat UI

Creer le socle qui permet de porter Codex proprement: obligations de licence, inventaire de sources, modules cibles, feature flag et contrat d'evenements transcript.

**Definition of Done:** Les obligations Apache-2.0 sont documentees, les modules Codex cibles sont inventories, un feature flag de migration existe, et Pyxis possede un contrat `TranscriptItem` / lifecycle utilisable par la nouvelle TUI sans casser le mode headless.

#### US-001: Inventaire de portage Codex et obligations Apache-2.0
**Description:** As a mainteneur Pyxis, I want inventorier les fichiers Codex repris ou derives et enregistrer les obligations de licence, so that le portage reste legalement propre sous GPL-3.0-or-later.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given le repo Codex local, when l'inventaire est produit, then chaque module cible est classe `copy`, `adapt`, `inspired`, ou `skip`.
- [ ] Given un fichier classe `copy` ou `adapt`, when il est porte, then un fichier `NOTICE-CODEX.md` ou equivalent mentionne la provenance Apache-2.0.
- [ ] Given la licence Pyxis GPL-3.0-or-later, when le portage est documente, then la compatibilite Apache-2.0 -> GPLv3 et les obligations de preservation sont explicites.
- [ ] Given une source Codex sans provenance claire (unhappy path), when l'implementation arrive dessus, then elle est marquee `skip` jusqu'a clarification.

#### US-002: Feature flag de migration TUI et scaffold de modules
**Description:** As a mainteneur Pyxis, I want introduire un flag de migration et les modules cibles du nouveau TUI, so that l'ancien renderer reste disponible jusqu'a parite.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given le crate `agent-tui`, when le flag `codex_tui_parity` est desactive, then le renderer actuel compile et fonctionne sans changement.
- [ ] Given le flag active, when le projet compile, then les modules scaffoldes existent: `history_cell`, `streaming`, `bottom_pane`, `insert_history`, `terminal_viewport`, `app_event`.
- [ ] Given un build sans le flag (unhappy path de rollback), when `cargo test --workspace` tourne, then aucune dependance nouvelle non utilisee ne casse le build.
- [ ] Given les modules scaffoldes, when un implementateur lit le code, then chaque module a une responsabilite documentee en tete de fichier.

#### US-003: Contrat `TranscriptItem` et lifecycle UI
**Description:** As a client TUI, I want un contrat intermediaire entre `AgentEvent` et `HistoryCell`, so that Pyxis puisse representer `ItemStarted`, deltas, completions, exec, tools, diffs et approvals sans mettre le rendu dans le core.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un `AgentEvent` existant, when il arrive dans la TUI, then il est mappe vers un `TranscriptItem` stable avec id optionnel, role, kind, status et payload structure.
- [ ] Given un tool call, when il demarre et se termine, then le mapping produit un lifecycle start/delta/end exploitable par des cellules actives.
- [ ] Given un ancien event sans id (unhappy path), when il est mappe, then un id local stable est derive pour la session courante sans polluer `agent-core`.
- [ ] Given le mode headless `-p`, when les nouveaux types UI existent, then `agent-core` ne depend toujours pas de `agent-tui`.

### EP-002: Moteur transcript, cellules et scrollback terminal

Porter le coeur de l'experience Codex: cellules finalisees, cellule active mutable, overlay transcript, insertion scrollback et reflow au resize.

**Definition of Done:** Pyxis peut rendre une session via `HistoryCell`, inserer les cellules finalisees dans le scrollback, maintenir une zone inline redimensionnable, et reconstruire le transcript au resume.

#### US-004: Trait `HistoryCell` et cellules de base
**Description:** As a moteur de transcript, I want un trait `HistoryCell` et des cellules de base, so that chaque surface user, agent, reasoning, notice, error et composite puisse declarer ses lignes, sa hauteur et son rendu.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-003

**Acceptance Criteria:**
- [ ] Given une cellule, when `display_lines(width)` est appele, then elle retourne des `Line` Ratatui sans I/O.
- [ ] Given une cellule, when `desired_height(width)` est appele, then la hauteur correspond au wrapping effectif a cette largeur.
- [ ] Given des cellules user, agent markdown, reasoning, notice et composite, when elles sont rendues en snapshot, then elles correspondent aux prefixes et espacements Codex cibles.
- [ ] Given une largeur terminal inferieure a 8 colonnes (unhappy path), when une cellule est mesuree, then elle ne panic pas et degrade vers au moins 1 colonne utile.

#### US-005: Viewport inline et insertion du scrollback
**Description:** As a user terminal, I want que les cellules finalisees soient poussees dans le scrollback au-dessus de la zone active, so that les longues sessions se comportent comme Codex CLI au lieu de tout vivre dans l'alt-screen.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Given une cellule finalisee, when elle est inseree, then ses lignes sont ecrites au-dessus du viewport actif et le prompt reste visible.
- [ ] Given un resize, when la hauteur active change, then le viewport est recalcule et les lignes finalisees ne se dupliquent pas.
- [ ] Given une session dans Windows Terminal, when l'insertion est testee, then soit elle fonctionne, soit le fallback alt-screen est documente et active automatiquement.
- [ ] Given une erreur d'ecriture terminal (unhappy path), when l'insertion echoue, then Pyxis revient au renderer legacy pour la session et affiche une notice non bloquante.

#### US-006: `ChatSurface` avec `transcript_cells` et `active_cell`
**Description:** As a TUI, I want separer cellules finalisees et cellule active mutable, so that tools, exec, streaming et approvals puissent muter sans re-ecrire tout le transcript.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004, US-005

**Acceptance Criteria:**
- [ ] Given une cellule active, when elle termine, then elle est flush en `transcript_cells` puis inseree dans le scrollback.
- [ ] Given une cellule active outil, when une sortie delta arrive, then seule cette cellule change et la revision active augmente.
- [ ] Given une finalisation pendant un resize (unhappy path), when l'evenement arrive, then l'ordre finalisees/active reste stable et aucun doublon n'apparait.
- [ ] Given le renderer legacy desactive par flag, when la nouvelle surface rend une conversation simple, then user -> agent -> tool -> agent apparait dans le bon ordre.

#### US-007: Replay et resume vers cellules
**Description:** As a user resuming a session, I want reconstruire les `HistoryCell` depuis les messages persistants, so that une session reprise ait le meme rendu qu'une session live.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given une session JSONL existante, when `/resume` la charge, then user, assistant, reasoning, tool call et tool result deviennent des cellules.
- [ ] Given une session creee avant ce PRD, when elle est reprise, then le mapping legacy ne panic pas et affiche les informations disponibles.
- [ ] Given une session avec tool result orphelin (unhappy path), when elle est reprise, then une cellule generique l'affiche sans casser le reste du transcript.
- [ ] Given un replay initial, when la TUI dessine, then les cellules ne sont pas re-inserees deux fois dans le scrollback.

### EP-003: Streaming, markdown, diffs et cellules message

Porter le controle fin des deltas et consolider le rendu final: user cells, agent streaming tail, markdown final, reasoning, code blocks, tables et diffs.

**Definition of Done:** Les deltas assistant passent par un controller stable/tail, les tables incompletes sont retenues, les messages finaux sont consolides en markdown, et les diffs/fichiers gardent la qualite deja acquise par Pyxis.

#### US-008: `StreamController` stable prefix + tail mutable
**Description:** As a user reading streaming output, I want que le contenu stable soit committe progressivement et que le tail reste mutable, so that le streaming soit fluide sans flicker ni tables cassees.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given un delta assistant, when il contient des lignes terminees par newline, then le prefixe stable est rendu comme cellule stable ou segment stable.
- [ ] Given une table markdown incomplete, when elle streame, then le controller la retient dans le tail jusqu'a confirmation ou finalisation.
- [ ] Given un resize pendant le stream, when la largeur change, then le controller re-render le source brut avec la nouvelle largeur.
- [ ] Given un stream abandonne par `StreamReset` (unhappy path), when le reset arrive, then le tail mutable est retire et aucun contenu non finalise ne reste dans le transcript.

#### US-009: Cellules user, agent markdown, tail streaming et reasoning
**Description:** As a user, I want les cellules user/agent/reasoning de Codex, so that le fil de conversation ait la meme hierarchie visuelle et le meme comportement final/live.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004, US-008

**Acceptance Criteria:**
- [ ] Given un message utilisateur, when il est rendu, then il utilise le prefixe `› `, les text elements et les images locales/remote disponibles.
- [ ] Given un message assistant final, when il est consolide, then il devient une `AgentMarkdownCell` re-rendue au resize depuis le markdown source.
- [ ] Given un tail streaming, when il est actif, then il est marque comme continuation et ne force pas une consolidation prematuree.
- [ ] Given un reasoning vide ou malforme (unhappy path), when il est rendu, then il degrade en cellule discrète sans panic.

#### US-010: Markdown renderer compatible Codex
**Description:** As a reader, I want un rendu markdown Codex-compatible, so that listes, tables, blockquotes, code blocks et liens soient lisibles dans la TUI.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given un markdown avec paragraphes, listes, headings, blockquotes, code fences, liens et tables, when rendu, then chaque tag a un rendu defini.
- [ ] Given un fence `markdown` contenant une table, when rendu, then il est unwrappe uniquement dans les cas conservateurs documentes.
- [ ] Given un langage connu, when un code block est rendu, then la coloration Syntaxe utilise le moteur existant et le cache.
- [ ] Given un markdown incomplet en stream (unhappy path), when rendu, then aucun panic et aucun overlap ne se produit.

#### US-011: Cellules diff, file change et patch
**Description:** As a user reviewing edits, I want des cellules diff/file change proches de Codex, so that les modifications agent soient visibles comme des evenements de transcript, pas seulement comme resume de tool.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004, US-006

**Acceptance Criteria:**
- [ ] Given un edit ou write reussi, when la TUI recoit le resultat, then une cellule file change peut afficher Added/Deleted/Edited avec path, compte et diff.
- [ ] Given un patch multi-fichier, when rendu, then chaque fichier a un header stable et les hunks restent lisibles.
- [ ] Given un diff volumineux (unhappy path), when rendu, then il est tronque avec compteur de lignes masquees, sans bloquer la frame > 16 ms p95.
- [ ] Given une erreur apply patch, when rendue, then une cellule failure explicite apparait dans le transcript.

### EP-004: Outils, exec, approvals et feedback runtime

Porter les cellules operationnelles qui donnent a Codex son feedback de travail: commandes, outputs, MCP/tools, approvals, permissions, status et pending usage.

**Definition of Done:** Les commandes shell, outils Pyxis, permissions, approvals, status indicators et previews ont des cellules actives/finalisees equivalentes aux flows Codex cibles.

#### US-012: `ExecCell` et lifecycle des commandes
**Description:** As a user watching commands, I want une cellule exec qui groupe commandes, output delta, completion et sortie tronquee, so that les runs shell soient lisibles et non bruyants.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given une commande bash utilisateur ou agent, when elle demarre, then une `ExecCell` active affiche la commande nettoyee.
- [ ] Given des deltas stdout/stderr, when ils arrivent, then ils sont append a la cellule active avec troncature head/tail compatible Codex.
- [ ] Given plusieurs commandes exploratoires consecutives, when elles sont detectees read/list/search, then elles peuvent etre groupees comme Codex.
- [ ] Given une commande terminee sans cellule active (unhappy path), when completion arrive, then une cellule orpheline complete est creee sans panic.

#### US-013: Cellules outils Pyxis et MCP-ready
**Description:** As a user watching tools, I want des cellules outils specialisees pour Pyxis et pretes pour MCP, so that les tools aient des statuts, details et outputs supplementaires coherents.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given un tool call Pyxis, when il demarre, then une cellule active affiche tool, cible et statut `Calling`.
- [ ] Given un tool result, when il termine, then la cellule passe en `Called` avec resume et detail bornes.
- [ ] Given MCP tools non encore exposes dans l'agent loop, when le renderer reçoit un item MCP futur, then le type existe mais reste inactif derriere feature flag.
- [ ] Given un output image ou binaire non supporte (unhappy path), when il est rendu, then une cellule notice indique le type non affiche sans casser le transcript.

#### US-014: `ApprovalOverlay` et decisions de permission
**Description:** As a user approving risky operations, I want une overlay d'approbation equivalente a Codex, so that commandes, permissions, file changes et MCP elicitation aient un flow unifie.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-016

**Acceptance Criteria:**
- [ ] Given une `PermissionAsk`, when elle arrive, then elle est convertie en `ApprovalRequest` et poussee dans le bottom pane.
- [ ] Given plusieurs approvals pendant que l'utilisateur tape, when elles arrivent, then elles sont queuees et affichees apres le delai idle de saisie.
- [ ] Given une decision approve/deny/cancel, when elle est prise, then une cellule d'historique de decision est inseree et la reponse retourne au pipeline outil.
- [ ] Given une approval resolue ailleurs ou annulee (unhappy path), when le signal arrive, then l'overlay se ferme ou avance la queue sans laisser la session bloquee.

#### US-015: Status, footer et pending input preview
**Description:** As a user running turns, I want les indicateurs Codex de tache en cours, footer, status et input queued, so that je comprenne l'etat de la session sans lire les logs.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006, US-016

**Acceptance Criteria:**
- [ ] Given un tour actif, when la TUI rend le bottom pane, then un indicateur de travail et les raccourcis utiles apparaissent.
- [ ] Given une entree utilisateur soumise pendant un tour, when elle est queuee, then un preview pending input est affiche.
- [ ] Given des compteurs tokens ou usage disponibles, when le stream se termine, then le pending usage output est insere apres la consolidation.
- [ ] Given aucune tache active (unhappy path idle), when la frame est rendue, then aucun timer ne continue a consommer CPU pour le footer.

### EP-005: Bottom pane, composer, boucle app et parite snapshot

Porter le systeme interactif complet de Codex: view stack, list selection, composer, paste, keymaps, event loop, draw pipeline et verification de parite.

**Definition of Done:** Pyxis possede un bottom pane a vues empilees, un composer Codex-compatible, une boucle draw/event compatible scrollback inline, et une suite snapshot couvrant les flows critiques.

#### US-016: `BottomPaneView`, `ListSelectionView` et stack de vues
**Description:** As a TUI, I want un bottom pane a vues empilees, so that approvals, menus, popups, pickers et prompts partagent le meme contrat d'interaction.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given une vue bottom pane, when elle implemente `BottomPaneView`, then elle declare render, desired_height, completion, key handling, paste handling et action-required.
- [ ] Given une liste selectable, when elle est rendue, then navigation, recherche, tabs, disabled rows, footer hints et side content fonctionnent.
- [ ] Given une vue enfant acceptee, when le parent demande dismissal, then la stack retire les vues attendues comme Codex.
- [ ] Given Esc/Ctrl-C pendant une vue (unhappy path), when la vue prefere un routage specifique, then l'annulation ne valide jamais une action par accident.

#### US-017: Port du composer Codex et compat input Pyxis
**Description:** As a user typing prompts, I want le composer Codex porte dans Pyxis, so that paste, historique, mentions, popups, file search et queueing suivent la parite cible.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-016

**Acceptance Criteria:**
- [ ] Given une saisie simple, when Enter est presse, then le composer produit une action `Submitted` compatible avec `launch_turn`.
- [ ] Given paste multi-ligne ou burst paste, when le contenu arrive, then il est capture, preview et flush sans bloquer la boucle.
- [ ] Given slash commands Pyxis existantes, when le composer Codex est actif, then `/help`, `/models`, `/skills`, `/goal`, `/mcp`, `/resume`, `/new`, `/clear`, `/quit` restent disponibles.
- [ ] Given une divergence volontaire Pyxis (unhappy path de parite), when elle est conservee, then elle est documentee dans `docs/CURRENT_STATUS.md` ou un ADR avant merge.

#### US-018: Boucle app, draw pipeline et parite snapshot
**Description:** As a mainteneur, I want une boucle app qui orchestre events, draw, commit ticks, scrollback flush et snapshots, so that la migration puisse etre validee avant cutover.

**Priority:** P0
**Size:** XL (8 pts)
**Dependencies:** Blocked by US-005, US-006, US-008, US-012, US-014, US-017

**Acceptance Criteria:**
- [ ] Given un `AppEvent`, when il est dispatch, then insertion history, consolidation, commit tick, approvals, resize et fatal exit sont routes dans des modules dedies.
- [ ] Given un draw, when la TUI calcule la hauteur desiree, then elle flush les pending history lines avant de dessiner la zone active.
- [ ] Given les flows critiques, when les snapshots tournent, then au moins 20 snapshots couvrent: chat idle, user message, streaming deltas, markdown code, table, exec, tool success, tool error, diff, approval, queue input, resize.
- [ ] Given un echec de parite snapshot (unhappy path), when la difference est intentionnelle, then elle est enregistree comme divergence Pyxis ; sinon la story reste `IN_PROGRESS`.

## Functional Requirements

- FR-01: The system must support `HistoryCell` rendering with `display_lines(width)`, `desired_height(width)` and transcript/raw output support.
- FR-02: The system must maintain separate finalized transcript cells and one mutable active cell.
- FR-03: The system must insert finalized history above an inline active viewport when the parity flag is enabled.
- FR-04: The system must map `AgentEvent` into a richer UI lifecycle without adding Ratatui or ANSI to `agent-core`.
- FR-05: The system must stream assistant output through a stable-prefix/tail controller and consolidate final markdown.
- FR-06: The system must render user, agent, reasoning, exec, tool, MCP-ready, diff, approval and notice cells.
- FR-07: The system must provide a bottom pane view stack with list selection, approvals, composer, footer and status indicators.
- FR-08: The system must preserve existing Pyxis slash commands and session resume behavior.
- FR-09: The system must include legal notice handling for copied or adapted Codex Apache-2.0 source.
- FR-10: The system must provide snapshot and terminal verification before enabling the new TUI by default.

## Non-Functional Requirements

- **Performance:** P95 frame render < 16 ms on 500 transcript cells; CPU idle < 5 % with no active turn; no syntax highlight recomputation for finalized cells except resize or content change.
- **Reliability:** 0 panics on terminal width >= 8 columns; no duplicate history insertion across resize; 100 % legacy sessions can resume with at least generic cells.
- **Compatibility:** Linux terminal and Windows Terminal manually tested before default cutover; truecolor and 16-color degradation both verified.
- **Security:** `agent-core` remains 0 ANSI and 0 Ratatui/Crossterm imports; tool outputs stay sanitized before terminal rendering; approvals fail closed on missing decision.
- **Maintainability:** No new module over 500 lines without split; every high-risk module has focused unit or snapshot tests; copied/adapted Codex files documented in notice inventory.
- **Licensing:** 100 % of copied/adapted Codex source has Apache-2.0 notice preservation before merge.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty transcript | New session or `/clear` | Welcome or empty state renders, no scrollback insertion | "" |
| 2 | Tiny terminal | Width < 8 or height too small | Cells clamp to safe width and avoid panic | "Terminal too small" if needed |
| 3 | Resize during stream | Terminal width changes while deltas arrive | Stream source re-renders, stable queue rebuilt | "" |
| 4 | Stream reset | Provider recovery discards current response | Mutable tail removed, finalized cells preserved | "Stream reset" only if user-visible |
| 5 | Orphan tool result | Result id has no active call | Generic tool result cell rendered | "" |
| 6 | Approval cancelled | Esc/Ctrl-C or external resolution | Request returns cancel/deny, overlay closes | "Action cancelled" |
| 7 | Terminal insertion failure | Scroll region or write error | Fallback to legacy renderer for session | "Terminal scrollback fallback active" |
| 8 | Markdown table incomplete | Stream cuts inside table | Tail holdback, no broken table committed | "" |
| 9 | Huge diff/output | Tool produces large output | Bounded render with truncation count | "... +N lines" |
| 10 | Legacy session shape | Old JSONL lacks ids or item lifecycle | Local ids derived and generic cells used | "" |
| 11 | License uncertainty | Source provenance unclear | File is skipped until classified | "Port inventory incomplete" in dev output |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Inline scrollback behaves differently across terminals | High | High | Feature flag, Windows Terminal verification, legacy fallback on terminal write error |
| 2 | Scope becomes a wholesale Codex clone including unrelated surfaces | Med | High | Non-goals exclude app-server remote, pets, realtime audio, connectors and collab unless Pyxis needs them |
| 3 | Copying Codex code creates license or attribution gaps | Med | High | US-001 required before copied/adapted code, notice inventory gate |
| 4 | Event contract changes break headless mode or sessions | Med | High | UI lifecycle lives in `agent-tui`; any `AgentEvent` changes are additive and covered by resume tests |
| 5 | Composer port regresses current Pyxis slash commands | Med | Med | US-017 explicitly keeps existing commands and documents divergences |
| 6 | Snapshot parity hides runtime terminal bugs | Med | Med | Manual terminal verification required for scrollback, resize, paste and approvals |
| 7 | Module size explodes during port | High | Med | 500-line module cap with split requirement |

## Non-Goals

Explicit boundaries for this version:

- Codex app-server remote protocol, WebSocket server, desktop connectors and multi-thread remote control are out of scope.
- Codex pets, realtime audio, image generation surfaces, collab agents and subagent side conversations are out of scope unless a later Pyxis PRD reopens them.
- Provider orchestration, auth and model behavior are out of scope. This PRD is TUI/client architecture only.
- Paneflow GPUI implementation is out of scope. This PRD prepares event shapes but does not build Paneflow UI.
- Public branding must not become Codex. Names, command, docs language and product identity remain Pyxis.
- No default cutover until snapshots, manual terminal checks and rollback flag are present.

## Files NOT to Modify

- `crates/agent-core/src/message.rs` - canonical persisted message model. Only additive changes through a separate core PRD.
- `crates/agent-core/src/provider.rs` - provider-to-core stream contract. Do not bind it to TUI needs.
- `crates/agent-tools/src/*.rs` - tool execution logic. Rendering cells derive from outputs, they do not alter tool semantics.
- `crates/agent-auth/src/**` - auth is unrelated to TUI parity.
- `crates/agent-provider/src/**` - provider wire is unrelated except for additive event data already emitted by core.
- `docs/CURRENT_STATUS.md` - modify only when documenting an intentional divergence or cutover status.
- `Cargo.toml` / `Cargo.lock` - modify only for dependencies justified by a specific story.

## Technical Considerations

- **Architecture:** Should Pyxis model Codex `ThreadItem` exactly or create `TranscriptItem` tailored to current `AgentEvent`? Recommended: `TranscriptItem`, because Pyxis is not adopting app-server v2 in this PRD.
- **Event contract:** Should new lifecycle events live in `agent-core` or in an adapter in `agent-tui`? Recommended: start in `agent-tui`; add `AgentEvent` variants only when the core truly owns the fact.
- **Terminal model:** Should Pyxis replace alt-screen completely or feature-flag inline viewport first? Recommended: feature flag first, fallback legacy path until terminal compatibility is proven.
- **Composer:** Should Pyxis keep its current composer or port Codex fully? Recommended: port Codex composer for parity, then reapply Pyxis-specific commands and naming.
- **Dependencies:** Ratatui, Crossterm, pulldown-cmark, Syntect and two-face are already present. Avoid adding app-server protocol crates unless a story proves a need.
- **Migration:** Backward compatibility requirement: yes. Old sessions must replay into generic cells even if they lack item lifecycle data.
- **Rollback:** Single config/env flag disables the new TUI surface and restores legacy rendering for one release cycle.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Codex TUI flows covered by snapshots | 0 dedicated parity snapshots | >=20 snapshots | Month-1 | `cargo test -p agent-tui` or workspace tests |
| Frame latency on 500 cells | Not measured for new model | <16 ms p95 | Month-1 | TestBackend bench/instrumentation |
| CPU idle with no active turn | Existing app baseline | <5 % | Month-1 | Manual process measurement |
| Legacy session resume success | Existing resume works with `Block` model | 100 % sample sessions render via cells | Month-1 | Resume test fixtures |
| Approval flow completion | Current permission UI works | 100 % approve/deny/cancel paths close overlay and unblock pipeline | Month-1 | Integration tests |
| Manual terminal compatibility | Not verified for inline scrollback | Linux + Windows Terminal pass or fallback documented | Month-1 | Manual checklist |
| License inventory completion | No Codex port inventory | 100 % copied/adapted files classified | Before first merge | `NOTICE-CODEX.md` review |

## Open Questions

- Arthur should confirm before implementation whether any current Pyxis composer behavior must survive even if it diverges from Codex.
- Engineering should verify whether Windows Terminal supports the chosen scrollback insertion path well enough for default enablement.
- Engineering should decide whether `TranscriptItem` should be exported for Paneflow now or kept crate-private until Paneflow work starts.
- Engineering should decide whether the legacy TUI remains for one release or two after cutover.
[/PRD]
