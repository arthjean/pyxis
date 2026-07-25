[PRD]
# PRD: Pyxis — Orchestration des modèles Codex (post-MVP)

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-06-17 | Arthur Jean | Initial draft — leviers calibrés Codex issus de l'audit Claude Code + Pi + Codex CLI + run live |

> **Statut : PRD implémenté, avec deux nuances postérieures.** Le run live cité dans ce document est pré-rename ; `originator=pyxis` reste à revalider. L'écart AC3 sur l'estimation tokens a été corrigé ensuite par EP-009/US-030 (`estimate_static_input` + `static_input_tokens`). Voir `docs/codex-wire-spike.md`, `tasks/prd-codex-orchestration-status.json` et `docs/CURRENT_STATUS.md`.

## Problem Statement

Le MVP de Pyxis (PRD `prd-pyxis.md`, EP-001→EP-005) livre un agent de code fonctionnel : la boucle, la compaction en cascade, les garde-fous, le sandbox et le wire Responses API (backend ChatGPT/Codex, SSE stateless) tournent et ont été validés en live le 2026-06-17. Mais Pyxis **sous-exploite GPT-5.5 par rapport à Codex App et Codex CLI**, et c'est exactement le pain point que le projet vise (orchestrer Codex mieux que les autres harness). Un audit en deux passes (Claude Code comme référence de qualité, puis dissection de Pi et du Codex CLI officiel comme références Codex) a isolé des écarts concrets :

1. **Le wire n'est pas durci.** Aucun timeout sur le stream SSE (`agent-provider/src/chatgpt.rs:65,145`) : un backend silencieux (proxy, queue) gèle la boucle indéfiniment, sans erreur ni signal à la TUI. Les 429 « quota épuisé » (`GoUsageLimitError`/`FreeUsageLimitError`) sont retryés comme transitoires. Le header `Retry-After` est ignoré.
2. **L'édition échoue sur des divergences triviales.** `Edit` fait un match exact byte-pour-byte (`agent-tools/src/edit.rs:79`). Or GPT-5.x génère ses patches depuis sa mémoire de conversation, pas une lecture fraîche : un NBSP, un tiret typographique ou un guillemet auto-corrigé dans l'ancre fait échouer l'edit, et l'agent boucle (retry + relecture). Pi et Codex CLI absorbent ces cas via une normalisation Unicode à 4 passes.
3. **Le modèle travaille à l'aveugle.** Le system prompt est statique (2 phrases, `agent-cli/src/main.rs:30-32`). Aucune injection d'AGENTS.md, ni de contexte d'environnement (cwd, date), ni d'instructions calibrées pour un modèle GPT-5.x générique (le prompt long type `gpt_5_2` que le Codex CLI réserve aux slugs non fine-tunés). Le double-`Read` observé en live illustre l'absence de feedback outil guidant le modèle.
4. **Le cache backend et les sessions longues sont sous-optimisés.** `prompt_cache_key` (que Pi envoie sur chaque requête) est absent : chaque tour paie le cache miss plein. La compaction n'a ni garde anti-double-résumé (`SUMMARY_PREFIX`) ni baseline post-compaction ancrée sur l'`usage` réel, exposant à la double-compaction.

**Why now:** Le MVP est livré et le canal d'auth abonnement ChatGPT fonctionne (run live concluant). Le pivot Codex est acté (ADR-11). L'audit est terminé et a produit une liste de leviers précis, sourcés dans le code des harness Codex concurrents. C'est la fenêtre pour transformer un agent « qui marche » en « le harness le plus puissant pour Codex » avant d'élargir le scope (multi-provider, sous-agents).

## Overview

Ce PRD formalise les améliorations d'orchestration **strictement ciblées sur les modèles Codex/GPT-5.5** via la Responses API (backend ChatGPT, SSE stateless). Il ne touche ni au multi-provider ni aux sous-agents (hors scope explicite). Chaque story est calibrée sur ce que font réellement Pi et le Codex CLI officiel, pas sur une transposition de Claude Code (dont le context engineering est optimisé pour un autre modèle).

La solution s'organise en quatre épics : (EP-006) durcir le wire et la persistance de session pour qu'une session tienne des heures sans geler ni perdre d'état ; (EP-007) rendre l'édition fidèle au mode de génération de Codex (patch depuis la mémoire) et enrichir le feedback outil pour éliminer les tours de correction gaspillés ; (EP-008) donner au modèle le scaffold comportemental qu'il attend (system prompt long type `gpt_5_2`) et le contexte projet (AGENTS.md + environnement) injectés correctement pour la Responses API stateless ; (EP-009) activer le prompt cache backend et durcir la compaction pour les sessions longues.

