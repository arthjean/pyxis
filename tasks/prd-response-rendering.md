[PRD]
# PRD: Pyxis : Refonte du rendu des réponses du modèle (TUI)

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-06-17 | Arthur Jean | Initial draft, issu de l'audit comparatif Pyxis vs Claude Code + dissection Codex CLI (Rust). Direction validée : « Structure Claude Code, ADN Pyxis ». |

## Problem Statement

Le MVP de Pyxis a livré un frontend TUI minimal (`prd-pyxis.md`, US-019) : streaming token-par-token, diff brut dans le dialog de permission, dégradation monochrome. C'était volontairement le strict nécessaire : le PRD MVP a **explicitement différé** « le TUI riche (sélection souris, tables markdown, virtual scroll, syntax highlight incrémental) » à une phase ultérieure (`prd-pyxis.md`, Non-Goals). Cette phase, c'est ce PRD. Aujourd'hui le rendu est le maillon faible visible face à Claude Code et Codex App, et c'est précisément ce qui fait basculer le dogfooder sur un autre outil. L'audit a isolé des écarts concrets, dont la cause racine est le **modèle de données**, pas le style de rendu :

1. **L'information structurée des outils est jetée à l'arrivée.** `AppState::apply` aplatit le `ToolCallView.input` (qui contient path, old_string, new_string, content) en `summary: String` via `summarize()` (`crates/agent-tui/src/state.rs:447,998`). Impossible ensuite de produire `Update(path)`, `Added N lines, removed M`, ou un diff : la donnée n'existe plus.
2. **Les résultats d'outils sont masqués et non corrélés.** `render.rs:537-550` n'affiche un `ToolResult` que s'il est en erreur ; les deux blocs `ToolCall`/`ToolResult` ne partagent pas d'`id`, donc aucune hiérarchie call→result (le connecteur `⎿` de Claude Code), aucun résumé de succès.
3. **Aucun diff inline.** Le seul diff existe dans le dialog de permission, numéroté par index séquentiel `i+1` au lieu des vrais numéros de ligne, sans fond coloré ni coloration syntaxique (`render.rs:736-747`). Claude Code et Codex affichent un diff coloré après chaque édition réussie.
4. **Pas de coloration syntaxique ni de tables.** Les code-blocks markdown sont rendus en gris uni (`crates/agent-tui/src/markdown.rs:147-157`) ; les tables markdown sont silencieusement perdues (option `ENABLE_TABLES` active mais aucun handler `Tag::Table`).
5. **Aucune progression vivante.** Pas de spinner animé, pas de durée écoulée, pas de compteur de tokens : juste un `● réfléchit` statique dans la status line (`render.rs:698-701`).

**Why now:** Le cœur est durci (`prd-codex-orchestration.md`, EP-006→009 livrés) et l'édition est fiable (US-025). Le rendu est désormais le seul écran qui sépare Pyxis de la qualité Codex App. L'audit comparatif (Claude Code source + Codex CLI Rust) est terminé et a produit une grammaire visuelle précise ; la direction esthétique est tranchée (« Structure Claude Code, ADN Pyxis »). La stack de référence (Codex CLI : ratatui 0.29, similar 2.7, syntect 5) est connue et transposable. C'est la fenêtre pour faire de Pyxis un harness dont le rendu n'est plus une raison de basculer ailleurs.

## Overview

Ce PRD refond le rendu des **réponses du modèle** : texte markdown assistant, raisonnement, invocations d'outils, résultats, diffs, erreurs, et indicateurs de progression. La direction validée est « **Structure Claude Code, ADN Pyxis** » : on importe la structure qui fait la qualité de Claude Code (puce d'ancrage `●`, hiérarchie call→result via `⎿`, diffs inline lisibles, résumés d'outils structurés, coloration syntaxique, progression vivante) tout en préservant l'esthétique signature de Pyxis (monochrome + un accent teal). La couleur est **réservée au fonctionnel** : vert/rouge pour les diffs, coloration sobre pour le code ; le reste reste monochrome, la hiérarchie passant par le poids et la teinte. La règle de teinte : puce teal = parole de l'agent, puce grise = action outil, rouge = erreur.

La refonte est **confinée au crate `agent-tui`**, plus un enrichissement du type `Block` (`state.rs`) et un seul ajout dans la boucle (`agent-cli/src/interactive.rs` : un tick timer pour animer le spinner). Décision clé issue de l'audit : les contrats `agent-core` (`AgentEvent`) et `agent-tools` (`ToolOutput`) restent **intacts** : le `ToolCallView` porte déjà l'`input` complet, donc les diffs et les résumés se dérivent côté frontend, sans toucher le cœur ni les outils. Autre décision structurante : on **conserve le modèle viewport + scroll interne** actuel (alt-screen) plutôt que de migrer vers le pattern `insert_before` de Codex (scrollback natif), qui serait une refonte d'architecture hors-scope ; la contrepartie perf (re-render intégral à chaque frame) est neutralisée par un **cache des lignes stylées « baked »**, obligatoire avant d'introduire la coloration syntaxique.

