# Architecture de référence — Pyxis

> Statut : architecture de référence avec implémentation MVP en cours. Ce document fixe les invariants ; il ne fige pas chaque signature ni l'état livré exact.
>
> État livré courant : [`docs/CURRENT_STATUS.md`](./CURRENT_STATUS.md). Documents liés : [`docs/PROVIDERS.md`](./PROVIDERS.md) (couche multi-provider, taxonomie d'erreurs, stratégie cache-hit), [`docs/ROADMAP.md`](./ROADMAP.md) (phases, spike Phase 0), [`docs/DECISIONS.md`](./DECISIONS.md) (ADR, décisions structurantes).

Pyxis est une CLI agent IA en terminal, écrite en Rust natif, multi-provider first-class, bâtie sur un cœur headless découplé de tout frontend. La commande est `pyxis`. L'inspiration vient de l'architecture interne de Claude Code, mais transposée à un binaire Rust ultra-performant et agnostique au modèle.

Différenciateur : **qualité Claude Code, tous les providers frontier, perf Rust.** Pas de pari « vertical Rust verification-grounded » (TAM trop étroit), pas de pari « sandbox déclaratif » (Codex le fait déjà). Le pari est : full Rust natif ultra-perf + multi-provider de première classe (là où Claude Code est Anthropic-only).

---

## 0. Nommage des crates et du binaire

Le brief a sécurisé sur crates.io les noms `pyxis`, `pyxis-cli`, `pyxis-core` (libres — atout décisif pour un projet Cargo). Le travail d'architecture, lui, raisonne en crates **`agent-*`** (`agent-core`, `agent-cli`, `agent-tui`, …) pour nommer les responsabilités sans préfixe redondant à l'intérieur du workspace.

Convention retenue, à graver avant la première ligne de code (cf. ADR-5 dans [`docs/DECISIONS.md`](./DECISIONS.md)) :

- **Binaire publié et commande** : `pyxis`. Le crate qui produit ce binaire est `agent-cli` à l'intérieur du workspace, mais expose le nom de binaire `pyxis` (`[[bin]] name = "pyxis"`).
- **Crate racine publiée** : `pyxis` (façade/ré-export public si une API de bibliothèque est ouverte un jour) ; `pyxis-core` et `pyxis-cli` restent réservés et pointeront, le cas échéant, sur `agent-core` et `agent-cli`.
- **Crates internes** : noms `agent-*` dans le workspace (non publiés, ou publiés sous le namespace `pyxis-*` si besoin via `package.name`).

Autrement dit, les réservations `pyxis*` couvrent la **surface publique** (binaire, façade) ; les noms `agent-*` décrivent l'**organisation interne**. Les deux ne se contredisent pas : c'est une divergence assumée entre nom publié et nom de travail. Tout le reste du document emploie les identifiants internes `agent-*`.

---

## 1. Principe directeur — cœur headless, frontend = client

L'invariant fondateur : **`agent-core` est totalement découplé du frontend et n'émet QUE des événements structurés.** Jamais d'ANSI, jamais de couleur, jamais de mise en page sortant du cœur. Le cœur ne sait pas qu'un terminal existe.

Conséquences directes :

- Le frontend Ratatui (`agent-tui`) est un **simple client** : il consomme un flux d'`AgentEvent` et décide seul de leur rendu. On peut écrire un autre client sans toucher au cœur.
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
                          ┌─────────────┴─────────────┐
                          │                           │
                 ┌────────▼────────┐         ┌────────▼────────┐
                 │   agent-tui     │         │   mode -p       │
                 │ Ratatui client  │         │  headless print │
                 │ (terminal)      │         │  (JSON/texte)   │
                 └─────────────────┘         └─────────────────┘
```

Règle d'or absolue, vérifiée à la compilation par le graphe de dépendances Cargo : **`agent-core` ne dépend NI de `agent-tui` NI de `agent-provider`.** Le cœur reste testable sans réseau, sans terminal, sans modèle réel. Les implémentations I/O sont injectées via des traits (cf. `Deps`, §3).

---

## 2. Workspace de crates

Le projet est un workspace Cargo. Chaque crate a une responsabilité unique et un périmètre de dépendances contraint.

| Crate | Rôle | Dépendances interdites |
|---|---|---|
| `agent-core` | Boucle d'agent, state machine, types canoniques (messages, content blocks, transcript, budget). | Aucune dépendance TUI / HTTP. Ne connaît ni Ratatui ni reqwest. |
| `agent-provider` | Trait `Provider` + adapters (`reqwest`, `eventsource-stream`, `tokio-tungstenite`). Normalisation vers le format canonique, émission de `StreamEvent`. | Ne dépend pas de `agent-tui`. |
| `agent-tools` | `Registry`, trait `Tool`, dispatch concurrent/série, permissions, hooks, taint. | — |
| `agent-mcp` | Wrapper autour de `rmcp` (SDK MCP Rust officiel). Charge la config, suit le lifecycle stdio, liste les outils et les expose au modèle comme `DynTool` (nommage sûr, schéma strict, taint intégral). | — |
| `agent-tui` | Frontend Ratatui + crossterm. **Découplé du core via canaux.** | **Jamais importé par le core.** |
| `agent-runtime` | Runtime de thread durable : identité (`ThreadId`/`TurnId`/`StepId`/`EventId`/`AgentId`), cycle de vie des tours, mailbox bornée, steering, annulation hiérarchique, forks, superviseur de sous-agents. Contrat `ThreadStore` + adapter mémoire. | Aucun accès disque, aucun HTTP, aucune TUI. Ne dépend jamais de `agent-tools` ni de `agent-session`. |
| `agent-session` | Persistance JSONL append-only, compaction, resume. Porte l'adapter JSONL de `ThreadStore`, qui est AUSSI l'implémentation de `Session` : un thread a un fichier, un writer et un curseur. | — |
| `agent-sandbox` | Landlock FS + proxy réseau local + `PolicyEngine`. | — |
| `agent-auth` | Stockage de credentials (Secret Service / keyring), OAuth PKCE ChatGPT, refresh token. Les futurs flows BYOK/OAuth provider restent isolés ici. | — |
| `agent-tokenizer` | Comptage de tokens local (tiktoken-rs / tokenizers). Indispensable pour la compaction sur les providers sans usage fiable en stream. Headless. | Aucune dépendance TUI / HTTP. |
| `agent-cli` | Binaire `pyxis`, wiring. **Seul crate qui dépend de tout.** | — |

Observabilité : les crates **émettent** via la façade `tracing`, jamais sur
une sortie de processus. Le binaire est le seul à installer un souscripteur
(`PYXIS_LOG`) et le seul à écrire sur stdout/stderr. Émettre sans souscripteur
n'est pas une I/O : l'invariant 1 (cœur headless) reste tenu.

### Graphe de dépendances (sens des flèches = « dépend de »)

```
                              ┌───────────┐
                              │ agent-cli │  (binaire `pyxis`, wiring complet)
                              └─────┬─────┘
        ┌──────────┬──────────┬─────┴─────┬──────────┬──────────┐
        ▼          ▼          ▼           ▼          ▼          ▼
 ┌───────────┐ ┌────────┐ ┌────────┐ ┌─────────┐ ┌────────┐ ┌──────────┐
 │ agent-tui │ │ agent- │ │ agent- │ │ agent-  │ │ agent- │ │ agent-   │
 │ (Ratatui) │ │provider│ │ tools  │ │ session │ │sandbox │ │  auth    │
 └─────┬─────┘ └───┬────┘ └───┬────┘ └────┬────┘ └───┬────┘ └────┬─────┘
       │           │          │           │          │           │
       │           │     ┌────┴────┐ ┌────┴─────┐    │           │
       │           │     │agent-mcp│ │  agent-  │    │           │
       │           │     └────┬────┘ │ runtime  │    │           │
       │           │          │      └────┬─────┘    │           │
       └───────────┴──────────┴───────────┴──────────┴───────────┘
                              ▼
                        ┌───────────┐        ┌─────────────────┐
                        │agent-core │───────▶│ agent-tokenizer │
                        │ (headless)│        │ (comptage local)│
                        └───────────┘        └─────────────────┘
```

`agent-core` est en bas du graphe : tout le monde le connaît, il ne connaît personne **sauf** `agent-tokenizer` (lui-même headless, sans I/O), dont il dépend pour le fallback de comptage de tokens (§3.3). Les dépendances I/O (`agent-provider`, `agent-tui`) sont **injectées** dans le cœur via des traits (`injectable deps`, §3.2), jamais référencées en dur. `agent-mcp` dépend de `agent-tools` pour réutiliser le trait `Tool`/`DynTool` ; `agent-cli` est le seul à dépendre de l'ensemble.

`agent-runtime` s'intercale entre `agent-core` et les clients (ADR-12, §7bis). Les
flèches vers lui sont à sens unique et le restent : `agent-session` et
`agent-tools` en dépendent (pour l'adapter JSONL du store et pour les cinq outils
de sous-agents), lui ne dépend d'aucun des deux. C'est ce qui garde sa politique
d'autorité testable sans registre d'outils, et son cycle de vie testable sans
disque.

---

## 3. La boucle d'agent

### 3.1 State machine à transitions typées

La boucle est une **state machine dont les transitions sont un enum exhaustif vérifié par le compilateur.** Ajouter un état ou un cas sans le traiter casse la compilation : c'est le filet de sûreté principal d'un agent sans SDK officiel.

L'API consommateur est un **stream** via `async-stream` : `run_agent` renvoie un `Stream<Item = AgentEvent>` que le frontend (ou le mode `-p`) consomme. Le cœur ne « pousse » rien vers un terminal — il yield des événements.

> **Deux types d'événements, ne pas confondre.** `StreamEvent` (contrat défini dans `agent-core`, produit par les adapters de `agent-provider`, cf. [`docs/PROVIDERS.md`](./PROVIDERS.md)) circule **provider → core** : ce sont les fragments bruts normalisés du stream modèle (`TextDelta`, `ToolCallDelta`, `Usage`, `Done`, …). `AgentEvent` (défini dans `agent-core`, cf. §10.1) circule **core → clients** : c'est le contrat de présentation consommé par `agent-tui` et le mode `-p`. `agent-core` **consomme** les `StreamEvent`, les accumule, et **traduit** le résultat décisionnel en `AgentEvent`. Ce sont deux frontières distinctes ; aucune n'expose d'ANSI.

### 3.2 Patterns repris de Claude Code

| Pattern | Description | Pourquoi |
|---|---|---|
| **transcript-before-response** | Le message est persisté dans le transcript JSONL **avant** l'appel API (`sync_data`). | Crash pendant le stream = pas de perte ; resume cohérent. |
| **withholding** | Un `Option<PendingError>` retient **uniquement** une erreur PTL / max-tokens (contexte plein, troncature) **jusqu'à échec confirmé** du recovery (compaction réactive). On ne propage l'erreur que si la récupération échoue vraiment. | Évite de tuer une session récupérable par compaction. **Distinct du retry transverse** (529/Retryable), piloté par `run_agent`, pas par `PendingError`. |
| **injectable deps** | Provider, clock, tokenizer, sandbox, tools passés en paramètres (traits, struct `Deps`). | Boucle testable sans API réelle. |
| **ContextBudget unifié** | Calculé **une seule fois par modèle**, source unique de vérité pour compaction, troncature, alerte de fenêtre. | Pas de divergence entre deux estimateurs. |
| **circuit breaker autocompact** | Coupe après N échecs d'autocompact consécutifs au lieu de boucler. | Anti error-loop. |

**Précision withholding ↔ retry (point d'incohérence corrigé).** Deux mécanismes ne doivent jamais être mélangés :

- **Withholding** retient une erreur de **contexte** (PTL, max-tokens, `413`) dans `PendingError`, tente une **compaction réactive** (§5), et ne propage qu'en cas d'échec du recovery.
- **Le retry transverse** gère les erreurs **transitoires** (`Retryable`, `Overloaded`/`529`, `RateLimited`/`429`) via un budget d'attempts, backoff exponentiel, jitter et `Retry-After` dans l'unique boucle `run_agent` (cf. [`docs/PROVIDERS.md`](./PROVIDERS.md) §retry). L'adapter classifie, mais ne rouvre pas le provider. Ces erreurs **ne passent jamais** par `PendingError`.

### 3.3 Comptage de tokens et fallback local

`update_budget` lit le `Usage` émis par le `StreamEvent` du provider **si présent**. Sinon, fallback obligatoire sur `agent-tokenizer` (comptage local). Sans ce fallback, **la compaction est cassée** sur les providers qui ne renvoient pas d'usage fiable.

### 3.4 Taxonomie d'erreurs canonique — `ErrorClass`

Le type d'erreur classifiée est **`ErrorClass`** (nom canonique, partout dans le code et la doc). Il aligne `agent-core` et `agent-provider`. Sept variantes (cf. [`docs/PROVIDERS.md`](./PROVIDERS.md), source de vérité de la taxonomie) :

```rust
enum ErrorClass {
    Retryable,            // transitoire générique → backoff + jitter
    Overloaded(u16),      // 529 → fallback modèle si configuré, sinon backoff agressif
    RateLimited,          // 429 → honore Retry-After
    ContextLimit,         // compaction réactive puis réouverture dans le même budget
    ReasoningReplayRejected, // une reprise sans replay, dans le même budget
    Auth(AuthError),      // 401/credential → cf. AuthError ci-dessous
    InvalidRequest,       // 4xx non récupérable → propagation immédiate
}

enum AuthError {
    Expired,              // token expiré → refresh OAuth puis retry
    ThirdPartyBlocked,    // "This credential is only authorized for use with Claude Code…"
    Invalid,              // credential invalide → propagation
    ReconnectRequired,    // refresh absent, refusé ou déjà consommé
}
```

`classify_error(&e) -> ErrorClass` est implémenté dans chaque adapter provider. Routage de la boucle :

- `Retryable | Overloaded | RateLimited` → **retry transverse** (backoff, jitter, `Retry-After`, fallback modèle). Jamais retenu dans `PendingError`.
- `ContextLimit` → withholding et compaction réactive, mais toute réouverture consomme le même budget d'attempts.
- `ReasoningReplayRejected` → une seule réouverture sans replay, qui consomme l'attempt suivant.
- `Auth(Expired)` → au plus un refresh OAuth cancellation-guarded par sampling, puis réouverture avec les headers reconstruits.
- `Auth(ThirdPartyBlocked | Invalid | ReconnectRequired)` → propagation (erreur fatale d'auth).
- `InvalidRequest` → propagation immédiate.
- Erreur de **contexte** (PTL / max-tokens / `413`) → `ContextLimit`, qui alimente le **withholding** (`PendingError`) et déclenche la compaction réactive sans contourner le budget d'attempts.

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
                        // classify_error → ErrorClass (7 variantes, cf. §3.4).
                        match deps.provider.classify_error(&e) {
                            // Transitoires : RETRY TRANSVERSE (backoff/jitter), PAS de withholding.
                            ErrorClass::Retryable
                            | ErrorClass::Overloaded
                            | ErrorClass::RateLimited => {
                                yield retry_scheduled(ids, ordinal, delay, fingerprints);
                                cancel.guard(clock.sleep(delay)).await;
                                break; // on reboucle avec le même contexte
                            }
                            ErrorClass::ReasoningReplayRejected => {
                                disable_replay_once();
                                break;
                            }
                            ErrorClass::ContextLimit => {
                                compact_then_reopen_within_budget();
                                break;
                            }
                            ErrorClass::Auth(AuthError::Expired) => {
                                refresh_once_under_cancel().await;
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

            // Fallback usage : si le stream n'a pas émis d'Usage, compter en local.
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

### 3.6 Garde-fous déterministes (ADR-14)

Deux garde-fous vivent dans `crates/agent-core/src/guardrail.rs`. Ils **surchargent** la logique faillible du modèle, depuis l'extérieur de la boucle, et ils sont dans `agent-core` parce que le graphe interdit `core -> tools` et qu'arrêter la boucle est une décision de terminaison du cœur. Purs, sans horloge, sans aléa, sans I/O : la décision est une fonction de la suite des observations.

- **`UsageBudget`** : budget cumulé en jetons et en coût, avec kill-switch à 100 % et estimation pré-tour qui arrête *avant* un tour trop cher.
- **`LoopGuard`** : détecte le batch d'outils identique répété et **refuse de l'exécuter**. C'est un veto, pas un rappel consultatif.

**Signature.** La clé d'un batch est `nom\0json` par appel, triée puis jointe. Le `Display` de `serde_json::Value` produit du JSON compact aux clés triées, donc ni l'ordre des clés d'un argument ni l'ordre des appels dans le batch ne changent la signature. Les appels que le dispatcher déclare exempts (`ToolDispatch::loop_guard_exempt`) en sont retirés : le cœur ne voit qu'un nom et une valeur JSON, ce qui ne suffit pas à distinguer une cellule d'orchestration d'un outil homonyme, donc il demande au lieu de deviner. **La clé n'est jamais tronquée**, quelle que soit la taille des arguments.

**Échelle.** `LOOP_GUARD_THRESHOLDS = [3, 5, 8]` est une constante de crate validée à la compilation (invariant 15 : aucune clé de configuration pour une limite d'orchestration). En dessous de 3, le batch s'exécute. À partir de 3, **il ne s'exécute plus à aucun cran** ; ce qui escalade est le registre de la réponse rendue au modèle :

| Compte consécutif | Décision | Ce que le modèle reçoit |
|---|---|---|
| 1 à 2 | `Proceed` | le batch s'exécute normalement |
| 3 à 4 | `Signal(Gentle)` | rappel générique, court, **citant aucun argument** |
| 5 à 7 | `Signal(Detailed)` | rappel nommant l'outil, la longueur de la série et les arguments canoniques, bornés à `LOOP_GUARD_ARGS_PREVIEW_BYTES` (500 octets) sur une frontière de caractère, avec mention de ce qui a été retiré |
| 8 | `Abort` | arrêt déterministe |

Le garde parle sur des **plages** et non sur des comptes exacts : il doit rendre un `tool_result` par `tool_use` à chaque batch pour que le transcript reste valide, donc il ne peut pas se taire entre deux crans.

**Deux sites d'appel, une seule échelle**, portée par le type et non par les sites, deux échelles divergentes étant une seconde source de vérité.

- **Site externe**, `crates/agent-core/src/agent.rs` : `observe` est appelé **en amont** de la dispatch, ce qui fait qu'un appel refusé par permission ou portant un nom inconnu compte comme un appel ordinaire. Sur `Signal`, un `tool_result` est émis par `tool_use` avec `ToolErrorKind::Semantic` et la boucle repart. Sur `Abort`, un unique `AgentEvent::Exhausted(ExhaustReason::ToolLoop { count })` termine le tour (invariant 11), portant le compte réel.
- **Site imbriqué**, `crates/agent-code-mode/src/nested.rs` : même échelle par effet gardé, plus un **verrou**. Au cran terminal, `NestedLoopGuard` retient le message terminal et le rend à tout appel ultérieur du tour, quel que soit l'outil, sans atteindre le dispatcher. Une cellule est du JavaScript : elle peut attraper l'erreur et retenter, ou alterner les outils, là où le modèle externe ne peut pas ignorer un `Exhausted`.

**Frontières de la chaîne.** Deux déclencheurs de remise à zéro, et deux seulement : un tour neuf, et une entrée de steering qui entre effectivement dans le transcript au point sûr de `run_agent` (US-007), propagée au site imbriqué par `ToolDispatch::steering_input_accepted`, méthode à défaut no-op. Une répétition de part et d'autre d'une intervention humaine est un utilisateur qui redemande l'appel, pas un modèle qui tourne en rond. À l'inverse, un batch entièrement exempt est **transparent** : il ne compte pas et ne remet pas à zéro, faute de quoi un `wait` intercalé blanchirait la boucle dont il fait partie.

Le raisonnement complet, les alternatives écartées (détection sensible au résultat, fenêtre glissante, clé de configuration publique) et les risques assumés sont dans [`docs/DECISIONS.md`](./DECISIONS.md), ADR-14.

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

Ce que le `Registry` applique (`tool_search.rs`) :

- **Le déferrable est déclaré par l'outil** (`DynTool::is_deferrable`), pas déduit d'un nom. En pratique ce sont les outils MCP : la surface native est courte, stable et utile à presque chaque tour, alors que trois serveurs MCP pèsent plus que le transcript en schémas.
- **Le deferral est LOCAL**, pas délégué au backend. Le flag `defer_loading` du wire continue de voyager quand une spec le porte, mais la décision de masquer est prise ici, ce qui la rend valable sur tout provider.
- **`tool_search` est enregistré en permanence et exposé seulement quand quelque chose est réellement masqué.** Un outil masqué reste **dispatchable** : masquer une spec est une décision de coût de prompt, jamais de permission.
- L'ordre est **deferral puis regroupement en namespace** : un outil déjà plié dans un namespace échapperait au filtre.

### 4.6 Taint untrusted (OWASP LLM01 — prompt injection)

Tout output d'outil (`Bash`, `Read`, MCP, etc.) est **untrusted par défaut** (`returns_untrusted() == true`). Le taint se **propage** dans le contexte. Règle de défense : si un tour contient du taint récent et que le modèle demande une **action destructive ou réseau**, on **force `Ask`** quel que soit le mode de permission courant (hors `BypassPermissions`). C'est la mitigation directe de l'injection de prompt via contenu lu.

---

### 4.7 Déversement de sortie d'outil (ADR-15)

Borner une sortie d'outil ne veut plus dire la détruire. Au-delà de `MAX_TOOL_OUTPUT_BYTES` (30 000 octets, le plafond que toute sortie d'outil partage déjà), la sortie **complète est écrite sur disque avant toute réduction**, et le modèle reçoit un aperçu tête et queue suivi d'une notice portant l'adresse du fichier.

Trois responsabilités, trois endroits, et aucun ne fait le travail d'un autre :

| Module | Ce qu'il fait | Ce qu'il ne fait pas |
|---|---|---|
| `crates/agent-tools/src/spill.rs` | persiste un texte, rend un localisateur, borne la racine | ne décide jamais qu'un déversement a lieu |
| `crates/agent-tools/src/spill_policy.rs` | construit le remplacement borné et la notice | n'écrit rien, donc « meilleur effort » y est prouvable |
| `run_one_inner` (`registry.rs`) | décide **quand** | ne connaît ni la disposition du stockage ni la forme de l'aperçu |

Le point de décision est `run_one_inner` parce que c'est le seul endroit où le texte complet existe encore et où l'identifiant d'appel est connu. Le hook `PostToolUse` qui suit observe sans réécrire (US-019), et `bound_feedback` vit dans `agent-core`, qui n'a aucune I/O.

**La racine est `<workspace>/.pyxis/spill/<12 hex du hachage de l'identifiant de run>`**, créée par le binaire **avant** l'application du sandbox et déclarée dans les répertoires d'état inscriptibles, exactement comme le répertoire de sessions. Répertoire en `0700`, fichiers en `0600`, ouverture `create_new` qui échoue sur tout chemin préexistant, lien symbolique compris. Le nom d'outil, qu'un serveur MCP choisit librement, traverse un encodage injectif avant tout usage du système de fichiers. Elle est sous `.pyxis` et non sous le tmp du système parce que `confine` refuse à `read` et à `grep` tout chemin hors du workspace : un artefact ailleurs serait une adresse que le modèle ne peut pas ouvrir.

**Meilleur effort strict.** Absence de stockage, échec d'écriture, notice qui ne tient pas elle-même sous le plafond : chacun journalise à l'échelon avertissement et rend le résultat **original**, octet pour octet. `is_error` n'est jamais levé pour un échec de déversement. `read` n'est jamais déversé, faute de quoi la boucle « lire, déverser, relire » se refermerait, et un résultat non textuel reste intact.

**Le localisateur voyage dans le champ qui existait déjà**, `ToolResultTruncation.continuation_hint`, en chemin **relatif** au workspace : aucun chemin absolu n'entre dans le fil JSONL ni chez l'app-server. `bash` déverse au fil de l'acquisition, un fichier par flux, parce qu'il ne détient jamais sa propre sortie complète. La relecture est celle qui existe déjà : `read` pagine par `offset` et `limit` jusqu'au dernier octet, `grep` nomme les fichiers qu'il a sautés, et les parcours récursifs de `grep` et de `glob` n'entrent pas dans `.pyxis` alors que `confine` continue d'accepter un chemin qui y est explicitement visé.

**La racine est bornée** par `MAX_SPILL_ROOT_BYTES` (256 Mio), constante de crate et non clé de configuration (invariant 15). Au démarrage d'un thread, les répertoires de threads les plus anciens sont évincés jusqu'à repasser sous le plafond ; le répertoire du thread courant n'est jamais candidat, l'éviction porte sur un thread entier et journalise ce qu'elle supprime.

**Le déversement ne blanchit rien** : la relecture d'un artefact reste `untrusted`, et §4.6 s'y applique comme à toute lecture.

Le raisonnement complet, les alternatives écartées (racine sous le tmp du système, outil de relecture dédié, couture de stockage abstraite, clé de configuration du seuil) et les risques assumés sont dans [`docs/DECISIONS.md`](./DECISIONS.md), ADR-15. Le contrat de fil est décrit dans [`docs/EVENT_SCHEMA.md`](./EVENT_SCHEMA.md).

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

Pyxis consomme MCP via le SDK Rust officiel `rmcp` (wrappé dans `agent-mcp`). État livré courant : config, lifecycle stdio, listing d'outils et **appel des outils par le modèle**.

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

- **Description cappée à 2048 caractères** et **schéma d'entrée plafonné** (un serveur ne peut polluer le prompt ni par sa description ni par son schéma).
- **OAuth PKCE par serveur** (creds via `agent-auth`) : cible Phase 2, pas livré dans le MVP courant.
- Outils MCP enregistrés comme `DynTool` (uniformité §4.1) : livré. Le nom exposé est `mcp__{serveur}__{outil}`, assaini sur `^[A-Za-z0-9_-]+$`, raccourci de façon déterministe (empreinte FNV-1a) sous les 64 octets de l'API modèle, et l'unicité est vérifiée à l'enregistrement sur l'ensemble des serveurs. Le nom d'origine reste porté par l'outil : un nom raccourci atteint le bon serveur.
- Le schéma d'entrée servi par un serveur est **réécrit en mode strict** (`additionalProperties: false`, toute propriété dans `required`, propriété optionnelle rendue nullable puis null retiré avant l'appel) parce que le provider émet `strict: true` et qu'un `ToolSpec` invalide ferait échouer le tour entier. Un outil dont le schéma résiste est écarté avec un diagnostic, jamais laissé casser la session.
- **Tous** les outils MCP ont `returns_untrusted() == true`, `is_sensitive() == true` et un baseline de permission `Ask` ; le taint (§4.6) s'applique intégralement à leurs sorties. Les `annotations` d'un serveur ne sont jamais lues comme une décision de sécurité.
- Un serveur déclaré par l'espace de travail (ou masquant une entrée utilisateur, ou portant une variable d'environnement sensible) n'est **pas** connecté au démarrage : il reste derrière `/mcp <serveur> trust`. Ouvrir un dépôt ne suffit jamais à obtenir un spawn de processus.
- Séparation d'échec imposée par le protocole : `Ok(CallToolResult { is_error: true })` devient un résultat d'outil en erreur destiné au modèle ; `Err(ServiceError)` devient une erreur de pipeline nommant le serveur.