Décisions structurantes prises pendant l'audit : le transport reste **SSE stateless** (validé : le Codex CLI officiel est lui-même SSE-only, pas de WebSocket) ; les reasoning items restent **droppés à la compaction** (conforme à `should_keep_compacted_history_item` du Codex CLI — contrainte de protocole, pas un choix) ; le reasoning replay complet est rétrogradé en P2 (gain de cache marginal, jamais bloquant). AGENTS.md est injecté comme **message `user` contextuel** rechargé par tour, pas dans `instructions`, pour rester correct en stateless.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Fiabilité du wire (aucun gel silencieux) | 0 tour gelé > 60 s sans erreur émise | 0 régression ; idle timeout configurable |
| Fidélité d'édition (ancres à divergence Unicode) | 100 % des edits Unicode-divergents réussis (vs ~0 % aujourd'hui) | < 2 % d'edits rejetés toutes causes confondues sur sessions réelles |
| Contexte projet fourni au modèle | AGENTS.md injecté sur 100 % des sessions où le fichier existe | Hiérarchie multi-répertoires + env diffé par tour |
| Économie de contexte / coût | prompt_cache_key actif ; 0 relecture post-edit gratuite | Cache hit observable dans l'`usage` backend sur tours répétés |

## Target Users

### Arthur Jean — créateur & dogfooder principal
- **Role:** Solo indie maker, orchestre Codex via Pyxis au quotidien.
- **Behaviors:** Sessions longues d'orchestration de code (refactors, audits), abonnement ChatGPT/Codex, Fedora/Wayland, full Rust.
- **Pain points:** Pyxis « marche » mais reste en deçà de Codex App : édition qui boucle sur des divergences d'encodage, modèle qui ne connaît pas le repo, pas de feedback de gel.
- **Current workaround:** Bascule sur Codex App/CLI pour les tâches sérieuses ; Pyxis reste un prototype perso.
- **Success looks like:** Pyxis devient son harness Codex par défaut, plus fluide et contrôlable que les alternatives.

### Développeur Rust / systèmes — early adopter OSS
- **Role:** Dev qui essaie Pyxis depuis le repo GPL-3.0.
- **Behaviors:** Lit le code, attend une archive propre et des invariants clairs ; teste sur ses propres repos avec AGENTS.md.
- **Pain points:** Un harness Codex tiers qui dégrade GPT-5.5 (edits ratés, pas de contexte) est inutilisable ; il retourne au Codex CLI officiel.
- **Current workaround:** Codex CLI officiel.
- **Success looks like:** Pyxis tient la comparaison avec le CLI officiel sur l'édition et le contexte, avec un cœur Rust plus rigoureux.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- **Codex CLI officiel (openai/codex, Rust)** : référence directe. SSE-only (pas de WS), system prompts versionnés par slug (`core/gpt_5_2_prompt.md` ~300 lignes pour génériques, `*_codex_prompt.md` ~69 lignes pour fine-tunés), `apply_patch` avec localisation à 4 passes (`seek_sketch`/`seek_sequence`), AGENTS.md injecté comme message `user` rechargé par tour, `EnvironmentContext` XML par tour, compaction native (`/responses/compact`, `CompactionTrigger`, `AutoCompactWindow`, `SUMMARY_PREFIX`). On diffère par un cœur Rust à state machine typée et des garde-fous déterministes.
- **Pi (TS, `/home/arthur/dev/pi`)** : source du wire Pyxis. Envoie `prompt_cache_key` (clamp 64 code-points), edit fuzzy (même table Unicode que Codex CLI), troncation tail/head avec hint de continuation, compaction 7-sections (car summarizer Anthropic généraliste), file de mutations par `realpath`, distinction des 429 terminaux. On diffère par le keyring (vs `auth.json` clair) et l'absence de WebSocket.
- **Market gap:** GPT-5.5 Codex est excellent dans Codex App mais dégradé dans les harness tiers (Codex CLI, OpenCode, Pi) sur l'ergonomie et le contrôle. Pyxis vise le cœur Rust le plus rigoureux **avec** la fidélité Codex de l'officiel.