La solution s'organise en quatre épics livrables incrémentalement : (EP-010) le socle structurel (modèle de données enrichi, puces, view-models d'outils, hiérarchie `⎿`) qui à lui seul rapproche radicalement le rendu des captures, sans nouvelle dépendance ; (EP-011) le diff inline coloré via `similar` ; (EP-012) le markdown riche (tables, blockquotes) et la coloration syntaxique avec cache ; (EP-013) la progression vivante (spinner animé, durée, tokens, pill de défilement).

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Parité structurelle avec Claude Code | 6/6 éléments livrés : puce `●`, hiérarchie `⎿`, diff inline coloré, coloration syntaxique, spinner animé, durée+tokens | 0 régression ; toggle markdown brut + sortie d'outil colorée (ansi-to-tui) |
| Lisibilité des diffs | Diff inline coloré sur 100 % des edits/writes réussis | Numéros de ligne absolus si le dogfood les réclame |
| Performance de rendu | Frame < 16 ms p95 sur transcript ≤ 200 blocs (cache actif) ; CPU idle < 5 % | < 16 ms p95 sur ≤ 1000 blocs |
| Préservation de l'ADN | 0 couleur non-fonctionnelle (audit de teinte sur la palette) | Identité monochrome + accent teal maintenue |

## Target Users

### Arthur Jean, créateur & dogfooder principal
- **Role:** Solo indie maker ; orchestre Codex via Pyxis au quotidien.
- **Behaviors:** Sessions longues (refactors, audits) avec beaucoup d'éditions et de lectures de fichiers ; lit attentivement les diffs et la sortie des outils ; Fedora/Wayland, terminal truecolor.
- **Pain points:** Le rendu actuel est plat : pas de diff après édition, code non coloré, résultats d'outils masqués, aucun feedback de progression. Difficile de suivre ce que fait l'agent d'un coup d'œil.
- **Current workaround:** Bascule sur Codex App / Claude Code pour les tâches où le rendu compte ; Pyxis reste un prototype.
- **Success looks like:** Le rendu de Pyxis n'est plus jamais une raison de quitter l'outil ; les diffs et la progression sont aussi lisibles que dans Claude Code, avec la sobriété en plus.

### Développeur Rust / systèmes, early adopter OSS
- **Role:** Dev qui teste Pyxis depuis le repo GPL-3.0.
- **Behaviors:** Juge un harness sur la première session ; attend un rendu propre des diffs et du code de son propre repo.
- **Pain points:** Un TUI qui rend le code en gris uni et masque les diffs paraît inachevé face au Codex CLI officiel.
- **Current workaround:** Retourne au Codex CLI ou à Claude Code.
- **Success looks like:** Le rendu tient la comparaison avec Codex CLI dès la première session, avec une esthétique distincte (monochrome épuré).

## Research Findings

Key findings que ce PRD applique :

