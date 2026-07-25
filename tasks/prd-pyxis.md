[PRD]
# PRD: Pyxis — CLI agent IA de codage multi-provider (MVP)

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-06-15 | Arthur Jean | Draft initial — cadre le MVP (Phase 0 spikes + Phase 1) |

> **Statut : PRD historique pré-ADR-11.** Ce document conserve le cadrage initial du MVP (Ollama, OpenAI Chat Completions BYOK, Anthropic conditionnel). Le scope courant livré est supersedé par ADR-11 : `OpenAiChatGpt` d'abord, autres providers différés. Pour l'état actuel, voir `docs/CURRENT_STATUS.md`, `docs/DECISIONS.md` ADR-11 et `tasks/prd-pyxis-status.json`.

## Problem Statement

1. **Les CLI agents de codage de qualité sont enfermés sur un seul vendor.** Claude Code, le meilleur en UX, ne parle qu'aux modèles Anthropic. Les développeurs qui veulent cette qualité avec OpenAI, Gemini ou un modèle local doivent changer d'outil — et perdent la cohérence d'expérience.
2. **Le 4 avril 2026, Anthropic a coupé l'accès aux abonnements Pro/Max pour tout outil tiers** (confirmé par 9 sources). Les outils tiers basculés sur API key metered ont vu des coûts grimper jusqu'à ~50× ; certains utilisateurs ont été purement bloqués. Le lock-in d'auth est devenu un risque existentiel pour tout outil dépendant d'un seul canal subscription.
3. **Les CLI agents existants partagent des modes d'échec coûteux non maîtrisés** : boucles d'outils infinies (« le #1 fléau de l'agentique 2026 »), perte de contexte sur gros repos, coûts runaway (cas documenté : « refactor this » → facture API de 500 $ et 4000 commits), prompt injection indirecte via repos/MCP malveillants.
4. **Les CLI Node/TS paient une taxe de démarrage et de mémoire** (500 ms–2 s de startup, 200 Mo+ idle, runtime à embarquer) là où un binaire Rust natif démarre en <100 ms pour <50 Mo.

**Why now:** Le ban Anthropic du 4 avril 2026 transforme le model-agnosticism d'un confort en **moat structurel** : opencode a fait de « pas de lock-in » son argument central post-ban et a bondi de plusieurs dizaines de milliers de stars. La fenêtre narrative est ouverte maintenant.

## Overview

Pyxis est une CLI agent IA de codage en terminal, écrite en **Rust natif**, **multi-provider first-class** (BYOK : Anthropic, OpenAI, Gemini, Ollama/local, puis cloud), inspirée de l'architecture interne de Claude Code mais agnostique au modèle. La commande est `pyxis` ; elle s'ouvre dans le shell (frontend Ratatui monochrome), **pas** dans une fenêtre.

Le parti pris fondateur est un **cœur headless** (`agent-core`) qui n'émet que des événements structurés, jamais d'ANSI. Le frontend terminal n'est qu'un client : tout autre client peut embarquer `agent-core` in-process (types partagés, pas d'IPC) et consommer les mêmes events, sans casser le mode terminal par défaut.

Ce PRD couvre le **MVP** : une **Phase 0** de dé-risquage (5 spikes, dont le go/no-go d'accès provider) suivie d'une **Phase 1** livrant une boucle d'agent complète, le système d'outils avec garde-fous (loop guardrails, budgets, taint untrusted), 3 providers non-bloqués, l'auth BYOK, les sessions resumables, le frontend Ratatui et le sandbox Landlock FS. MCP, les providers cloud, le TUI riche, les sous-agents et le support multi-OS sont explicitement différés (voir Non-Goals).

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Providers frontier fonctionnels (BYOK) | 3 (Ollama, OpenAI, Anthropic) | 6+ (+ Gemini, OpenRouter, 1 cloud) |
| Latence de démarrage (`pyxis` → prompt prêt) | <100 ms (P95) | <100 ms maintenu, publié avec artefact |
| Adoption — GitHub stars (proxy de distribution) | 2 000–5 000 | 15 000–30 000 |
| Substance : sessions dogfood/jour (Arthur) | ≥1/jour | usage quotidien maintenu |
| Contributeurs externes (PR mergées) | ≥1 | ≥10 |

## Target Users

### Arthur Jean — créateur & dogfooder principal
- **Role:** Solo indie maker, mainteneur unique.
- **Behaviors:** Orchestre des agents (Claude Code, Codex) toute la journée ; code en Rust ; déteste npm/Node, préfère Bun et les binaires natifs.
- **Pain points:** Les agents TS sont lents à démarrer ; le ban Anthropic l'a touché directement (Max 20×).
- **Current workaround:** Utilise plusieurs CLI tierces côte à côte.
- **Success looks like:** `pyxis` démarre instantanément, parle à n'importe quel modèle, et il l'utilise tous les jours sur ses propres projets.

### Développeur Rust/systèmes — early adopter OSS
- **Role:** Dev backend/systèmes qui vit dans le terminal.
- **Behaviors:** BYOK, jongle entre providers selon coût/qualité, fuit le lock-in vendor.
- **Pain points:** Claude Code est Anthropic-only ; les alternatives OSS sont en TS/Go (lentes, lourdes) ; peur des coûts runaway et des boucles d'outils.
- **Current workaround:** opencode/aider en BYOK, en tolérant la lenteur et l'absence de garde-fous de coût.
- **Success looks like:** Un binaire unique, rapide, multi-provider, avec budgets et kill-switch fiables, sans surprise de facture.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- **opencode (~172k stars, ~6.5M MAU, MIT)** : standard OSS de facto, repositionné explicitement model-agnostic post-ban. Pyxis diffère par : Rust natif (perf mesurable), garde-fous de coût/boucle de première classe.
- **Codex CLI (~90k, Rust, Apache-2.0)** : meilleur Terminal-Bench (83,4 %), prouve la viabilité Rust. Pyxis diffère par : multi-provider BYOK ouvert (Codex est OpenAI-centré).
- **Claude Code (~131k, propriétaire, TS)** : référence UX, mais Anthropic-only et lock-in subscription. Pyxis reprend ses patterns internes en les ouvrant à tous les providers.
- **Market gap:** un agent CLI **Rust + BYOK multi-provider + garde-fous de coût/boucle de première classe + intégration terminal-natif profonde**, qu'aucun acteur ne combine en juin 2026.

### Best Practices Applied
- **BYOK multi-provider par défaut** (post-ban Anthropic) — modèle d'auth attendu, pas avancé.
- **Loop guardrails déterministes externes** qui overrident la logique faillible du modèle (détection de répétition, limites d'itérations).
- **Budgets de coût/tokens avec hard limits et kill-switch** — réponse directe aux coûts runaway.
- **Défense prompt-injection en profondeur** : permissions fail-closed, taint untrusted des sorties d'outils/MCP, confirmation humaine pour actions sensibles (les LLM ne distinguent pas instruction/donnée — contrôles architecturaux requis).
- **Streaming token-par-token sans buffering** ; le startup <100 ms est un delighter de DX (les appels LLM dominent le temps de session).

*Full research sources available in project documentation (`docs/`).*

## Assumptions & Constraints

### Assumptions (to validate)
- **L'auth provider du MVP est viable sans dépendre d'un abonnement bloqué** — à valider en US-001 (spike, go/no-go). Évidence : ban Anthropic confirmé, mais OpenAI/Ollama au token/local ne sont pas concernés.
- **Un binaire Rust atteint <100 ms de startup et <50 Mo RSS** — évidence : pi_agent_rust (source primaire) mesure <100 ms / <50 Mo / ~21 Mo binaire.
- **Le format canonique Anthropic-like mappe proprement OpenAI Chat Completions et Ollama** — à éprouver dès US-015/US-016/US-017.
- **Arthur peut tenir la vélocité Rust en solo sur le scope MVP** — hypothèse de risque (voir Risks).

### Hard Constraints
- **Full Rust**, workspace Cargo. Binaire publié = `pyxis` (crate interne `agent-cli`).
- **Pas de runtime Node/Bun embarqué** ; single static binary.
- **Cœur `agent-core` sans dépendance TUI ni HTTP** (testable headless, embarquable in-process).
- **Linux-first** pour le MVP (sandbox Landlock).
- **L'architecture est figée dans `docs/`** (ARCHITECTURE, PROVIDERS, ROADMAP, DECISIONS) — le PRD ne doit pas la contredire.
- **Lints clippy d'Arthur obligatoires** (`panic`/`unimplemented`/`dbg_macro` = deny ; `unwrap_used`/`expect_used` = warn).

## Quality Gates

These commands must pass for every user story:
- `cargo check --workspace --all-targets` - compilation de tout le workspace
- `cargo clippy --workspace --all-targets --no-deps` - lints ; `panic`/`unimplemented`/`dbg_macro` en deny doivent être absents, `unwrap_used`/`expect_used` (warn) à revoir avant merge
- `cargo nextest run --workspace` - suite de tests (fallback `cargo test --workspace`)
- `cargo fmt --all --check` - formatage

Pour les stories de frontend (US-019) : vérification visuelle manuelle dans un terminal truecolor — rendu monochrome épuré, streaming token-par-token fluide, aucune bordure ASCII lourde.

## Epics & User Stories

### EP-001: Dé-risquage (Phase 0 — spikes)

Valider les incertitudes qui peuvent tuer le projet avant d'investir dans le MVP. Chaque spike est jetable ; il prouve une hypothèse, pas une feature finie.

**Definition of Done:** Les 5 spikes ont un verdict écrit (passe / à réévaluer). Le go/no-go d'accès provider (US-001) est tranché. Aucune ligne du MVP n'est écrite avant.

#### US-001: Spike — valider l'accès provider (go/no-go auth)
**Description:** As a créateur, I want déterminer avec quelles credentials l'utilisateur final parle au modèle sans être bloqué, so that je ne construis pas le MVP sur un canal d'auth mort.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une API key OpenAI, when on lance un appel streaming réel, then la réponse est reçue et le coût est metered correctement.
- [ ] Given un serveur Ollama local, when on lance un appel, then la réponse streame sans credentials distantes.
- [ ] Given une tentative d'auth Anthropic via token d'abonnement, when l'appel est émis, then le blocage est observé et documenté (message exact capturé), confirmant que le MVP ne peut PAS en dépendre.
- [ ] Un verdict écrit fixe le(s) provider(s) du MVP. Si aucun canal viable n'existe, le projet est mis en pause (unhappy path).

#### US-002: Spike — provider canonique sur 1 stream SSE
**Description:** As a créateur, I want parser un flux SSE provider vers des `StreamEvent` canoniques via `reqwest` + `eventsource-stream`, so that je prouve que la couche maison tient sans SDK.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given un endpoint streaming du provider retenu, when le flux arrive, then les deltas de texte et les tool calls sont émis comme `StreamEvent` typés.
- [ ] Given un flux interrompu en milieu de message, when la connexion tombe, then l'erreur est classifiée (pas de panic) et propagée proprement (unhappy path).
- [ ] Given un chunk malformé, when il est reçu, then il est ignoré ou remonté en erreur typée sans crasher le parseur.

#### US-003: Spike — boucle minimale stream → outil → reboucle
**Description:** As a créateur, I want une boucle qui streame, exécute un outil Bash, réinjecte le résultat et reboucle, so that je prouve la state-machine + `async-stream`.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un prompt déclenchant un `tool_use` Bash, when la boucle tourne, then l'outil s'exécute et son résultat est réinjecté pour le tour suivant.
- [ ] Given un outil qui dépasse un timeout, when il bloque, then la boucle reprend la main et signale l'échec sans se figer (unhappy path).
- [ ] Given `stop_reason: end_turn`, when atteint, then la boucle se termine proprement.

#### US-004: Spike — rendu TUI streaming brut (Ratatui)
**Description:** As a créateur, I want afficher du texte streamé token-par-token + un champ de saisie en Ratatui, so that je prouve la fluidité du rendu (risque TUI).

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un flux de tokens, when ils arrivent, then ils s'affichent en <50 ms après réception, sans scintillement.
- [ ] Given un redimensionnement du terminal en cours de stream, when l'utilisateur redimensionne, then le rendu se réajuste sans corruption (unhappy path).
- [ ] Given un terminal sans truecolor, when le rendu s'affiche, then il dégrade en monochrome lisible.

#### US-005: Spike — sandbox Landlock FS + appel réseau filtré par proxy
**Description:** As a créateur, I want restreindre les écritures FS via Landlock et filtrer un appel réseau par hostname via un proxy local, so that je découvre tôt la réalité du sandbox (Landlock ne filtre pas le réseau).

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une politique Landlock restreignant l'écriture au workspace, when un outil tente d'écrire hors workspace, then l'écriture est refusée au niveau kernel.
- [ ] Given un proxy local avec allow-list d'hostnames, when une requête vers un hôte non autorisé est émise, then elle est bloquée par le proxy (unhappy path) et l'événement est journalisé.
- [ ] Un verdict écrit confirme la faisabilité solo du proxy réseau, ou bascule vers nftables.

---

### EP-002: Cœur d'agent (`agent-core`)

La boucle headless complète, le budget de contexte, la compaction et la persistance de session. Aucune dépendance TUI/HTTP.

**Definition of Done:** `agent-core` mène une conversation multi-tours complète en mode headless (`-p`), compacte automatiquement, et reprend une session après crash, le tout testable sans API réelle (deps injectables).

#### US-006: Boucle d'agent en state-machine + mode headless
**Description:** As a développeur, I want une boucle d'agent modélisée en `enum Transition` exhaustif avec withholding et transcript-before-response, so that le contrôle de flux est vérifié à la compilation et robuste au crash.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-003

**Acceptance Criteria:**
- [ ] Given une conversation multi-tours, when elle progresse, then chaque transition (NextTurn, MaxTokensRecovery, etc.) est gérée exhaustivement (match compilé).
- [ ] Given un message utilisateur, when il est soumis, then il est persisté (`sync_data`) AVANT l'appel API.
- [ ] Given `pyxis -p "..."`, when invoqué, then l'agent répond sur stdout sans charger Ratatui.
- [ ] Given une erreur PromptTooLong, when elle survient, then le withholding retient l'erreur jusqu'à échec confirmé du recovery, sans terminer prématurément (unhappy path).

#### US-007: ContextBudget + comptage de tokens local
**Description:** As a développeur, I want un `ContextBudget` calculé une fois par modèle et un comptage de tokens local avec fallback, so that la compaction a toujours un signal, même sur Ollama sans usage en stream.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given un modèle donné, when la session démarre, then le budget (total, réserve sortie, seuil autocompact) est calculé depuis une source unique.
- [ ] Given un flux fournissant `usage`, when il arrive, then le budget consomme cette valeur.
- [ ] Given un provider sans `usage` (Ollama), when un tour se termine, then le comptage local estime les tokens et alimente le seuil (unhappy path : pas de signal manquant).

#### US-008: Compaction en cascade
**Description:** As a développeur, I want une compaction micro → auto → full (agent forké), so that la session ne meurt pas quand le contexte se remplit.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-007

**Acceptance Criteria:**
- [ ] Given des vieux tool results, when le seuil micro est atteint, then ils sont élagués en premier.
- [ ] Given le seuil autocompact franchi, when atteint, then un résumé total proactif est produit (images strippées).
- [ ] Given un échec d'autocompact répété, when il se répète, then un circuit breaker stoppe les tentatives et signale clairement, sans boucle d'appels (unhappy path).
- [ ] Given une erreur 413 réelle de l'API, when reçue, then une compaction réactive distincte se déclenche.

#### US-009: Sessions JSONL append-only + resume
**Description:** As a développeur, I want une persistance JSONL append-only avec resume par dossier, so that je reprends une session interrompue sans perte.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given une session active, when des entries sont écrites, then elles sont append-atomiques et discriminées (Message | CompactBoundary | FileHistorySnapshot).
- [ ] Given `pyxis --resume` dans un dossier, when invoqué, then la dernière session est reconstruite depuis le log.
- [ ] Given un log tronqué par un crash en plein écrit, when le resume tourne, then la dernière entry partielle est ignorée et la session démarre depuis le dernier état valide (unhappy path).

---

### EP-003: Système d'outils & garde-fous (`agent-tools`)

Le trait Tool, le dispatch concurrent/série, les outils de base, le modèle de permissions, le taint untrusted, et les garde-fous (loop guardrails, budgets) issus de la recherche sur les modes d'échec.

**Definition of Done:** L'agent exécute les 6 outils de base via un pipeline strict avec permissions fail-closed, taint untrusted, détection de boucle déterministe et budget de coût/tokens avec kill-switch.

#### US-010: Trait Tool + registry + dispatch + pipeline d'exécution
**Description:** As a développeur, I want un trait Tool fail-closed (+ `DynTool`) avec dispatch concurrent/série et un pipeline d'exécution strict, so that l'ajout d'un outil est uniforme et sûr par défaut.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given des outils concurrency-safe consécutifs, when dispatchés, then ils tournent en parallèle via `buffer_unordered(10)` ; les mutants tournent en série.
- [ ] Given un appel d'outil, when exécuté, then il passe par : parse serde → validate_input → hooks Pre → permission → call() (sous `tokio::time::timeout`) → taint → hooks Post.
- [ ] Given un argument d'outil qui échoue au parse de schéma, when reçu, then l'erreur est renvoyée à l'agent sans exécuter l'outil (unhappy path, fail-closed).

#### US-011: Outils de lecture — Read, Glob, Grep
**Description:** As a développeur, I want des outils de lecture du système de fichiers, so that l'agent comprend le repo avant d'agir.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**
- [ ] Given un chemin, when Read est appelé, then le contenu (avec numéros de ligne) est retourné, marqué untrusted.
- [ ] Given un pattern, when Grep/Glob est appelé, then les correspondances avec contexte sont retournées.
- [ ] Given un fichier inexistant ou binaire, when lu, then une erreur explicite est retournée sans crash (unhappy path).

#### US-012: Outils de mutation — Write, Edit, Bash
**Description:** As a développeur, I want des outils d'écriture, d'édition ancrée et d'exécution shell, so that l'agent applique des changements.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-010, US-013

**Acceptance Criteria:**
- [ ] Given un Edit avec ancre unique, when appliqué, then seul le bloc ciblé change ; une ancre ambiguë échoue avec un message clair (unhappy path).
- [ ] Given une commande Bash, when exécutée, then elle tourne sous timeout et son stdout/stderr est capturé et marqué untrusted.
- [ ] Given une mutation hors workspace, when tentée, then elle est refusée par le sandbox (lien US-020).

#### US-013: Modèle de permissions (5 modes) + taint untrusted
**Description:** As a développeur, I want 5 modes de permission et un taint des sorties d'outils, so that les actions sensibles requièrent confirmation et l'injection indirecte est contenue (OWASP LLM01).

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**
- [ ] Given le mode Default, when un outil mutant/réseau est appelé, then une confirmation est demandée ; en BypassPermissions, elle est sautée.
- [ ] Given une sortie d'outil, when produite, then elle est marquée `untrusted` par défaut et le taint se propage aux messages dérivés.
- [ ] Given une action destructive/réseau déclenchée dans un tour contenant du taint récent, when elle survient, then une confirmation est forcée même en AcceptEdits (unhappy path / défense injection).

#### US-014: Loop guardrails + budgets de coût/tokens (kill-switch)
**Description:** As a développeur, I want une détection de boucle déterministe et des budgets de coût/tokens avec kill-switch, so that les deux modes d'échec les plus coûteux (boucles, coûts runaway) sont neutralisés.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given le même outil rappelé avec des arguments identiques N fois (défaut N=3), when détecté, then la boucle est stoppée et l'agent reçoit un signal explicite.
- [ ] Given un budget de tokens/coût configuré, when le seuil est atteint, then l'exécution s'arrête (kill-switch) et demande confirmation pour continuer (unhappy path).
- [ ] Given un dépassement de coût estimé avant un gros tour, when prévu, then une confirmation est demandée au-delà d'un seuil paramétrable.

---

### EP-004: Couche multi-provider (`agent-provider` + `agent-auth`)

Le trait Provider, le format canonique, les adapters non-bloqués (Ollama, OpenAI Chat, Anthropic de référence), le retry, et l'auth BYOK.

**Definition of Done:** Pyxis parle à au moins Ollama et OpenAI via une interface unique, avec credentials stockées dans le keyring, retry robuste, et bascule de provider à la volée.

#### US-015: Trait Provider + format canonique + retry
**Description:** As a développeur, I want un trait Provider avec format canonique Anthropic-like et taxonomie d'erreurs/retry, so that tout provider se branche derrière une interface unique.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given le trait Provider, when un adapter Anthropic de référence est implémenté, then il valide que le canonique mappe l'API Anthropic (quasi-identité).
- [ ] Given une erreur API, when reçue, then `classify_error` la trie (Retryable | Overloaded(529) | Auth | InvalidRequest) ; un 529 déclenche un backoff agressif honorant `Retry-After`.
- [ ] Given le message Anthropic « This credential is only authorized for use with Claude Code… », when reçu, then il est classifié `Auth::ThirdPartyBlocked` avec un message utilisateur explicite (unhappy path).

#### US-016: Adapter Ollama (local)
**Description:** As a développeur, I want un adapter Ollama, so that l'agent tourne sur un modèle local sans credentials ni coût.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-015, US-007

**Acceptance Criteria:**
- [ ] Given un Ollama local, when on lance une session, then le streaming et les tool calls fonctionnent via l'interface canonique.
- [ ] Given l'absence de `usage` dans le flux, when un tour se termine, then le fallback tokenizer fournit l'estimation (lien US-007, unhappy path).
- [ ] Given Ollama non démarré, when on tente une connexion, then une erreur actionnable est retournée (« démarrez Ollama »).

#### US-017: Adapter OpenAI (Chat Completions)
**Description:** As a développeur, I want un adapter OpenAI Chat Completions, so that l'agent parle à GPT en BYOK.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-015

**Acceptance Criteria:**
- [ ] Given une API key OpenAI, when une session tourne, then text + tool calls + usage sont normalisés vers le canonique.
- [ ] Given une clé invalide/expirée (401), when l'appel est émis, then l'erreur est classifiée Auth et un message clair est affiché (unhappy path).
- [ ] La cible est Chat Completions ; la Responses API est hors scope (voir Non-Goals).

#### US-018: Auth BYOK — keyring + flow de credentials
**Description:** As a développeur, I want stocker mes clés providers dans le secret store de l'OS, so that le multi-provider BYOK est sûr et sans dépendance subscription.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-015

**Acceptance Criteria:**
- [ ] Given une clé saisie, when enregistrée, then elle est stockée via Secret Service/keyring, jamais en clair sur disque.
- [ ] Given plusieurs providers configurés, when l'utilisateur bascule (`--model`), then la bonne credential est sélectionnée.
- [ ] Given un secret store indisponible, when on lit une clé, then une erreur explicite est retournée avec fallback documenté (unhappy path).

---

### EP-005: Frontend terminal & sandbox

Le frontend Ratatui monochrome (client du cœur headless) et le sandbox d'exécution Landlock FS + proxy réseau.

**Definition of Done:** `pyxis` offre une session interactive monochrome fluide (streaming, diff, dialogs de permission) ; toute écriture FS est confinée au workspace et le réseau est filtré par allow-list.

#### US-019: Frontend TUI Ratatui monochrome
**Description:** As a utilisateur, I want une UI terminal épurée et moderne (streaming, diff, dialogs de permission), so that l'expérience rivalise avec Claude Code sans être un TUI à l'ancienne.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004, US-006, US-013

**Acceptance Criteria:**
- [ ] Given une réponse streamée, when elle arrive, then elle s'affiche token-par-token, monochrome + un accent, sans bordure ASCII lourde.
- [ ] Given une mutation proposée, when affichée, then un diff lisible (gouttière non sélectionnable) est rendu avant application.
- [ ] Given une demande de permission, when soulevée, then un dialog clair propose accepter/refuser ; un refus interrompt proprement l'action (unhappy path).
- [ ] Given un terminal étroit ou sans truecolor, when rendu, then la mise en page dégrade sans corruption.

#### US-020: Sandbox d'exécution — Landlock FS + proxy réseau
**Description:** As a utilisateur, I want que les outils s'exécutent dans un bac à sable FS+réseau, so that une instruction injectée ne puisse pas exfiltrer ni détruire hors périmètre.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005, US-012

**Acceptance Criteria:**
- [ ] Given une politique active, when un outil écrit, then l'écriture est confinée au workspace au niveau kernel (Landlock).
- [ ] Given une allow-list réseau, when une requête vers un hôte non autorisé est émise, then le proxy la bloque et la journalise (unhappy path).
- [ ] Given une plateforme non-Linux, when le sandbox est demandé, then il dégrade explicitement en mode désactivé avec avertissement (Linux-first).

## Functional Requirements

- FR-01: Le système doit exécuter une boucle agentique multi-tours (stream modèle → exécution d'outils → réinjection → reboucle) jusqu'à `end_turn`, max turns, budget épuisé, ou kill-switch.
- FR-02: Le système doit parler à au moins Ollama et OpenAI (Chat Completions) derrière une interface Provider unique, avec credentials BYOK stockées dans le keyring.
- FR-03: Le système doit exécuter les outils Read, Glob, Grep, Write, Edit, Bash via un pipeline parse → validate → hooks → permission → exécution (timeout) → taint → hooks.
- FR-04: Le système doit marquer toute sortie d'outil comme untrusted par défaut et forcer une confirmation pour toute action destructive/réseau déclenchée dans un contexte tainté.
- FR-05: Le système doit détecter les boucles d'outils (même outil + mêmes args, seuil configurable) et les stopper de façon déterministe.
- FR-06: Le système doit appliquer des budgets de coût/tokens avec kill-switch et confirmation au-delà d'un seuil.
- FR-07: Le système doit compacter le contexte en cascade (micro → auto → full) avant saturation, avec circuit breaker sur échecs répétés.
- FR-08: Le système doit persister chaque session en JSONL append-only et la reprendre par dossier (`--resume`), y compris après crash.
- FR-09: Le système doit fonctionner en mode interactif (TUI Ratatui) et en mode headless (`-p`, sans Ratatui).
- FR-10: Le système doit confiner les écritures FS au workspace (Landlock) et filtrer le réseau par allow-list (proxy) sous Linux.
- FR-11: Le système ne doit PAS dépendre d'un canal d'authentification par abonnement d'un provider unique.

## Non-Functional Requirements

IMPORTANT: chiffres mesurables uniquement.

- **Performance:** Démarrage `pyxis` → prompt prêt en <100 ms (P95) ; <200 ms acceptable. Premier token affiché <50 ms après réception réseau (streaming sans buffering). Empreinte mémoire idle <50 Mo RSS ; sous charge raisonnable <150 Mo.
- **Distribution:** Single static binary <30 Mo, installable sans runtime externe (`curl | sh` / `cargo binstall`).
- **Security:** Permissions fail-closed par défaut. Sorties d'outils/MCP traitées comme untrusted (OWASP LLM01). Écritures FS confinées au workspace au niveau kernel (Landlock). Réseau filtré par allow-list. Credentials jamais persistées en clair (keyring uniquement).
- **Reliability:** Retry automatique 3× sur erreurs transitoires (backoff exponentiel + jitter, plafond 32 s) ; backoff agressif distinct sur 529. Resume de session fonctionnel après kill -9 (transcript-before-response). Loop guardrail : arrêt après ≤3 répétitions identiques d'outil.
- **Cost safety:** Kill-switch déclenché à 100 % du budget de tokens/coût configuré ; confirmation requise au-delà d'un seuil paramétrable avant un tour estimé coûteux.
- **Testabilité:** `agent-core` exécutable de bout en bout sans API réelle (deps injectables) ; couverture des chemins de transition de la boucle par tests unitaires.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Auth provider bloquée | Anthropic via abonnement | Classifier `ThirdPartyBlocked`, suggérer API key / autre provider | "Cette credential est réservée à Claude Code. Utilisez une API key ou un autre provider." |
| 2 | Boucle d'outil | Même outil + args ×3 | Hard stop déterministe, signal à l'agent | "Boucle détectée sur {outil} — arrêt. Reformulez ou intervenez." |
| 3 | Budget dépassé | Tokens/coût ≥ seuil | Kill-switch + demande de confirmation | "Budget atteint ({n} tokens / {coût} $). Continuer ?" |
| 4 | Contexte saturé | Budget de contexte franchi | Compaction auto, signal clair (pas de troncature silencieuse) | "Contexte compacté." |
| 5 | Sortie d'outil untrusted déclenche action sensible | Taint + action destructive/réseau | Confirmation forcée même en AcceptEdits | "Action sensible issue de contenu non fiable — confirmer ?" |
| 6 | Réseau coupé / API timeout | Connexion perdue | Retry backoff, puis erreur actionnable | "Réseau indisponible — réessai… puis abandon après 3 tentatives." |
| 7 | Écriture hors workspace | Outil tente d'écrire dehors | Refus kernel (Landlock) | "Écriture refusée hors du workspace." |
| 8 | Resume après crash | Log JSONL partiel | Ignorer l'entry partielle, reprendre au dernier état valide | "Session reprise (dernière action incomplète ignorée)." |
| 9 | Ollama sans `usage` | Provider local | Fallback tokenizer local, compaction préservée | — |
| 10 | Terminal sans truecolor / étroit | Capacités limitées | Dégradation monochrome, mise en page réajustée | — |
| 11 | Ancre d'Edit ambiguë | Plusieurs correspondances | Échec explicite, aucune mutation | "Ancre ambiguë ({n} correspondances) — précisez." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Accès provider (ban Anthropic tiers, 04/04/2026) | High | High | Historique : BYOK model-agnostic avec provider MVP non-bloqué (Ollama/OpenAI). Supersedé par ADR-11 : MVP courant `OpenAiChatGpt`, plan de sortie BYOK futur. |
| 2 | Vélocité Rust en solo (temps fragmenté) | High | High | Scope MVP serré + spikes d'abord ; `mold`+`sccache`+`nextest` ; réévaluer après Phase 0 |
| 3 | Boucles d'outils runaway | Med | High | Loop guardrails déterministes (US-014), override de la logique modèle |
| 4 | Coûts runaway (facture surprise) | Med | High | Budgets + kill-switch + estimation pré-tour (US-014) |
| 5 | Prompt injection indirecte (repo/MCP) | Med | High | Taint untrusted + confirmation actions sensibles + sandbox (US-013, US-020) |
| 6 | MCP absent au MVP (table-stake 2026) | High | Med | Tolérable pour le dogfood ; MCP en TÊTE de Phase 2 avant toute promo publique |
| 7 | Pas de SDK Anthropic Rust officiel | Med | Med | Adapter isolé + SDK communautaires comme réf. wire format ; tests VCR en CI (Phase 3) |
| 8 | Sandbox cross-platform (Landlock = Linux) | Med | Med | Linux-first ; backend en enum ; dégradation explicite ailleurs |

## Non-Goals

Frontières explicites — ce que le MVP ne fait PAS :

- **MCP (Model Context Protocol)** — différé à Phase 2 (en tête de file). Table-stake 2026 : bloque la promo publique mais pas le dogfood.
- **Providers cloud et avancés** — Gemini, OpenRouter, AWS Bedrock, Google Vertex, Azure : Phase 2. OpenAI **Responses API** : Phase 2 (le canonique cible Chat Completions).
- **Support multi-OS** — macOS (Seatbelt) / Windows : Phase 3. MVP = Linux-first.
- **TUI riche** — sélection souris, tables markdown, virtual scroll, syntax highlight incrémental : Phase 2. MVP = streaming + diff brut + dialogs.
- **Sous-agents / teams, mémoire vectorielle (sqlite-vec), skills/commands & hooks** — Phase 2.
- **Distribution packagée (curl|sh, binstall, télémétrie OTel, tests VCR)** — Phase 3 (durcissement).

## Files NOT to Modify

Section historique. Au moment du draft, aucun code Rust n'existait ; ce n'est plus vrai. Les surfaces de référence actuelles sont :
- `docs/CURRENT_STATUS.md` : état livré, risques vivants et features différées.
- `tasks/prd-pyxis-status.json` : suivi machine-readable des stories.
- `docs/DECISIONS.md` : ADR, en particulier ADR-10 et ADR-11.
- `docs/ARCHITECTURE.md`, `docs/PROVIDERS.md`, `docs/ROADMAP.md` : invariants et trajectoire, avec notes de supersession quand le scope a changé.

## Technical Considerations

Formulé comme questions pour validation à l'implémentation :

- **Architecture:** Boucle en `enum Transition` + `async-stream` — recommandé. Confirmer que l'object-safety du trait Tool passe bien par `DynTool` (concession assumée).
- **Tokenizer:** `tiktoken-rs` pour l'estimation locale par défaut — suffisant pour le seuil de compaction ? Tokenizers exacts par provider différés.
- **Sandbox réseau:** proxy local (allow-list par hostname) confirmé comme best-effort applicatif (Landlock ne filtre pas le DNS/SNI). Alternative nftables à évaluer si le proxy solo s'avère fragile (US-005).
- **Dependencies:** `ratatui` + `crossterm`, `reqwest` + `eventsource-stream`, `tokio`, `async-stream`, `keyring`, `landlock`, `tiktoken-rs`, `serde`/`schemars`. Versions à figer au démarrage du workspace.
- **Provider canonique:** format Anthropic-like (content blocks) — confirmer qu'il absorbe proprement OpenAI Chat + Ollama dès US-015/016/017 ; le piège Responses API est explicitement hors MVP.
- **Migration:** N/A (nouveau projet). Format de session JSONL : décider tôt s'il vise la compat avec le JSONL de Claude Code (interop resume) ou un format propre.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Providers frontier fonctionnels | N/A (nouveau) | 3 → 6+ | M1 → M6 | tests d'intégration par adapter |
| Latence de démarrage (P95) | N/A | <100 ms | M1, maintenu M6 | benchmark reproductible publié (artefact) |
| GitHub stars (distribution) | 0 | 2–5k → 15–30k | M1 → M6 | star-history.com |
| Sessions dogfood/jour (Arthur) | 0 | ≥1 → usage quotidien | M1 → M6 | logs de session locaux |
| Contributeurs externes (PR mergées) | 0 | ≥1 → ≥10 | M1 → M6 | GitHub insights |
| Incidents de coût runaway signalés | N/A | 0 | M6 | issues étiquetées `cost` |

## Open Questions

- **Format de session JSONL** — propre ou compatible Claude Code (interop resume) ? À trancher avant US-009. Décideur : Arthur.
- **Seuils par défaut des garde-fous** — N répétitions de boucle (3 ?) et seuil de budget par défaut : à calibrer pendant le dogfood (US-014). Décideur : Arthur, M1.
- **Provider du premier dogfood** — Ollama (local, gratuit) vs OpenAI (qualité) selon le verdict US-001. Dépend du spike.
- **Modèle de licence / OSS** — MIT vs Apache-2.0 (alignement écosystème) : à fixer avant la première release publique. Décideur : Arthur.
[/PRD]