### Best Practices Applied
- Édition fuzzy 4 passes (exact → trim_end → trim → normalisation Unicode) — partagée par Pi et Codex CLI, motivée par le fait que le modèle patche de mémoire.
- System prompt long pour modèles génériques GPT-5.x (AGENTS.md spec, Autonomy, Preamble, plan) ; injection AGENTS.md + environnement comme messages `user` rechargés (Responses API stateless).
- Wire : `connect_timeout` + idle timeout per-event, `prompt_cache_key`, distinction 429 terminaux, `Retry-After` honoré, sérialisation du body une seule fois.
- Compaction : `SUMMARY_PREFIX` guard, baseline post-compaction ancrée sur l'`usage` réel, drop des reasoning items (contrainte protocole).

*Full research sources available in `docs/openai-subscription-auth.md`, `docs/PROVIDERS.md`, et les transcripts d'audit (workflows Claude-Code-vs-Pyxis et Codex-harness-dissection).*

## Assumptions & Constraints

### Assumptions (to validate)
- **Le backend Codex accepte `originator: pyxis`** — à valider en US-021 (spike live). Évidence : Pi utilise `pi`, le Codex CLI `codex`/`codex_cli_rs` ; le backend *peut* valider contre une liste. Fallback documenté : emprunter `codex_cli_rs`.
- **`gpt-5.5` se comporte comme un modèle GPT-5.x générique (non fine-tuné Codex)** et bénéficie du prompt long type `gpt_5_2` — le slug n'a pas le suffixe `-codex`. À confirmer empiriquement (US-027) ; défaut sûr = prompt long.
- **La table de normalisation Unicode de Codex CLI/Pi couvre 99 %+ des divergences observées** — reprise verbatim (dashes U+2010-2015/2212, quotes typographiques, NBSP).
- **`prompt_cache_key` est honoré par le backend ChatGPT pour l'abonnement** (Pi l'envoie systématiquement). Mesurable via l'`usage.input_tokens` sur tours répétés.

### Hard Constraints
- **Scope strictement Codex/GPT-5.5** : pas de multi-provider, pas de sous-agents, pas de MCP-tools-dans-la-boucle dans ce PRD.
- **Transport SSE stateless uniquement** (ADR-10/11). Pas de WebSocket ni `previous_response_id`. Décision validée par le Codex CLI officiel.
- **Reasoning items droppés à la compaction** (contrainte de protocole Responses API) — ne pas les réinjecter dans un historique compacté.
- **Licence GPL-3.0-or-later**, lints clippy obligatoires (`panic`/`unimplemented`/`dbg` = deny ; `unwrap`/`expect` = warn).
- **Architecture en crates `agent-*`** : `agent-core` ne dépend ni de `agent-tui` ni de `agent-provider` (testable headless). Émettre uniquement des `AgentEvent`, jamais d'ANSI.
- **Compatibilité resume** : le format JSONL des sessions ne doit pas casser les sessions existantes.

## Quality Gates

These commands must pass for every user story:
- `cargo check --workspace` - compilation de tout le workspace
- `cargo clippy --workspace --all-targets` - lints (hook déterministe : `panic`/`unimplemented`/`dbg_macro` bloquants)
- `cargo test --workspace` - suite de tests (chaque story ajoute ses tests unitaires ; `agent-core` testable headless via doubles injectés)

Pour US-021 (spike live) : vérification manuelle d'un run réel `pyxis -p "<prompt forçant un tool>" --yes` contre le backend Codex, plus inspection du transcript JSONL produit.

## Epics & User Stories

### EP-006: Robustesse du wire & de la session Codex

Durcir le canal Responses API et la persistance pour qu'une session Codex tienne des heures sans geler, sans retry inutile, et sans perdre d'état au resume.

**Definition of Done:** Aucun gel silencieux du stream ; les 429 terminaux ne sont jamais retryés ; `Retry-After` honoré ; le tour assistant final est persisté ; le comportement du header `originator` est tranché en live.

#### US-021: Spike — valider le comportement wire en live (originator + cycle reasoning/tool)
**Description:** As a dogfooder, I want valider en live le comportement du backend Codex sur `originator=pyxis` et sur un cycle multi-tour avec outil, so that les décisions wire reposent sur des faits et non des hypothèses.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un run `pyxis -p` forçant au moins 2 tours modèle avec un outil entre, when il s'exécute, then le cycle se termine sans 400 et le transcript contient `user → assistant(tool_use) → tool(tool_result)`.
- [ ] Given `originator=pyxis`, when une requête est envoyée, then on consigne si le backend l'accepte ou le rejette ; en cas de rejet (unhappy path), le fallback `codex_cli_rs` est documenté et testé.
- [ ] Given le run, when l'`usage.input_tokens` réel revient, then on consigne l'écart avec l'estimation locale `HeuristicCounter` pour calibrer la marge de compaction.
- [ ] Le verdict (originator OK/KO, écart tokenizer) est écrit dans `docs/` ou en commentaire d'ADR.