---

## 7. Sessions — JSONL + resume

Persistance **JSONL append-only**. Chaque ligne est une `entry` discriminée :

```rust
enum SessionEntry {
    Meta { schema_version: u32 },
    Message(Message),                 // tour user/assistant/tool
    CompactBoundary(CompactKind),     // marque une frontière de compaction
    CompactCheckpoint { kind, messages },
    EncryptedReasoningRedacted,
    FileHistorySnapshot(FileSnapshot),// état d'un fichier pour rollback/diff
}
```

- **Append durable par entrée** : chaque entry réussie est écrite puis `flush` + `sync_data`. Un crash peut laisser une queue partielle ; au resume elle est ignorée, et avant tout nouvel append elle est tronquée au dernier offset valide.
- **Resume** = on **rejoue le log** et on **reconstruit l'état** (messages, frontières de compaction, snapshots fichiers). Couplé au transcript-before-response (§3.2), une session interrompue en plein stream se rouvre proprement. `schema_version` protège les futurs formats incompatibles.
- **Deux lignes de plus depuis le runtime d'orchestration durable**, additives : `thread_meta` lie le fichier à un `ThreadId` (et, pour une branche, à sa provenance), `thread_event` porte les événements d'orchestration (entrée soumise, transition de tour, fork, filiation d'agent). Une session v1 reste lisible et poursuivable : son préfixe n'est jamais réécrit, son `ThreadId` est dérivé une fois puis matérialisé au premier append. Un fork **copie** le préfixe durable jusqu'à la frontière du tour visé dans un fichier indépendant, ce qui fait qu'une branche survit à la suppression de sa source.

---

## 7bis. Runtime de thread — `agent-runtime` (ADR-12)

`run_agent` reste le moteur d'UN tour. Ce qu'il ne possède délibérément pas —
l'identité durable, l'ordre des commandes, le cycle de vie, l'annulation, les
branches — appartient à un **actor local par conversation**.

```
client (TUI ou headless)
      │ submit / steer / interrupt / fork / shutdown   (mailbox bornée, 64)
      ▼
 ThreadHandle ──▶ ThreadActor ──TurnRunner──▶ run_agent  (le SEUL moteur)
      │                │
      │                ├── ThreadStore   (JSONL local | mémoire)
      │                ├── TurnContext   (figé au démarrage du tour)
      │                ├── StepContext   (reconstruit avant CHAQUE requête)
      │                └── AgentSupervisor (enfants parent-owned)
      ├── watch<ThreadStatus>            (dernier état, jamais un backlog)
      └── broadcast<RuntimeEvent>        (flux live, borné à 256)
```

Quatre règles portent tout le reste :

- **Un seul propriétaire.** L'ordre d'acceptation est décidé dans l'actor, pas
  dans le client. C'est ce qui rend la course steer/terminal arbitrable : une
  entrée appartient soit au tour qu'elle visait, soit à un tour neuf après son
  terminal. Jamais les deux, jamais aucun.
- **Durable avant acquittement.** Une opération acceptée est dans le journal
  avant que le client n'apprenne qu'elle est acceptée. Un `client_message_id`
  déjà accepté rend les identifiants d'origine, sans réexécuter quoi que ce soit.
- **Un arbre d'annulation.** `tokio_util::sync::CancellationToken` : le runtime,
  le thread, le tour, l'outil et chaque enfant sont des nœuds enfants du
  précédent. Annuler descend, jamais ne remonte. `TaskTracker` compte les tâches
  dynamiques, donc un shutdown ferme l'admission, annule, attend, **puis**
  aborte les récalcitrants.
- **Zéro clé de configuration.** Toutes les limites v1 sont des constantes du
  crate (mailbox 64, flux live 256, entrées en attente 16, 4 enfants actifs,
  8 créés, profondeur 1). `/status` les affiche ; `settings.toml` ne les connaît
  pas.

Le store et la session sont **le même fichier** : l'adapter JSONL implémente
`ThreadStore` et `Session`. Deux writers sur un fichier de session se
disputeraient son verrou et s'entrelaceraient sans curseur commun.

## 8. Sous-agents

Un sous-agent **est un thread** : son propre `ThreadHandle`, son propre journal
durable, son propre cycle de vie. Ce que le superviseur ajoute est le côté parent
de la relation : la comptabilité du graphe, le nœud d'annulation dont les enfants
pendent, le handoff borné que produit chaque terminal, et les refus qui empêchent
un parent d'atteindre l'enfant d'un autre.

```
        thread parent (ThreadHandle)
              │ AgentSupervisor : lease atomique, puis création
   ┌──────────┼──────────┐
   ▼          ▼          ▼
 enfant     enfant     enfant       ← journal, mailbox et tours propres
   │          │          │
   └──────────┴──────────┘
              ▼
    handoff borné, marqué untrusted, injecté une seule fois
```

Bornes v1, constantes : 4 enfants actifs, 8 créés par thread racine, profondeur 1.
L'autorité d'un enfant est l'**intersection** de celle du parent et de la demande
de spawn, jamais plus large ; par défaut, lecture seule. Un enfant mutateur n'est
pas livré (`docs/DECISIONS.md`, no-go mesuré).

État livré : le runtime, les bornes, l'autorité, le handoff et les cinq outils
(`spawn_agent`, `list_agents`, `wait_agent`, `send_agent`, `interrupt_agent`) sont
écrits et testés. Le binaire, lui, ne les enregistre pas encore et démarre son
thread sans superviseur : exposer un enfant demande un spawner côté client
capable de construire un registre d'outils restreint à son autorité, ce qui reste
à livrer (voir `docs/CURRENT_STATUS.md`, section *Deferred*).

---

## 9. Frontend `agent-tui` — Ratatui + crossterm

`pyxis` s'ouvre **directement dans le shell**, ce n'est pas une fenêtre. Stack : **Ratatui + crossterm**.

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

## 10. Protocole d'événements cœur → frontend

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

Deux consommateurs partagent ce **même** flux d'`AgentEvent` :

1. `agent-tui` (Ratatui) — rendu terminal monochrome.
2. Mode `-p` headless — sérialisation JSON / texte.

Le découplage du §1 garantit qu'un client supplémentaire (GUI, serveur, intégration IDE) pourrait consommer ce même flux sans modification du cœur : le protocole s'étend par ajout de variantes, sans jamais casser le mode terminal par défaut.

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
7. La compaction se cassera sur tout provider sans `Usage` fiable si le fallback `agent-tokenizer` n'est pas branché : `update_budget` lit le `Usage` du stream **sinon** compte en local.
8. **Withholding ≠ retry.** Seules les erreurs de contexte (PTL / max-tokens / `413`) alimentent `PendingError` et la compaction réactive ; les transitoires (`Retryable` / `Overloaded` / `RateLimited`) sont absorbées par le backoff transverse et n'entrent jamais dans `PendingError`.
9. Le type d'erreur classifiée est **`ErrorClass`** (7 variantes), nommé identiquement dans tout le code et toute la doc.
10. `run_agent` est le **seul** moteur modèle-outils. `agent-runtime` l'atteint par `TurnRunner` et ne réimplémente jamais retry, compaction ni dispatch. Le jour où le seam a besoin d'une boucle à lui, l'architecture est fausse (ADR-12, §7bis).
11. Un tour produit **exactement un** état terminal, persisté avant d'être publié, et une seconde transition terminale est refusée par une erreur typée.
12. Une opération acceptée est **durable avant son acquittement**, et une resoumission portant un `client_message_id` déjà accepté rend les identifiants d'origine sans rien réexécuter.
13. Un seul arbre d'annulation : chaque thread, tour, outil et enfant est un nœud ENFANT du précédent. Annuler descend, jamais ne remonte, et un `JoinHandle::abort` côté client est interdit — il couperait le futur entre un `tool_use` et son résultat.
14. Les deux clients passent par la **même** interface de runtime. Aucune sémantique de tour ne vit dans un client.
15. Aucune clé de configuration publique pour l'orchestration : chaque limite v1 est une constante du crate.
