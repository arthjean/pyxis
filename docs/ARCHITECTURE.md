# Architecture de référence — Numen

> Statut : étude / design, pré-implémentation. Aucun code écrit. Ce document est la source de vérité architecturale. Il fixe les invariants ; il ne fige pas chaque signature.
>
> Documents liés : [`docs/PROVIDERS.md`](./PROVIDERS.md) (couche multi-provider, taxonomie d'erreurs, stratégie cache-hit), [`docs/ROADMAP.md`](./ROADMAP.md) (phases, spike Phase 0), [`docs/DECISIONS.md`](./DECISIONS.md) (ADR — décisions structurantes).

Numen est une CLI agent IA en terminal, écrite en Rust natif, multi-provider first-class, conçue pour partager son cœur avec Paneflow (GPUI). La commande est `numen`. L'inspiration vient de l'architecture interne de Claude Code, mais transposée à un binaire Rust ultra-performant et agnostique au modèle.

Différenciateur : **qualité Claude Code, tous les providers frontier, perf Rust + intégration profonde avec Paneflow.** Pas de pari « vertical Rust verification-grounded » (TAM trop étroit), pas de pari « sandbox déclaratif » (Codex le fait déjà). Le pari est : full Rust natif ultra-perf + multi-provider de première classe (là où Claude Code est Anthropic-only) + cœur embarquable in-process dans Paneflow.

---

## 0. Nommage des crates et du binaire

Le brief a sécurisé sur crates.io les noms `numen`, `numen-cli`, `numen-core` (libres — atout décisif pour un projet Cargo). Le travail d'architecture, lui, raisonne en crates **`agent-*`** (`agent-core`, `agent-cli`, `agent-tui`, …) pour nommer les responsabilités sans préfixe redondant à l'intérieur du workspace.

Convention retenue, à graver avant la première ligne de code (cf. ADR-5 dans [`docs/DECISIONS.md`](./DECISIONS.md)) :

- **Binaire publié et commande** : `numen`. Le crate qui produit ce binaire est `agent-cli` à l'intérieur du workspace, mais expose le nom de binaire `numen` (`[[bin]] name = "numen"`).
- **Crate racine publiée** : `numen` (façade/ré-export public si une API de bibliothèque est ouverte un jour) ; `numen-core` et `numen-cli` restent réservés et pointeront, le cas échéant, sur `agent-core` et `agent-cli`.
- **Crates internes** : noms `agent-*` dans le workspace (non publiés, ou publiés sous le namespace `numen-*` si besoin via `package.name`).

Autrement dit, les réservations `numen*` couvrent la **surface publique** (binaire, façade) ; les noms `agent-*` décrivent l'**organisation interne**. Les deux ne se contredisent pas : c'est une divergence assumée entre nom publié et nom de travail. Tout le reste du document emploie les identifiants internes `agent-*`.

---

## 1. Principe directeur — cœur headless, frontend = client

L'invariant fondateur : **`agent-core` est totalement découplé du frontend et n'émet QUE des événements structurés.** Jamais d'ANSI, jamais de couleur, jamais de mise en page sortant du cœur. Le cœur ne sait pas qu'un terminal existe.

Conséquences directes :

- Le frontend Ratatui (`agent-tui`) est un **simple client** : il consomme un flux d'`AgentEvent` et décide seul de leur rendu. On peut écrire un autre client sans toucher au cœur.
- **Paneflow embarque `agent-core` in-process** (pas d'IPC, pas de FFI : même process, types Rust partagés) et rend les mêmes événements via GPUI — diffs GPU-accélérés, arbre de plan, review par hunk. C'est un enrichissement *futur* via protocole, qui ne casse jamais le mode terminal par défaut.
- Le mode headless `-p` (print) marche **sans Ratatui** : on consomme le stream d'événements et on les sérialise. Le cœur est testable sans I/O réelle.

```
                         ┌──────────────────────────────┐
                         │          agent-core           │
                         │  boucle + state machine +     │
                         │  types canoniques (headless)  │
                         │  émet: Stream<AgentEvent>     │
                         └──────────────┬───────────────┘
                                        │  événements structurés
                                        │  (jamais d'ANSI)
                 ┌──────────────────────┼──────────────────────┐
                 │                      │                      │
        ┌────────▼────────┐    ┌────────▼────────┐    ┌────────▼─────────┐
        │   agent-tui     │    │   mode -p       │    │  Paneflow (GPUI) │
        │ Ratatui client  │    │  headless print │    │  embed in-process│
        │ (terminal)      │    │  (JSON/texte)   │    │  rendu GPU         │
        └─────────────────┘    └─────────────────┘    └──────────────────┘
```

Règle d'or absolue, vérifiée à la compilation par le graphe de dépendances Cargo : **`agent-core` ne dépend NI de `agent-tui` NI de `agent-provider`.** Le cœur reste testable sans réseau, sans terminal, sans modèle réel. Les implémentations I/O sont injectées via des traits (cf. `Deps`, §3).

---

## 2. Workspace de crates

Le projet est un workspace Cargo. Chaque crate a une responsabilité unique et un périmètre de dépendances contraint.

| Crate | Rôle | Dépendances interdites |
|---|---|---|
| `agent-core` | Boucle d'agent, state machine, types canoniques (messages, content blocks, transcript, budget). | Aucune dépendance TUI / HTTP. Ne connaît ni Ratatui ni reqwest. |
| `agent-provider` | Trait `Provider` + adapters (reqwest + eventsource-stream). Normalisation vers le format canonique, émission de `StreamEvent`. | Ne dépend pas de `agent-tui`. |
| `agent-tools` | `Registry`, trait `Tool`, dispatch concurrent/série, permissions, hooks, taint. | — |
| `agent-mcp` | Wrapper autour de `rmcp` (SDK MCP Rust officiel). Expose les outils MCP comme `DynTool`. Dépend de `agent-tools` pour le trait. | — |
| `agent-tui` | Frontend Ratatui + crossterm. **Découplé du core via canaux.** | **Jamais importé par le core.** |
| `agent-session` | Persistance JSONL append-only, compaction, resume. | — |
| `agent-sandbox` | Landlock FS + proxy réseau local + `PolicyEngine`. | — |
| `agent-auth` | Stockage de credentials (Secret Service / keyring), OAuth, refresh token. **C'est ici que se joue le go/no-go auth Anthropic.** | — |
| `agent-tokenizer` | Comptage de tokens local (tiktoken-rs / tokenizers). Indispensable pour la compaction sur les providers sans usage en stream (Ollama). Headless. | Aucune dépendance TUI / HTTP. |
| `agent-cli` | Binaire `numen`, wiring. **Seul crate qui dépend de tout.** | — |

### Graphe de dépendances (sens des flèches = « dépend de »)

```
                              ┌───────────┐
                              │ agent-cli │  (binaire `numen`, wiring complet)
                              └─────┬─────┘
        ┌──────────┬──────────┬─────┴─────┬──────────┬──────────┐
        ▼          ▼          ▼           ▼          ▼          ▼
 ┌───────────┐ ┌────────┐ ┌────────┐ ┌─────────┐ ┌────────┐ ┌──────────┐
 │ agent-tui │ │ agent- │ │ agent- │ │ agent-  │ │ agent- │ │ agent-   │
 │ (Ratatui) │ │provider│ │ tools  │ │ session │ │sandbox │ │  auth    │
 └─────┬─────┘ └───┬────┘ └───┬────┘ └────┬────┘ └───┬────┘ └────┬─────┘
       │           │          │           │          │           │
       │           │     ┌────┴────┐      │          │           │
       │           │     │agent-mcp│      │          │           │
       │           │     └────┬────┘      │          │           │
       └───────────┴──────────┴───────────┴──────────┴───────────┘
                              ▼
                        ┌───────────┐        ┌─────────────────┐
                        │agent-core │───────▶│ agent-tokenizer │
                        │ (headless)│        │ (comptage local)│
                        └───────────┘        └─────────────────┘
```

`agent-core` est en bas du graphe : tout le monde le connaît, il ne connaît personne **sauf** `agent-tokenizer` (lui-même headless, sans I/O), dont il dépend pour le fallback de comptage de tokens (§3.3). Les dépendances I/O (`agent-provider`, `agent-tui`) sont **injectées** dans le cœur via des traits (`injectable deps`, §3.2), jamais référencées en dur. `agent-mcp` dépend de `agent-tools` pour réutiliser le trait `Tool`/`DynTool` ; `agent-cli` est le seul à dépendre de l'ensemble.

---

## 3. La boucle d'agent

### 3.1 State machine à transitions typées

La boucle est une **state machine dont les transitions sont un enum exhaustif vérifié par le compilateur.** Ajouter un état ou un cas sans le traiter casse la compilation : c'est le filet de sûreté principal d'un agent sans SDK officiel.

L'API consommateur est un **stream** via `async-stream` : `run_agent` renvoie un `Stream<Item = AgentEvent>` que le frontend (ou le mode `-p`) consomme. Le cœur ne « pousse » rien vers un terminal — il yield des événements.

> **Deux types d'événements, ne pas confondre.** `StreamEvent` (défini dans `agent-provider`, cf. [`docs/PROVIDERS.md`](./PROVIDERS.md)) circule **provider → core** : ce sont les fragments bruts normalisés du stream modèle (`TextDelta`, `ToolCallDelta`, `Usage`, `Done`, …). `AgentEvent` (défini dans `agent-core`, cf. §10.1) circule **core → clients** : c'est le contrat de présentation consommé par `agent-tui`, le mode `-p` et Paneflow. `agent-core` **consomme** les `StreamEvent`, les accumule, et **traduit** le résultat décisionnel en `AgentEvent`. Ce sont deux frontières distinctes ; aucune n'expose d'ANSI.

### 3.2 Patterns repris de Claude Code

| Pattern | Description | Pourquoi |
|---|---|---|
| **transcript-before-response** | Le message est persisté dans le transcript JSONL **avant** l'appel API (`sync_data`). | Crash pendant le stream = pas de perte ; resume cohérent. |
| **withholding** | Un `Option<PendingError>` retient **uniquement** une erreur PTL / max-tokens (contexte plein, troncature) **jusqu'à échec confirmé** du recovery (compaction réactive). On ne propage l'erreur que si la récupération échoue vraiment. | Évite de tuer une session récupérable par compaction. **Distinct du retry transverse** (529/Retryable), qui relève du backoff provider, pas du `PendingError`. |
| **injectable deps** | Provider, clock, tokenizer, sandbox, tools passés en paramètres (traits, struct `Deps`). | Boucle testable sans API réelle. |
| **ContextBudget unifié** | Calculé **une seule fois par modèle**, source unique de vérité pour compaction, troncature, alerte de fenêtre. | Pas de divergence entre deux estimateurs. |
| **circuit breaker autocompact** | Coupe après N échecs d'autocompact consécutifs au lieu de boucler. | Anti error-loop. |

**Précision withholding ↔ retry (point d'incohérence corrigé).** Deux mécanismes ne doivent jamais être mélangés :

- **Withholding** retient une erreur de **contexte** (PTL, max-tokens, `413`) dans `PendingError`, tente une **compaction réactive** (§5), et ne propage qu'en cas d'échec du recovery.
- **Le retry transverse** gère les erreurs **transitoires** (`Retryable`, `Overloaded`/`529`, `RateLimited`/`429`) via backoff exponentiel + jitter, dans la couche provider (cf. [`docs/PROVIDERS.md`](./PROVIDERS.md) §retry). Ces erreurs **ne passent jamais** par `PendingError`.

### 3.3 Comptage de tokens et fallback Ollama

`update_budget` lit le `Usage` émis par le `StreamEvent` du provider **si présent**. Sinon (cas Ollama, usage souvent absent en stream), fallback obligatoire sur `agent-tokenizer` (comptage local). Sans ce fallback, **la compaction est cassée** sur les providers qui ne renvoient pas d'usage.

### 3.4 Taxonomie d'erreurs canonique — `ErrorClass`

Le type d'erreur classifiée est **`ErrorClass`** (nom canonique, partout dans le code et la doc — jamais `ErrClass`). Il aligne `agent-core` et `agent-provider`. Cinq variantes (cf. [`docs/PROVIDERS.md`](./PROVIDERS.md), source de vérité de la taxonomie) :

```rust
enum ErrorClass {
    Retryable,            // transitoire générique → backoff + jitter
    Overloaded,           // 529 → backoff agressif, fallback modèle après 3×529
    RateLimited,          // 429 → honore Retry-After
    Auth(AuthError),      // 401/credential → cf. AuthError ci-dessous
    InvalidRequest,       // 4xx non récupérable → propagation immédiate
}

enum AuthError {
    Expired,              // token expiré → refresh OAuth puis retry
    ThirdPartyBlocked,    // "This credential is only authorized for use with Claude Code…"
    Invalid,              // credential invalide → propagation
}
```

`classify_error(&e) -> ErrorClass` est implémenté dans chaque adapter provider. Routage de la boucle :

- `Retryable | Overloaded | RateLimited` → **retry transverse** (backoff, jitter, `Retry-After`, fallback modèle). Jamais retenu dans `PendingError`.
- `Auth(Expired)` → refresh OAuth via `agent-auth`, puis retry.
- `Auth(ThirdPartyBlocked | Invalid)` → propagation (erreur fatale d'auth).
- `InvalidRequest` → propagation immédiate.
- Erreur de **contexte** (PTL / max-tokens / `413`) → **pas** une variante `ErrorClass` transitoire : elle alimente le **withholding** (`PendingError`) et déclenche la compaction réactive.

### 3.5 Pseudo-Rust

```rust
/// Transition exhaustive : chaque variante est un événement décisionnel de la boucle.
/// Le compilateur force le traitement de tous les cas dans le `match` du driver.
enum Transition {
    /// Le modèle a fini son tour sans tool_use → on rend la main à l'utilisateur.
    EndTurn,
    /// Le modèle demande l'exécution d'un ou plusieurs outils.
    RunTools(Vec<ToolCall>),
    /// Budget de contexte dépassé proactivement → compaction avant le prochain appel.
    Compact(CompactKind),
    /// Erreur de contexte retenue (withholding : PTL / max-tokens) à récupérer avant de propager.
    Recover(PendingError),
    /// Plafond de tours / budget épuisé.
    Exhausted(ExhaustReason),
    /// Erreur fatale non récupérable → on propage.
    Fail(AgentError),
}

/// API consommateur : un stream d'événements structurés (AgentEvent). Aucun ANSI ici.
fn run_agent(mut ctx: AgentContext, deps: Deps) -> impl Stream<Item = AgentEvent> {
    async_stream::stream! {
        // ContextBudget calculé 1x pour ce modèle : source unique de vérité.
        let budget = ContextBudget::for_model(ctx.model());
        // withholding : retient UNIQUEMENT une erreur de contexte (PTL / max-tokens).
        let mut pending: Option<PendingError> = None;

        loop {
            // transcript-before-response : on persiste AVANT l'appel API.
            ctx.session.sync_data(&ctx.messages).await;

            // Compaction proactive si le budget est dépassé (cf. §5, cascade).
            if budget.exceeds_threshold(&ctx) {
                ctx = compact(ctx, CompactKind::Auto, &deps).await;
                yield AgentEvent::Compacted(CompactKind::Auto);
            }

            // Appel modèle : stream de StreamEvent provider normalisés (cf. §11 multi-provider).
            let mut stream = deps.provider.stream(&ctx.request()).await;
            let mut acc = Accumulator::new();

            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(StreamEvent::TextDelta(t))       => yield AgentEvent::Text(t),
                    Ok(StreamEvent::ReasoningDelta(r))  => yield AgentEvent::Reasoning(r),
                    Ok(StreamEvent::ToolCallStart(c))   => acc.open_call(c),
                    Ok(StreamEvent::ToolCallDelta(d))   => acc.push_call(d),
                    Ok(StreamEvent::ToolCallEnd(id))    => acc.close_call(id),
                    Ok(StreamEvent::Usage(u))           => budget.update_budget(u),
                    Ok(StreamEvent::Done)               => break,
                    Err(e) => {
                        // classify_error → ErrorClass (5 variantes, cf. §3.4).
                        match deps.provider.classify_error(&e) {
                            // Transitoires : RETRY TRANSVERSE (backoff/jitter), PAS de withholding.
                            ErrorClass::Retryable
                            | ErrorClass::Overloaded
                            | ErrorClass::RateLimited => {
                                deps.backoff.wait_for(&e).await; // honore Retry-After, jitter, 3×529 → fallback
                                break; // on reboucle avec le même contexte
                            }
                            ErrorClass::Auth(AuthError::Expired) => {
                                deps.auth.refresh().await;
                                break; // retry après refresh
                            }
                            ErrorClass::Auth(_) | ErrorClass::InvalidRequest => {
                                yield AgentEvent::Error(e.into());
                                return;
                            }
                        }
                    }
                }
            }

            // Erreur de CONTEXTE (PTL / max-tokens / 413) détectée sur le tour → withholding.
            if let Some(ctx_err) = acc.context_error() {
                pending = Some(PendingError::from(ctx_err));
            }

            // Fallback usage : si le stream n'a pas émis d'Usage (Ollama), compter en local.
            if !budget.usage_seen() {
                budget.update_budget(deps.tokenizer.count(&ctx, &acc));
            }

            // Calcul de la transition à partir de l'état accumulé.
            let transition = decide_transition(&acc, &budget, pending.take());

            match transition {
                Transition::EndTurn => { yield AgentEvent::EndTurn; return; }
                Transition::RunTools(calls) => {
                    // Dispatch concurrent/série + pipeline strict (cf. §4).
                    let results = deps.tools.dispatch(calls, &mut ctx).await;
                    for r in &results { yield AgentEvent::ToolResult(r.clone()); }
                    ctx.append_tool_results(results);
                    // on reboucle : le modèle voit les résultats
                }
                Transition::Compact(kind) => {
                    ctx = compact(ctx, kind, &deps).await;
                    yield AgentEvent::Compacted(kind);
                }
                Transition::Recover(err) => {
                    // withholding : on tente la récupération (compaction réactive) ;
                    // si elle échoue, on propage l'erreur de contexte retenue.
                    match try_recover(&mut ctx, &err, &deps).await {
                        Ok(()) => continue,
                        Err(_) => { yield AgentEvent::Error(err.into()); return; }
                    }
                }
                Transition::Exhausted(why) => { yield AgentEvent::Exhausted(why); return; }
                Transition::Fail(err)      => { yield AgentEvent::Error(err); return; }
            }
        }
    }
}
```

`decide_transition` est pur (pas d'I/O), donc testable unitairement : on lui passe un `Accumulator` + un `ContextBudget` + un `Option<PendingError>` et on vérifie la transition produite. C'est le nœud de la testabilité headless. Noter la séparation stricte : les transitoires (`Retryable`/`Overloaded`/`RateLimited`) sont absorbés par `deps.backoff` **dans** la boucle de stream et ne deviennent jamais des `PendingError` ; seules les erreurs de contexte alimentent le withholding.

---

## 4. Système d'outils

### 4.1 Trait `Tool` fail-closed + `DynTool`

Le trait `Tool` impose des **defaults fail-closed** : si l'auteur d'un outil ne précise rien, l'outil est considéré comme dangereux (non concurrent, non read-only, sortie non fiable). On élargit les permissions explicitement, jamais par défaut.

```rust
trait Tool: Send + Sync {
    type Input: DeserializeOwned + Send;
    type Output: Serialize + Send;

    fn name(&self) -> &str;
    fn prompt(&self) -> String; // description fournie au modèle, cappée

    /// Defaults FAIL-CLOSED : on assume le pire tant qu'on n'a pas prouvé le contraire.
    fn is_concurrency_safe(&self) -> bool { false }  // pas de parallélisme par défaut
    fn is_read_only(&self) -> bool { false }         // on assume une mutation
    fn returns_untrusted(&self) -> bool { true }     // sortie taintée par défaut (OWASP LLM01)

    fn validate_input(&self, input: &Self::Input) -> Result<(), ValidationError>;
    fn check_permissions(&self, input: &Self::Input, ctx: &PermCtx) -> PermissionDecision;

    async fn call(&self, input: Self::Input, ctx: &mut ToolCtx)
        -> Result<Self::Output, ToolError>;
}

/// Object-safety : le trait générique n'est pas object-safe (assoc. types + generics).
/// DynTool est le wrapper dyn-compatible stocké dans le Registry et utilisé pour MCP.
trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn prompt(&self) -> String;
    fn is_concurrency_safe(&self) -> bool;
    fn is_read_only(&self) -> bool;
    fn returns_untrusted(&self) -> bool;
    async fn call_json(&self, raw: serde_json::Value, ctx: &mut ToolCtx)
        -> Result<ToolOutput, ToolError>;
}
```

Les outils MCP (cf. §6) sont enregistrés comme `DynTool` pour uniformité : du point de vue du dispatch, un outil natif et un outil MCP sont indistinguables.

### 4.2 Dispatch concurrent / série

Le `Registry` partitionne les `ToolCall` d'un batch :

- **Concurrent-safe** (`is_concurrency_safe() == true`, typiquement les reads) : exécutés en parallèle via `buffer_unordered(10)` (10 en vol max).
- **Le reste** : exécuté en série dans un `for`.
- Les **contextModifiers** (outils qui mutent le contexte de l'agent) passent **en série, après** le batch concurrent.

```rust
async fn dispatch(&self, calls: Vec<ToolCall>, ctx: &mut ToolCtx) -> Vec<ToolResult> {
    let (concurrent, serial) = self.partition_by_safety(calls);

    // batch concurrent : reads en parallèle, plafond 10
    let mut results: Vec<ToolResult> = stream::iter(concurrent)
        .map(|call| self.run_one(call, ctx))
        .buffer_unordered(10)
        .collect()
        .await;

    // batch sériel : mutations, une par une
    for call in serial {
        results.push(self.run_one(call, ctx).await);
    }
    results
}
```

### 4.3 Pipeline d'exécution STRICT (par outil)

Chaque appel d'outil traverse exactement cette séquence, dans cet ordre, sans court-circuit :

```
serde parse
   └─▶ validate_input
        └─▶ hooks PreToolUse
             └─▶ check_permissions + règles globales
                  ├─ deny ─▶ erreur (on n'appelle jamais call())
                  └─ allow ─▶ call()  [wrappé dans tokio::time::timeout]
                                └─▶ TAINT untrusted output
                                     └─▶ hooks PostToolUse
                                          └─▶ Message (résultat injecté dans le transcript)
```

`call()` est systématiquement enveloppé dans un `tokio::time::timeout` : un outil qui pend ne bloque pas la boucle.

### 4.4 Permissions — 5 modes

| Mode | Comportement |
|---|---|
| `Default` | Demande à l'utilisateur sur action sensible. |
| `AcceptEdits` | Auto-accepte les éditions de fichiers, demande le reste. |
| `DontAsk` | N'interrompt pas (pour automatisations contrôlées). |
| `BypassPermissions` | Court-circuite les checks (usage avancé / sandbox). |
| `Plan` | Lecture seule, aucune mutation autorisée — phase de planification. |

### 4.5 Defer / ToolSearch

Les outils peuvent être **chargés à la demande** via `ToolSearch` : le modèle découvre un outil quand il en a besoin plutôt que de le porter en permanence dans le prompt. **Seuil : ne pas déférer si moins de 15 outils.** En dessous, le coût de prompt est négligeable et le defer ajoute de la latence pour rien.

### 4.6 Taint untrusted (OWASP LLM01 — prompt injection)

Tout output d'outil (`Bash`, `Read`, MCP, etc.) est **untrusted par défaut** (`returns_untrusted() == true`). Le taint se **propage** dans le contexte. Règle de défense : si un tour contient du taint récent et que le modèle demande une **action destructive ou réseau**, on **force `Ask`** quel que soit le mode de permission courant (hors `BypassPermissions`). C'est la mitigation directe de l'injection de prompt via contenu lu.

---

## 5. Compaction en cascade

La compaction va du **moins** au **plus** destructeur. On ne déclenche un niveau plus agressif que si le précédent ne suffit pas.

| Niveau | Déclencheur | Action |
|---|---|---|
| **microcompact** | Pression légère sur le budget | Élague les **vieux tool results** (les plus volumineux, les moins utiles rétroactivement). |
| **snip / collapse** | Pression moyenne (feature-gated, hors MVP) | Replie / résume des segments intermédiaires. |
| **autocompact** | Seuil de budget atteint (proactif) | Résumé total proactif **avant** de heurter la limite API. |
| **reactive** | `413` API réel reçu (erreur de contexte) | Compaction de secours après échec confirmé. **C'est le mécanisme déclenché par le withholding** (§3.2) : l'erreur PTL / max-tokens retenue dans `PendingError` provoque cette compaction réactive ; échec → propagation. |

**Full compact** = l'agent est **forké** (`tokio::spawn`) en mode resume : on relance une boucle sur le transcript compacté, **images strippées** (on ne re-paye pas les tokens vision dans le résumé). Le `ContextBudget` unifié (§3.2) pilote tous ces seuils depuis une source unique.

Articulation withholding ↔ reactive (rappel explicite) : seules les erreurs **de contexte** (PTL / max-tokens / `413`) déclenchent la branche `reactive`. Les `529`/`429`/`Retryable` sont absorbés en amont par le backoff transverse (§3.4) et n'entrent jamais dans cette cascade.

---

## 6. MCP via `rmcp`

Numen consomme MCP via le SDK Rust officiel `rmcp` (wrappé dans `agent-mcp`).

L'état d'un serveur MCP est un **enum discriminé** : le `client` n'est **accessible que dans la variante `Connected`.** Impossible d'appeler un serveur non connecté — le compilateur l'interdit.

```rust
enum McpServer {
    Disconnected { config: McpConfig },
    Connecting   { config: McpConfig },
    Connected    { client: RmcpClient, tools: Vec<DynToolHandle> },
    Failed       { config: McpConfig, error: McpError },
}
```

Règles MCP :

- **Description cappée à 2048 caractères** (un serveur ne peut pas polluer le prompt).
- **OAuth PKCE par serveur** (creds via `agent-auth`).
- Outils MCP enregistrés comme `DynTool` (uniformité §4.1).
- **Tous** les outils MCP ont `returns_untrusted() == true` — le taint (§4.6) s'applique intégralement à leurs sorties.

---

## 7. Sessions — JSONL + resume

Persistance **JSONL append-only**. Chaque ligne est une `entry` discriminée :

```rust
enum SessionEntry {
    Message(Message),                 // tour user/assistant/tool
    CompactBoundary(CompactKind),     // marque une frontière de compaction
    FileHistorySnapshot(FileSnapshot),// état d'un fichier pour rollback/diff
}
```

- **Append atomique** : chaque entry est écrite intégralement ou pas du tout (pas de ligne JSONL tronquée).
- **Resume** = on **rejoue le log** et on **reconstruit l'état** (messages, frontières de compaction, snapshots fichiers). Couplé au transcript-before-response (§3.2), une session interrompue en plein stream se rouvre proprement.

---

## 8. Sous-agents

Un sous-agent est lancé via `tokio::spawn(run_agent(...))` avec un **transcript séparé**. La communication parent ↔ enfant se fait par **`mpsc`** : l'enfant yield ses `AgentEvent`, le parent les agrège.

Variante **InProcessTeammate** : implémentée via `tokio::task_local`, pour partager du contexte ambiant (config, session root) sans le passer explicitement à chaque appel.

```
        run_agent (parent)
              │ tokio::spawn
   ┌──────────┼──────────┐
   ▼          ▼          ▼
sous-agent  sous-agent  sous-agent     ← transcripts séparés
   │ mpsc     │ mpsc     │ mpsc
   └──────────┴──────────┘
              ▼
        agrégation des AgentEvent côté parent
```

Chaque sous-agent réutilise la **même** `run_agent` (§3) : un sous-agent est un agent comme un autre, avec son propre budget et son propre transcript.

---

## 9. Frontend `agent-tui` — Ratatui + crossterm

`numen` s'ouvre **directement dans le shell**, ce n'est pas une fenêtre. Stack : **Ratatui + crossterm**.

GPUI a été **envisagé puis rejeté** pour le frontend standalone : GPUI ouvre une fenêtre GPU (app desktop), pas une CLI terminal. Clarification importante : Ink (de Claude Code) **est** un TUI — il rend de l'ANSI dans le terminal, ce n'est pas magique. **Le plafond visuel d'un terminal est identique pour Ink et Ratatui** ; c'est le **design** qui fait toute la différence.

Esthétique cible : **monochrome, moderne, épurée (Rauch / Vercel).** Pas de TUI « à l'ancienne » avec bordures doubles et couleurs criardes.

Découplage : `agent-tui` consomme le `Stream<AgentEvent>` du cœur **via un canal**, jamais par appel direct au cœur. Le cœur ne connaît pas Ratatui (§1, règle d'or). Le TUI est, architecturalement, interchangeable.

```
agent-core ──Stream<AgentEvent>──▶ [canal] ──▶ agent-tui (boucle de rendu Ratatui)
                                                  │
                          input clavier/crossterm │  (commandes utilisateur)
                                                  ▼
                                    renvoi vers le cœur (nouveau message)
```

---

## 10. Protocole d'événements cœur → frontend + embedding Paneflow

### 10.1 Le contrat `AgentEvent`

`AgentEvent` est **le** contrat entre le cœur et tout client. Il est structuré, sérialisable, et ne contient aucune décision de présentation.

```rust
enum AgentEvent {
    Text(String),                 // delta de texte assistant
    Reasoning(String),            // delta de raisonnement (si le provider en émet)
    ToolCall(ToolCallView),       // un outil va s'exécuter
    ToolResult(ToolResultView),   // résultat (taint inclus dans le view-model)
    Compacted(CompactKind),       // une compaction vient d'avoir lieu
    PermissionAsk(PermissionReq), // demande d'autorisation à l'utilisateur
    EndTurn,
    Exhausted(ExhaustReason),
    Error(AgentError),
}
```

> **`AgentEvent` ≠ `StreamEvent`.** Ce sont deux enums distincts et c'est délibéré : `StreamEvent` (`agent-provider`, cf. [`docs/PROVIDERS.md`](./PROVIDERS.md)) est l'événement **provider → core**, bas niveau, lié au wire format ; `AgentEvent` (`agent-core`, ici) est l'événement **core → clients**, lié à la présentation. Le cœur consomme les `StreamEvent`, accumule, décide une `Transition`, et émet des `AgentEvent`. Ne jamais router un `StreamEvent` directement vers un client : il porterait des détails provider et casserait le découplage.

Trois consommateurs partagent ce **même** flux d'`AgentEvent` :

1. `agent-tui` (Ratatui) — rendu terminal monochrome.
2. Mode `-p` headless — sérialisation JSON / texte.
3. Paneflow (GPUI) — rendu GPU.

### 10.2 Embedding in-process par Paneflow (enrichissement futur)

Paneflow est en GPUI (Rust). Il peut donc **embarquer `agent-core` in-process** : pas d'IPC, pas de FFI, même process, types Rust partagés. Paneflow instancie `run_agent`, consomme le `Stream<AgentEvent>`, et rend chaque événement nativement :

- `ToolResult` d'une édition → **diff GPU-accéléré**, review par hunk.
- séquence de `ToolCall` → **arbre de plan** interactif.
- `PermissionAsk` → dialogue natif GPUI.

C'est précisément ce que le découplage du §1 rend possible : le **même** cœur, **sans modification**, alimente le terminal *et* Paneflow. L'enrichissement Paneflow se fait **via le protocole d'events** (potentiellement étendu de variantes additionnelles), **sans jamais casser** le mode terminal par défaut. C'est le levier d'intégration profonde qui constitue, avec la perf Rust et le multi-provider, le différenciateur de Numen.

---

## 11. Mémoire vectorielle — hors périmètre MVP (Phase 2)

La **mémoire vectorielle (`sqlite-vec`)** est un livrable de **Phase 2** (cf. [`docs/ROADMAP.md`](./ROADMAP.md)), pas du MVP. Elle n'est pas détaillée ici car elle ne contraint aucun invariant de Phase 0/1. Notes d'ancrage pour quand elle arrivera :

- Embedding et stockage isolés dans un crate dédié (`agent-memory`, à créer), **headless** comme `agent-core` (aucune dépendance TUI).
- Récupération exposée au cœur comme une **dépendance injectable** (trait), au même titre que provider/tokenizer — pas de couplage en dur.
- Sortie de la recherche mémoire traitée comme **untrusted** si elle ré-injecte du contenu issu d'outils (le taint §4.6 doit survivre à la mise en mémoire puis à la relecture).

Tant que Phase 2 n'est pas ouverte, la mémoire vectorielle est **explicitement hors périmètre** de ce document d'architecture.

---

## Invariants à ne jamais violer

1. `agent-core` ne dépend ni de `agent-tui` ni de `agent-provider` (seule dépendance hors-core autorisée : `agent-tokenizer`, headless). Vérifié par Cargo.
2. Le cœur n'émet que des `AgentEvent` structurés — **jamais d'ANSI.** Les `StreamEvent` provider sont consommés **à l'intérieur** du cœur, jamais relayés tels quels à un client.
3. Tout output d'outil est **untrusted par défaut** ; le taint se propage et force `Ask` sur action destructive/réseau en présence de taint récent.
4. Les defaults du trait `Tool` sont **fail-closed.**
5. `ContextBudget` est calculé **une seule fois par modèle** et reste la source unique de vérité de la compaction.
6. transcript persisté **avant** l'appel API.
7. La compaction se cassera sur Ollama si le fallback `agent-tokenizer` n'est pas branché : `update_budget` lit le `Usage` du stream **sinon** compte en local.
8. **Withholding ≠ retry.** Seules les erreurs de contexte (PTL / max-tokens / `413`) alimentent `PendingError` et la compaction réactive ; les transitoires (`Retryable` / `Overloaded` / `RateLimited`) sont absorbées par le backoff transverse et n'entrent jamais dans `PendingError`.
9. Le type d'erreur classifiée est **`ErrorClass`** (5 variantes), nommé identiquement dans tout le code et toute la doc — jamais `ErrClass`.