### Competitive Context
- **Claude Code (React/Ink, Node)** : la cible de finition. Puce `●` d'ancrage par étape, hiérarchie call→result via le connecteur `⎿`, `StructuredDiff` (dual-frame caching + ANSI slicing) pour les diffs colorés, `/diff` par tour, spinner à verbes. Coût : 200-400 Mo de RAM (Ink). Pyxis vise la même finition en Rust natif à ~1/10 de la RAM.
- **Codex CLI (openai/codex, Rust+ratatui)** : la référence directe et transposable. Stack confirmée : ratatui 0.29, `similar` 2.7 (diff lignes + intra-ligne), `syntect` 5, `ansi-to-tui` 7, `textwrap` 0.16, `unicode-width` 0.2. Pattern d'archi `insert_before()` (scrollback natif), qu'on **écarte** ici (voir Technical Considerations). Tables markdown longtemps mauvaises (leçon : les soigner), `ansi` rendu en RGB qui casse les thèmes (leçon : ne pas hardcoder RGB hors truecolor).
- **opencode** : contre-modèle perf documenté (issue #811 : 25-30 % CPU idle, re-render sur timer). Leçon directe : rendre event-driven, cacher tout ce qui est « baked ».
- **Gemini CLI** : bugs markdown chroniques (listes tronquées, tables illisibles) car parsing incrémental d'un stream qui coupe les structures au milieu. Leçon : parser tolérant à l'incomplet (pulldown-cmark ferme implicitement, déjà acquis) + prévoir un toggle rendu/brut.
- **Market gap:** atteindre la finition de rendu de Claude Code en binaire Rust natif, avec une esthétique monochrome distincte et un cœur headless réutilisable.

### Best Practices Applied
- **Cacher les lignes stylées « baked »** et ne reconstruire que la ligne en cours de stream ; n'invalider que sur resize (reflow) ou édition de contenu. La coloration syntaxique ne tourne **jamais** par frame.
- **`similar` pour le diff** : `grouped_ops(context)` pour les hunks, `iter_inline_changes` pour l'emphase mot-à-mot ; word-diff calculé en lazy uniquement sur les lignes des hunks modifiés.
- **Ne jamais hardcoder de RGB** sans détection truecolor ; dégrader proprement (Pyxis le fait déjà via `COLORTERM`).
- **Largeur unicode** via `unicode-width` / itération par graphème (amélioration vs `chars().count()` actuel).

*Sources complètes : transcripts d'audit de cette session (rendu Claude Code source + dissection Codex CLI), Cargo.toml de openai/codex, docs ratatui.*

## Assumptions & Constraints

### Assumptions (to validate)
- **L'`input` du tool_call suffit à reconstruire un diff lisible sans relire le fichier.** Vrai pour `edit` (old_string/new_string) et `write` (content) : l'input contient tout (`crates/agent-tools/src/edit.rs:18-24`, `write.rs:13-17`). À confirmer sur cas réels (US-037).
- **Le re-render intégral + cache des lignes baked tient le 60 fps** sur transcripts réalistes (≤ 200 blocs). À mesurer avant d'introduire syntect (US-041).
- **Un moteur de coloration couvre Rust/TS/JS/JSON/TOML/Markdown avec une qualité suffisante.** À trancher par spike (US-040) : `synoptic` (léger, pur Rust) vs `syntect` 5 + `fancy-regex`.
- **L'approximation tokens = caractères/4** (comme Claude Code) est acceptable pour l'affichage ; pas besoin du tokenizer exact dans la boucle de rendu.

### Hard Constraints
- **Refonte confinée à `agent-tui`** ; les contrats `agent-core` (`AgentEvent`) et `agent-tools` (`ToolOutput`) restent inchangés (0 diff). Seul ajout hors `agent-tui` : le tick timer du spinner dans `agent-cli/src/interactive.rs`.
- **Monochrome + accent teal préservé** ; couleur réservée au fonctionnel (diffs, syntaxe). Pas de palette sémantique décorative.
- **Dégradation 16-couleurs** sans truecolor, sans corruption (invariant historique US-019 AC4).
- **Rendu pur testable via `TestBackend`** (invariant conservé : `render` ne fait pas d'I/O).
- **Lints clippy obligatoires** : `panic`/`unimplemented`/`dbg_macro` = deny ; `unwrap`/`expect` = warn.
- **Licence GPL-3.0-or-later** ; edition 2024 ; rustc 1.95.
- **Aucune régression** des fonctions existantes : écran d'accueil, menu slash, scroll borné, dialog de permission, saisie, historique.

## Quality Gates

These commands must pass for every user story:
- `cargo check --workspace` - compilation de tout le workspace
- `cargo clippy --workspace --all-targets` - lints (`panic`/`unimplemented`/`dbg_macro` bloquants ; `unwrap`/`expect` en warning)
- `cargo test --workspace` - suite de tests (chaque story ajoute ses tests `TestBackend` et/ou unitaires)

Pour toute story de rendu (ce PRD est intégralement UI) : **vérification visuelle manuelle** dans un terminal truecolor (rendu attendu) ET en forçant la dégradation 16-couleurs (`COLORTERM` absent), sur terminal large et étroit, sans corruption ni panic.

## Epics & User Stories

### EP-010: Socle structurel & view-models d'outils

Refondre le modèle de données et la grammaire visuelle de base : puce d'ancrage, hiérarchie call→result via `⎿`, labels et résumés d'outils structurés, grammaire d'erreur. Aucune nouvelle dépendance. À lui seul, cet épic rapproche radicalement le rendu des captures cibles.

**Definition of Done:** Chaque tour assistant est ancré par une puce ; chaque outil affiche un label structuré et (pour les mutations) un résumé secondaire `⎿` ; les erreurs et rejets ont des grammaires distinctes ; les contrats `agent-core`/`agent-tools` sont inchangés ; tous les tests `TestBackend` passent y compris en dégradation.

#### US-032: Theme étendu et extraction du module `theme.rs`
**Description:** As a développeur du frontend, I want extraire `Theme` dans un module dédié avec une palette sémantique étendue (tons de diff, succès) gardée monochrome + teal, so that les couleurs fonctionnelles sont centralisées et la dégradation 16-couleurs reste cohérente.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given le `Theme` actuel, when il est déplacé dans `theme.rs`, then `render.rs` et `markdown.rs` l'importent sans changement de comportement visuel sur le rendu existant.
- [ ] Given un terminal truecolor, when on demande les styles `diff_add`/`diff_remove`/`diff_add_word`/`diff_remove_word`/`success`, then chacun renvoie une couleur fonctionnelle distincte (vert/rouge sombre + variantes saturées), et le chrome (texte, puces non-diff) reste monochrome + teal.
- [ ] Given un terminal SANS truecolor (unhappy path), when ces mêmes styles sont demandés, then ils dégradent en 16-couleurs / modifiers (gras, vidéo inverse) sans panic et restent distinguables.
- [ ] Given un audit de teinte, when on liste la palette, then aucune couleur non-fonctionnelle (purement décorative) n'est introduite.

#### US-033: Modèle `Block` enrichi et appariement call↔result
**Description:** As a moteur de rendu, I want que `Block::ToolCall` conserve l'`input` structuré et que `Block::ToolResult` porte le `call_id` corrélé, so that le rendu peut dériver labels, résumés et diffs sans que l'information soit perdue à l'`apply`.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un `AgentEvent::ToolCall`, when `apply` le range, then le `Block::ToolCall` conserve `id` + `name` + `input: serde_json::Value` (la fonction `summarize` n'est plus appelée à l'`apply`).
- [ ] Given un `AgentEvent::ToolResult`, when `apply` le range, then le `Block::ToolResult` porte le `call_id` permettant de retrouver son `ToolCall`.
- [ ] Given une session reprise, when `blocks_from_messages` reconstruit le transcript, then l'`input` et le `call_id` sont repeuplés depuis les `ContentBlock::ToolUse`/`ToolResult` (compat resume préservée).
- [ ] Given un `ToolResult` sans `ToolCall` correspondant (unhappy path : id orphelin), when le rendu l'affiche, then il dégrade en affichage générique sans panic.
- [ ] Given les contrats, when le crate compile, then `agent-core` et `agent-tools` n'ont aucun diff.

#### US-034: Puce d'ancrage `●`, grammaire d'espacement et raisonnement
**Description:** As a utilisateur, I want que chaque tour assistant soit ancré par une puce `●` (teal) avec un corps aligné, et que le raisonnement soit replié proprement, so that je suis le fil des étapes d'un coup d'œil au lieu de lire un mur de texte.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-032

**Acceptance Criteria:**
- [ ] Given un tour assistant, when il est rendu, then une puce `●` en accent teal précède le markdown, et le corps wrappé reste aligné sous la puce (indentation à 2 colonnes).
- [ ] Given une succession de blocs (assistant, outils) dans un tour, when ils sont rendus, then l'espacement vertical les sépare lisiblement (1 ligne) et les outils consécutifs restent groupés.
- [ ] Given un bloc de raisonnement, when il n'est pas le dernier, then il est replié en un libellé discret ; when il est en cours (dernier bloc), then un court aperçu des dernières lignes est affiché.
- [ ] Given une réponse assistant vide (unhappy path), when rendue, then aucune puce orpheline ni ligne vide parasite n'apparaît.
- [ ] Given un terminal étroit, when le corps wrappe, then l'alignement et la puce ne corrompent pas la mise en page.

#### US-035: View-models d'outils (`tool.rs`) : labels et résumés `⎿`
**Description:** As a utilisateur, I want que chaque outil affiche un label structuré (`Verb`/`Verb(cible)`) et un résumé secondaire indenté sous `⎿`, so that je comprenne ce que fait l'agent et son résultat sans lire la sortie brute.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-032, US-033

**Acceptance Criteria:**
- [ ] Given un outil de lecture (`read`/`glob`/`grep`/`bash`), when rendu, then une ligne condensée résume l'action au passé (ex. `Read 142 lines`, `Listed 1 directory`, `Searched "<pattern>"`, `Ran 1 shell command`), nombres en évidence.
- [ ] Given un outil de mutation (`edit`/`write`), when rendu, then le label `Update(<path>)` / `Write(<path>)` (puce grise) est suivi d'un résumé `⎿` (`Added N lines, removed M lines` / `Wrote N lines`).
- [ ] Given un nouvel outil non reconnu (unhappy path), when rendu, then un view-model générique (nom + cible best-effort depuis l'input) s'affiche sans panic.
- [ ] Given le connecteur `⎿`, when rendu sans truecolor, then il reste lisible (dim/16-couleurs) et l'indentation est préservée.
- [ ] Given la pluralisation (1 vs N), when un résumé est produit, then le singulier/pluriel est correct (`1 file` vs `2 files`).

#### US-036: Grammaire d'erreur vs rejet
**Description:** As a utilisateur, I want distinguer visuellement un échec d'outil d'un rejet volontaire, so that je ne confonde pas une erreur du système avec une action que j'ai refusée.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-032, US-035

**Acceptance Criteria:**
- [ ] Given un `ToolResult` en erreur, when rendu, then il apparaît sous `⎿` en couleur erreur (rouge), préfixé `Error:`, tronqué à un nombre borné de lignes avec un indicateur `… +N lignes`.
- [ ] Given un rejet utilisateur (permission refusée), when rendu, then il apparaît en ton atténué (`subtle`, pas rouge) avec un libellé explicite (ex. `Rejeté : <action> sur <path>`).
- [ ] Given un message d'erreur multi-lignes très long (unhappy path), when rendu, then la troncature ne coupe pas au milieu d'un caractère multi-octet et n'introduit pas d'ANSI résiduel.
- [ ] Given un terminal sans truecolor, when une erreur est rendue, then elle reste distinguable (rouge 16-couleurs + gras).

---

### EP-011: Diff inline coloré

Afficher un diff coloré et lisible après chaque édition réussie, et refondre le diff du dialog de permission pour réutiliser le même moteur. Introduit la dépendance `similar`.

**Definition of Done:** Tout `edit`/`write` réussi affiche un diff inline (ajouts/suppressions colorés + emphase intra-ligne) ; le dialog de permission réutilise le même rendu avec de vrais signes et couleurs ; dégradation 16-couleurs correcte ; le tout calculé depuis l'`input`, contrats intacts.

#### US-037: Module `diff.rs` : calcul du diff via `similar`
**Description:** As a moteur de rendu, I want un module qui calcule, depuis l'`input` d'un `edit`/`write`, un diff structuré (lignes ajoutées/supprimées/contexte + segments mot-à-mot), so that le rendu dispose d'une représentation prête à styler.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-033

**Acceptance Criteria:**
- [ ] Given un `edit` (old_string/new_string), when le diff est calculé, then `similar` produit des hunks groupés avec lignes de contexte (`grouped_ops`) et l'emphase intra-ligne sur les lignes modifiées (`iter_inline_changes`).
- [ ] Given un `write` (création/remplacement), when le diff est calculé, then le contenu écrit est présenté en lignes ajoutées (avec aperçu borné pour un gros fichier).
- [ ] Given un edit sans changement net ou un input dégénéré (unhappy path), when le diff est calculé, then la fonction renvoie un résultat vide/borné sans panic et sans diff trompeur.
- [ ] Given un fichier volumineux, when le diff est calculé, then un garde de taille/temps borne le coût (troncature `… +N lignes` au-delà d'un seuil).
- [ ] Le calcul est une fonction pure, testée unitairement (cas ajout, suppression, remplacement intra-ligne, multi-hunks).

#### US-038: Rendu du diff inline après mutation
**Description:** As a utilisateur, I want voir le diff coloré directement dans le transcript après une édition réussie, so that je vérifie ce qui a changé sans relire le fichier.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-037, US-035

**Acceptance Criteria:**
- [ ] Given un `edit`/`write` réussi, when rendu, then le diff suit le résumé `⎿`, avec une gouttière de numéros (relatifs au nouveau fichier), des fonds vert/rouge sombres pour ajout/suppression, et l'emphase intra-ligne saturée sur les segments changés.
- [ ] Given une suppression et un ajout adjacents, when rendus, then les portions identiques restent neutres et seules les différences mot-à-mot sont surlignées.
- [ ] Given un terminal sans truecolor (unhappy path), when le diff est rendu, then ajout/suppression restent distinguables via signe `+`/`-` + gras/dim (pas de dépendance à la couleur seule).
- [ ] Given un edit échoué (ancre introuvable), when rendu, then aucun diff n'est affiché : seule l'erreur (EP-010) apparaît.
- [ ] Given une ligne de diff plus large que le terminal, when rendue, then elle wrappe ou tronque sans corrompre la gouttière.

#### US-039: Refonte du diff du dialog de permission
**Description:** As a utilisateur, I want que le diff montré avant d'autoriser une mutation utilise le même moteur que le diff inline, so that l'aperçu de permission soit aussi lisible et cohérent que le rendu post-édition.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-037

**Acceptance Criteria:**
- [ ] Given une demande de permission pour un `edit`/`write`, when le dialog s'affiche, then le diff réutilise `diff.rs` (mêmes signes, couleurs, gouttière) au lieu de l'index séquentiel actuel.
- [ ] Given le dialog, when rendu, then la gouttière reste non sélectionnable (préservation du comportement existant) et la hauteur du dialog reste bornée.
- [ ] Given un diff de permission très long (unhappy path), when rendu, then il est tronqué à une hauteur max sans masquer les actions `[o]/[n]`.
- [ ] Given un terminal sans truecolor, when le dialog est rendu, then le diff dégrade lisiblement.

---

### EP-012: Markdown riche & coloration syntaxique

Compléter le rendu markdown (tables, blockquotes) et colorer le code (code-blocks et diffs), avec un cache de lignes stylées obligatoire pour tenir la performance.

**Definition of Done:** Les tables et blockquotes markdown sont rendues ; le code (code-blocks + diffs) est coloré syntaxiquement pour au moins Rust/TS-JS/JSON/TOML/Markdown ; la coloration ne tourne jamais par frame (cache) ; la perf cible est tenue.

#### US-040: Spike : choix du moteur de coloration syntaxique
**Description:** As a créateur, I want trancher entre `synoptic` (léger, pur Rust) et `syntect` 5 + `fancy-regex` sur des critères mesurés, so that le choix repose sur le poids binaire, la perf et la qualité réels, pas sur une intuition.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un échantillon Rust/TS/JSON/TOML/Markdown, when chaque moteur le colore, then la qualité (tokens reconnus) est comparée et consignée.
- [ ] Given chaque moteur, when intégré en prototype, then le surcoût de poids binaire et le temps de coloration d'un bloc typique sont mesurés et consignés.
- [ ] Given un moteur retenu, when la décision est prise, then elle est documentée (rationale : poids, perf, absence de toolchain C, qualité) ; défaut attendu = `fancy-regex` (pas de dépendance C, binaire distribuable).
- [ ] Given un langage non couvert (unhappy path), when un code-block d'un langage inconnu est rencontré, then le rendu retombe proprement sur du texte non coloré.

#### US-041: Cache des lignes stylées par bloc
**Description:** As a moteur de rendu, I want cacher les `Vec<Line>` déjà construites des blocs « baked » et ne reconstruire que le bloc en cours de stream, so that le re-render intégral à chaque frame reste sous le budget de 16 ms même avec coloration et diffs.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-034

**Acceptance Criteria:**
- [ ] Given un transcript dont seul le dernier bloc change (stream), when une frame est rendue, then les blocs antérieurs sont servis depuis le cache (pas de re-parse markdown ni de re-coloration).
- [ ] Given un redimensionnement du terminal (unhappy path), when la largeur change, then le cache est invalidé et le wrap recalculé sans corruption.
- [ ] Given l'édition d'un bloc (ex. marqueur de complétion strippé, finalize streaming), when le bloc change, then sa seule entrée de cache est invalidée.
- [ ] Given un transcript de 200 blocs, when une frame est rendue en idle, then le temps de rendu est < 16 ms p95 (mesuré via bench) et le CPU idle < 5 %.

#### US-042: Coloration syntaxique des code-blocks et des diffs
**Description:** As a utilisateur, I want que le code dans les réponses et dans les diffs soit coloré syntaxiquement, so that je lise le code aussi confortablement que dans un éditeur.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-040, US-041, US-038

**Acceptance Criteria:**
- [ ] Given un code-block markdown avec langage déclaré (Rust/TS-JS/JSON/TOML/Markdown), when rendu, then les tokens sont colorés via le moteur retenu et le résultat est mis en cache.
- [ ] Given un diff inline, when rendu, then la coloration syntaxique s'applique au contenu des lignes en complément des fonds ajout/suppression (sans masquer le signe ni le fond).
- [ ] Given la performance, when un bloc déjà coloré est re-rendu, then la coloration n'est PAS recalculée (cache hit).
- [ ] Given un terminal sans truecolor (unhappy path), when du code coloré est rendu, then il dégrade en monochrome lisible (la coloration ne corrompt pas le 16-couleurs).
- [ ] Given un code-block sans langage ou langage inconnu, when rendu, then il s'affiche en texte neutre indenté sans erreur.

#### US-043: Tables et blockquotes markdown
**Description:** As a utilisateur, I want que les tables et citations markdown des réponses soient rendues, so that les comparatifs et notes du modèle soient lisibles au lieu d'être perdus.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-032

**Acceptance Criteria:**
- [ ] Given une table markdown, when rendue, then les colonnes sont alignées avec en-têtes en évidence, largeurs calculées sur la largeur disponible.
- [ ] Given une table plus large que le terminal (unhappy path), when rendue, then elle dégrade lisiblement (wrap par cellule ou bascule en paires clé/valeur) sans déborder ni corrompre.
- [ ] Given un blockquote, when rendu, then chaque ligne est préfixée d'une barre (`▎`) atténuée et le texte est légèrement distinct.
- [ ] Given une table malformée/incomplète issue d'un stream (unhappy path), when rendue, then aucun panic ni troncature corrompue (rendu best-effort).

---

### EP-013: Progression vivante

Communiquer en continu l'activité de l'agent : spinner animé à verbes, durée écoulée, estimation de tokens, et pill de défilement. Ajoute un tick timer dans la boucle.

**Definition of Done:** Pendant un tour, un spinner animé avec verbe + durée + tokens est affiché ; une pill signale les nouveaux messages quand l'utilisateur a remonté le transcript ; le reduced-motion est respecté ; aucune régression de la boucle d'orchestration.

#### US-044: Spinner animé + tick de rendu dans la boucle
**Description:** As a utilisateur, I want un spinner animé pendant que l'agent travaille, so that je sache que la session est vivante et pas gelée.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-032

**Acceptance Criteria:**
- [ ] Given un tour en cours, when l'agent réfléchit/exécute, then un spinner animé (frames en ping-pong) accompagné d'un verbe s'affiche dans la status line.
- [ ] Given la boucle `select!` d'`interactive.rs`, when un tour est actif, then un tick timer redessine périodiquement pour animer le spinner, et il s'arrête (pas de redraw inutile) quand aucun tour n'est actif (unhappy path : 0 CPU brûlé en idle).
- [ ] Given un environnement reduced-motion (ex. `NO_COLOR` ou variable dédiée), when le spinner s'affiche, then il dégrade en un point `●` pulsé lentement plutôt qu'animé.
- [ ] Given le tick timer ajouté, when le crate compile, then la logique d'orchestration et de boucle d'objectif (`/goal`) reste inchangée (seul le redraw est ajouté).

#### US-045: Indicateurs de durée et de tokens
**Description:** As a utilisateur, I want voir la durée écoulée et une estimation de tokens du tour en cours, so that j'aie une idée du coût et de l'avancement.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-044

**Acceptance Criteria:**
- [ ] Given un tour en cours, when la durée dépasse un seuil (ex. quelques secondes), then la status line affiche la durée formatée (`Ns`, puis `Nm Ns`).
- [ ] Given le flux de texte du tour, when des tokens arrivent, then une estimation (caractères/4) est affichée et formatée de façon compacte (`84.2k`).
- [ ] Given la fin d'un tour, when il se termine, then les indicateurs se figent ou disparaissent proprement (pas de compteur qui continue).
- [ ] Given un tour très court (unhappy path), when il se termine avant le seuil d'affichage, then aucun indicateur transitoire parasite ne clignote.

#### US-046: Pill « nouveau message / revenir en bas »
**Description:** As a utilisateur, I want une indication quand de nouveaux contenus arrivent alors que j'ai remonté le transcript, so that je sache que je ne suis pas en bas et que je puisse y revenir facilement.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-032

**Acceptance Criteria:**
- [ ] Given l'utilisateur a scrollé vers le haut et que du nouveau contenu arrive, when la frame est rendue, then une pill discrète signale le nombre de nouveaux messages (ou « revenir en bas ») avec le raccourci associé.
- [ ] Given l'utilisateur est déjà collé en bas (auto-follow), when du contenu arrive, then aucune pill ne s'affiche (le suivi est automatique).
- [ ] Given un terminal étroit (unhappy path), when la pill s'affiche, then elle ne déborde pas et ne masque pas l'input.

## Functional Requirements

- FR-01: Le système doit ancrer chaque tour assistant par une puce `●` (accent teal) et aligner le corps markdown sous la puce.
- FR-02: Le système doit afficher un label d'outil structuré ; les lectures (`read`/`glob`/`grep`/`bash`) sont résumées en une ligne condensée, les mutations (`edit`/`write`) affichent `Verb(path)` + résumé `⎿`.
- FR-03: Le système doit apparier chaque `tool_result` à son `tool_call` (par `id`) pour produire la ligne secondaire `⎿`.
- FR-04: Le système doit afficher un diff coloré (ajouts/suppressions + emphase intra-ligne) après chaque `edit`/`write` réussi, calculé depuis l'`input` du call.
- FR-05: Le système doit colorer syntaxiquement les code-blocks markdown et le contenu des diffs pour au moins Rust, TS/JS, JSON, TOML, Markdown.
- FR-06: Le système doit rendre les tables et blockquotes markdown (qui sont aujourd'hui ignorées/perdues).
- FR-07: Pendant un tour, le système doit afficher un spinner animé, la durée écoulée et une estimation de tokens.
- FR-08: Le système doit rendre les erreurs d'outil distinctement des rejets utilisateur (couleur et libellé).
- FR-09: Le système NE DOIT PAS recalculer la coloration ni le diff d'un bloc déjà « baked » à chaque frame (cache obligatoire).
- FR-10: Le système NE DOIT PAS émettre d'ANSI dans le buffer (sanitize maintenu) ni dépendre de la couleur seule pour la hiérarchie (poids/teinte).
- FR-11: Tout le rendu doit rester confiné à `agent-tui` ; aucun changement des contrats `agent-core`/`agent-tools`.

## Non-Functional Requirements

- **Performance:** Rendu d'une frame < 16 ms p95 sur transcript ≤ 200 blocs avec cache actif ; CPU idle < 5 % (aucun redraw sur timer hors animation de spinner) ; coloration syntaxique recalculée 0 fois sur un bloc déjà rendu (cache hit 100 % sur les blocs baked).
- **Compatibilité terminal:** Rendu correct en truecolor ET dégradation 16-couleurs sans corruption ; pas de panic sur terminal ≥ 8 colonnes ; reflow correct au resize.
- **Reliability:** 0 fuite ANSI dans le buffer (sanitize conservé) ; markdown incomplet (stream coupé en plein milieu) rendu sans panic ni troncature corrompue ; troncatures char-aware (jamais au milieu d'un caractère multi-octet).
- **Accessibilité:** Reduced-motion respecté (spinner dégradé en point pulsé) ; hiérarchie visuelle jamais portée par la couleur seule (poids + teinte) ; couleur strictement fonctionnelle.
- **Maintainability:** Aucun module de rendu > 400 lignes après découpage (`theme`, `transcript`, `tool`, `diff`, `markdown`, `spinner`) ; contrats `agent-core`/`agent-tools` à 0 diff ; `render` reste pur (testable `TestBackend`).

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Transcript vide | Première session / après `/new` | Écran d'accueil (carte + logo) inchangé, aucun rendu de réponse | - |
| 2 | Réponse assistant vide | Tour sans texte (outils seuls) | Pas de puce orpheline ni ligne vide parasite | - |
| 3 | Tour en cours | Stream actif | Bloc assistant en streaming + spinner animé + durée/tokens | verbe + `Ns` |
| 4 | Échec d'outil | `edit` ancre introuvable, bash exit≠0 | Ligne `⎿` rouge `Error:`, tronquée, aucun diff | `Error: <message> … +N lignes` |
| 5 | Rejet utilisateur | Permission refusée | Ligne `⎿` ton atténué (pas rouge) | `Rejeté : <action> sur <path>` |
| 6 | Terminal sans truecolor | `COLORTERM` absent | Dégradation 16-couleurs / modifiers, diffs via signe + gras/dim | - |
| 7 | Terminal étroit / resize | Largeur réduite en cours de stream | Reflow + invalidation cache sans corruption | - |
| 8 | Diff volumineux | Gros edit/write | Aperçu borné + `… +N lignes` | `… +N lignes` |
| 9 | Markdown incomplet (stream) | Table/liste coupée au milieu | Rendu best-effort, pas de panic (pulldown ferme implicitement) | - |
| 10 | Langage de code inconnu | Code-block sans langage / langage non supporté | Texte neutre indenté, pas de coloration, pas d'erreur | - |
| 11 | `tool_result` orphelin | id sans `tool_call` correspondant | Affichage générique dégradé sans panic | - |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Re-render intégral à chaque frame + diff/syntaxe → CPU élevé (cf. opencode #811) | High | High | Cache des lignes baked (US-041) **avant** d'introduire syntect (US-042) ; l'ordre des dépendances l'impose ; NFR perf mesurée |
| 2 | Dérive vers une « réplique colorée de Claude Code » diluant l'ADN monochrome | Med | Med | Règle de teinte stricte (couleur = fonctionnel) codée dans `theme.rs` (US-032) + audit de teinte dans les critères |
| 3 | Poids binaire / toolchain C de syntect | Med | Med | Spike (US-040) ; défaut `fancy-regex` (pur Rust) ou `synoptic` (léger) |
| 4 | Numéros de diff relatifs perçus comme trompeurs vs captures Claude Code | Low | Med | Numéroter le nouveau fichier (honnête) ; option absolue documentée (enrichir retour `Edit`) si le dogfood le réclame |
| 5 | Markdown streamé coupé au milieu → rendu cassé (cf. Gemini CLI) | Med | Med | pulldown-cmark ferme implicitement (déjà acquis) ; tests sur markdown incomplet (US-043) |
| 6 | Largeur unicode/CJK mal calculée (`chars().count()` actuel) | Low | Med | Adopter `unicode-width` pour les calculs de largeur de troncature/gouttière |

## Non-Goals

Frontières explicites, ce que cette version ne fait PAS :

- **Migration vers `insert_before` / scrollback natif** (modèle Codex) : on conserve le viewport + scroll interne actuel (alt-screen). Refonte d'architecture distincte, hors-scope ; revisitée seulement si le scroll interne devient limitant.
- **Sortie d'outils colorée et expansible** (ctrl-o, ansi-to-tui sur la sortie bash) : on garde le résumé `⎿` ; l'affichage expansible de la sortie complète est un Could Have ultérieur.
- **Toggle rendu markdown / brut** (alt+m) : filet de sécurité noté (cas Gemini CLI), différé.
- **`/diff` interactif par tour** (vue dédiée multi-fichiers façon Claude Code) : hors-scope ; le diff inline suffit pour le MVP de rendu.
- **Tier 256-couleurs intermédiaire** : on reste sur truecolor OU 16-couleurs (suffisant sur la cible Linux/Fedora).
- **Refonte de l'input, du menu slash, de l'écran d'accueil, du dialog de permission (hors diff)** : déjà soignés, préservés tels quels.
- **Changement des contrats `agent-core`/`agent-tools`** : explicitement interdit (numéros de diff absolus exclus pour cette raison).

## Files NOT to Modify

- `crates/agent-core/src/event.rs` - contrat `AgentEvent` (structuré, jamais d'ANSI) ; invariant d'architecture.
- `crates/agent-core/src/message.rs` - types canoniques de message ; compat resume.
- `crates/agent-tools/src/*.rs` - contrats d'outils (`ToolOutput` plat) ; le rendu dérive tout de l'`input`/`content`, on ne change pas les outils.
- `crates/agent-tui/src/term.rs` - setup terminal (raw mode, alt screen, détection truecolor) ; ne pas toucher (sauf si un tier couleur était ajouté, hors-scope).
- `crates/agent-cli/src/interactive.rs` - modifiable UNIQUEMENT pour ajouter le tick timer du spinner (US-044) ; ne pas toucher l'orchestration, la boucle d'objectif `/goal`, ni la gestion des permissions/MCP.

## Technical Considerations

Questions pour l'engineering (recommandations, pas mandats) :

- **Architecture scroll:** garder viewport + scroll interne (actuel) vs `insert_before` (Codex). Recommandé : **garder l'actuel** ; l'alt-screen, l'écran d'accueil et le menu overlay en dépendent. `insert_before` = évolution future hors-scope.
- **Lib de diff:** `similar` 2.7 recommandé (API `iter_inline_changes` + `grouped_ops`, aligné Codex). Alternative : `imara-diff` (≈30× plus rapide mais orienté lignes) seulement si un goulot perf réel apparaît sur de très gros fichiers.
- **Moteur de coloration:** `synoptic` (léger, pur Rust, à configurer) vs `syntect` 5 + `fancy-regex` (fidèle, pur Rust, +~1-2 Mo) vs `syntect` + `onig` (rapide, dépendance C). Recommandé : trancher par spike US-040, défaut `fancy-regex` (binaire distribuable sans toolchain C). Caveat dur : la coloration ne tourne jamais par frame → cache.
- **Numéros de diff:** relatifs au nouveau fichier (dérivés de l'input, 0 changement de contrat) vs absolus (enrichir le retour `Edit` avec l'offset). Recommandé : **relatif d'abord**.
- **Cache:** clé d'invalidation = (identité du bloc + version de contenu + largeur de rendu) ; invalider sur resize et sur édition de bloc. À arbitrer : `Paragraph` + `Wrap` natif (simple) vs wrap manuel (contrôle total, requis si on veut gouttière de diff + numéros parfaitement alignés sur les continuations wrappées).
- **Largeur unicode:** adopter `unicode-width` / segmentation par graphème pour les troncatures et la gouttière (vs `chars().count()` actuel).
- **Dépendances nouvelles:** `similar` (EP-011) ; un moteur de coloration `synoptic` OU `syntect`+`fancy-regex` (EP-012, après spike). Aucune autre.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Éléments de rendu Claude Code répliqués | 0/6 | 6/6 (puce, `⎿`, diff coloré, syntaxe, spinner, durée+tokens) | Month-1 | Revue visuelle manuelle |
| Latence de frame (transcript 200 blocs) | N/A (à établir au bench) | < 16 ms p95 | Month-1 | Bench `TestBackend` / instrumentation |
| CPU idle (session active, hors stream) | re-render actuel (à mesurer) | < 5 % | Month-1 | `top`/`perf` pendant idle |
| Edits/writes avec diff inline coloré | 0 % | 100 % des réussis | Month-1 | Dogfood |
| Tests de rendu | suite actuelle | +≥1 par story, 0 panic sur les 11 edge cases | Month-1 | `cargo test --workspace` |
| Bascule vers Codex App pour raison de rendu | fréquente | « plus jamais pour des raisons de rendu » | Month-6 | Dogfood qualitatif |

## Open Questions

- Faut-il rendre la sortie complète des outils de lecture expansible (façon ctrl-o) ou le résumé `⎿` suffit-il ? Arthur, à trancher à l'usage ; impacte un éventuel besoin de `ansi-to-tui`.
- Les numéros de ligne absolus sont-ils nécessaires à la parité visuelle, ou le relatif est-il acceptable ? À valider en dogfood après EP-011 ; conditionne un enrichissement du retour `Edit`.
- Un toggle markdown rendu/brut (alt+m) apporte-t-il de la valeur pour Pyxis ? À évaluer après EP-012 (filet de sécurité observé sur Gemini CLI).
- Quel seuil exact de durée avant d'afficher les indicateurs de progression (US-045) pour éviter le clignotement sur tours courts ? À calibrer en dogfood.
[/PRD]