#### US-022: Watchdog SSE — connect timeout + idle timeout per-event
**Description:** As a user en session longue, I want que le provider détecte un backend silencieux, so that la boucle ne gèle jamais sans signal.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un backend qui n'établit pas la connexion, when 20 s s'écoulent, then `reqwest` échoue via `connect_timeout(20s)` et l'erreur est classifiée `Retryable`.
- [ ] Given un stream SSE ouvert qui n'émet plus d'event, when `idle_timeout` (défaut 60 s, configurable) s'écoule sans event, then la consommation `es.next()` est annulée via `tokio::time::timeout` et un `ProviderError::Stream("idle timeout")` est propagé.
- [ ] Given l'idle timeout, when il se déclenche, then l'erreur remonte comme `Retryable` et la boucle agent retry selon le backoff (pas de gel, un `AgentEvent::Error` est émis si les retries sont épuisés).
- [ ] Test : un mock de stream qui se bloque déclenche l'idle timeout en `< idle_timeout + marge`.

#### US-023: Taxonomie 429 terminaux + Retry-After honoré
**Description:** As a user, I want que Pyxis distingue un quota épuisé d'une surcharge transitoire et respecte le délai serveur, so that une session ne grille pas ses tentatives ni ne harcèle un compte bloqué.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une réponse 429 dont le corps JSON porte `GoUsageLimitError`/`FreeUsageLimitError`/billing, when elle est classifiée, then elle devient `InvalidRequest`/`Auth` (terminal, jamais retryée) avec un message explicite.
- [ ] Given une réponse 429/503 transitoire, when elle est reçue, then elle reste `RateLimited`/`Overloaded` et est retryée.
- [ ] Given un header `Retry-After` ou `retry-after-ms`, when présent sur un 429/503, then le délai de backoff utilisé = `max(backoff_exponentiel, retry_after)` (ms exact prioritaire).
- [ ] `ProviderError::Http` transporte `retry_after_ms: Option<u64>` ; tests couvrant entier-secondes, date HTTP, et `retry-after-ms`.

#### US-024: Persistance du tour assistant final (sync post-EndTurn)
**Description:** As a user qui reprend une session, I want que la dernière réponse de l'agent soit persistée, so that `/resume` ne perde jamais le dernier message assistant.

**Priority:** P1
**Size:** XS (1 pt)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un tour qui se termine en `EndTurn` après un message assistant, when la boucle retourne, then `session.sync(&messages)` est appelé une dernière fois avant le `return` (le message final est dans le JSONL).
- [ ] Given un crash simulé juste après `EndTurn`, when on resume, then le dernier message assistant est présent dans le transcript reconstruit (unhappy path).
- [ ] Le sync final reste idempotent (pas de doublon si déjà syncé).

---

### EP-007: Fidélité d'édition & feedback des outils

Aligner l'édition sur le mode de génération de Codex (patch depuis la mémoire) et enrichir le retour des outils pour éliminer les tours de correction gaspillés.

**Definition of Done:** `Edit` absorbe les divergences Unicode courantes ; les sorties tronquées guident le modèle vers la suite ; les invariants comportementaux critiques sont co-localisés avec chaque outil.

#### US-025: Edit fuzzy matching à 4 passes (seek_sequence)
**Description:** As a Codex orchestrator, I want que `Edit` localise l'ancre avec tolérance, so that un patch généré de mémoire ne soit pas rejeté pour un NBSP ou un tiret typographique.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une ancre `old_string` qui ne matche pas exact mais matche après `trim_end`, puis `trim`, puis normalisation Unicode, when `Edit` s'exécute, then l'edit réussit en appliquant le remplacement sur les offsets originaux du fichier (contenu non-cible intact).
- [ ] Given une ancre normalisée qui apparaît exactement une fois après normalisation, when elle apparaissait plusieurs fois en exact, then la résolution reste déterministe et documentée.
- [ ] Given une ancre toujours ambiguë (≥ 2 occurrences) ou introuvable après les 4 passes, when `Edit` s'exécute, then il est rejeté avec un message demandant plus de contexte (unhappy path, aucune mutation).
- [ ] Le `ToolOutcome` indique le niveau de passe utilisé (ex. « niveau 4 : normalisation Unicode ») pour l'observabilité.
- [ ] Table Unicode couvrant au minimum : dashes U+2010-U+2015 et U+2212 → `-`, quotes U+2018/U+2019/U+201C/U+201D → ASCII, NBSP U+00A0 → espace.

