# Roadmap Numen

**Principe directeur : le dur et le risque en premier.** On n'écrit pas une ligne d'architecture confortable avant d'avoir tué les inconnues qui peuvent invalider le projet. Chaque phase descend d'un cran dans le risque résiduel. La Phase 0 n'est pas un sprint de fondation : c'est une série de spikes jetables dont la seule fonction est de produire un verdict go/no-go.

Le risque d'exécution N1 est connu et assumé : Rust en solo = vélocité plus lente que TS/Bun. La roadmap compense en concentrant l'incertitude technique le plus tôt possible, pour que le coût de pivot reste faible tant qu'aucune dette d'archi n'est posée.

**Documents liés.** Décisions de fond et providers : [`docs/DECISIONS.md`](./DECISIONS.md) (ADRs), [`docs/PROVIDERS.md`](./PROVIDERS.md) (couche multi-provider), [`docs/ARCHITECTURE.md`](./ARCHITECTURE.md) (workspace, boucle, événements). Les correspondances clés : ADR-7 (de-risquage) ↔ ce document ; ADR-4 ↔ `docs/PROVIDERS.md` (taxonomie d'erreurs, retry) ; ADR-2/ADR-3 ↔ `docs/ARCHITECTURE.md` (crates, boucle) ; `docs/PROVIDERS.md` §6 ↔ Phase 0 ci-dessous (spike provider).

---

## Conventions de nommage (à lire avant le reste)

Deux divergences de nommage sont **actées explicitement** ici pour éviter toute ambiguïté dans la suite du document et avec les autres docs.

**Crates.** Le nom **`numen`** est réservé et libre sur crates.io (atout décisif, cf. ADR-5) ; il désigne le **crate racine publié** et le **binaire** (commande `numen`). Les crates internes du workspace conservent le préfixe `agent-*` (`agent-core`, `agent-cli`, `agent-tui`, …) tel que défini dans le BRIEF et `docs/ARCHITECTURE.md`. Autrement dit : la façade publique est `numen` (binaire + crate racine = `agent-cli` republié sous ce nom), l'intérieur du workspace reste `agent-*`. Les réservations `numen-cli` / `numen-core` sont gardées en réserve défensive, sans usage interne. Cette convention est la version autoritative : si un doc nomme encore un crate de travail `numen-core`, c'est `agent-core` qui fait foi.

**Erreurs.** Le type canonique de classification est **`ErrorClass`** (jamais `ErrClass`). Sa taxonomie de référence détaillée vit dans `docs/PROVIDERS.md` ; ce document n'en reprend que ce qui conditionne la roadmap.

**Deux familles d'événements (rappel).** `StreamEvent` (provider → `agent-core`) et `AgentEvent` (`agent-core` → clients TUI/Paneflow) sont **deux enums distincts et volontairement séparés**. `agent-core` consomme les `StreamEvent` du provider et les traduit en `AgentEvent` pour les clients ; aucun client ne voit jamais un `StreamEvent` brut ni de l'ANSI. Détail dans `docs/ARCHITECTURE.md` §10.1 et `docs/PROVIDERS.md` §2.

---

## Phase 0 — Spike de dé-risquage

**Objectif.** Répondre par oui/non aux questions qui peuvent tuer le projet, avec du code jetable. Aucune de ces briques n'est destinée à survivre telle quelle ; elles existent pour produire un verdict. Tant que la Phase 0 n'est pas verte, la Phase 1 n'existe pas.

### Spikes et critères de passage

| Spike | Objet | Critère de passage (go) | Critère d'échec (no-go / pivot) |
|---|---|---|---|
| **[P0 ABSOLU] Auth Anthropic** (1 jour, dans `agent-auth`) | Vérifier qu'un agent tiers peut (ou non) s'authentifier et appeler les modèles Claude — abonnement Pro/Max **et** token API. Inclut le flux OAuth + refresh token nécessaire à Anthropic. | Un flux d'auth fonctionnel produit un stream Claude exploitable, OU le verdict est clair : « abonnement bloqué, token OK ». Le résultat, quel qu'il soit, est **documenté et figé**. | Aucun chemin d'auth viable même au token → Anthropic sort du provider set MVP, on bascule sur un positionnement strictement model-agnostic. |
| Provider canonique sur 1 stream | Valider `Provider::stream -> BoxStream<StreamEvent>` (reqwest + eventsource-stream) sur **un seul** provider, format canonique Anthropic-like. | Un prompt produit un flux d'`StreamEvent` (`TextDelta`, `ReasoningDelta`, `ToolCallStart/Delta/End`, `Usage`, `Done`) correctement décodé de bout en bout. | SSE/parse ingérable ou format canonique qui ne tient pas sur le premier provider testé → revoir le format canonique avant d'aller plus loin. |
| Boucle minimale | Stream → 1 outil `Bash` → reboucle. Valide la state machine à transitions typées dans sa forme la plus réduite. | L'agent appelle `Bash`, récupère le résultat, le réinjecte, reboucle jusqu'à `end_turn`. | La state machine ne se ferme pas proprement ou les transitions ne sont pas exhaustives → revoir l'`enum Transition`. |
| TUI streaming brut | Ratatui + crossterm affiche le stream en direct. Aucune feature, juste le tube `agent-core → canal → agent-tui`. | Le texte du modèle s'affiche token par token dans le terminal, le core n'émettant que des `AgentEvent` (jamais d'ANSI). | Couplage core/TUI qui fuit (le core doit émettre des events, jamais de l'ANSI) → corriger la frontière avant Phase 1. |
| Sandbox Landlock FS + 1 appel réseau filtré | Landlock restreint réellement le FS au niveau kernel ; un appel réseau passe (ou est bloqué) par le **proxy local** (pas Landlock — Landlock ne filtre pas par hostname). | Un accès FS hors périmètre est refusé par le kernel ; un appel réseau traverse le proxy de manière observable. | Landlock indisponible/insuffisant sur la cible, ou proxy infaisable → réviser le modèle de sécurité avant de s'engager. |

`ReasoningDelta` est listé pour mémoire dans le spike provider ; son traitement réel (rendu du raisonnement) n'est pas requis pour valider le spike, seul le décodage de bout en bout l'est.

**Hors scope Phase 0.** Multi-provider, MCP, compaction, permissions complètes, persistance, diffs, sous-agents, toute esthétique TUI. Le code de Phase 0 est explicitement jetable : aucune dette à porter, aucune API à figer (hormis le verdict auth).

---

## Phase 1 — MVP

**Objectif.** Un agent de code utilisable en terminal, full Rust/Linux, sur un provider **non bloqué** par le risque N1. C'est le premier artefact qui doit survivre : on pose ici le workspace de crates définitif et les contrats internes.

**Livrables.**
- Workspace de crates en place avec la règle d'or respectée : `agent-core` ne dépend ni de `agent-tui` ni de `agent-provider`, testable headless (`-p`) sans Ratatui ni API.
- Boucle d'agent complète :
  - **transcript-before-response** : persistance du message *avant* l'appel API (`sync_data`) ;
  - **withholding** : `Option<PendingError>` retient **uniquement** les erreurs de budget de contexte (PTL / max-tokens) jusqu'à **échec confirmé du recovery** (tentative de compaction réactive). À ne pas confondre avec le retry transverse : les `Overloaded(529)` / `Retryable` relèvent du backoff provider (voir `docs/PROVIDERS.md` §5.1), **pas** du `PendingError` ;
  - deps injectables (boucle testable sans API) ;
  - `ContextBudget` calculé une fois par modèle (source unique de vérité) ;
  - circuit breaker sur autocompact consécutifs.
- Couche provider maison sur **Ollama** + **OpenAI Chat Completions**, **Anthropic conditionnel** au verdict Phase 0.
- Système d'outils : trait `Tool` à defaults fail-closed (`is_concurrency_safe=false`, `is_read_only=false`, `returns_untrusted=true`), object-safety via `DynTool`, dispatch concurrent (`buffer_unordered(10)`) / sériel, pipeline strict par outil (parse → `validate_input` → `PreToolUse` → permissions + règles globales → `call()` sous `tokio::time::timeout` → taint untrusted → `PostToolUse`).
- Permissions 5 modes (Default / AcceptEdits / DontAsk / BypassPermissions / Plan) + **taint untrusted** (OWASP LLM01) : tout output d'outil est untrusted par défaut, propagé ; une action destructive/réseau dans un turn contenant du taint récent force `Ask`.
- Tokenizer local (`agent-tokenizer`) pour le comptage quand le provider n'émet pas de `Usage` en stream (Ollama). `update_budget` lit le `Usage` du stream sinon retombe sur `agent-tokenizer` (sans quoi la compaction casse sur Ollama).
- Compaction en cascade : micro + auto + full (fork de l'agent via `tokio::spawn` en mode resume, images strippées). `snip`/`collapse` reste feature-gated et hors scope MVP.
- Sandbox Landlock FS + proxy réseau.
- TUI streaming + diff brut + dialogs de permission.
- Sessions JSONL append-only (`Message | CompactBoundary | FileHistorySnapshot`), append atomique + resume (rejeu du log, reconstruction d'état).

### Auth au MVP — quel niveau d'OAuth ?

Distinction explicite pour lever l'ambiguïté « keyring vs OAuth » :

| Au MVP (Phase 1) | Reporté (Phase 2) |
|---|---|
| Stockage de credentials via keyring / Secret Service (`agent-auth`). | OAuth **PKCE par serveur MCP** (chaque serveur a son flux). |
| Flux **OAuth + refresh token pour Anthropic** si — et seulement si — Anthropic entre au MVP (verdict Phase 0). C'est le strict nécessaire pour un seul provider OAuth. | Gestion **multi-serveurs** de tokens, orchestration de refresh complexe au-delà du cas single-provider. |
| `401 → refresh OAuth` pour le provider Anthropic. | — |

Autrement dit, « OAuth/refresh complexe » reporté = **multi-serveur** (MCP) ; l'OAuth single-provider d'Anthropic, lui, est dans le MVP dès lors qu'Anthropic y est conditionnellement inclus.

### Inclus / Exclu

| Inclus dans le MVP | Explicitement HORS scope (reporté) |
|---|---|
| Ollama (local), OpenAI Chat Completions | OpenAI Responses API (server-side state, gated) |
| Anthropic **conditionnel** au go/no-go auth | Gemini, OpenRouter, Bedrock, Vertex, Azure |
| Outils Bash, Read, Edit, Write, Glob, Grep | MCP (`agent-mcp` / rmcp) |
| Permissions 5 modes + taint untrusted | Skills / commands / hooks utilisateur |
| Auth keyring + OAuth single-provider (Anthropic, conditionnel) | OAuth PKCE multi-serveur (MCP), refresh multi-serveur |
| Tokenizer local (fallback `Usage`) | — |
| Compaction micro + auto + full | `snip`/`collapse` (feature-gated) |
| Sandbox Landlock FS + proxy réseau | macOS Seatbelt |
| TUI streaming + diff brut + dialogs | TUI riche (arbre de plan, review par hunk) |
| Sessions JSONL + resume | Sous-agents / teams |
| — | Mémoire vectorielle, protocole d'enrichissement Paneflow |

**Plateforme et distribution.** Linux uniquement. macOS et cross-compile sont en Phase 3. **La distribution publique (`cargo binstall` + `curl | pipe`) n'arrive qu'en Phase 3** : au MVP, on s'installe par `cargo build`/`cargo install` local. Le README montre la commande cible `numen` (et `numen -p`) : c'est l'**interface visée**, pas l'état de distribution — aucun canal de release n'existe avant la Phase 3.

---

## Phase 2 — v1

**Objectif.** Couvrir l'ensemble des providers frontier et faire de Numen un agent complet, avec la première amorce de l'intégration profonde Paneflow.

**Livrables.**
- **Tous les providers** :
  - **OpenAI Responses** : état server-side via `previous_response_id`, ne mappe **pas** sur le canonique → mode **gated** sur `capabilities.server_side_state`, jamais le défaut.
  - **Gemini** : réassemblage des function calls **fragmentées en stream** avant d'émettre `ToolCallEnd`, `systemInstruction`, context cache.
  - **OpenRouter** : méta-routeur OpenAI-compat (un seul adapter, 200+ modèles, perte des features natives).
  - **Bedrock / Vertex / Azure** : auth injectable (SigV4 / OAuth Google / endpoint custom), **pas** des adapters complets — ils réutilisent l'adapter Anthropic/OpenAI/Gemini sous-jacent. Toutes les creds via `agent-auth`.
- **Transverses provider durcis.** `classify_error -> ErrorClass` avec, au minimum, les variantes `Retryable | Overloaded(529) | Auth | InvalidRequest` (taxonomie de référence : `docs/PROVIDERS.md`). Backoff exponentiel + jitter, `Overloaded(529)` = backoff agressif, honore `Retry-After`, fallback modèle après 3×529, `401 → refresh OAuth`. Le message Anthropic « This credential is only authorized for use with Claude Code… » → classifié `Auth` avec raison `ThirdPartyBlocked`. Stratégie cache-hit : ordre stable (system → tools → CLAUDE.md → historique), blocs cacheables en tête, jamais de contenu volatile avant un bloc cache. `cache_control` ephemeral TTL 1h et thinking adaptive côté Anthropic. Betas Anthropic gated sur `kind == Anthropic`. Multimodal canonique via `ContentBlock::Image`.
- **MCP** via rmcp officiel : état en enum discriminé (client accessible uniquement dans `Connected`), description cappée 2048 chars, **OAuth PKCE par serveur**, outils MCP enregistrés comme `DynTool` (uniformité), tous `returns_untrusted=true`.
- Skills / commands + hooks utilisateur.
- Sous-agents / teams : `tokio::spawn(run_agent)` avec transcript séparé, comm via `mpsc` ; InProcessTeammate via `tokio::task_local`.
- TUI riche.
- **Mémoire vectorielle sqlite-vec.** Livrable Phase 2 ; à noter que `docs/ARCHITECTURE.md` ne couvre pas encore ce sous-système. Action de cohérence doc : ajouter au minimum une note « futur Phase 2 » dans `docs/ARCHITECTURE.md`, ou acter explicitement que la mémoire vectorielle est hors périmètre du doc d'archi actuel. Tant que ce n'est pas fait, sqlite-vec n'a pas de section architecturale de référence.
- **Protocole d'enrichissement Paneflow.** Paneflow embarque `agent-core` **in-process** (pas d'IPC, types partagés) et rend les `AgentEvent` via GPUI : diffs GPU-accélérés, arbre de plan, review par hunk — **sans casser le mode terminal par défaut**.

**Hors scope Phase 2.** Durcissement OS multi-plateforme (macOS Seatbelt), cross-compile, télémétrie, suite VCR en CI, canaux de distribution publics — tout cela est concentré en Phase 3.

---

## Phase 3 — Durcissement & distribution

**Objectif.** Rendre Numen distribuable, multi-plateforme et tenable dans la durée sans SDK officiel.

**Livrables.**
- **macOS Seatbelt** : équivalent de la sandbox Landlock côté macOS.
- **Cross-compile** via `cargo-zigbuild`.
- **Télémétrie OTel** (OpenTelemetry).
- **Tests VCR sur payloads providers — CI OBLIGATOIRE.** C'est le filet de sécurité qui remplace l'absence de SDK officiel : on enregistre les payloads réels de chaque provider et on les rejoue en CI pour détecter toute dérive de wire format. Sans SDK qui absorbe les changements d'API en amont, ces tests VCR sont la seule garde contre une rupture silencieuse d'un adapter. **Non négociable, bloquant en CI.**
- **Distribution** : `cargo binstall` + install par `curl | pipe`.

**Hors scope Phase 3.** Aucune nouvelle fonctionnalité produit : Phase 3 est exclusivement industrialisation. Toute idée de feature qui surgit ici est reversée au backlog post-v1.

---

## Note go/no-go — l'auth provider avant toute archi

Le risque N1 produit du projet est externe et hors de notre contrôle : depuis janvier 2026 (durci en avril 2026), Anthropic bloque les outils tiers qui s'authentifient via un abonnement Pro/Max. Un agent tiers ne peut plus consommer un abonnement Max.

**Conséquence opérationnelle stricte : on répond à la question auth provider avant d'écrire une ligne d'architecture.** Le spike auth Anthropic est le `[P0 ABSOLU]` de la Phase 0 — la première chose qu'on fait, en 1 jour, dans `agent-auth`. Son verdict conditionne tout le reste :

- **Si Anthropic est exploitable (token au minimum)** → il entre dans le provider set MVP en mode conditionnel ; on garde le cache-hit, les betas gated `kind == Anthropic`, et l'OAuth single-provider + refresh associé.
- **Si Anthropic est inexploitable** → le MVP est conçu **non bloqué par design**. Ollama (local) + OpenAI (au token) suffisent à livrer un agent fonctionnel, et le positionnement reste **model-agnostic**. Numen ne dépend d'aucun provider unique pour exister.

La mitigation est structurelle, pas réactive : le différenciateur (**full Rust natif ultra-perf + multi-provider first-class + cœur partagé avec Paneflow**) tient indépendamment du sort d'Anthropic. Le spike auth ne décide pas si le projet vit — il décide seulement quels providers sont dans le MVP. Mais il se tranche **en premier**, avant tout engagement d'architecture.