#### US-026: Feedback outils riche — troncation tail bash, hints de continuation, guidelines par outil
**Description:** As a Codex orchestrator, I want des sorties d'outils qui préservent l'information critique et guident la pagination, so that le modèle ne hallucine pas le contenu manquant ni ne perde les erreurs.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une sortie `Bash` dépassant le seuil, when elle est tronquée, then la **fin** est conservée (tail) avec un préfixe `[... sortie tronquée, N octets, début omis]` (les erreurs et l'exit code, en queue, restent visibles).
- [ ] Given une lecture `Read` hors plage ou tronquée, when elle est retournée, then elle inclut un hint `[lignes X-Y sur Z ; offset=Y+1 pour continuer]` plutôt qu'un rejet sec.
- [ ] Given un fichier > 2 Mo, when `Read` est appelé, then il retourne une lecture partielle avec hint de pagination (au lieu de `Rejected`).
- [ ] Given un `Grep` tronqué à la limite, when retourné, then il signale `truncated` et le moyen de paginer.
- [ ] Le trait `Tool` expose `behavioral_guidelines(&self) -> &[&'static str]` (défaut vide) ; `Edit` y déclare au minimum « `old_string` est cherché dans le fichier original, pas après d'autres edits du même tour ». Ces guidelines sont collectées et injectées dans le system prompt.

---

### EP-008: Contexte & instructions calibrés Codex

Donner au modèle le scaffold comportemental qu'il attend et le contexte projet, injectés correctement pour la Responses API stateless.

**Definition of Done:** Le system prompt s'adapte au slug ; AGENTS.md et l'environnement sont injectés comme messages `user` rechargés par tour ; le modèle connaît le repo, le cwd et la date.

#### US-027: System prompt calibré Codex + dispatcher slug → instructions
**Description:** As a Codex orchestrator, I want un system prompt long type `gpt_5_2` sélectionné selon le slug, so that GPT-5.5 (générique) reçoive la spec AGENTS.md, l'autonomie, les preamble messages et la guidance d'édition qu'il attend.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un slug générique (`gpt-5.*` sans `-codex`), when le system prompt est construit, then le prompt long (sections : AGENTS.md spec, Autonomy & Persistence, Responsiveness/preamble, guidance outils, instruction anti-relecture post-edit) est utilisé.
- [ ] Given un slug fine-tuné (`*-codex`), when le prompt est construit, then un prompt court est utilisé.
- [ ] Given un changement de modèle via `/models`, when le slug change, then `compose_system` est recalculé avec le bon template (pas seulement `cfg.model`).
- [ ] Given un slug inconnu (unhappy path), when le prompt est construit, then on retombe sur le prompt long par défaut (sûr).
- [ ] Le prompt embarque l'instruction anti-relecture : « ne relis pas un fichier après un `edit`/`write` réussi ; relis seulement si le tool a retourné une erreur ».
- [ ] Les templates sont embarqués via `include_str!` dans `agent-cli`.

#### US-028: Injection AGENTS.md (message user) + bloc environnement par tour
**Description:** As a Codex orchestrator, I want qu'AGENTS.md et l'environnement (cwd, date, shell) soient injectés comme messages `user` rechargés par tour, so that le modèle connaisse les conventions du repo et son contexte d'exécution malgré le stateless.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un workspace contenant `AGENTS.md`, when un tour démarre, then le contenu est injecté comme message `user` avec marqueurs `# AGENTS.md instructions` / `<INSTRUCTIONS>cwd: …</INSTRUCTIONS>`, découvert en remontant de cwd jusqu'au marqueur `.git`.
- [ ] Given des AGENTS.md à plusieurs niveaux, when découverts, then ils sont concaténés ordre parent → cwd (le plus proche prime), sous un budget d'octets borné.
- [ ] Given l'absence d'AGENTS.md (unhappy path), when un tour démarre, then aucun message n'est injecté et aucune erreur n'est levée ; un fallback `CLAUDE.md` est toléré.
- [ ] Given chaque tour, when la requête est construite, then un bloc `<environment><cwd/><shell/><current_date/><timezone/></environment>` est injecté comme message `user` (date fournie par le harness).
- [ ] L'injection ne pollue ni `instructions` (system) ni le transcript persistant de façon dupliquée à chaque tour (rechargé, pas accumulé).

---

### EP-009: Cache backend & compaction durcie

Activer le prompt cache du backend et fiabiliser la compaction pour les sessions Codex longues.

**Definition of Done:** `prompt_cache_key` envoyé ; la compaction ne se double pas ni ne re-résume ses propres résumés ; le reasoning replay est disponible mais isolé (P2).

#### US-029: prompt_cache_key clampé 64 code-points
**Description:** As a user, I want un `prompt_cache_key` stable par session, so that le backend ChatGPT réutilise son cache et réduise latence et tokens d'entrée sur les tours répétés.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une session, when le provider est créé, then un `session_id` stable (UUID v4) est généré et conservé.
- [ ] Given chaque requête, when le body est construit, then `prompt_cache_key` = `session_id` clampé à 64 **code-points** Unicode-safe (pas d'octets) est présent.
- [ ] Given une clé déjà ≤ 64 code-points, when clampée, then elle est inchangée (boundary value).
- [ ] Mesure consignée : l'`usage.input_tokens` sur un 2ᵉ tour identique est ≤ celui du 1ᵉʳ (cache hit observable), ou le résultat est documenté si le backend ne l'honore pas.

#### US-030: Durcissement de la compaction (SUMMARY_PREFIX guard + baseline post-compaction + budget + MidTurn)
**Description:** As a user en session longue, I want une compaction qui ne se double pas et ne dégrade pas ses résumés, so that le contexte opérationnel survive à plusieurs cycles.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un transcript déjà compacté, when une recompaction se déclenche, then `is_summary_message()` (test du `SUMMARY_PREFIX`) exclut l'ancien résumé du nouveau prompt de résumé (pas de résumé de résumé).
- [ ] Given une compaction réussie, when le premier `usage.input_tokens` réel revient, then il devient le baseline (`prefill`) et le seuil d'auto-compaction mesure `current - prefill` jusqu'au prochain cycle (pas de double-compaction immédiate).
- [ ] Given un seuil franchi pendant un tour à long tool_result, when le message assistant vient d'être accumulé, then un check MidTurn peut déclencher la compaction avant de relancer un tour d'outils.
- [ ] `max_output_tokens` du summarizer porté de 1024 à 4096 ; les blocs `Thinking` explicitement strippés avant l'appel summarizer (décision documentée).
- [ ] Given un échec de compaction (unhappy path), when il survient, then le circuit breaker existant reste actif (pas de boucle).

#### US-031: Reasoning replay isolé (préservation id fc/rs + cache) — basse priorité
**Description:** As a perf-conscious user, I want optionnellement réinjecter les reasoning items chiffrés dans les fenêtres non-compactées, so that le backend réutilise son raisonnement (gain cache/continuité) sans risquer de 400.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-029

**Acceptance Criteria:**
- [ ] Given un `response.output_item.done` de type `reasoning`, when reçu, then son `encrypted_content` et son `id` sont capturés dans un `ContentBlock::EncryptedReasoning { id, encrypted_content }`.
- [ ] Given un message assistant avec reasoning + function_call dans la même génération, when réémis dans `input[]`, then le reasoning item est réémis **avant** son function_call apparié (paire `rs`/`fc` cohérente, pas de 400).
- [ ] Given un tour avorté laissant un reasoning orphelin (unhappy path), when l'historique est reconstruit, then le reasoning orphelin est **sauté** (pas de 400 « orphaned reasoning item »).
- [ ] Given un changement de modèle via `/models` (unhappy path), when le nouveau slug diffère, then les reasoning items du modèle précédent ne sont pas réinjectés (convertis/droppés).
- [ ] Given une compaction, when elle s'exécute, then les reasoning items sont droppés (conforme US-030, contrainte protocole).
- [ ] Feature isolée derrière un flag : désactivable sans régression du chemin « à plat » actuel (validé en live comme sûr).

## Functional Requirements

- FR-01: Le provider Codex doit appliquer un `connect_timeout` (20 s) et un idle timeout per-event (60 s configurable) sur le stream SSE.
- FR-02: Le provider doit distinguer les 429 terminaux (codes `GoUsageLimitError`/`FreeUsageLimitError`) des transitoires et ne jamais retryer les terminaux.
- FR-03: La boucle doit honorer `Retry-After`/`retry-after-ms` quand présent, en prenant `max(backoff, retry_after)`.
- FR-04: La boucle doit persister le message assistant final avant de retourner sur `EndTurn`.
- FR-05: `Edit` doit localiser l'ancre via 4 passes (exact, trim_end, trim, normalisation Unicode) et appliquer le remplacement sur les offsets originaux.
- FR-06: Les outils doivent tronquer `Bash` par la fin (tail), fournir des hints de continuation sur `Read`/`Grep`, et exposer des guidelines comportementales par outil injectées dans le system prompt.
- FR-07: Le system prompt doit être sélectionné par slug (long pour génériques GPT-5.x, court pour `*-codex`) et recomposé au changement de modèle.
- FR-08: AGENTS.md (découvert cwd → `.git`) et un bloc environnement XML doivent être injectés comme messages `user` rechargés par tour.
- FR-09: Chaque requête doit inclure `prompt_cache_key` = `session_id` clampé 64 code-points.
- FR-10: La compaction doit marquer ses résumés (`SUMMARY_PREFIX`), exclure les anciens résumés du re-résumé, et ancrer son seuil sur le premier `usage` réel post-compaction.
- FR-11: Le système ne doit PAS réinjecter de reasoning item dans un historique compacté, ni réémettre un reasoning orphelin.

## Non-Functional Requirements

- **Performance:** démarrage TUI < 500 ms ; overhead d'injection AGENTS.md + env < 50 ms par tour ; le fuzzy edit ajoute < 5 ms sur un fichier de 2 000 lignes.
- **Reliability:** 0 gel de boucle > 60 s sans `AgentEvent::Error` émis ; backoff exponentiel avec jitter ±10 %, plafonné à 32× ; circuit breaker de compaction à N=3 échecs.
- **Fidélité d'édition:** 100 % des ancres à divergence Unicode courante (NBSP, dashes, quotes typographiques) résolues ; 0 mutation sur ancre ambiguë/introuvable.
- **Sécurité:** aucun secret loggé (clés clampées ≠ tokens) ; les en-têtes propriétaires restent issus du keyring ; le sandbox Landlock et le taint untrusted du MVP sont préservés (OWASP LLM01).
- **Compatibilité:** le format JSONL de session reste rétro-compatible (resume des sessions existantes intact) ; `agent-core` reste sans dépendance vers `agent-tui`/`agent-provider`.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Backend silencieux | Connexion établie, aucun event SSE | Idle timeout 60 s → erreur Retryable → retry → si épuisé, `AgentEvent::Error` | "Le modèle ne répond plus (timeout), nouvelle tentative…" |
| 2 | Quota épuisé | 429 `GoUsageLimitError` | Classé terminal, jamais retryé, session s'arrête proprement | "Quota d'abonnement épuisé — réessaie plus tard." |
| 3 | Ancre Unicode divergente | `old_string` avec NBSP/tiret typo | Résolu via passe 4, edit appliqué sur offsets originaux | — |
| 4 | Ancre introuvable/ambiguë | 0 ou ≥ 2 occurrences après 4 passes | Rejet sans mutation, demande de contexte | "Ancre ambiguë/introuvable : précise davantage." |
| 5 | AGENTS.md absent | Repo sans fichier | Aucune injection, aucune erreur ; fallback CLAUDE.md toléré | — |
| 6 | originator rejeté | Backend refuse `pyxis` | Spike détecte, fallback `codex_cli_rs` documenté | (consigné en audit) |
| 7 | Sortie bash volumineuse | Compilation avec 400 warnings + 3 erreurs | Tail conservé (erreurs visibles) + hint | "[sortie tronquée, début omis]" |
| 8 | Recompaction | 2ᵉ cycle de compaction | Ancien résumé exclu via SUMMARY_PREFIX guard | — |
| 9 | Reasoning orphelin | Tour avorté, reasoning sans function_call | Reasoning sauté (pas de 400) | — |
| 10 | Changement de modèle mid-session | `/models gpt-5.x` | Reasoning du modèle précédent non réinjecté ; system prompt recomposé | "Modèle changé : nouveau contexte appliqué." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | `originator=pyxis` rejeté par le backend | Med | High | Spike US-021 en premier ; fallback `codex_cli_rs` prêt |
| 2 | `gpt-5.5` est en réalité fine-tuné Codex → prompt long contre-productif | Low | Med | Défaut sûr = prompt long ; confirmer empiriquement, dispatcher trivial à ajuster |
| 3 | Table Unicode incomplète → edits encore rejetés | Low | Med | Reprise verbatim de Pi/Codex CLI (validée en prod) ; niveau de passe consigné pour diagnostic |
| 4 | `prompt_cache_key` non honoré par le canal abonnement | Med | Low | Mesure en US-029 ; bénéfice nul mais aucun coût/risque si ignoré |
| 5 | Wire format Codex dérive (pas de SDK officiel) | Med | High | Tests VCR sur payloads réels (Phase 3 roadmap) ; sources alignées sur Codex CLI officiel |
| 6 | Double-compaction sans baseline | Med | Med | US-030 ancre le seuil sur l'`usage` réel post-compaction |

## Non-Goals

Explicit boundaries — what this version does NOT include:

- **Multi-provider** (Anthropic, Gemini, OpenAI au token, OpenRouter) — hors scope ; le scope est strictement Codex/GPT-5.5 tant que Pyxis n'est pas parfait dessus.
- **Sous-agents / orchestration multi-agent** — déféré (Phase 2 roadmap) ; le pari est l'excellence single-agent sur Codex d'abord.
- **WebSocket + `previous_response_id`** — explicitement abandonné (le Codex CLI officiel est SSE-only ; casserait compaction/resume).
- **apply_patch format shell-heredoc** — Pyxis garde son `Edit` par ancre (rendu fuzzy) ; le format diff multi-op est une optimisation future, pas un prérequis.
- **Compaction native `/responses/compact` + `CompactionTrigger`** — primitive backend avancée, déférée ; la compaction LLM ordinaire suffit au scope.
- **Reasoning replay activé par défaut** — disponible (US-031) mais P2, derrière un flag, jamais le chemin par défaut.
- **MCP tools branchés dans la boucle modèle** — hors scope de ce PRD.

## Files NOT to Modify

- `crates/agent-auth/src/oauth/openai_chatgpt.rs` — flux OAuth validé en live ; ne pas casser le refresh rotatif ni l'extraction du `chatgpt_account_id`.
- `crates/agent-sandbox/src/fs.rs` — Landlock, sécurité kernel ; toute modification touche la surface d'attaque.
- `crates/agent-core/src/transition.rs` (enum `Transition`) — étendre par nouvelles variantes uniquement ; ne pas casser le `match` exhaustif ni l'invariant de la state machine.
- Format JSONL des sessions (`crates/agent-session/`) — rétro-compatibilité resume ; n'ajouter que des variantes optionnelles.
- `spikes/` — code jetable Phase 0, ne pas réintégrer.

## Technical Considerations

Frame as questions for engineering input — not mandates:

- **Watchdog SSE:** recommandé `connect_timeout` via `reqwest::ClientBuilder` + `tokio::time::timeout` par event. Idle par défaut 60 s — confirmer la valeur sur le comportement réel du backend (Pi : 20 s header ; Codex CLI : 300 s/event).
- **`ProviderError::Http`:** ajouter `retry_after_ms: Option<u64>` — confirmer qu'aucun consommateur existant ne casse (variante de struct).
- **`ContentBlock`:** ajouter `EncryptedReasoning { id, encrypted_content }` (US-031) — extension du canonique ; vérifier l'impact sur `Accumulator` et la sérialisation.
- **System prompt templates:** `include_str!` de fichiers `.md` embarqués dans `agent-cli` — recommandé (pattern Codex CLI). Où les ranger (`crates/agent-cli/prompts/`) ?
- **Injection per-turn:** AGENTS.md + env comme messages `user` rechargés. Trade-off cache : un contenu volatil par tour réduit le cache hit du `prompt_cache_key` ; mettre le contenu stable (AGENTS.md) avant le volatil (date) pour préserver le préfixe cacheable.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Tours gelés > 60 s sans erreur | non borné (aucun timeout) | 0 | Month-1 | run live + logs `AgentEvent::Error` |
| Edits Unicode-divergents réussis | ~0 % (match exact) | 100 % | Month-1 | tests unitaires + sessions réelles |
| Relectures post-edit gratuites | ~1 read/edit (observé live) | 0 | Month-1 | inspection transcript JSONL |
| Sessions avec contexte projet injecté | 0 % (pas d'AGENTS.md) | 100 % où le fichier existe | Month-1 | présence du message `# AGENTS.md instructions` |
| Cache hit sur tour répété | N/A (pas de cache_key) | `input_tokens` ≤ tour précédent | Month-6 | `usage.input_tokens` backend |
| Double-compaction immédiate | possible (baseline estimé) | 0 | Month-6 | logs de compaction |

## Open Questions

- `gpt-5.5` est-il traité comme générique ou fine-tuné Codex côté backend ? — tranché empiriquement en US-027/US-021 ; impacte le choix de template.
- Le backend honore-t-il `prompt_cache_key` sur le canal abonnement ? — mesuré en US-029 ; impacte le ROI de EP-009.
- Quelle valeur d'idle timeout colle au comportement réel du backend (queue, proxy d'entreprise) ? — calibré après US-021/US-022.
- Faut-il un format diff multi-op (apply_patch-like) à terme, ou le `Edit` fuzzy suffit-il ? — à réévaluer après mesure du taux d'edits multi-hunk sur sessions réelles.
[/PRD]
