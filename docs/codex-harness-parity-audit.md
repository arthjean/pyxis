# Audit de parité harness : Pyxis vs Codex CLI

Audit en lecture seule, réalisé le 2026-07-24. Référence amont : Codex CLI (~363k lignes Rust, ~100 crates). Cible : Pyxis (~39k lignes, 10 crates).

**Méthode.** Douze dimensions du harness cartographiées en parallèle, puis chaque écart allégué soumis à un vérificateur adversarial chargé de le réfuter en cherchant la capacité dans Pyxis sous un autre nom, une autre crate ou une autre forme. 221 écarts retenus, 7 réfutés et retirés. Toute affirmation porte une preuve `chemin:ligne`. Les écarts marqués `non-applicable` relèvent de l'infrastructure interne OpenAI, du support Windows/macOS ou du multi-provider déjà assumé comme différé (ADR-11).

**Périmètre de « harness ».** Tout ce qui entoure le modèle et détermine ce que l'agent peut faire, voir et décider : boucle de tour, protocole d'événements, prompt système, outils exposés, politique d'approbation, sandbox, configuration, contexte projet injecté, sessions et reprise, compaction, extensibilité (MCP, skills, hooks, commandes), modes d'exécution, observabilité.

## Verdict

La parité fonctionnelle brute n'est pas le principal enseignement. Trois constats d'une autre nature dominent l'audit.

**Un bug de corruption de session.** Une interruption pendant l'exécution d'outils laisse un `function_call` persisté sans son `function_call_output`. Le tour suivant le réémet et le backend Responses rejette la requête : la session est inutilisable jusqu'à un `/new`. Chaîne vérifiée : `crates/agent-core/src/agent.rs:746` (persistance avant dispatch) → `crates/agent-cli/src/session.rs:36` (capture du snapshot) → `crates/agent-cli/src/interactive.rs:1076` (`abort()` brutal) → `crates/agent-cli/src/interactive.rs:265` (relecture du snapshot) → `crates/agent-provider/src/chatgpt_request.rs:186-200` (réémission). `sanitize_messages` (`crates/agent-cli/src/session.rs:73`) ne retire que le marqueur `/goal`, aucune réconciliation n'existe. Codex traite ce cas en injectant un marqueur d'historique visible du modèle avant d'émettre `TurnAborted` (`codex-rs/core/src/tasks/mod.rs:899-916`).

**Un composer inutilisable au-delà d'une ligne.** `AppState.input` est un `String` plat (`crates/agent-tui/src/state.rs:558`), `Enter` soumet systématiquement (`state.rs:1717`), aucun binding n'insère de saut de ligne, et `render_input` dessine sur une `Rect` de hauteur 1 sans wrap ni défilement horizontal (`crates/agent-tui/src/render.rs:1394-1413`). Au-delà de la largeur du terminal, la saisie devient aveugle. Un collage multi-lignes est inséré brut dans ce champ (`crates/agent-tui/src/bottom_pane.rs:110`).

**Un système de suivi qui déclare fait ce qui ne l'est pas.** `tasks/prd-codex-tui-parity-status.json` marque US-017 « Port du composer Codex » et US-018 « parité snapshot » comme `DONE`. Or `crates/agent-tui/src/bottom_pane/` ne contient qu'un `tests.rs`, `insta` n'est pas une dépendance du workspace et le repo compte zéro snapshot, alors que le critère d'acceptation exige « au moins 20 snapshots » (`tasks/prd-codex-tui-parity.md:388`). Il n'existe par ailleurs aucun répertoire `.github/`, donc aucune CI n'exécute les tests. Tant que ce décalage subsiste, les fichiers de statut ne constituent pas un signal de vérification exploitable.

## Ce que Pyxis fait mieux que Codex

Les garde-fous déterministes sont plus explicites : `ExhaustReason` typé (`crates/agent-core/src/transition.rs:35`), loop-guard signal-puis-abort (`crates/agent-core/src/guardrail.rs:27`), taint untrusted propagé au sens OWASP LLM01 (`crates/agent-tools/src/taint.rs`), et une machine à transitions pure validée par un `Accumulator` qui fait échouer fail-closed tout provider hors contrat (`transition.rs:167-220`). Codex n'a pas d'équivalent aussi net.

## Parité par dimension

| Dimension | Parité | Écarts pertinents | Discutables | Non applicables |
|---|---|---|---|---|
| Boucle agentique et protocole d'événements | partial | 12 | 7 | 1 |
| Suite d'outils exposée au modèle | partial | 7 | 11 | 3 |
| Sandbox et approbations | partial | 14 | 3 | 0 |
| Configuration, profils, précédence | minimal | 14 | 6 | 3 |
| Contexte projet injecté et prompt système | partial | 6 | 8 | 0 |
| Extensibilité et commandes (skills, hooks, plugins, slash commands) | minimal | 8 | 6 | 2 |
| MCP | minimal | 14 | 2 | 2 |
| Persistance et gestion du contexte | partial | 5 | 7 | 3 |
| Interface terminal | partial | 18 | 11 | 3 |
| Modes non interactifs et intégration (exec, headless, app-server, CI) | minimal | 6 | 5 | 1 |
| Modèles, providers, authentification | partial | 4 | 11 | 2 |
| Observabilité et assurance qualité du harness | minimal | 9 | 5 | 2 |
| Angles morts (critique de complétude) | partial | 8 | 2 | 0 |

Échelle de parité : `none` < `minimal` < `partial` < `substantial` < `full`.

## Écarts bloquants

### [Boucle agentique et protocole d'événements] Une interruption pendant l'execution d'outils laisse un function_call orphelin persiste et reinjecte au tour suivant

`absent` · effort `M`

**Impact.** Apres un Echap pendant un `bash` ou un `edit`, le prochain prompt de la meme session envoie un `function_call` sans `function_call_output` : le backend Responses rejette la requete (400 « No tool output found for function call ») et la session devient inutilisable jusqu'a un /new. Le modele n'a par ailleurs aucune trace du fait que la commande a pu s'executer partiellement.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tasks/mod.rs:899-916 injecte un marqueur model-visible (`interrupted_turn_history_marker`, texte dans core/src/context/turn_aborted.rs:9-10) dans l'historique et flushe le rollout AVANT d'emettre TurnAborted, precisement pour que le modele sache que des outils ont pu s'executer partiellement; core/src/session/rollout_reconstruction.rs reconstruit ensuite un transcript coherent.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:746 persiste le message assistant (avec ses tool_use) AVANT le dispatch; /home/arthur/dev/pyxis/crates/agent-cli/src/session.rs:96 met a jour le snapshot memoire relu par le tour suivant (interactive.rs:265); /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1074 abort le JoinHandle sans reconciliation; /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:185-200 reemet le function_call sans filtrer les appels sans function_call_output. Greps infructueux sur `orphan|dangling|repair|reconcile` dans agent-core et sur `ToolUse` dans agent-core/src/agent.rs (aucune logique d'appariement).

### [MCP] Les outils MCP ne sont jamais exposés au modèle: MCP est purement diagnostic

`absent` · effort `XL`

**Impact.** Un utilisateur qui configure un serveur MCP dans Pyxis voit la liste de ses outils dans `/mcp <srv> tools` mais le modèle ne peut en invoquer aucun. MCP est donc, en pratique, non fonctionnel: c'est un inspecteur de serveur, pas une intégration. C'est l'écart structurant de toute cette dimension: presque tous les autres écarts en découlent.

**Codex.** codex-rs/core/src/tools/handlers/mcp.rs:32-120 (McpHandler implémente le trait outil, spec/handle/parallélisme/hooks) et codex-rs/core/src/mcp_tool_call.rs:110 (handle_mcp_tool_call): chaque outil MCP devient un outil appelable du routeur, avec appel réel via codex-rs/codex-mcp/src/connection_manager.rs:612 (client.call_tool)

**Pyxis.** Aucun `call_tool` dans tout le dépôt Pyxis (grep -rn "call_tool\|CallTool" --include=*.rs → 0 résultat). crates/agent-mcp/src/client.rs:69-155 n'expose que connect/connect_hardened/list_tools/cancel. crates/agent-tools/src/registry.rs:432-433 a un `register_dyn` commenté « futur outil MCP » jamais appelé. docs/CURRENT_STATUS.md:12 le confirme explicitement.

**Statut documentaire.** Déjà connu et planifié: docs/CURRENT_STATUS.md:19 et docs/ROADMAP.md:86,113 le mettent en Phase 2; tasks/prd-pyxis.md:441 le classe risque n°6 « MCP absent au MVP (table-stake 2026) », bloquant pour la promo publique mais pas pour le dogfood. tasks/prd-codex-orchestration.md:345 l'exclut explicitement de son scope.

### [Interface terminal] Composer mono-ligne : aucune insertion de saut de ligne

`absent` · effort `L`

**Impact.** Impossible de rediger un prompt multi-paragraphes, de coller un extrait de code lisible ou de structurer une instruction longue : chaque retour a la ligne envoie le message. C'est le blocage d'usage le plus immediat du composer.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:963-969 (insert_newline = Ctrl+J, Ctrl+M, Enter, Shift+Enter, Alt+Enter) adosse a /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/textarea.rs (3919 l., buffer multiligne avec wrap et hauteur desiree)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1717-1732 : `KeyCode::Enter` soumet toujours, aucune branche d'insertion de '\n' ; /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1395-1400 : la zone de saisie fait exactement 1 ligne. Greps infructueux : `insert_newline`, `Shift.*Enter`, `alt\(KeyCode::Enter\)`, `'\\n'` dans state.rs

**Statut documentaire.** tasks/prd-codex-tui-parity.md:365-377 (US-017 « Port du composer Codex ») est marque DONE dans tasks/prd-codex-tui-parity-status.json alors que le composer Codex n'a pas ete porte ; aucune divergence n'est documentee dans docs/CURRENT_STATUS.md comme l'exigeait le dernier critere d'acceptation.

### [Interface terminal] Saisie longue tronquee : ni wrap ni defilement horizontal

`absent` · effort `M`

**Impact.** Au-dela de la largeur du terminal l'utilisateur tape a l'aveugle : le texte disparait et le curseur ne bouge plus, ce qui rend l'edition d'un prompt de plus d'une ligne d'ecran pratiquement impossible.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs (rendu via TextArea qui calcule desired_height et wrappe) et /home/arthur/dev/codex/codex-rs/tui/src/wrapping.rs (adaptive_wrap_lines)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1402-1413 : les spans sont rendus dans une `Line` unique sans Wrap ; render.rs:1416-1422 : la colonne du curseur est clampee a `inner.right()-1`, donc le curseur se fige au bord des que la saisie depasse la largeur

---

## Détail par dimension

### Boucle agentique et protocole d'événements

**Parité estimée : partial**

*Surface Codex.* Codex separe explicitement une **submission queue** et une **event queue** : `Submission { id, op, client_user_message_id, trace }` (`codex-rs/protocol/src/protocol.rs:176`) porte un `Op` de ~25 variantes (`protocol.rs:522` : `Interrupt`, `UserInput`, `Compact`, `Review`, `ThreadRollback`, `ExecApproval`, `RunUserShellCommand`, `ThreadSettings`, `RefreshMcpServers`, `Shutdown`, `CleanBackgroundTerminals`, …) et chaque `Event { id, msg }` (`protocol.rs:1261`) est correle a l'id de sa submission. `EventMsg` compte ~80 variantes (`protocol.rs:1279-1478`) : cycle de vie (`TurnStarted`/`TurnComplete`/`TurnAborted`/`SessionConfigured`), deltas (`AgentMessageContentDelta`, `ReasoningContentDelta`, `ReasoningRawContentDelta`, `PlanDelta`), items v2 (`ItemStarted`/`ItemCompleted`, `RawResponseItem`/`RawResponseCompleted`), outils (`ExecCommandBegin/OutputDelta/End`, `McpToolCallBegin/End`, `WebSearchBegin/End`, `ImageGenerationBegin/End`, `PatchApply*`, `DynamicToolCall*`), et telemetrie (`TokenCount`, `StreamError`, `Warning`, `ModelReroute`, `SafetyBuffering`). L'execution passe par des **tasks typees** (`core/src/tasks/mod.rs:217` trait `SessionTask` + `TaskKind`) : `RegularTask` (`tasks/regular.rs:29`), `CompactTask` (`tasks/compact.rs:18`), `ReviewTask` (`tasks/review.rs:43`, sous-thread Codex complet via `codex_delegate::run_codex_thread_one_shot`), `UserShellTask`. L'interruption est cooperative : `abort_all_tasks(TurnAbortReason::{Interrupted,Replaced,ReviewEnded,BudgetLimited})` (`tasks/mod.rs:509`, `protocol.rs:4206`) annule le `CancellationToken`, attend `GRACEFULL_INTERRUPTION_TIMEOUT_MS`, puis `handle.abort()` + `SessionTask::abort` (`tasks/mod.rs:867-897`), et **injecte un marqueur model-visible dans l'historique** (`tasks/mod.rs:899-916`, texte dans `core/src/context/turn_aborted.rs:9-10`) flushe avant d'emettre `TurnAborted`. Le retry SSE est instrumente : `handle_retryable_response_stream_error` (`core/src/responses_retry.rs:22`) honore `err.retry_delay()`, bascule WebSocket→HTTPS en fallback de transport, et **notifie l'UI** (`sess.notify_stream_error(... "Reconnecting... n/max")`, `responses_retry.rs:62`) via `EventMsg::StreamError` (`protocol.rs:3649`). Les quotas remontent structurellement : `ResponseEvent::RateLimits(RateLimitSnapshot)` (`codex-api/src/common.rs:122`) → `TokenCountEvent { info, rate_limits }` (`protocol.rs:2138-2160`) avec `RateLimitWindow{used_percent, window_minutes, resets_at}`, `CreditsSnapshot`, `plan_type`, `RateLimitReachedType`; `UsageLimitReached` produit un message plan/reset dedie (`protocol/src/error.rs:640-690`). Le stream expose `ResponseEvent::{Created, OutputItemAdded, OutputItemDone, OutputTextDelta, ToolCallInputDelta, ReasoningSummaryDelta{summary_index}, ReasoningSummaryDone, ReasoningContentDelta, ServerModel, ServerReasoningIncluded, SafetyBuffering, Completed{response_id, token_usage, end_turn}}` (`codex-api/src/common.rs:76-122`) et `ResponseItem` couvre message/reasoning(+`encrypted_content`)/function_call/custom_tool_call/tool_search_call/web_search_call/image_generation_call/local_shell_call/compaction (`protocol/src/models.rs:799-1010`). La fin de tour n'est pas seulement l'absence d'outils : `Completed { end_turn: Some(false) }` force `needs_follow_up` et relance un sampling (`core/src/session/turn.rs:2369`); un `ContextWindowExceeded` declenche `run_auto_compact` inline (`turn.rs:1233`, `turn.rs:1012`); le steering mid-tour draine `input_queue.get_pending_input` dans la boucle du tour courant (`core/src/session/input_queue.rs:204`, consomme `turn.rs:249-256`).

*Surface Pyxis.* Pyxis implemente une boucle unique `run_agent(ctx, deps) -> impl Stream<Item = AgentEvent>` (`crates/agent-core/src/agent.rs:331`) batie sur une **state machine a transitions typees** : deux fonctions pures `pre_stream_transition` (`transition.rs:79`, priorite recover > exhaust > compact) et `post_stream_transition` (`transition.rs:98`), avec un `enum Transition { EndTurn, RunTools, Compact, Recover, Exhausted, Fail }` (`transition.rs:62`) matche exhaustivement par le driver (`agent.rs:726`). L'`Accumulator` (`transition.rs:149`) valide le contrat provider a l'execution : id vide, id duplique, delta sans start, end sans start, JSON d'arguments invalide sont des `ProviderFailure::contract` (`transition.rs:167-220`), et un stream sans event terminal echoue fail-closed (`transition.rs:101`). Le vocabulaire de streaming canonique est `StreamEvent { TextDelta, ReasoningDelta, ToolCallStart/Delta/End, EncryptedReasoning, Usage, Done{StopReason} }` (`provider.rs:35`), mappe depuis le SSE Responses dans `agent-provider/src/chatgpt_events.rs:49-99`. Le contrat client est `AgentEvent` a 11 variantes (`event.rs:13`) : `StreamReset, Text, Reasoning, ToolCall, ToolResult, Compacted, PermissionAsk, EndTurn, Interrupted, Exhausted, Error` - structure, serialisable, sans ANSI, mais **sans aucun identifiant de tour ou de submission**. Le retry est solide et bien teste : classification `ErrorClass{Retryable, RateLimited, Overloaded(u16), Auth(...), InvalidRequest}` (`provider.rs:382`), backoff exponentiel plafonne a 32x avec jitter, `max(backoff, Retry-After)` borne a 60 s (`agent.rs:148-231`), 429 terminaux (`GoUsageLimitError`, `insufficient_quota`) reclassifies en `InvalidRequest` jamais retryes (`agent-provider/src/chatgpt.rs:354-376,733-736`), fallback de modele apres surcharge (`agent.rs:233`), refresh OAuth sur `Auth(Expired)` (`agent.rs:506`), watchdog d'idle SSE 60 s (`chatgpt.rs:282`). Le withholding distingue erreurs de contexte et erreurs transitoires (`provider.rs:371`, invariant 8) : un PTL/413 arme un `PendingError` qui declenche une compaction reactive au lieu d'un retry, et un `MaxTokens` en plein tool_call declenche `Recover` plutot qu'un EndTurn qui perdrait l'appel (`transition.rs:119-126`). Les garde-fous deterministes sont plus explicites que chez Codex : `ExhaustReason::{MaxTurns, TokenBudget, CostBudget, ToolLoop, MaxOutputTokens}` (`transition.rs:35`) plus un loop-guard signal-puis-abort (`guardrail.rs:27`, `agent.rs:754`). La persistance suit transcript-before-response : `session.sync` en tete de boucle (`agent.rs:387`), avant dispatch d'outils (`agent.rs:746`) et avant `EndTurn` (`agent.rs:733`), avec `SharedSession` qui maintient le snapshot memoire relu par le tour suivant (`agent-cli/src/session.rs:94-98`). L'interruption vit entierement dans le client : `InputAction::Interrupt` fait `active_turn.abort()` (JoinHandle tokio) puis fabrique lui-meme `AgentEvent::Interrupted` (`agent-cli/src/interactive.rs:1071-1078`); le coeur ne connait ni token d'annulation ni notion de tour annule.

#### Écarts pertinents

##### Une interruption pendant l'execution d'outils laisse un function_call orphelin persiste et reinjecte au tour suivant

`bloquant` · `absent` · effort `M`

**Impact.** Apres un Echap pendant un `bash` ou un `edit`, le prochain prompt de la meme session envoie un `function_call` sans `function_call_output` : le backend Responses rejette la requete (400 « No tool output found for function call ») et la session devient inutilisable jusqu'a un /new. Le modele n'a par ailleurs aucune trace du fait que la commande a pu s'executer partiellement.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tasks/mod.rs:899-916 injecte un marqueur model-visible (`interrupted_turn_history_marker`, texte dans core/src/context/turn_aborted.rs:9-10) dans l'historique et flushe le rollout AVANT d'emettre TurnAborted, precisement pour que le modele sache que des outils ont pu s'executer partiellement; core/src/session/rollout_reconstruction.rs reconstruit ensuite un transcript coherent.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:746 persiste le message assistant (avec ses tool_use) AVANT le dispatch; /home/arthur/dev/pyxis/crates/agent-cli/src/session.rs:96 met a jour le snapshot memoire relu par le tour suivant (interactive.rs:265); /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1074 abort le JoinHandle sans reconciliation; /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:185-200 reemet le function_call sans filtrer les appels sans function_call_output. Greps infructueux sur `orphan|dangling|repair|reconcile` dans agent-core et sur `ToolUse` dans agent-core/src/agent.rs (aucune logique d'appariement).

##### Le coeur n'a aucun mecanisme d'annulation : l'interruption est un abort brutal du JoinHandle cote client

`majeur` · `absent` · effort `M`

**Impact.** L'abort tombe a un point arbitraire du future : pas de fenetre de terminaison propre, pas de flush de session, pas de hook de cleanup pour les outils autres que bash (MCP, ecritures fichier partielles). Une interruption pendant un `write`/`edit` peut laisser un etat disque incoherent sans que rien ne soit trace.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tasks/mod.rs:232 (`SessionTask::run` recoit un `CancellationToken`), tasks/mod.rs:867-897 (`handle_task_abort` : cancel du token, attente gracieuse GRACEFULL_INTERRUPTION_TIMEOUT_MS sur `task.done`, puis `handle.abort()`, puis `SessionTask::abort` pour le cleanup), tasks/mod.rs:245 (`SessionTask::abort` dedie au nettoyage de ressources).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:331 `run_agent` ne prend aucun token d'annulation; grep `CancellationToken|cancel_token|abort_handle` dans crates/ ne retourne aucun hit hors `tokio::select!` (agent.rs:806, interactive.rs:612); /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:109-114 `ActiveTurn::abort` fait `handle.abort()`. Le seul filet est `kill_on_drop(true)` sur le process bash (/home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:109).

##### `AgentEvent::Interrupted` est fabrique par le client, jamais emis par le coeur; aucune raison d'abandon typee

`moyen` · `divergent` · effort `S`

**Impact.** Un second client du coeur (mode headless, embarquement Paneflow) doit reimplementer la semantique d'interruption, y compris l'invariant « quand un abort est-il sur ? ». Les autres causes d'abandon (tour remplace par un nouveau prompt, arret budgetaire) ne sont pas distinguables : `Exhausted` et un abort utilisateur ne portent pas la meme information de fin de tour.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:4206 `TurnAbortReason { Interrupted, Replaced, ReviewEnded, BudgetLimited }` et protocol.rs:1436 `EventMsg::TurnAborted(TurnAbortedEvent)` avec turn_id, started_at, completed_at, duration_ms; emis par le serveur en tasks/mod.rs:935.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/event.rs:31 declare `Interrupted`, mais grep `AgentEvent::Interrupted` dans crates/ ne le trouve qu'en construction cote clients (/home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1077) et en consommation (agent-tui/src/state.rs:950, app_event.rs:533) - jamais un `yield` dans agent-core/src/agent.rs.

##### Aucune remontee des rate limits et quotas d'abonnement a l'UI

`moyen` · `absent` · effort `M`

**Impact.** Sur un abonnement ChatGPT (le seul provider livre, ADR-11), l'utilisateur ne voit jamais son pourcentage de quota consomme ni l'heure de reset. Il decouvre la limite au moment ou la session casse, avec un message brut du corps HTTP au lieu d'un « limite hebdomadaire atteinte, reset a X ».

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:2145-2210 `RateLimitSnapshot { primary/secondary: RateLimitWindow{used_percent, window_minutes, resets_at}, credits, individual_limit, spend_control_reached, plan_type, rate_limit_reached_type }`; /home/arthur/dev/codex/codex-rs/codex-api/src/common.rs:122 `ResponseEvent::RateLimits(...)`; consomme en core/src/session/turn.rs:2325-2330 puis emis via `EventMsg::TokenCount` (core/src/session/mod.rs:3870). Message plan/reset dedie en protocol/src/error.rs:640-690.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:325 ne lit des headers que `Retry-After`; greps `rate_limit|RateLimit|quota|weekly|resets_at|used_percent|x-codex` sur crates/ ne renvoient que la classification d'erreur 429 (chatgpt.rs:354-376, provider.rs:384) - aucune capture de fenetre de quota, aucun event.

**Statut documentaire.** Aucun ADR ni US ne couvre le sujet : prd-codex-orchestration.md traite les 429 sous l'angle retry/terminal (US-023) mais pas la telemetrie de quota.

##### L'usage tokens reel et la fenetre de contexte ne sont jamais exposes aux clients

`moyen` · `absent` · effort `S`

**Impact.** L'utilisateur n'a aucun indicateur fiable de remplissage du contexte ni de cout du tour : il subit la compaction sans la voir venir, et le compteur affiche est une heuristique connue pour sous-estimer 3 a 24x (note US-021 dans tasks/prd-codex-orchestration-status.json).

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:2138 `TokenCountEvent { info: Option<TokenUsageInfo>, rate_limits }` avec `TokenUsageInfo { total_token_usage, last_token_usage, model_context_window }` (protocol.rs:2090-2135) et `percent_of_context_window_remaining` (protocol.rs:2244); emis a chaque `Completed` en core/src/session/mod.rs:3870.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:568 consomme `StreamEvent::Usage` uniquement en interne (`budget.observe_usage`); /home/arthur/dev/pyxis/crates/agent-core/src/event.rs:13-34 n'a aucune variante d'usage; le TUI affiche une estimation locale caracteres/4 (/home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:613-617 `turn_chars`). L'usage backend n'est visible que derriere `PYXIS_DEBUG_USAGE` sur stderr (agent.rs:559-566).

**Statut documentaire.** US-021/US-029 (prd-codex-orchestration) ont ajoute la sonde de calibration PYXIS_DEBUG_USAGE mais explicitement pas d'event client.

##### Les retries reseau sont silencieux : aucun event pendant le backoff

`moyen` · `absent` · effort `S`

**Impact.** Sur un 429 avec `Retry-After` de 60 s (cap honore, agent.rs:156), la TUI reste figee sur le spinner pendant une minute entiere sans expliquer pourquoi. L'utilisateur ne peut pas distinguer un backend lent d'un agent bloque, et n'a aucun signal pour decider d'interrompre.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/responses_retry.rs:59-68 emet `notify_stream_error(... "Reconnecting... {retry_count}/{max_retries}")`, et responses_retry.rs:37-43 un `EventMsg::Warning` sur bascule de transport; type dedie `StreamErrorEvent { message, codex_error_info, additional_details }` en protocol/src/protocol.rs:3649.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:496-510 et 604-645 : sur `Retryable|RateLimited|Overloaded` la boucle dort puis `continue` sans rien yielder; le seul event est `AgentEvent::StreamReset` (agent.rs:601) et uniquement si du texte visible avait deja ete emis. Grep `Warning|Notice|Retrying` dans agent-core : aucune variante d'AgentEvent correspondante.

##### Pas de steering : un message envoye pendant un tour n'est traite qu'apres la fin du tour

`moyen` · `partial` · effort `L`

**Impact.** Corriger l'agent en cours de route (« non, pas ce fichier ») exige d'interrompre le tour, avec la perte de travail et le risque de function_call orphelin decrit plus haut. C'est l'interaction la plus frequente d'un agent de code en usage reel.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/session/input_queue.rs:204 `get_pending_input`, drainee dans la boucle interne du tour en cours (core/src/session/turn.rs:249-256 `let pending_input = if can_drain_pending_input { ... }`), et tasks/regular.rs:88 reboucle sur `run_turn` tant que `has_pending_input`. Le message utilisateur entre donc dans le MEME tour, avant la requete de sampling suivante.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:673-675 met le prompt dans `queued_prompts` et affiche « Message queued. »; il n'est depile qu'apres un evenement terminal (interactive.rs:1088 sur interruption, interactive.rs:1181-1189 apres EndTurn/Error/Exhausted). `AgentContext` (/home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:68) est construit une fois par tour, sans canal d'injection.

##### Reasoning aplati : pas de distinction summary/raw, pas de section break, pas d'index de sommaire

`mineur` · `divergent` · effort `S`

**Impact.** Le rendu ne peut pas titrer les blocs de raisonnement ni les replier par section, et le raw chain-of-thought (quand il est renvoye) est melange au resume destine a l'affichage. La frontiere de section est un artefact textuel non distinguable d'un vrai double saut de ligne du modele.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:1344-1360 `AgentReasoning`, `AgentReasoningRawContent`, `AgentReasoningSectionBreak`, et protocol.rs:1455-1458 `ReasoningContentDelta`/`ReasoningRawContentDelta`; cote stream, /home/arthur/dev/codex/codex-rs/codex-api/src/common.rs:104-119 `ReasoningSummaryDelta{delta, summary_index}`, `ReasoningSummaryDone{item_id, text, summary_index}`, `ReasoningSummaryPartAdded{summary_index}`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_events.rs:62-67 mappe `response.reasoning_summary_text.delta` ET `response.reasoning_text.delta` sur le meme `StreamEvent::ReasoningDelta`, et simule une frontiere de section en injectant la chaine `"\n\n"` sur `reasoning_summary_part.done`; `AgentEvent::Reasoning(String)` (event.rs:20) n'a aucune structure.

##### La politique de fin de tour ignore le signal `end_turn` du backend

`mineur` · `divergent` · effort `S`

**Impact.** Un tour ou le backend indique explicitement que le modele n'a pas fini (par exemple une reponse coupee par un rollover interne) est traite comme une fin propre : la reponse rendue est tronquee et l'utilisateur doit relancer manuellement. Note secondaire : `status == "cancelled"` est mappe sur `StopReason::Refusal` (chatgpt_events.rs:304), donc une annulation serveur remonte comme un refus du modele.

**Codex.** /home/arthur/dev/codex/codex-rs/codex-api/src/common.rs:94-100 `Completed { response_id, token_usage, end_turn: Option<bool> }` avec le commentaire « Did the model affirmatively end its turn? »; /home/arthur/dev/codex/codex-rs/core/src/session/turn.rs:2369 `if let Some(false) = end_turn { needs_follow_up = true; }` relance un sampling meme sans appel d'outil.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_events.rs:302-311 `stop_for` derive le `StopReason` du seul champ `status` de la reponse plus le flag local `saw_tool_call`; aucun grep de `end_turn` dans crates/agent-provider ou agent-core. `Transition::EndTurn` (transition.rs:130) est donc pris des qu'il n'y a pas d'appel d'outil.

##### Un quota d'abonnement epuise remonte comme une erreur brute, sans plan ni date de reset

`mineur` · `partial` · effort `S`

**Impact.** Le point fort (ne pas retryer un quota epuise) est deja acquis, mais l'utilisateur recoit un JSON backend brut au lieu d'une indication actionnable. C'est le cas d'erreur le plus frequent d'un agent adosse a un abonnement.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/error.rs:640-690 construit un message utilisateur specifique par `RateLimitReachedType` et par `PlanType` (Plus, Pro, ...), avec l'horaire de reset; `CodexErrorDetails::UsageLimitReached(UsageLimitReachedError)` (protocol/src/error.rs:126).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:733-736 : un 429 dont le corps matche `GoUsageLimitError|insufficient_quota|quota exceeded` devient `ErrorClass::InvalidRequest`, propage tel quel via `AgentError::Provider(ProviderFailure{status:429, message})` (/home/arthur/dev/pyxis/crates/agent-core/src/error.rs:90-99). Aucun parsing du plan ni du reset.

**Statut documentaire.** US-023 (tasks/prd-codex-orchestration.md:140) couvre la classification terminale des 429, pas la presentation.

##### Le mode headless n'expose pas le flux d'evenements, seulement le texte agrege

`mineur` · `absent` · effort `S`

**Impact.** `AgentEvent` est deja `Serialize` (event.rs:11) et concu comme « LE contrat coeur → clients ... Paneflow » : le mode `-p` jette cette valeur. Aucune orchestration externe (script, Paneflow, CI) ne peut suivre les appels d'outils, les permissions ou les compactions d'un run headless.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:65-70 flag `--json` (alias `--experimental-json`); /home/arthur/dev/codex/codex-rs/exec/src/event_processor_with_jsonl_output.rs serialise le flux; taxonomie dediee `ThreadEvent`/`ThreadItem` en exec/src/exec_events.rs:11-107.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:960-993 `run_headless` n'agrege qu'un `String` et un compteur d'events, en jetant explicitement toutes les autres variantes (`_ => {}` ligne 984); /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:735-739 imprime le texte ou bail. Grep `--json|OutputFormat|jsonl` dans agent-cli/src/main.rs : aucun mode de sortie structuree.

**Statut documentaire.** ADR-3 (docs/DECISIONS.md:69) pose le coeur headless emettant des evenements structures pour les clients, ce qui rend l'absence de sortie JSON contradictoire avec l'intention affichee.

##### Les evenements de debut et fin de tour ne portent aucune metadonnee

`mineur` · `partial` · effort `S`

**Impact.** Chaque client doit reimplementer l'horloge du tour et n'a aucun moyen de connaitre la fenetre de contexte du modele actif ou le diff cumule produit par le tour. Les traces d'un run ne sont pas auto-suffisantes pour un post-mortem.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tasks/regular.rs:51-57 `TurnStartedEvent { turn_id, trace_id, started_at, model_context_window, collaboration_mode_kind }`; `TurnCompleteEvent` et `TurnAbortedEvent { started_at, completed_at, duration_ms }` (protocol/src/protocol.rs:4195-4202); `EventMsg::TurnDiff` agrege le diff du tour (protocol.rs:1434).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/event.rs:30 `EndTurn` est une variante nue, et il n'existe aucune variante de debut de tour; le TUI mesure la duree lui-meme (`turn_elapsed`, /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:612). Grep `TurnDiff|turn_diff` dans agent-core : aucun hit.

#### Écarts discutables

##### Pas de tours imbriques typees : ni review mode, ni compaction manuelle, ni commande shell utilisateur

`moyen` · `absent` · effort `L`

**Impact.** Impossible de forcer une compaction avant une grosse tache, ni de lancer une revue de diff isolee qui ne pollue pas le contexte principal. La compaction ne se declenche qu'au seuil automatique ou en reaction a un PTL.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tasks/mod.rs:217 trait `SessionTask` + `TaskKind`; implementations `RegularTask` (tasks/regular.rs:29), `CompactTask` (tasks/compact.rs:18, declenche par `Op::Compact`), `ReviewTask` (tasks/review.rs:43, qui ouvre un sous-thread Codex complet via `codex_delegate::run_codex_thread_one_shot` et emet `EnteredReviewMode`/`ExitedReviewMode`, protocol.rs:1440-1443), `UserShellTask` (tasks/user_shell.rs, declenche par `Op::RunUserShellCommand`, protocol.rs:670).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:331 expose une seule boucle; la compaction est inline dans le `match transition` (agent.rs:855-900) et n'est jamais declenchable par l'utilisateur : la liste des slash commands (/home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:681-1052) contient /help /models /effort /permissions /goal /resume /new /clear /providers /mcp /skills /quit - pas de /compact, pas de /review, pas de `!cmd`. Greps `review|/compact|CompactKind::Manual` dans agent-core : aucun hit.

**Statut documentaire.** docs/CURRENT_STATUS.md:24 defere explicitement les sous-agents; la revue de hunks est listee comme differee ligne 23. La compaction manuelle n'est mentionnee nulle part.

##### Pas de protocole de soumission ni de correlation submission/evenement dans le contrat coeur

`mineur` · `absent` · effort `L`

**Impact.** Tout second consommateur du coeur (Paneflow in-process, futur mode serveur, tests d'integration) doit dupliquer la logique de correlation et de commandes. Une reponse tardive d'un tour annule ne peut etre rejetee que par une convention exterieure au contrat.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:176 `Submission { id, op, client_user_message_id, trace }`, protocol.rs:522 `enum Op` (~25 variantes de controle), protocol.rs:1261 `Event { id, msg }` correle a l'id de submission; boucle de dispatch en core/src/session/handlers.rs:704.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/event.rs:13 `AgentEvent` ne porte aucun identifiant; la correlation est reconstruite ad hoc par le client via un wrapper `AgentTurnEvent { turn_id, event }` (/home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1110 `active_turn.is_current(turn_event.turn_id)`). Aucune notion d'`Op` : les commandes de controle sont des branches de `match cmd` dans la boucle TUI (interactive.rs:681-1052).

**Statut documentaire.** docs/codex-port-inventory.md:65 classe explicitement les surfaces app-server en `skip` (non-goal PRD). ADR-3 pose le coeur headless + clients, sans exiger de protocole de submission.

##### Taxonomie d'items de reponse reduite : web search, custom tool freeform, image generation, local shell sont silencieusement ignores

`mineur` · `divergent` · effort `M`

**Impact.** Si le backend Codex emet un `web_search_call` ou un `custom_tool_call` (freeform), Pyxis ne l'affiche pas et ne le renvoie pas dans le transcript : l'item disparait du contexte au tour suivant, ce qui peut casser l'appariement cote serveur ou faire boucler le modele sur une recherche invisible.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/models.rs:799-1010 `ResponseItem` couvre Message, Reasoning, FunctionCall, CustomToolCall/Output (freeform), ToolSearchCall/Output, WebSearchCall, ImageGenerationCall, LocalShellCall, Compaction, ContextCompaction; events correspondants en protocol/src/protocol.rs:1375-1381 (`WebSearchBegin/End`, `ImageGenerationBegin/End`, `ViewImageToolCall`) et items v2 en protocol/src/items.rs:44-74.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_events.rs:106 `if item.get("type") != Some("function_call") { return Vec::new(); }` - tout item non `function_call` est jete a l'ouverture; /home/arthur/dev/pyxis/crates/agent-core/src/message.rs:44 `ContentBlock` se limite a Text, Thinking, ToolUse, ToolResult, Image, Summary, EncryptedReasoning.

##### Le reasoning chiffre n'est pas persiste : le replay est perdu des le resume

`mineur` · `divergent` · effort `M`

**Impact.** Apres un `/resume`, le modele repart sans son etat de raisonnement chiffre : qualite degradee sur les taches longues et perte du benefice du cache backend sur le prefixe. La difference de comportement entre une session continue et une session reprise n'est ni documentee ni signalee a l'utilisateur.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/models.rs:834-846 `ResponseItem::Reasoning { summary, content, encrypted_content }` est un item de premiere classe persiste dans le rollout (protocol/src/protocol.rs:3183 `RolloutItem`), et rejoue tel quel dans les requetes suivantes.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:68 retire tous les `ContentBlock::EncryptedReasoning` avant ecriture, et lib.rs:272 ecrit une entree `EncryptedReasoningRedacted`; le replay n'existe donc que pour la duree du process (capture en /home/arthur/dev/pyxis/crates/agent-core/src/transition.rs:199 et reemission en /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:163-184).

**Statut documentaire.** Choix apparemment deliberé (redaction de donnees sensibles au repos), mais aucun ADR ne le trace : greps `EncryptedReasoning|reasoning_replay|US-031` dans docs/DECISIONS.md et docs/ROADMAP.md sont vides.

##### Pas de rollback de tours (annuler les N derniers echanges du contexte)

`mineur` · `absent` · effort `M`

**Impact.** Apres un echange qui a pollue le contexte (gros tool_result inutile, mauvaise piste), le seul recours est de repartir de zero et de perdre tout l'historique utile.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:655-661 `Op::ThreadRollback { num_turns }` avec l'event correspondant `EventMsg::ThreadRolledBack(ThreadRolledBackEvent)` (protocol.rs:1318).

**Pyxis.** Greps `rollback|undo|rewind|edit_prev` sur /home/arthur/dev/pyxis/crates/ : aucun hit fonctionnel (uniquement `InlineScrollback` du TUI). Le seul reset disponible est /new ou /clear qui vide tout le contexte (/home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:955-985).

##### Signaux backend ignores : reroutage de modele, verification de compte, safety buffering

`mineur` · `absent` · effort `M`

**Impact.** Pyxis attaque exactement le meme backend Codex : si le backend reroute silencieusement vers un autre modele (safety routing), l'UI continue d'afficher le slug demande et la comptabilite de fenetre de contexte devient fausse. Un `X-Reasoning-Included: true` non lu signifie une double comptabilisation des tokens de raisonnement dans l'estimation locale.

**Codex.** /home/arthur/dev/codex/codex-rs/codex-api/src/common.rs:82-92 `ResponseEvent::{ServerModel(String), ModelVerifications(...), TurnModerationMetadata(...), SafetyBuffering(...)}`; events correspondants `EventMsg::{ModelReroute, ModelVerification, TurnModerationMetadata, SafetyBuffering}` (protocol/src/protocol.rs:1303-1315), consommes en core/src/session/turn.rs:2310-2325.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_events.rs:96-97 : tous les types SSE non reconnus retombent sur `_ => Ok(Vec::new())`; /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs ne lit aucun header `OpenAI-Model` ni `X-Reasoning-Included` (seul `Retry-After` est parse, chatgpt.rs:325).

##### Sous-agents, delegation et evenements collab

`mineur` · `absent` · effort `XL`

**Impact.** Aucune : l'absence est assumee et documentee. A signaler uniquement pour completude de l'inventaire.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/codex_delegate.rs (run_codex_thread_one_shot, utilise par tasks/review.rs:22) et la famille d'events `CollabAgentSpawnBegin/End`, `CollabAgentInteractionBegin/End`, `SubAgentActivity` (/home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:1465-1478), plus `AgentStatus`/`SubAgentSource` (protocol.rs:1727, 2819).

**Pyxis.** Greps `sub_agent|subagent|delegate|spawn_agent` sur /home/arthur/dev/pyxis/crates/ : aucun hit. Une seule boucle `run_agent` (/home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:331).

**Statut documentaire.** /home/arthur/dev/pyxis/docs/CURRENT_STATUS.md:24 liste explicitement les sous-agents parmi les elements differes.

#### Non applicables à Pyxis

- **Ops et evenements de conversation temps reel (audio/WebRTC)** (mineur) : Aucune : surface produit propre a Codex, hors du perimetre d'un agent de code terminal mono-provider.

### Suite d'outils exposée au modèle

**Parité estimée : partial**

*Surface Codex.* La surface Codex est construite dynamiquement par tour dans `codex-rs/core/src/tools/spec_plan.rs:176-211` (`build_tool_specs_and_registry`), qui agrege 6 sources: shell (`add_shell_tools`, spec_plan.rs:650-691), ressources MCP (:706-712), utilitaires coeur (:715-806), collaboration multi-agents (:809-895), outils d'extension/plugins (:935-943), outils dynamiques pousses par le client (:898-928), puis les specs hebergees (:302-331). Le type `ToolSpec` (codex-rs/tools/src/tool_spec.rs:19-52) porte 5 formes: `Function`, `Namespace`, `ToolSearch`, `WebSearch` (hebergee cote Responses), et `Freeform` (grammaire lark, serialisee `type: "custom"`). Famille shell: `exec_command` (PTY, params `cmd`, `workdir`, `tty`, `yield_time_ms` 250-30000ms, `max_output_tokens` defaut 10000, `shell`, `login`, `environment_id` + params d'escalade, shell_spec.rs:21-111) et `write_stdin` (`session_id`, `chars`, `yield_time_ms`, shell_spec.rs:113-155) pour les sessions persistantes, ou `shell_command` one-shot (`command`, `workdir`, `timeout_ms` defaut 10000ms via exec.rs:58, shell_spec.rs:157-225). Le choix entre les deux est fait par `shell_type_for_model_and_features` (codex-rs/tools/src/tool_config.rs:81-116) a partir de `ModelInfo.shell_type` et des feature flags. Edition: `apply_patch` en outil FREEFORM a grammaire lark multi-hunk/multi-fichier (Add/Update/Delete/Move, handlers/apply_patch.lark), gate sur `model_info.apply_patch_tool_type` (spec_plan.rs:782-786), avec localisation floue 4 passes exact/rstrip/trim/normalise (apply-patch/src/seek_sequence.rs:44,76) et detection heredoc dans les commandes shell (apply-patch/src/invocation.rs:27). Autres outils modele: `update_plan` (plan_spec.rs:7-58), `view_image` (`path`, `detail: high|original`, output_schema image_url, view_image_spec.rs:15-69), `request_permissions` (profil FS/reseau, shell_spec.rs:227-262), `request_user_input` (questions structurees, request_user_input_spec.rs:8-60), `get_context_remaining`/`new_context` (token budget), `clock/curr_time` + `clock/sleep`, `list_mcp_resources`/`list_mcp_resource_templates`/`read_mcp_resource` (mcp_resource_spec.rs:24,52,80), les outils MCP prefixes `mcp__` (handlers/mcp.rs:29-45), `tool_search` (decouverte BM25 des outils differes, tool_search_spec.rs:13-76), `web.run` (extension, ext/web-search/src/tool.rs:54) ou `web_search` hebergee (hosted_spec.rs:14-46), `image_gen.imagegen`, la famille multi-agents (`spawn_agent`, `send_message`, `followup_task`, `wait_agent`, `interrupt_agent`, `list_agents`, `close_agent`, multi_agents_spec.rs:87-349), et le code-mode (`codex` freeform JS qui appelle les outils en nested, code_mode/execute_spec.rs:7-39). Le gating est a 4 niveaux d'exposition (`Direct`, `Deferred`, `DirectModelOnly`, `Hidden`, tools/src/tool_executor.rs:15-36), plus capacites provider, feature flags, plan de compte et modalites du modele. La troncature de sortie est pilotee par modele (`TruncationPolicy::Bytes|Tokens`, protocol.rs:3326-3356) et coupe au MILIEU avec un entete `Warning: truncated output (original token count: N) / Total output lines: M` (utils/output-truncation/src/lib.rs:12-23). Codex n'expose AUCUN outil natif de lecture/recherche de fichiers: pas de `read_file`, `grep` ni `glob` (le crate `file-search` n'est utilise que par app-server/tui/rollout, jamais expose au modele) - tout passe par le shell.

*Surface Pyxis.* Pyxis expose exactement 6 outils, enregistres en dur et sans condition dans `crates/agent-cli/src/main.rs:688-699` (et `crates/agent-tools/src/lib.rs:47-62` pour `default_registry`): `read` (path, offset, limit - plafond 2 Mo, rendu numerote avec hints de pagination, read.rs:16,44-56,116-160), `glob` (pattern, path - 1000 resultats max, glob.rs:15,40-51), `grep` (pattern, path, glob - 500 matches, fichiers > 5 Mo ignores, lignes coupees a 300 octets, grep.rs:16-21,52-62), `write` (path, content - 2 Mo max, write.rs:33-45), `edit` (path, old_string, new_string - ancre unique, localisation 4 passes exact/trim_end/trim/Unicode reprise de Codex, edit.rs:41-53,110-140) et `bash` (parametre unique `command`, bash.rs:44-52). Toutes les specs passent par `Registry::tool_specs()` (registry.rs:78-96) qui cappe la description a 2048 caracteres et trie par nom pour un prompt deterministe, puis par `ToolSpec::validate()` (agent-core/src/provider.rs:137-165) qui impose un schema STRICT (`additionalProperties: false`, tous les champs dans `required`), et enfin par `build_tools` (agent-provider/src/chatgpt_request.rs:255-268) qui emet uniquement `{"type":"function", ..., "strict": true}`. Le pipeline d'execution est un vrai point fort: parse+validate fail-closed, permission (5 modes x taint OWASP LLM01), `call()` sous `tokio::time::timeout`, marquage taint (registry.rs:125-227), metadonnees fail-closed par defaut (`is_read_only=false`, `is_sensitive=true`, `returns_untrusted=true`, tool.rs:122-146), segmentation du batch avec reads surs en parallele `buffer_unordered(10)` (registry.rs:34,255-268), confinement de chemin par outil (`confine` + `ensure_existing_path_no_links`, path.rs) et guidelines comportementales co-localisees injectees dans le system prompt (`behavioral_guidelines`, registry.rs:99-114). Les plafonds sont globaux et constants: entree outil 4 Mo, write 2 Mo, fichier edit 5 Mo, ancre 200 Ko, commande 16 Ko (tool.rs:22-26); timeout unique de 120 s non configurable (tool.rs:65, aucun `.timeout(` dans agent-cli); sortie bash bornee a 30 000 octets avec troncature de QUEUE (on garde la fin, bash.rs:16,205-206,265-290). Le reseau du bash est filtre par un proxy CONNECT a allow-list de hostnames (agent-sandbox/src/proxy.rs:18-30). Les outils MCP sont decouverts mais PAS exposes au modele (agent-mcp/src/lib.rs:7-8, docs/CURRENT_STATUS.md:12). Aucun outil de plan, d'image, de recherche web, ni de session shell persistante.

#### Écarts pertinents

##### Aucun acces web expose au modele (ni tool hebergee, ni tool client)

`majeur` · `absent` · effort `M`

**Impact.** L'agent ne peut pas consulter une doc d'API, un changelog ou un message d'erreur inconnu. Sur une tache de debug avec une lib recente, il n'a que ses poids. Et comme Pyxis est mono-provider sur le backend Codex, la version hebergee est gratuite en implementation cote execution : c'est juste une entree dans le tableau `tools`.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/hosted_spec.rs:14-46 - `create_web_search_tool` emet `ToolSpec::WebSearch` avec `external_web_access`, `indexed_web_access`, `filters`, `user_location`, `search_context_size`, `search_content_types` ; /home/arthur/dev/codex/codex-rs/ext/web-search/src/tool.rs:54-88 - alternative client `web.run` (namespace `web`), selectionnee par `standalone_web_search_enabled` (core/src/tools/spec_plan.rs:634-643).

**Pyxis.** Grep `web_search|websearch|fetch|http_get|url` sur /home/arthur/dev/pyxis/crates/agent-tools/ : aucun outil. Le seul acces reseau du modele est `bash` + curl, filtre par un proxy CONNECT a allow-list fail-closed (/home/arthur/dev/pyxis/crates/agent-sandbox/src/proxy.rs:18-30) - donc bloque par defaut (`ProxyPolicy::default().is_allowed(...) == false`, proxy.rs:136).

##### Les outils MCP sont decouverts mais jamais exposes au modele

`majeur` · `absent` · effort `L`

**Impact.** C'est la seule voie d'extension de la suite d'outils par l'utilisateur. Sans elle, les 6 outils natifs sont la totalite de ce que l'agent peut faire, definitivement. L'architecture est prete (`DynTool` est explicitement concu pour que natif et MCP soient indistinguables au dispatch, tool.rs:2-5), l'ecart est purement d'implementation.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/mcp.rs:29-45 - `McpHandler` convertit chaque `ToolInfo` en `ToolSpec` et le nomme `mcp__<serveur>__<outil>` ; core/src/tools/spec_plan.rs:706-712 - `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` ajoutes des qu'un serveur existe ; mcp.rs:76-80 - parallelisme herite du flag read-only du serveur.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-mcp/src/lib.rs:7-8 - « Sont reportes : le wrapping des outils en `DynTool` (registre `agent-tools`), l'OAuth PKCE par serveur et les transports SSE / HTTP » ; /home/arthur/dev/pyxis/docs/CURRENT_STATUS.md:12 - « MCP tools are not yet exposed as callable model tools ». Le point d'entree existe pourtant deja : `RegistryBuilder::register_dyn` (agent-tools/src/registry.rs:424-427).

**Statut documentaire.** Ecart connu et assume comme non-goal du PRD courant (tasks/prd-codex-orchestration.md:347 « MCP tools branches dans la boucle modele - hors scope de ce PRD ») mais planifie en Phase 2 (docs/ROADMAP.md:86, :113 « outils MCP enregistres comme DynTool (uniformite), tous returns_untrusted=true » ; docs/CURRENT_STATUS.md:19).

##### Le canal provider ne sait emettre que des tools `function` : ni freeform/grammaire, ni web_search hebergee, ni namespace

`moyen` · `absent` · effort `M`

**Impact.** C'est le verrou structurel sous plusieurs autres ecarts : tant que la couche provider ne modelise qu'un seul type d'outil, Pyxis ne peut ni brancher la recherche web hebergee du backend Codex (qui est un tool `web_search` cote Responses, pas une function), ni adopter un outil de patch a grammaire, ni exposer des namespaces. C'est une extension d'enum + un match dans `build_tools`, mais elle conditionne le reste.

**Codex.** /home/arthur/dev/codex/codex-rs/tools/src/tool_spec.rs:19-52 - `ToolSpec` a 5 variantes (`Function`, `Namespace`, `ToolSearch`, `WebSearch`, `Freeform` serialise `type: "custom"`), et `create_tools_json_for_responses_api` (tool_spec.rs:78+) les serialise toutes ; `apply_patch` (core/src/tools/handlers/apply_patch_spec.rs:18-26) et le code-mode (core/src/tools/code_mode/execute_spec.rs:24-38) dependent directement de `Freeform`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:255-268 - `build_tools` code en dur `"type": "function"` + `"strict": true` pour chaque spec ; /home/arthur/dev/pyxis/crates/agent-core/src/provider.rs:131-135 - `ToolSpec { name, description, input_schema }` n'a aucun discriminant de type ni `output_schema`. Grep `"custom"|Freeform|grammar|lark|web_search` sur crates/ : aucun resultat.

##### Pas d'outil `update_plan` : le modele n'a aucun canal structure pour exposer son plan

`moyen` · `absent` · effort `M`

**Impact.** Sur les taches longues, c'est le principal signal de progression rendu dans la TUI et le principal garde-fou contre la derive de l'agent. Son absence coute a la fois en UX (aucune vue du plan) et en qualite d'execution (le modele n'est pas force de materialiser ses etapes). Le PRD TUI parity prevoit du rendu de transcript mais aucune cellule de plan.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/plan_spec.rs:7-58 - `update_plan(explanation?, plan: [{step, status: pending|in_progress|completed}])`, avec l'invariant « au plus un step in_progress » dans la description ; enregistre sous condition `turn_context.config.update_plan_enabled` (core/src/tools/spec_plan.rs:720-722).

**Pyxis.** Grep `update_plan|todo|TodoWrite|plan_item` sur /home/arthur/dev/pyxis/crates/ : seules occurrences = `PermissionMode::Plan` (agent-cli/src/settings.rs:29) et un nom de repertoire de test (agent-tools/src/tests_integration.rs:1430). Le mode Plan de Pyxis est une politique de permission read-only (agent-tools/src/permission.rs:27-29), pas un outil de planification.

##### `bash` est one-shot : pas de PTY, pas de session persistante, pas de stdin (`exec_command`/`write_stdin`)

`moyen` · `partial` · effort `L`

**Impact.** Tout ce qui est interactif ou long est hors de portee : serveur de dev a lancer puis interroger, REPL, `git rebase -i`, prompt sudo, build de 10 minutes. Avec un timeout fixe a 120 s (tool.rs:65) l'agent perd le process ET sa sortie. C'est l'ecart fonctionnel le plus couteux de la dimension.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:21-111 - `exec_command` rend un `session_id` quand la commande tourne encore, avec `tty` (allocation PTY), `yield_time_ms` (250-30000 ms) et un `output_schema` portant `session_id`/`exit_code`/`chunk_id`/`wall_time_seconds` (:264-296) ; :113-155 - `write_stdin(session_id, chars, yield_time_ms, max_output_tokens)` pour interagir avec le process vivant.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:20-23 - `BashInput { command }`, un seul champ ; :107-110 - `stdin(Stdio::null())` ; :143-160 - au timeout le process-tree est tue (`kill_process_tree`), aucun handle n'est conserve. Aucune structure de session dans crates/agent-tools/.

##### Les noms/schemas d'outils divergent de ceux sur lesquels les modeles `*-codex` sont fine-tunes

`moyen` · `divergent` · effort `M`

**Impact.** Tension reelle et propre a la position mono-provider de Pyxis : le raccourcissement du prompt sur les modeles `*-codex` suppose que le comportement outillage est dans les poids, or ces poids ont ete entraines sur `shell_command`/`apply_patch`/`update_plan`, pas sur `bash`/`edit`. Le pari peut tenir (les modeles generalisent aux schemas fournis) mais il n'est etaye par aucune mesure dans le repo. C'est le genre d'ecart qui se manifeste en taux d'appels malformes ou en sur-utilisation de `bash` la ou `edit` serait attendu - a instrumenter avant de conclure.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:157-225 (`shell_command`) et :21-111 (`exec_command`), handlers/apply_patch_spec.rs:19 (`apply_patch`), handlers/plan_spec.rs:43 (`update_plan`) - c'est ce vocabulaire exact que le backend Codex sert aux modeles fine-tunes, dont la selection depend de `ModelInfo.shell_type` / `apply_patch_tool_type` (tools/src/tool_config.rs:81-116).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:693-698 - `read`, `glob`, `grep`, `write`, `edit`, `bash` ; /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs:28-30 - Pyxis detecte pourtant explicitement les slugs `-codex` pour raccourcir le system prompt, en pariant que « la spec est dans les poids » (prompt.rs:14) ; /home/arthur/dev/pyxis/crates/agent-cli/prompts/codex_finetuned.md:1 - le prompt court re-enumere neanmoins les outils Pyxis (« read, glob, grep, write, edit, bash »), ce qui compense partiellement.

**Statut documentaire.** Le PRD reconnait l'incertitude adjacente sur le traitement du modele (tasks/prd-codex-orchestration.md:327 risque 2 « gpt-5.5 est en realite fine-tune Codex -> prompt long contre-productif », et question ouverte finale « gpt-5.5 est-il traite comme generique ou fine-tune Codex cote backend ? »), mais aucun ADR ne traite l'impact du vocabulaire d'OUTILS sur les modeles fine-tunes.

##### Aucune auto-approbation des commandes shell manifestement inoffensives : tout `bash` declenche une confirmation

`mineur` · `absent` · effort `M`

**Impact.** Pyxis a des outils natifs read/glob/grep qui couvrent une bonne partie du besoin sans approbation, ce qui attenue le probleme. Mais des que l'agent doit faire `git status`, `cargo --version` ou `ls -la`, il declenche une modale. En pratique cela pousse a basculer en `DontAsk`, ce qui desactive du meme coup la protection sur `rm -rf` - l'absence de granularite degrade la securite reelle plus que l'ergonomie.

**Codex.** /home/arthur/dev/codex/codex-rs/shell-command/src/command_safety/is_safe_command.rs:67-110 - allow-list explicite (`cat`, `cd`, `cut`, `echo`, `grep`, `head`, `ls`, `nl`, `pwd`, `sed`-like, `stat`, `tail`, `wc`, `which`, `whoami`, ...) avec analyse d'arguments dangereux pour les cas limites (`base64 -o`, options d'execution de `find`, :112+).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:78-80 - `permission()` retourne `PermissionDecision::Ask` inconditionnellement, sans jamais inspecter `input.command` ; /home/arthur/dev/pyxis/crates/agent-tools/src/permission.rs:106-120 - seuls les modes `DontAsk`/`BypassPermissions` court-circuitent, il n'existe aucun parsing de commande dans agent-tools/ (grep `parse_command|is_safe|allowlist` : rien).

#### Écarts discutables

##### Le modele n'a aucun moyen de demander une escalade de permission ou une regle d'approbation reutilisable

`moyen` · `absent` · effort `L`

**Impact.** Deux consequences : (1) le modele ne peut pas expliquer POURQUOI il a besoin d'une action risquee, donc l'utilisateur approuve a l'aveugle sur un resume JSON tronque a 200 caracteres (registry.rs:364-371) ; (2) il n'existe aucun mecanisme « approuver `cargo test` pour la session », donc chaque commande sensible redemande, ce qui pousse mecaniquement l'utilisateur vers `DontAsk`/`BypassPermissions` et annule le modele de permissions.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:298-344 - `create_approval_parameters` ajoute a `exec_command`/`shell_command` les champs `sandbox_permissions` (`use_default` | `with_additional_permissions` | `require_escalated`), `justification` (question posee a l'utilisateur), `prefix_rule` (ex. `["git","pull"]`, regle d'approbation reutilisable) et `additional_permissions` (profil FS/reseau) ; :227-262 - outil dedie `request_permissions(permissions, reason?, environment_id?)`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:44-52 - le schema de `bash` n'expose que `command` ; /home/arthur/dev/pyxis/crates/agent-tools/src/registry.rs:161-186 - la seule voie est `Resolved::Ask` -> `Approver::approve`, dont le prompt est genere par le harness (`ask_reason`, registry.rs:355-361) et non par le modele ; aucune notion de regle de prefixe persistante dans permission.rs (418 lignes lues, aucun `prefix`, `allowlist` ou cache d'approbation).

##### `bash` n'a ni `workdir` ni timeout par appel ; le timeout global de 120 s n'est meme pas configurable

`mineur` · `partial` · effort `S`

**Impact.** Sans `workdir`, chaque commande dans un sous-crate doit prefixer `cd ...&&`, ce que la doc Codex deconseille explicitement et qui casse la lisibilite des approbations. Sans timeout par appel, un `ls` et une suite de tests partagent le meme budget de 120 s : trop long pour detecter un hang trivial, trop court pour un build reel - et l'agent n'a aucun levier pour arbitrer.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:36-42 - parametre `workdir` (« Working directory for the command. Defaults to the turn cwd. ») ; :208-210 - la description ordonne « Always set the `workdir` param when using the shell_command function. Do not use `cd` unless absolutely necessary. » ; :170-176 - `timeout_ms` (« Maximum command runtime. Defaults to 10000 ms. »), defaut dans core/src/exec.rs:58 (`DEFAULT_EXEC_COMMAND_TIMEOUT_MS = 10_000`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:44-52 - schema limite a `command` ; :104 - `cmd.current_dir(&ctx.workspace)` en dur ; /home/arthur/dev/pyxis/crates/agent-tools/src/tool.rs:65 - `timeout: Duration::from_secs(120)` ; grep `\.timeout\(` sur crates/agent-cli/src/ : aucun resultat, donc la valeur par defaut n'est jamais surchargee.

##### Troncature de sortie : plafond fixe en octets, coupe de tete seulement, pas de budget par appel ni de politique par modele

`mineur` · `divergent` · effort `M`

**Impact.** Le choix de la queue est deliberement documente et defendable (bash.rs:265-270 : sur une compilation, les erreurs et l'exit code sont en fin de sortie) et Pyxis annonce correctement l'omission. Mais trois choses manquent : le modele ne peut pas demander plus de budget pour une sortie qu'il sait grosse, le plafond est en octets alors que le cout reel est en tokens, et on perd definitivement la tete de sortie (premiere erreur d'un test runner, en-tete de diff). Codex conserve les deux extremites et annonce le volume original.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:54-59 - parametre `max_output_tokens` (« Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy. ») ; core/src/unified_exec/mod.rs:70 - `DEFAULT_MAX_OUTPUT_TOKENS = 10_000` ; protocol/src/protocol.rs:3326-3356 - `TruncationPolicy::Bytes|Tokens` portee par `ModelInfo.truncation_policy` ; utils/output-truncation/src/lib.rs:12-30 - coupe au MILIEU (`truncate_middle_chars` / `truncate_middle_with_token_budget`) precedee de `Warning: truncated output (original token count: N)\nTotal output lines: M`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:16 - `MAX_OUTPUT: usize = 30_000` (octets, constante) ; :205-206 et :265-290 - `truncate_tail` conserve la QUEUE et jette le debut, avec un marqueur `[... output truncated, N bytes, beginning omitted]` ; aucun comptage de tokens, aucun parametre d'appel, aucune dependance a `ModelInfo`.

##### Pas d'outil `view_image` et, plus profondement, un `ToolOutcome` incapable de porter une image

`mineur` · `absent` · effort `L`

**Impact.** Le modele ne peut pas regarder un screenshot, un graphe genere ou un rendu de test visuel qu'il vient lui-meme de produire - meme si le canal multimodal existe deja cote requete. L'ecart n'est pas l'outil (trivial) mais le type de retour : il faut elargir `ToolOutcome` a des blocs de contenu, ce qui touche le coeur et la session JSONL.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/view_image_spec.rs:15-50 - `view_image(path, detail?: high|original, environment_id?)` avec `output_schema` `{image_url, detail}` (:52-69) ; enregistre des qu'un environnement existe (core/src/tools/spec_plan.rs:797-805), et le niveau `original` est gate sur `model_info.supports_image_detail_original`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/tools.rs:22-29 - `ToolOutcome { content: String, ... }` : le resultat d'outil est du texte, point ; /home/arthur/dev/pyxis/crates/agent-tools/src/tool.rs:83-87 - meme contrainte sur `ToolOutput`. Pyxis sait pourtant deja envoyer des images en entree utilisateur (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:134-137, `input_image` en data URL).

##### La suite d'outils est statique : aucun gating par modele, capacite ou configuration

`mineur` · `absent` · effort `M`

**Impact.** Asymetrie assumee nulle part : le system prompt s'adapte au modele, la suite d'outils non. Concretement, un `/models` en cours de session change le prompt mais pas les outils, et il n'existe aucun point d'accroche pour desactiver `bash` en mode audit, ou pour exposer un outil different a un modele fine-tune. Une grande partie du gating Codex (capacites provider, plan de compte, modalites) est non-applicable en mono-provider, mais le gating par slug/config ne l'est pas.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/spec_plan.rs:176-211 - la liste est replanifiee a chaque tour ; codex-rs/tools/src/tool_config.rs:81-116 - `shell_type_for_model_and_features` arbitre `UnifiedExec` vs `ShellCommand` vs `Disabled` selon `ModelInfo.shell_type` et les features ; core/src/tools/spec_plan.rs:782-786 - `apply_patch` uniquement si `model_info.apply_patch_tool_type.is_some()` ; codex-rs/tools/src/tool_executor.rs:15-36 - 4 niveaux d'exposition (`Direct`, `Deferred`, `DirectModelOnly`, `Hidden`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:688-699 - les 6 outils sont enregistres inconditionnellement au demarrage ; /home/arthur/dev/pyxis/crates/agent-tools/src/registry.rs:78-96 - `tool_specs()` retourne toujours l'integralite du registre. Pyxis fait pourtant deja de la selection par slug ailleurs : /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs:20-30 choisit le system prompt selon `-codex` dans le slug.

##### `edit` est mono-fichier mono-ancre : pas de patch multi-hunk/multi-fichier ni de creation/suppression/deplacement en un appel

`mineur` · `divergent` · effort `L`

**Impact.** Un refactor touchant 5 fichiers = 5 appels, 5 approbations, 5 aller-retours modele, et aucune atomicite si le 3e echoue. Pyxis est en revanche a parite exacte sur la partie difficile (les 4 passes de localisation floue, reprises verbatim), et le format ancre economise des tokens sur les petites editions. L'ecart est reel mais il a ete pese.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/apply_patch.lark:1-16 - grammaire `hunk+` avec `add_hunk` / `delete_hunk` / `update_hunk` + `change_move`, donc N operations sur N fichiers dans un seul appel atomique ; handlers/apply_patch_spec.rs:9-27 - expose en outil FREEFORM (pas de JSON a echapper) ; apply-patch/src/seek_sequence.rs:44,76 - localisation floue exact/rstrip/trim/normalise.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/edit.rs:20-25 - `EditInput { path, old_string, new_string }`, une ancre, un fichier ; :110-140 - localisation 4 passes exact/trim_end/trim/Unicode, strictement equivalente a `seek_sequence` cote Codex ; write.rs:33-45 pour la creation ; aucune primitive de suppression ni de rename dans crates/agent-tools/ (grep `delete|rename|move` : rien).

**Statut documentaire.** Rejet explicite et argumente : tasks/prd-codex-orchestration.md:342 « apply_patch format shell-heredoc - Pyxis garde son `Edit` par ancre (rendu fuzzy) ; le format diff multi-op est une optimisation future, pas un prerequis », re-ouvert comme question ouverte a :381 « Faut-il un format diff multi-op (apply_patch-like) a terme ? - a reevaluer apres mesure du taux d'edits multi-hunk sur sessions reelles ».

##### Le `strict: true` systematique force tous les parametres optionnels en `required` nullable

`mineur` · `divergent` · effort `S`

**Impact.** Divergence assumee et globalement saine (le mode strict garantit que le modele ne peut pas halluciner un champ, et Pyxis le verifie a l'exposition, ce que Codex ne fait pas). Le cout : le modele doit emettre `"offset": null, "limit": null` a chaque `read`, ce qui ajoute du bruit dans chaque appel et augmente la probabilite d'une erreur de forme sur les outils a beaucoup d'optionnels. A surveiller si la surface d'outils grossit ; pas un defaut aujourd'hui.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:102 et :217 - `strict: false` sur `exec_command` et `shell_command`, `required` limite au strict necessaire (`["cmd"]`, `["command"]`) et les optionnels restent absents du schema ; meme choix sur `view_image` (view_image_spec.rs:47, `required: ["path"]`) et `update_plan` (plan_spec.rs:53).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:264 - `"strict": true` en dur pour tous les outils ; consequence directe dans les schemas : /home/arthur/dev/pyxis/crates/agent-tools/src/read.rs:53 `"required": ["path", "offset", "limit"]` avec `"type": ["integer", "null"]`, grep.rs:59 `"required": ["pattern", "path", "glob"]`, glob.rs:48 `"required": ["pattern", "path"]` ; validation imposee en amont par agent-core/src/provider.rs:177-200 (`validate_strict_schema_object`).

##### Les specs d'outils ne declarent pas de `output_schema`

`mineur` · `absent` · effort `S`

**Impact.** Impact faible tant que les sorties restent du texte libre : la description en prose suffit a un modele. Devient bloquant si Pyxis adopte un jour le retour structure (exit_code separe, session_id) ou si le backend Codex se met a exploiter `output_schema` pour le typage cote serveur.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:264-296 - `unified_exec_output_schema()` declare `chunk_id`, `wall_time_seconds`, `exit_code`, `session_id`, `original_token_count`, `output` ; view_image_spec.rs:52-69 et get_context_remaining_spec.rs:21-35 font de meme ; le champ est porte par `ResponsesApiTool.output_schema`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/provider.rs:131-135 - `ToolSpec` n'a pas de champ de sortie ; /home/arthur/dev/pyxis/crates/agent-tools/src/tool.rs:83-87 - `ToolOutput { content: String, is_error: bool }`, le contrat de sortie n'est decrit qu'en prose dans la description (ex. bash.rs:35-40 « return stdout/stderr plus the exit code »).

##### Pas d'outil `request_user_input` : le modele ne peut pas poser une question structuree

`mineur` · `absent` · effort `M`

**Impact.** Sur une consigne ambigue, l'agent n'a que deux options : deviner et implementer le mauvais choix, ou terminer son tour avec une question en texte libre (ce qui casse l'autonomie que les prompts Pyxis exigent explicitement, prompts/codex_finetuned.md:5 « continue until completion... without asking for confirmation »). Un choix structure a 2-3 options resout la classe entiere.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/request_user_input_spec.rs:8-60 - `request_user_input` avec des questions typees (`id`, `header` <= 12 caracteres, `question`, `options[]` avec `label`/`description`, 2-3 choix mutuellement exclusifs, recommande en premier), bornes de resolution automatique 60-240 s ; expose en `ToolExposure::DirectModelOnly` sous `experimental_request_user_input_enabled` (core/src/tools/spec_plan.rs:736-743).

**Pyxis.** Grep `request_user_input|ask_user|question|clarif` sur /home/arthur/dev/pyxis/crates/agent-tools/ et agent-core/ : aucun outil. Le seul canal modele -> utilisateur en cours de tour est la demande de permission (agent-core/src/tools.rs:31-34 `ToolDispatchEvent::PermissionAsk`), qui est declenchee par le harness, pas par le modele, et ne peut porter que oui/non.

##### Pas d'outils d'introspection de budget de contexte ni d'horloge (`get_context_remaining`, `new_context`, `clock/curr_time`, `clock/sleep`)

`mineur` · `absent` · effort `S`

**Impact.** Impact reel faible : la date est deja dans le contexte et la compaction est pilotee par le harness, ce qui est le bon endroit. Le seul manque exploitable serait `get_context_remaining`, qui permet a l'agent d'adapter sa strategie (lire par plages plutot qu'en entier) avant de declencher une compaction. Marginal tant que la compaction automatique fonctionne.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/get_context_remaining_spec.rs:8-19 - `get_context_remaining()` -> `{tokens_left}` ; new_context_window_spec.rs:6-17 - `new_context()` en `DirectModelOnly` ; core/src/tools/handlers/current_time.rs:23-24 et sleep.rs:24-25 - namespace `clock` avec `curr_time` et `sleep` ; tous gates sur des features (`Feature::TokenBudget`, `Feature::CurrentTimeReminder`, core/src/tools/spec_plan.rs:749-764).

**Pyxis.** Grep `context_remaining|tokens_left|new_context|curr_time|sleep` sur /home/arthur/dev/pyxis/crates/agent-tools/ : aucun resultat. Pyxis compte pourtant les tokens cote harness (`Deps.tokenizer: Arc<HeuristicCounter>`, agent-cli/src/main.rs:709) et injecte la date via le bloc `<environment>` (prompts/gpt5_generic.md:13) - l'information existe, elle n'est simplement pas interrogeable par le modele.

##### Famille d'outils multi-agents (`spawn_agent`, `send_message`, `followup_task`, `wait_agent`, `interrupt_agent`, `list_agents`, `close_agent`)

`mineur` · `absent` · effort `XL`

**Impact.** Aucune consequence a scope constant : c'est un non-goal declare et le pari produit de Pyxis est explicitement l'excellence single-agent avant toute orchestration. Liste ici uniquement pour l'exhaustivite de l'enumeration de la surface Codex.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/multi_agents_spec.rs:87-349 - les 7 outils, en deux generations (V1 differee via `tool_search`, V2 namespacee), avec bornes de timeout d'attente configurables (core/src/tools/spec_plan.rs:405-419) et limite de profondeur de spawn (:352-361).

**Pyxis.** Grep `spawn_agent|subagent|sub-agent|multi_agent` sur /home/arthur/dev/pyxis/crates/ : aucun resultat.

**Statut documentaire.** Non-goal explicite : tasks/prd-codex-orchestration.md:341 « Sous-agents / orchestration multi-agent - defere (Phase 2 roadmap) ; le pari est l'excellence single-agent sur Codex d'abord » ; egalement hors scope du PRD TUI (tasks/prd-codex-tui-parity.md:446).

#### Non applicables à Pyxis

- **Pas d'exposition differee ni de `tool_search` : les 6 outils sont toujours integralement dans le prompt** (mineur) : Sans objet a 6 outils : le cout en tokens de la liste complete est negligeable et le chargement paresseux ajouterait un aller-retour pour rien. Ne deviendra pertinent que si les outils MCP sont branches (gap `no-mcp-tool
- **Pas de code-mode (appel des outils depuis du JavaScript execute en bac a sable)** (mineur) : Le gain du code-mode (composer 10 appels d'outils en une seule execution, avec boucles et conditions) suppose une suite d'outils riche et un runtime JS embarque. A 6 outils dont `bash`, l'agent compose deja par le shell,
- **Outils lies aux produits et a l'infrastructure interne OpenAI (imagegen, memories, goal, plugins, environnements distants, test_sync)** (mineur) : Sans objet pour Pyxis : ces outils dependent soit de backends proprietaires OpenAI (generation d'image facturee, memoires ChatGPT, steering interne), soit du modele multi-environnements de l'exec-server (sandbox distant)

### Sandbox et approbations

**Parité estimée : partial**

*Surface Codex.* Codex separe deux axes orthogonaux. Axe sandbox: `SandboxPolicy` (protocol/src/protocol.rs:995-1043) offre `danger-full-access`, `read-only { network_access }`, `external-sandbox`, `workspace-write { writable_roots, network_access, exclude_tmpdir_env_var, exclude_slash_tmp }`; les racines writables par defaut incluent cwd, `/tmp` et `$TMPDIR` (protocol.rs:1199-1234), et chaque racine porte des `read_only_subpaths` + `protected_metadata_names` qui protegent `.git` et `.codex` contre l escalade par git hooks (protocol.rs:1050-1097, permissions.rs:22-24). L application est **par commande**, pas process-wide: `SandboxType {None, MacosSeatbelt, LinuxSeccomp, WindowsRestrictedToken}` (sandboxing/src/manager.rs:35-40) et `spawn_process` (sandboxing/src/spawn.rs:41) enveloppent chaque exec; sur Linux c est bubblewrap pour le FS plus un filtre seccomp qui coupe les syscalls socket quand le reseau est refuse, avec `PR_SET_NO_NEW_PRIVS` (linux-sandbox/src/landlock.rs:41-84). Axe approbation: `AskForApproval {UnlessTrusted, OnRequest, Granular(GranularApprovalConfig), Never}` (protocol.rs:908-949), ou `UnlessTrusted` n auto-approuve que les commandes classees sures. La classification est reelle: `is_known_safe_command` (shell-command/src/command_safety/is_safe_command.rs:12-49) et `dangerous_command_match` (is_dangerous_command.rs:19-52) parsent le shell via tree-sitter-bash (shell-command/src/bash.rs:13-20). Par-dessus, l `execpolicy` est un DSL de regles compile en `Decision {Allow, Prompt, Forbidden}` (execpolicy/src/decision.rs:9-16) avec regles prefixes, alternatives et `justification` (execpolicy/src/rule.rs:38-92), charge depuis `default.rules` (core/src/exec_policy.rs:52) et amendable a chaud (`blocking_append_allow_prefix_rule`, `blocking_append_network_rule`, execpolicy/src/lib.rs:11-13). L escalade est un cycle complet: le modele peut demander `sandbox_permissions: require_escalated` + `justification` + prefixe d approbation reutilisable dans le schema du tool shell (core/src/tools/handlers/shell_spec.rs:305-329); en cas d echec, `is_likely_sandbox_denied` (sandboxing/src/denial.rs:6-56) declenche un retry escalade apres approbation (core/src/tools/orchestrator.rs:1-8 et 302-370), avec garde-fou si la politique contient des deny-read (core/src/tools/sandboxing.rs:242-300). Les decisions utilisateur sont riches: `ReviewDecision {Approved, ApprovedExecpolicyAmendment, ApprovedForSession, NetworkPolicyAmendment, Denied, TimedOut, Abort}` (protocol.rs:4094-4125). Le reseau a son propre proxy (HTTP, CONNECT, SOCKS5 TCP/UDP, MITM) avec regles par domaine en globset, classification d IP non publiques et audit (network-proxy/src/network_policy.rs:22-60, policy.rs:16-45). Enfin: trust de repertoire persiste (`TrustLevel`, protocol/src/config_types.rs:598-601; ecran d onboarding tui/src/onboarding/trust_directory.rs:70) qui pilote le profil de permission par defaut (core/src/config/permissions.rs:49-58), durcissement pre-main du process (prctl `PR_SET_DUMPABLE`, RLIMIT_CORE=0, purge `LD_*` - process-hardening/src/lib.rs:12-61), escalade via shell patche et wrapper execve (shell-escalation/src/unix/mod.rs:1-13), et outillage CLI dedie `codex sandbox` / `codex execpolicy` (cli/src/main.rs:166-172) plus `--dangerously-bypass-approvals-and-sandbox` (cli/src/main.rs:2875).

*Surface Pyxis.* Pyxis fusionne les deux axes en un seul modele, plus simple et plus fail-closed, mais nettement moins expressif. Cote sandbox FS: `agent_sandbox::enforce_process` (crates/agent-sandbox/src/fs.rs:66-139) pose UNE ruleset Landlock ABI V7, process-wide et irreversible, sur le thread principal avant tokio: lecture sur `/`, ecriture complete uniquement sous le workspace, plus `/dev/tty` et `/dev/null` (fs.rs:60) et les fichiers explicitement writables passes par l appelant (en pratique le seul `~/.pyxis/settings.toml`, crates/agent-cli/src/main.rs:302-305). Le statut est explicite et remonte a l utilisateur: `SandboxStatus {Enforced, PartiallyEnforced, NotEnforced, UnsupportedPlatform}` avec message d avertissement (fs.rs:16-43, main.rs:305-320). Cote reseau: un proxy CONNECT local avec allow-list de hostnames en egalite stricte, fail-closed par defaut vide, qui refuse tout non-CONNECT en 405 et repond 403 sur hote interdit (crates/agent-sandbox/src/proxy.rs:18-32 et 89-107); les sous-process outils et MCP recoivent `HTTP(S)_PROXY/ALL_PROXY/NO_PROXY=""` via `set_proxy_env`, qui fait en plus un `env_clear()` suivi d une allow-list stricte de variables (crates/agent-sandbox/src/lib.rs:19-73) - un scrubbing de secrets que Codex ne fait pas par defaut. Cote approbations: 5 modes type Claude Code (`Default`, `AcceptEdits`, `DontAsk`, `BypassPermissions`, `Plan`) resolus par une fonction pure et testee `resolve_permission` (crates/agent-tools/src/permission.rs:18-30 et 110-164), avec deny terminal, `Plan` = lecture seule stricte au niveau outil, et surtout l invariant taint: toute sortie d outil est untrusted par defaut et une action mutante ou sensible dans la fenetre de taint force `Ask` quel que soit le mode hors Bypass (crates/agent-tools/src/taint.rs:22 et 69-78, ADR-7 R5 docs/DECISIONS.md:206). Le tracker de taint est fail-closed sur mutex empoisonne (taint.rs:75-77) et l approbateur TUI refuse sur canal ferme ou reponse perdue (crates/agent-cli/src/approver.rs:29-36); en headless l approbateur est `AutoDeny` (main.rs:682-687). Les outils sont confines lexicalement au workspace en defense applicative avant le kernel (crates/agent-tools/src/path.rs:35-49), y compris en LECTURE, ce qui est plus strict que Codex. Surface CLI: seulement `--allow <host>`, `--yes`, `--no-sandbox` (main.rs:44-46, 122-124); le mode de permission n est pas selectionnable en ligne de commande, seulement via `~/.pyxis/settings.toml` ou `/permissions` en session (crates/agent-cli/src/settings.rs:21-32, crates/agent-cli/src/interactive.rs:796-798). Une notion de trust existe, mais pour les serveurs MCP uniquement (interactive.rs:1509-1537).

#### Écarts pertinents

##### Racines writables non configurables et /tmp non writable

`majeur` · `absent` · effort `S`

**Impact.** Sous sandbox, tout ce qui ecrit dans /tmp ou $TMPDIR echoue: c est le comportement par defaut de nombreux toolchains (compilateurs via TMPDIR, pipelines shell avec mktemp, certains linkers). L utilisateur n a aucun moyen d elargir sans passer --no-sandbox, ce qui supprime tout le confinement. C est le chemin de contournement le plus probable en pratique.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:1021-1042 (writable_roots, exclude_tmpdir_env_var, exclude_slash_tmp) et protocol.rs:1199-1234 : /tmp et $TMPDIR sont writables par defaut sur Unix, sauf exclusion explicite.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/fs.rs:85-128 : seuls `/` en lecture, le workspace en RW, /dev/tty, /dev/null et les `writable_files` explicites. /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:302-305 : `writable` ne contient que le chemin de settings.toml. Aucun flag CLI (main.rs:38-53).

##### Pas de sous-chemins proteges dans le workspace writable (.git/hooks, .pyxis)

`majeur` · `absent` · effort `S`

**Impact.** Un agent detourne par injection indirecte peut ecrire `.git/hooks/post-commit` ou `.git/config` (core.fsmonitor, alias). Le prochain `git commit` de l utilisateur execute ce code HORS de tout sandbox et hors du proxy reseau. Meme chose pour `.pyxis/sessions/*.jsonl` (falsification de transcript) et un futur AGENTS.md/settings de projet. C est l ecart de securite le plus concret de cette dimension.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:1045-1097 (WritableRoot.read_only_subpaths + protected_metadata_names, avec commentaire explicite sur .git/hooks comme vecteur d escalade de privileges) et protocol/src/permissions.rs:22-24 (PROTECTED_METADATA_GIT_PATH_NAME='.git', PROTECTED_METADATA_CODEX_PATH_NAME='.codex').

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/fs.rs:92-96 : `PathBeneath::new(PathFd::new(workspace), AccessFs::from_all(abi))` accorde tous les droits sur toute la hierarchie du workspace, sans exception. /home/arthur/dev/pyxis/crates/agent-tools/src/path.rs:35-49 : `confine` ne filtre que la sortie du workspace, pas les sous-chemins sensibles. Grep '.git/hooks', 'read_only_subpaths', 'protected' dans crates/ : aucune occurrence.

##### Pas de filtrage reseau kernel: le proxy est cooperatif et contournable

`majeur` · `partial` · effort `L`

**Impact.** Un `python -c 'import socket...'`, un binaire compile pendant la session, ou tout client ignorant HTTP_PROXY exfiltre librement. Comme Pyxis autorise en plus la lecture de tout le disque au niveau kernel (fs.rs:85-88), le couple lecture-large + reseau contournable rend l exfiltration realiste apres injection indirecte.

**Codex.** /home/arthur/dev/codex/codex-rs/linux-sandbox/src/landlock.rs:41-84 : install_network_seccomp_filter_on_current_thread + PR_SET_NO_NEW_PRIVS quand le reseau est refuse, applique par commande, donc un socket brut est bloque par le kernel. sandboxing/src/manager.rs:35-40 (SandboxType::LinuxSeccomp).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/proxy.rs:8-10 : commentaire explicite 'un binaire qui ouvre un socket brut en ignorant HTTP_PROXY echappe au filtre'. /home/arthur/dev/pyxis/crates/agent-sandbox/src/lib.rs:54-73 : le filtrage repose uniquement sur les variables d environnement injectees. Aucun usage de seccomp/seccompiler dans crates/ (grep 'seccomp' : 0 occurrence).

**Statut documentaire.** Assume et documente: ADR-7 R3 (docs/DECISIONS.md:202) et docs/CURRENT_STATUS.md:27 ('not a kernel-level network sandbox and does not block raw sockets by itself'). L ecart est connu, pas ignore, mais il reste un ecart de modele de securite.

##### Aucun mode de sandbox selectionnable (read-only / workspace-write / danger-full-access)

`moyen` · `partial` · effort `M`

**Impact.** L utilisateur n a que tout-ou-rien: sandbox workspace-write, ou aucun sandbox du tout. Impossible de lancer une session d exploration reellement read-only au niveau kernel, ni d autoriser une session temporairement plus large sans desactiver entierement Landlock. PermissionMode::Plan approche le read-only mais seulement au niveau outil (permission.rs:126-132) : un `bash` sous Plan est Deny, pas confine.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:995-1043 definit SandboxPolicy en 4 variantes (DangerFullAccess, ReadOnly{network_access}, ExternalSandbox, WorkspaceWrite{...}); protocol/src/config_types.rs:87-95 expose SandboxMode read-only/workspace-write/danger-full-access; cli/src/main.rs:3263 passe --sandbox.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/fs.rs:66-139 : une seule politique en dur (read sur /, write sous workspace). /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:44-46 et 122-124 : le seul levier est le booleen --no-sandbox. Greps sur 'read-only', 'workspace-write', 'danger-full-access', 'SandboxMode' dans crates/ : aucune occurrence hors les alias de PermissionMode dans settings.rs:29.

**Statut documentaire.** ADR-7 R3 (docs/DECISIONS.md:202) fixe le principe Landlock FS + proxy mais ne tranche pas la question des modes; aucune US du PRD (tasks/prd-pyxis.md:379-389, US-020) ne mentionne de modes de sandbox.

##### Aucune classification du risque des commandes shell (safe / dangereuse)

`moyen` · `absent` · effort `L`

**Impact.** En mode Default, `ls`, `pwd` et `git status` declenchent la meme confirmation que `rm -rf`. La fatigue d approbation pousse mecaniquement l utilisateur vers DontAsk ou BypassPermissions, qui suppriment le controle pour TOUTES les commandes. Symetriquement, aucune commande n est jamais bloquee d office: il n existe pas d equivalent a Decision::Forbidden.

**Codex.** /home/arthur/dev/codex/codex-rs/shell-command/src/command_safety/is_safe_command.rs:12-49 (is_known_safe_command, allow-list de binaires read-only, gestion des options dangereuses de find/base64) ; is_dangerous_command.rs:19-52 (dangerous_command_match, ForcedRm, recursion dans les wrappers) ; shell-command/src/bash.rs:13-20 et 29-52 (parsing tree-sitter-bash, sequence de commandes word-only avec operateurs surs).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:80-82 : `fn permission(...) -> PermissionDecision { PermissionDecision::Ask }` inconditionnellement, sans regarder `input.command`. bash.rs:68-79 : `validate_input` ne verifie que le vide et la taille. Greps 'is_safe_command', 'dangerous', 'tree_sitter', 'shlex', 'parse.*command' dans crates/ : aucune occurrence.

##### Pas d execpolicy: aucun systeme de regles allow/prompt/forbidden par commande

`moyen` · `absent` · effort `XL`

**Impact.** Aucun moyen de dire 'cargo test est toujours autorise', 'curl demande toujours', 'sudo est interdit'. Pas de politique d equipe versionnable, pas d amendement persistant apres une approbation. Combine avec l absence de cache d approbation, c est ce qui rend le mode Default couteux a l usage.

**Codex.** /home/arthur/dev/codex/codex-rs/execpolicy/src/decision.rs:9-16 (Decision Allow/Prompt/Forbidden) ; execpolicy/src/rule.rs:38-92 (PrefixPattern avec tokens alternatifs, RuleMatch portant une justification) ; execpolicy/src/policy.rs:88-120 (add_prefix_rule, add_network_rule) ; core/src/exec_policy.rs:52 (default.rules charge depuis CODEX_HOME) ; execpolicy/src/lib.rs:11-13 (amendements persistants) ; cli/src/main.rs:171-172 (sous-commande execpolicy).

**Pyxis.** Greps 'execpolicy', 'exec policy', 'Forbidden', 'rules', 'PrefixRule' sur /home/arthur/dev/pyxis/crates et /home/arthur/dev/pyxis/docs : aucune occurrence. Le seul point de decision par outil est `Tool::permission` (crates/agent-tools/src/tool.rs et permission.rs:69-79), qui retourne une constante par outil sans consulter aucune regle utilisateur.

**Statut documentaire.** Aucun ADR de docs/DECISIONS.md ne rejette explicitement un systeme de regles; l absence semble etre un non-dit du scope MVP (tasks/prd-pyxis.md:238-251 decrit le pipeline d outils sans couche de regles).

##### Pas d escalade apres echec impute au sandbox

`moyen` · `absent` · effort `XL`

**Impact.** Quand une commande echoue parce que Landlock a refuse une ecriture hors workspace, le modele recoit un 'Permission denied' opaque et boucle souvent sur des contournements inutiles. Aucune voie ne permet a l utilisateur d autoriser ponctuellement l operation. C est la consequence directe du choix process-wide irreversible: il faudrait un helper reexecute par commande pour l offrir.

**Codex.** /home/arthur/dev/codex/codex-rs/sandboxing/src/denial.rs:6-56 (is_likely_sandbox_denied: mots-cles stderr, SIGSYS sous seccomp) ; core/src/tools/orchestrator.rs:1-8 (doc: approval -> select sandbox -> attempt -> retry escalade sur denial) et orchestrator.rs:302-370 (detection SandboxErr::Denied puis retry conditionne a la politique d approbation) ; core/src/tools/sandboxing.rs:242-300 (sandbox_override_for_first_attempt, garde-fou deny-read).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:220-236 : un exit code non nul produit un `ToolOutput::error` brut, sans aucune analyse de la cause. Greps 'escalat', 'denied', 'retry' dans crates/agent-tools et crates/agent-cli : aucune occurrence liee au sandbox. Structurellement bloque: fs.rs:8 documente que `restrict_self` est irreversible et applique au process entier.

**Statut documentaire.** ADR-7 R3 (docs/DECISIONS.md:202) assume Landlock FS process-wide; la consequence sur l escalade n est pas discutee.

##### Pas de memorisation d approbation (always allow / approved-for-session)

`moyen` · `partial` · effort `M`

**Impact.** La meme commande repetee dix fois dans un tour declenche dix dialogues identiques. Sans soupape granulaire, la seule issue est de basculer globalement en DontAsk ou AcceptEdits, ce qui desarme aussi les protections sur des actions reellement dangereuses. Le taint force Ask reste actif, ce qui limite le risque, mais le cout d usage est reel.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:4104-4113 (ReviewDecision::ApprovedForSession, ApprovedExecpolicyAmendment, NetworkPolicyAmendment) ; core/src/tools/sandboxing.rs:325-337 (approval_keys, cache d approbation par cle, semantique 'Allow, don t ask again' multi-cibles pour apply_patch).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/permission.rs:184-188 : `Approver::approve` retourne un simple `bool`, sans canal pour une decision persistante. /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:529-536 : PermissionPrompt ne porte que title/reason/preview/mode/taint_forced, pas d option 'toujours'. Greps 'always', 'don\'t ask', 'approved_for_session', 'approval_cache' dans crates/ : aucune occurrence.

##### Allow-list reseau: egalite stricte, pas de wildcard, pas de protocole, pas d approbation runtime

`moyen` · `partial` · effort `M`

**Impact.** Autoriser un ecosysteme reel (crates.io + ses CDN, registry npm, github + codeload) demande d enumerer chaque hote a la main avant de lancer pyxis. Quand une commande echoue sur un 403, l utilisateur doit tuer la session, relancer avec un --allow de plus, et perdre le contexte. En pratique cela pousse a ne rien autoriser puis a desactiver le sandbox.

**Codex.** /home/arthur/dev/codex/codex-rs/network-proxy/src/policy.rs:16-45 (Host normalise, GlobSet pour les domaines, is_loopback_host, is_non_public_ip avec CIDR RFC) ; network-proxy/src/network_policy.rs:22-60 (NetworkProtocol Http/HttpsConnect/Socks5Tcp/Socks5Udp, NetworkPolicyDecision Deny/Ask, audit otel) ; protocol/src/protocol.rs:4109-4113 (NetworkPolicyAmendment persistee apres decision utilisateur) ; core/src/tools/network_approval.rs:171-182 (DeniedByApproval vs DeniedByPolicy).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/proxy.rs:29-31 : `self.allow.iter().any(|h| h == host)` - egalite exacte, pas de suffixe ni de glob (test proxy.rs:133-134 confirme le refus du match partiel comme un choix). proxy.rs:89-94 : tout non-CONNECT est 405, donc HTTP simple impossible; pas de SOCKS5. /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:629 : la politique est figee au demarrage depuis `--allow`, aucun chemin de mise a jour en session.

##### Le mode de permission n est pas selectionnable en ligne de commande

`moyen` · `partial` · effort `S`

**Impact.** Un usage scripte ou CI ne peut pas exprimer 'mode read-only' ou 'mode ask' de facon explicite: il herite silencieusement du mode persiste dans ~/.pyxis/settings.toml, ce qui rend le comportement d une meme commande dependant d un etat invisible et non versionne. Le parser existe deja, seul le flag manque.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:2876 et cli/src/doctor.rs:3263-3265 : --ask-for-approval et --sandbox sont des flags de premier niveau; cli/src/main.rs:2875 ajoute --dangerously-bypass-approvals-and-sandbox et main.rs:1974-1979 le cablage vers approval_policy/sandbox_mode.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:21-32 : `permission_mode_from_arg` accepte deja 'ask'|'accept-edits'|'auto'|'full-access'|'read-only', mais ses seuls appelants sont settings.rs:44 (lecture du fichier) et interactive.rs:796 (commande /permissions). /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:38-53 et 105-140 : aucun flag correspondant dans Args ni dans le parseur; main.rs:273-286 derive le mode uniquement de (headless, --yes).

##### Sandbox process-wide irreversible vs sandbox par commande

`moyen` · `divergent` · effort `XL`

**Impact.** Divergence de modele, pas simplement un manque. Avantage Pyxis: le process agent lui-meme est confine, donc un bug de l agent (pas seulement d un sous-process) ne peut pas ecrire hors workspace, ce que Codex ne garantit pas. Cout: aucune granularite par commande, aucune escalade possible, et tout ce qui doit etre lu ou ecrit hors workspace doit l etre avant l enforcement (main.rs:333-353 liste skills, config MCP, contexte projet, credential, settings comme chargements pre-sandbox), ce qui contraint durablement l architecture de demarrage.

**Codex.** /home/arthur/dev/codex/codex-rs/sandboxing/src/spawn.rs:29-41 (SpawnRequest par commande avec son SandboxType) et sandboxing/src/manager.rs:35-40 ; linux-sandbox/src/landlock.rs:33-40 : 'Apply sandbox policies inside this thread so only the child inherits them, not the entire CLI process'.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/fs.rs:4-8 : la restriction est posee sur le thread principal avant tokio, heritee par tous les workers et sous-process, et `restrict_self` est irreversible. /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:369 : appel unique au demarrage. /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:120-124 : le sous-process n ajoute que le durcissement env, le FS est deja herite.

**Statut documentaire.** Consequence directe d ADR-7 R3 (docs/DECISIONS.md:202) et du choix Linux-first; le compromis est coherent avec le budget de complexite du MVP.

##### Le journal des hotes bloques est ecrit mais jamais lu

`mineur` · `partial` · effort `S`

**Impact.** Un blocage reseau n apparait ni dans le TUI ni dans un evenement: l utilisateur voit seulement un echec applicatif obscur dans la sortie de la commande (au mieux le corps '403 blocked by pyxis network allow-list' si le client l affiche). Le diagnostic 'c est mon allow-list' n est pas immediat, ce qui alimente le reflexe --no-sandbox.

**Codex.** /home/arthur/dev/codex/codex-rs/network-proxy/src/network_policy.rs:12-20 : evenement d audit `codex.network_proxy.policy_decision` emis pour chaque decision ; core/src/tools/network_approval.rs:179-182 : le refus remonte au tour de l agent avec un message distinguant politique et approbation.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/proxy.rs:38-40 : `blocked: Arc<Mutex<Vec<String>>>` documente comme 'lisible par le frontend'. Grep '\.blocked' dans /home/arthur/dev/pyxis/crates/agent-cli et crates/agent-tui : aucune lecture; main.rs:629-632 ne conserve que `proxy.addr`. Le champ est donc mort en production, teste uniquement dans proxy.rs:207-211.

##### Pas de trust de repertoire au premier lancement

`mineur` · `partial` · effort `M`

**Impact.** Lancer pyxis dans un depot clone inconnu applique immediatement le mode de permission persiste (potentiellement AcceptEdits ou DontAsk depuis une session precedente, settings.rs:41-45) et injecte le AGENTS.md du depot dans le prompt systeme, sans aucune etape de consentement. Le trust MCP montre que le concept existe deja cote Pyxis, il n est simplement pas etendu au repertoire.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/config_types.rs:594-601 (TrustLevel Trusted/Untrusted, 'determines the approval policy and sandbox mode applied') ; tui/src/onboarding/trust_directory.rs:70 (ecran 'Do you trust the contents of this directory?') ; core/src/config/permissions.rs:49-58 (le profil par defaut depend du trust du projet actif) ; core/src/exec_policy_tests.rs:2281 (les regles execpolicy d un projet ne sont chargees que depuis des couches trusted).

**Pyxis.** Grep '\btrust\b' hors 'untrusted' sur /home/arthur/dev/pyxis/crates : uniquement le trust des serveurs MCP (crates/agent-cli/src/interactive.rs:1504-1537, crates/agent-tui/src/state.rs:1406-1417). Aucun trust de repertoire. /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:331 : le workspace est simplement `current_dir()`, sans verification. Le contexte projet AGENTS.md est lu sans prompt (main.rs:346-349).

##### Pas d outillage pour inspecter ou tester la politique de sandbox

`mineur` · `absent` · effort `S`

**Impact.** Impossible de verifier avant une session ce que le sandbox autorise reellement sur une machine donnee (kernel sans Landlock complet, montages exotiques). Le seul signal est un avertissement binaire au demarrage, ce qui rend le debogage d un echec de commande couteux.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:166 (sous-commande `codex sandbox` qui execute une commande arbitraire sous la politique active) et cli/src/main.rs:169-172 (`codex debug`, `codex execpolicy`) ; cli/src/debug_sandbox.rs:1-40 (backends seatbelt/landlock exposes en debug).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:67-82 (HELP) : aucune sous-commande, seulement des options. La seule observabilite est le message `[sandbox] {warning}` sur stderr au demarrage (main.rs:306-309) derive de SandboxStatus::warning (crates/agent-sandbox/src/fs.rs:29-42).

#### Écarts discutables

##### Le modele ne peut pas demander une escalade avec justification

`mineur` · `absent` · effort `M`

**Impact.** Le dialogue d approbation Pyxis montre la commande mais jamais le pourquoi du modele. L utilisateur doit reconstruire l intention depuis le transcript pour decider, ce qui degrade la qualite des decisions sur les actions ambigues et augmente les approbations reflexes.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/shell_spec.rs:305-329 : le schema du tool shell expose `sandbox_permissions` (use_default / with_additional_permissions / require_escalated), une `justification` destinee a l utilisateur, et un prefixe d approbation reutilisable. protocol/src/models.rs:49 (requires_escalated_permissions). core/src/tools/approvals.rs:33-42 : la justification remonte jusqu a la demande d approbation.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:44-53 : le schema JSON n a qu une propriete `command`, avec `additionalProperties: false`. La `reason` de PermissionRequest (crates/agent-tools/src/permission.rs:172) est generee cote harness (registry.rs:354), jamais fournie par le modele.

##### Pas de durcissement du process agent (ptrace, core dumps, LD_*)

`mineur` · `divergent` · effort `S`

**Impact.** Le process pyxis detient en memoire le token OAuth d abonnement ChatGPT (main.rs:587). Sans PR_SET_DUMPABLE=0, un autre process du meme utilisateur peut y attacher un ptrace ou lire un core dump et recuperer la credential. C est exactement le scenario contre lequel Codex se protege, et il n est pas couvert par Landlock.

**Codex.** /home/arthur/dev/codex/codex-rs/process-hardening/src/lib.rs:12-61 : pre_main_hardening appele en ctor, prctl(PR_SET_DUMPABLE,0) avec sortie du process en cas d echec, RLIMIT_CORE=0, suppression des variables LD_*; responses-api-proxy/src/main.rs:6 montre l appel effectif.

**Pyxis.** Grep 'prctl', 'PR_SET_DUMPABLE', 'RLIMIT_CORE', 'ptrace' sur /home/arthur/dev/pyxis/crates : aucune occurrence (seul 'LD_PRELOAD' apparait, et uniquement comme cle sensible dans la config MCP, crates/agent-cli/src/interactive.rs:1525). /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:329-331 : main() ne fait aucun durcissement avant parse_args.

##### Lecture kernel non restreinte: les secrets du home sont lisibles par les sous-process

`mineur` · `divergent` · effort `M`

**Impact.** Les outils read/glob/grep sont plus stricts que Codex (confines au workspace, la ou Codex autorise la lecture disque complete), mais `bash` contourne totalement cette defense: `cat ~/.ssh/id_rsa` ou `cat ~/.aws/credentials` reussit. L asymetrie est incoherente: la defense applicative est plus forte que le kernel, donc elle ne tient que tant que le modele passe par les outils typés.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/sandboxing.rs:274-283 : has_denied_read_restrictions bloque le bypass du sandbox pour preserver les deny-read, ce qui implique une politique FS avec chemins interdits en lecture ; protocol/src/permissions.rs:568-612 (FileSystemSandboxPolicy avec entrees par chemin, sous-chemins read-only par defaut).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/fs.rs:85-88 : `add_rule(PathBeneath::new(PathFd::new("/"), AccessFs::from_read(abi)))` avec le commentaire assume 'La confidentialite FS n est pas l objectif de cette politique, seulement le confinement en ecriture'. Cote outils, /home/arthur/dev/pyxis/crates/agent-tools/src/path.rs:35-49 confine bien les lectures au workspace, mais `bash` n a aucun filtre de chemin (bash.rs:89-125).

**Statut documentaire.** Choix explicite documente dans fs.rs:82-84; a considerer comme divergence assumee, mais l incoherence avec path::confine merite d etre tranchee.

### Configuration, profils, précédence

**Parité estimée : minimal**

*Surface Codex.* Codex traite la configuration comme un sous-systeme a part entiere : un crate dedie `codex-rs/config/` (~50 fichiers) plus `codex-rs/features/`. Le schema utilisateur `ConfigToml` (codex-rs/config/src/config_toml.rs:150-511) expose ~90 cles de premier niveau reparties en familles : modele (`model`, `review_model`, `model_provider`, `model_context_window`:160, `model_auto_compact_token_limit`:163, `model_reasoning_effort`:347, `model_reasoning_summary`:349, `model_verbosity`:351, `personality`:358, `model_catalog_json`:355), securite (`approval_policy`:170, `sandbox_mode`:195, `sandbox_workspace_write`:198 avec `writable_roots`/`network_access`/`exclude_tmpdir_env_var` a config/src/types.rs:925, `default_permissions`:203, `[permissions]`:207, `[projects.<path>].trust_level`:420/563), prompt (`instructions`:214, `developer_instructions`:218, `model_instructions_file`:236, `compact_prompt`:239, `include_*_instructions`:221-230), contexte projet (`project_doc_max_bytes`:287, `project_doc_fallback_filenames`:291, `project_root_markers`:468), execution (`shell_environment_policy` avec inherit/exclude/set/include_only/filters a config/src/shell_environment_policy.rs:15-38, `allow_login_shell`:192, `tool_output_token_limit`:294, `background_terminal_max_timeout`:298), extensibilite (`mcp_servers`:260, `hooks`:441, `skills`:438, `plugins`:445, `marketplaces`:449, `agents`:432/683, `memories`:435, `tools`:426/632), UX/TUI (`tui` a config/src/types.rs:692-800 : notifications, animations, vim_mode_default, alternate_screen, status_line, terminal_title, theme, keymap, session_picker_view, resume_cwd), et plomberie (`history`:317, `log_dir`:326, `sqlite_home`:321, `file_opener`:333, `notify`:211, `check_for_update_on_startup`:473, `disable_paste_burst`:478, `debug.config_lockfile`:526). La precedence est explicite et documentee dans le loader (codex-rs/config/src/loader/mod.rs:96-107) et encodee numeriquement (codex-rs/config/src/config_layer_source.rs:31-48) : MDM(0) < system /etc/codex/config.toml(10) < cloud enterprise(15) < user $CODEX_HOME/config.toml(20) < profil $CODEX_HOME/<name>.config.toml(21) < projet .codex/config.toml remonte cwd->racine(25) < flags de session -c(30). Les couches projet sont chargees mais desactivees si le repertoire n'est pas trust (loader/mod.rs:1214-1340) et une denylist interdit au config de repo de choisir base_url/provider/notify/profile/otel (loader/mod.rs:64-76). Les overrides a la volee `-c foo.bar.baz=value` sont parses en TOML avec fallback string litterale et projection en chemin pointe (codex-rs/utils/cli/src/config_override.rs:19-90), avec canonicalisation de cle. `CODEX_HOME` est decouvert par variable d'env avec validation d'existence, defaut ~/.codex (codex-rs/utils/home-dir/src/lib.rs:14-21). Les feature flags sont centralises dans un enum `Feature` (~60 variantes, codex-rs/features/src/lib.rs:84-210) avec configs typees par feature (features/src/feature_configs.rs) et un gating requirements-only pour certaines. La validation est serieuse : `deny_unknown_fields` schemars sur chaque struct, mode `--strict-config` (codex-rs/tui/src/cli.rs:16) qui rejette les cles inconnues y compris dans les `-c` (loader/mod.rs:562-583), diagnostics avec chemin + ligne + colonne (config/src/diagnostics.rs:35-84), alias de cles legacy (config/src/key_aliases.rs:11-22), validation semantique des providers avec messages actionnables (config_toml.rs:901-946), et generation de JSON Schema (config/src/schema.rs). Enfin une couche admin `requirements.toml` (~30 cles de contrainte : allowed_approval_policies, allowed_sandbox_modes, allowed_permission_profiles, feature_requirements, mcp_servers, rules, config/src/config_requirements.rs:875-909) borne ce que l'utilisateur peut choisir.

*Surface Pyxis.* Pyxis n'a pas de sous-systeme de configuration : il a un fichier de preferences a trois cles. `crates/agent-cli/src/settings.rs:6-9` declare exactement `permission_mode`, `reasoning_effort` et `model`, stockes dans `$PYXIS_HOME/settings.toml` ou `~/.pyxis/settings.toml` (settings.rs:34-39). Le fichier n'est meme pas parse par un parseur TOML : `load_string_key` (settings.rs:85-99) fait un `split_once('=')` ligne par ligne et `parse_tomlish_string` (settings.rs:141-150) retire des guillemets a la main ; toute section `[table]`, tout tableau, toute valeur multiligne est invisible, et une cle inconnue est silencieusement ignoree sans diagnostic. L'ecriture est symetrique (settings.rs:101-132) et preserve les autres lignes. La precedence effective se reduit a deux niveaux ad hoc cables dans main.rs : `--model` explicite prime sur le modele persiste (main.rs:555-570), l'effort persiste sert de valeur initiale (main.rs:572-585), le mode de permission persiste prime sur le defaut derive du mode d'execution (main.rs:672-681). Il n'y a ni couche systeme, ni couche projet, ni profils, ni override `-c`. Le CLI est un parseur manuel de 13 flags (main.rs:36-52, HELP main.rs:66-80) : `-p/--print`, `--resume`, `--model`, `--allow`, `-y/--yes`, `--no-sandbox`, quatre flags de budget, `--overload-fallback-model`, `-h`. Les variables d'environnement existent mais sont ad hoc et non documentees (4 occurrences seulement dans docs/, toutes dans un doc de spike) : `PYXIS_HOME`, `PYXIS_TOKEN_BUDGET`/`PYXIS_COST_BUDGET_MICRO_USD`/`PYXIS_INPUT_COST_MICRO_PER_KTOK`/`PYXIS_OUTPUT_COST_MICRO_PER_KTOK` (main.rs:198-220), `PYXIS_OVERLOAD_FALLBACK_MODEL` (main.rs:249), `PYXIS_IDLE_TIMEOUT_SECS` (main.rs:596), `PYXIS_REDUCED_MOTION` (main.rs:769), `PYXIS_EXPERIMENTAL_MCP_CONNECT` (interactive.rs:1391), `PYXIS_DEBUG_TUI`, `PYXIS_DEBUG_USAGE`, `PYXIS_ORIGINATOR`, `PYXIS_CODEX_CLIENT_VERSION`. Les autres fichiers lus sont : `<workspace>/.mcp.json` et `~/.claude.json` pour les serveurs MCP au format Claude Code (crates/agent-mcp/src/config.rs:146-151, fusion workspace-au-dessus-de-user a main.rs:388-412), `AGENTS.md`/`CLAUDE.md` remontes jusqu'au `.git` (context.rs:17,44-50), et `~/.agents/skills/` en listage de repertoires (main.rs:513-525). Tout le reste est en dur : budget AGENTS.md 32 Ko (context.rs:13), marqueur de racine `.git` (context.rs:46), profondeur de remontee 24 (context.rs:20), seuils de compaction 70 %/80 % (agent-core/src/budget.rs:46-52), `RunConfig` complet avec max_turns 50 / max_output_tokens 4096 / max_retries 3 (agent-core/src/agent.rs:32-65), plafond de sortie Bash 30 000 octets (agent-tools/src/bash.rs:16), fenetre de contexte via `DEFAULT_MAX_CONTEXT` (main.rs:688). Aucun ADR de `docs/DECISIONS.md` (11 ADR, ADR-1 a ADR-11) ne traite la configuration, et aucune tache de `tasks/*.md` ne mentionne `config.toml`, `settings.toml`, `.pyxis` ou la notion de profil.

#### Écarts pertinents

##### Aucun fichier de configuration declaratif : 3 cles de preferences au lieu d'un config.toml

`majeur` · `partial` · effort `L`

**Impact.** L'utilisateur ne peut rien preconfigurer : chaque preference non couverte par les 3 cles doit etre repassee en flag a chaque lancement ou n'existe pas du tout. Un projet ne peut pas transporter ses reglages, et un nouveau poste ne peut pas reprendre une configuration versionnee.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:150-511 - struct ConfigToml expose ~90 cles de premier niveau (model, approval_policy, sandbox_mode, instructions, tui, mcp_servers, hooks, history, notify, file_opener, ...), toutes lisibles depuis ~/.codex/config.toml.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:6-9 - SETTINGS_FILE="settings.toml" avec exactement PERMISSION_MODE_KEY, REASONING_EFFORT_KEY, MODEL_KEY. Grep sur `config.toml|ConfigToml|\[tui\]|\[profiles\]` dans crates/ : aucun resultat hors .mcp.json. Aucun ADR dans docs/DECISIONS.md (ADR-1..ADR-11) ne rejette explicitement un fichier de config.

##### Le parseur de settings.toml n'est pas un parseur TOML

`moyen` · `divergent` · effort `S`

**Impact.** Le fichier s'appelle `.toml` mais toute syntaxe TOML reelle (tables `[section]`, tableaux, chaines multilignes, valeurs sur plusieurs lignes) est ignoree en silence. Un utilisateur qui ecrit `[tui]\nmodel = "x"` obtient une lecture fausse sans aucun message, et l'extension du format a plus de 3 cles est bloquee par ce parseur.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/loader/mod.rs:507-511 - `toml::from_str(&contents)` avec erreur mappee en ConfigError localise ; chaque struct porte `#[schemars(deny_unknown_fields)]` (config_toml.rs:149).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:85-99 - `load_string_key` fait `contents.lines().find_map(|line| line.split_once('='))` ; settings.rs:141-150 `parse_tomlish_string` retire les guillemets a la main. Aucune dependance `toml` dans agent-cli (verifie par l'absence d'import toml dans settings.rs).

##### Aucune precedence multi-couches (systeme / utilisateur / projet / session)

`moyen` · `partial` · effort `M`

**Impact.** Impossible d'avoir un defaut personnel surcharge par un reglage de projet, ou de comprendre d'ou vient une valeur effective. Chaque nouvelle cle de configuration exigera son propre `if` de fusion dans main.rs, ce qui ne passe pas l'echelle.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_layer_source.rs:31-48 - precedence numerique MDM(0) < System(10) < EnterpriseManaged(15) < User(20) < User+profil(21) < Project(25) < SessionFlags(30) ; documentee en clair dans loader/mod.rs:96-107.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:555-570 et :672-681 - la seule precedence est un `if !args.model_from_cli { args.model = model_persiste }` et un `.unwrap_or(policy.mode)`, cables a la main par cle. Grep `precedence|layer|merge` dans crates/agent-cli/ : aucun resultat.

##### Aucune configuration par projet ni modele de confiance associe

`moyen` · `partial` · effort `L`

**Impact.** Un depot ne peut pas declarer ses propres reglages (modele, mode de permission, racines inscriptibles). Et le jour ou ce sera ajoute, il n'existe aucune notion de trust : Codex a du construire toute une denylist parce qu'un fichier de config venant d'un repo clone est une surface d'attaque.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/loader/mod.rs:1214-1340 - `load_project_layers` remonte cwd->racine de projet en cherchant `.codex/config.toml`, chaque couche etant desactivee si le repertoire n'est pas trust ; loader/mod.rs:64-76 denylist interdisant a un config de repo de fixer openai_base_url, model_provider, notify, profile, otel ; config_toml.rs:420,563 `[projects.<path>].trust_level`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:636-637 - `<workspace>/.pyxis/` n'est utilise que pour `sessions/`. Grep `\.pyxis` dans crates/ : seulement settings.rs:38 (~/.pyxis) et main.rs:637 (sessions). Le seul contenu de repo lu est AGENTS.md (context.rs:17) et .mcp.json (agent-mcp/src/config.rs:148).

##### Aucune validation ni diagnostic de configuration : cles inconnues ignorees en silence

`moyen` · `partial` · effort `M`

**Impact.** Une faute de frappe dans settings.toml est indetectable : la valeur est ignoree, le defaut s'applique, et l'utilisateur croit avoir configure quelque chose. C'est la classe de bug de configuration la plus couteuse a diagnostiquer.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/diagnostics.rs:35-84 - ConfigError porte path + TextRange ligne/colonne et s'affiche `fichier:ligne:colonne: message` ; mode strict `--strict-config` (codex-rs/tui/src/cli.rs:15-17) qui echoue sur cle inconnue, y compris dans les `-c` (loader/mod.rs:562-583) ; validations semantiques a messages actionnables (config_toml.rs:901-946, 959-973).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:92-98 - `find_map` sur les lignes : toute ligne dont la cle ne correspond pas est simplement sautee, aucune collecte des cles inconnues. Les seules erreurs remontees sont des `io::Error` d'ouverture, affichees en `eprintln!("[settings] {err}")` (main.rs:563, 578, 677).

##### Les preferences persistees sont totalement ignorees en mode headless

`moyen` · `divergent` · effort `S`

**Impact.** Le modele choisi via `/models` en interactif n'est pas repris par `pyxis -p`, qui retombe sur DEFAULT_MODEL. Deux surfaces d'execution du meme produit donnent des reglages differents, ce qui casse la reproductibilite entre exploration interactive et automatisation.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/loader/mod.rs:118-131 - `load_config_layers_state` est le point d'entree unique, partage par le TUI et par `codex exec` ; la couche User (precedence 20, config_layer_source.rs:36-42) est chargee independamment du mode d'execution.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:356-366 - `let settings_path = if args.prompt.is_none() { ... } else { None }` : en `-p`, settings_path vaut None, donc load_model (main.rs:558), load_reasoning_effort (main.rs:573) et load_permission_mode (main.rs:672) ne lisent rien.

##### Politique de sandbox et d'approbation non configurable en fichier

`moyen` · `partial` · effort `M`

**Impact.** Un utilisateur qui travaille toujours avec les memes hotes autorises ou un repertoire de build hors workspace doit repasser les flags a chaque lancement. C'est la famille de config la plus sensible en securite et c'est celle qui n'a aucune persistance auditables.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:170-207 - `approval_policy`, `sandbox_mode`, `sandbox_workspace_write` (writable_roots, network_access, exclude_tmpdir_env_var, exclude_slash_tmp a config/src/types.rs:925), `default_permissions`, `[permissions]` profils nommes ; derivation du profil effectif a config_toml.rs:752-825.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:7 - seul `permission_mode` est persistable ; le sandbox n'est pilote que par le flag `--no-sandbox` (main.rs:124) et les hotes reseau par `--allow` repete (main.rs:122), passes a `ProxyPolicy::new(args.allow_hosts)` (main.rs:629). Aucune racine inscriptible configurable : `enforce_process(workspace, &writable)` ne recoit que le chemin des settings (main.rs:304-306).

##### Decouverte du contexte projet entierement en dur (budget, noms de fichiers, marqueur de racine)

`mineur` · `absent` · effort `S`

**Impact.** Un monorepo dont la racine n'est pas un `.git` (worktree, sous-module, repertoire sans VCS) ne verra jamais ses instructions racines. Et un projet avec un fichier d'instructions nomme autrement est invisible. Les valeurs choisies sont bonnes, mais elles ne sont pas negociables.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:285-291 - `project_doc_max_bytes` (defaut 32 KiB, config_toml.rs:68) et `project_doc_fallback_filenames` ; `project_root_markers` (config_toml.rs:466-468, defaut [".git"]) resolu avant meme le chargement des couches projet (loader/mod.rs:305-317).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:13 `const AGENTS_BUDGET: usize = 32_000` ; context.rs:17 `const CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md"]` ; context.rs:20 `MAX_WALK_DEPTH = 24` ; context.rs:46 marqueur `.git` en dur dans la boucle de remontee.

**Statut documentaire.** context.rs:13 documente explicitement l'alignement sur le defaut Codex `project_doc_max_bytes`, mais sans la cle qui va avec.

##### Aucune cle pour surcharger le prompt systeme, les instructions developpeur ou le prompt de compaction

`mineur` · `partial` · effort `M`

**Impact.** Impossible d'ajouter une instruction developpeur permanente (conventions d'equipe, langue de sortie, contraintes de securite) sans recompiler. C'est exactement le levier qu'un utilisateur avance veut d'abord.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:213-239 - `instructions`, `developer_instructions`, `include_permissions_instructions`, `include_apps_instructions`, `include_environment_context`, `model_instructions_file` (chemin absolu vers un fichier remplacant les instructions du modele), `compact_prompt` ; plus `experimental_compact_prompt_file` (config_toml.rs:507).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs:9-37 - le prompt est selectionne en dur par `uses_codex_finetuned_prompt(model)` entre deux constantes compilees ; agent-core/src/compaction.rs:33 `SUMMARY_MAX_OUTPUT: u32 = 4096` en dur. Grep `instructions` dans crates/agent-cli/src/settings.rs : aucun resultat.

##### Fenetre de contexte, seuils de compaction et limites du run non configurables

`mineur` · `partial` · effort `M`

**Impact.** Un utilisateur qui compacte trop tot ou trop tard, ou dont un outil produit des sorties tronquees a 30 Ko, n'a aucun levier. Ce sont les reglages qui determinent le comportement en session longue, la ou l'agent coute le plus cher.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:159-167 - `model_context_window`, `model_auto_compact_token_limit`, `model_auto_compact_token_limit_scope` ; `tool_output_token_limit` (config_toml.rs:293-294) ; `background_terminal_max_timeout` (config_toml.rs:296-298).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/budget.rs:46-52 - seuils micro 70 % / auto 80 % calcules en dur dans `for_model` ; agent-core/src/agent.rs:50-64 `RunConfig::default` fixe max_turns 50, max_output_tokens 4096, max_retries 3, micro_keep_recent 2, loop_guard_threshold 3 ; agent-tools/src/bash.rs:16 `MAX_OUTPUT: usize = 30_000` ; main.rs:688 la fenetre vient de `agent_provider::DEFAULT_MAX_CONTEXT`. Seuls token_budget et cost_budget sont exposes (main.rs:199-233).

##### Aucune famille de configuration TUI (theme, keymap, notifications, animations, statut)

`mineur` · `partial` · effort `L`

**Impact.** Les raccourcis, le theme et les notifications de fin de tour sont les reglages que les utilisateurs de TUI changent en premier. Aucun n'est atteignable, meme par variable d'environnement.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/types.rs:692-800 - struct Tui : notifications/notification_method/notification_condition (types.rs:661-676), animations, show_tooltips, vim_mode_default, raw_output_mode, alternate_screen, status_line + status_line_use_colors, terminal_title, theme, pet, session_picker_view, resume_cwd, keymap (config/src/tui_keymap.rs) ; plus `file_opener` pour hyperlier les citations (config_toml.rs:331-333).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/theme.rs:13-17 - struct Theme construit par `Theme::new(truecolor)` sans aucune source de configuration ; le seul reglage d'apparence est `reduced_motion` derive de NO_COLOR ou PYXIS_REDUCED_MOTION (main.rs:768-769). Grep `keymap|status_line|notification` dans crates/ : aucun resultat hors dependances.

##### Variables d'environnement ad hoc, non documentees et sans mapping vers la config

`mineur` · `partial` · effort `S`

**Impact.** Onze leviers de comportement invisibles pour l'utilisateur, dont un (`PYXIS_HOME`) qui redirige silencieusement le stockage des preferences vers un chemin non verifie. Un chemin invalide echoue tard, avec un message io generique.

**Codex.** /home/arthur/dev/codex/codex-rs/utils/home-dir/src/lib.rs:14-21 - `CODEX_HOME` valide (existence verifiee, erreur explicite sinon) ; Codex garde une surface env minimale et fait passer les reglages par config.toml plus `-c`, si bien que le grep des env::var dans core/config/tui ne remonte quasi que des variables de detection de terminal.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:204,249,596,769 et interactive.rs:1391, agent-core/src/agent.rs:561, agent-tui/src/debug_log.rs:14, agent-auth/src/oauth/openai_chatgpt.rs:58,94 - au moins 11 variables PYXIS_* pilotant du comportement. `grep -rn "PYXIS_" docs/ README.md` renvoie 4 lignes, toutes dans docs/codex-wire-spike.md (PYXIS_ORIGINATOR, PYXIS_DEBUG_USAGE) : aucune reference d'environnement. `PYXIS_HOME` (settings.rs:35) n'est valide nulle part, contrairement a CODEX_HOME.

##### Aucune documentation de reference de la configuration

`mineur` · `absent` · effort `S`

**Impact.** Les 3 cles de settings.toml et les 11 variables d'environnement ne sont decouvrables qu'en lisant le code source. Meme l'existence de `~/.pyxis/settings.toml` n'est mentionnee nulle part pour un utilisateur.

**Codex.** /home/arthur/dev/codex/docs/config.md:1-9 - pointe vers une reference complete config-basic / config-advanced / config-reference, et docs/example-config.md fournit un exemple ; le schema est genere par code (codex-rs/config/src/schema.rs, config/examples/generate-proto.rs).

**Pyxis.** `ls /home/arthur/dev/pyxis/docs/` : ARCHITECTURE.md, codex-port-inventory.md, codex-wire-spike.md, CURRENT_STATUS.md, DECISIONS.md, openai-subscription-auth.md, PROVIDERS.md, ROADMAP.md - aucun document de configuration. `grep -n -i "config|settings|toml|profil" docs/ROADMAP.md docs/CURRENT_STATUS.md` : une seule ligne, sur le chargement MCP (CURRENT_STATUS.md:12).

##### Aucune cle pour l'historique, le repertoire de logs ou les notifications externes

`mineur` · `absent` · effort `M`

**Impact.** Un utilisateur ne peut ni desactiver la persistance des sessions dans un depot sensible, ni rediriger les logs hors du workspace, ni brancher une notification de fin de tour. Le chemin de sessions dans le workspace est en plus un choix non negociable qui pollue chaque depot.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:315-333 - `history` (persistance de ~/.codex/history.jsonl, types.rs:195-217), `sqlite_home`, `log_dir` (activant aussi le log texte du TUI), `file_opener` ; `notify` (config_toml.rs:209-211) pour une commande externe de notification ; `check_for_update_on_startup` (config_toml.rs:470-473).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:636-637 - repertoire de sessions fixe a `<workspace>/.pyxis/sessions`, aucun reglage de persistance ni de desactivation ; agent-tui/src/debug_log.rs:14 le trace fichier n'est pilotable que par PYXIS_DEBUG_TUI. Grep `notify|history|log_dir` dans crates/agent-cli/src/settings.rs : aucun resultat.

#### Écarts discutables

##### Aucun profil nomme pour basculer entre configurations

`mineur` · `absent` · effort `M`

**Impact.** Pas moyen de basculer d'un jeu de reglages 'revue prudente' a 'execution autonome' sans repasser tous les flags. C'est le mecanisme qui rend une config riche utilisable au quotidien.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/profile_toml.rs:22-70 - struct ConfigProfile (model, approval_policy, sandbox_mode, reasoning_effort, verbosity, personality, tools, features, tui...) ; selection par `profile`/`[profiles.<name>]` (config_toml.rs:309-313) ou par fichier `$CODEX_HOME/<name>.config.toml` selectionne via `--profile/-p` (codex-rs/cli/src/lib.rs:65-66).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:36-52 - struct Args ne contient aucun champ profil ; HELP main.rs:66-80 ne liste aucune option `--profile`. Grep `profile|profil` dans crates/agent-cli/ : seul `model_profile` cote provider (agent-provider/src/chatgpt.rs:59), sens different.

##### Aucun override a la volee de type -c cle.pointee=valeur

`mineur` · `absent` · effort `M`

**Impact.** Toute experimentation ponctuelle (changer un seuil, un chemin, un toggle) exige un nouveau flag code en dur. Avec un `-c` generique, chaque cle de config devient immediatement pilotable par script et par CI sans surface CLI supplementaire.

**Codex.** /home/arthur/dev/codex/codex-rs/utils/cli/src/config_override.rs:19-90 - `-c/--config key=value` global, valeur parsee en TOML avec repli chaine litterale, chemin pointe projete en table imbriquee par `apply_toml_override` (config/src/overrides.rs:16-55), injecte en couche SessionFlags de precedence 30.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:104-146 - le parseur de flags reconnait 13 options fixes et rejette tout le reste par `anyhow::bail!("unknown argument: {other}")` (main.rs:141). Aucun `-c`, aucun `--config`.

**Statut documentaire.** Depend de l'existence prealable d'un config.toml (voir no-declarative-config-file).

##### Serveurs MCP configurables uniquement via le format Claude Code, pas via la config Pyxis

`mineur` · `divergent` · effort `M`

**Impact.** La configuration MCP vit dans un format et des fichiers appartenant a un autre produit. Elle ne participe a aucune precedence Pyxis, n'est pas editable par le CLI, et le point de configuration se dedouble le jour ou Pyxis aura son propre fichier. Le cote positif : la reutilisation de `~/.claude.json` donne une valeur immediate a l'installation, avec des diagnostics par entree (config.rs:60-72) plus explicites que la moyenne.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:256-278 - `[mcp_servers.<name>]` dans config.toml, avec `mcp_oauth_credentials_store`, `mcp_oauth_callback_port`, `mcp_oauth_callback_url` ; edition programmatique par `codex mcp add/remove` (config/src/mcp_edit.rs, cli/src/mcp_cmd.rs:279-447).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-mcp/src/config.rs:146-151 - `McpConfigFile::load` lit `<dir>/.mcp.json` et `load_claude` lit les `mcpServers` de `~/.claude.json` ; fusion workspace-au-dessus-de-user a main.rs:388-412. Aucune cle MCP dans settings.rs, aucun sous-commande `pyxis mcp` (HELP main.rs:66-80).

##### Aucun mecanisme de feature flags ou de gating

`mineur` · `partial` · effort `M`

**Impact.** Chaque future fonctionnalite experimentale ajoutera sa propre variable d'environnement ad hoc, sans decouvrabilite ni moyen de lister ce qui est actif. Avec ~10 crates c'est encore tenable, mais le pattern est deja pose (PYXIS_EXPERIMENTAL_MCP_CONNECT).

**Codex.** /home/arthur/dev/codex/codex-rs/features/src/lib.rs:84-210 - enum Feature d'environ 60 variantes (ShellTool, CodexHooks, CodeMode, UnifiedExec, WebSearchRequest, MemoryTool, NetworkProxy, Plugins, ...) avec configs typees par feature (features/src/feature_configs.rs:7-135), table `[features]` validee par schema (config_toml.rs:451-455), warning d'instabilite (config_toml.rs:458) et gating requirements-only pour certaines (features/src/lib.rs:186-203).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1391 - `std::env::var_os("PYXIS_EXPERIMENTAL_MCP_CONNECT").is_some()` : la seule fonctionnalite experimentale est gardee par une variable d'environnement isolee et non documentee. Aucune table `[features]`, aucun registre.

##### Aucune politique d'environnement pour les sous-processus

`mineur` · `absent` · effort `M`

**Impact.** Impossible d'empecher une variable sensible (token CI, cle API d'un autre service) de fuiter dans un sous-processus lance par le modele, ni d'injecter une variable specifique au projet. C'est un levier de securite direct, pas un confort.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/shell_environment_policy.rs:15-38 - `[shell_environment_policy]` avec inherit, ignore_default_excludes, exclude, set, include_only, filters (table canonique fusionnee entre couches) et experimental_use_profile ; plus `allow_login_shell` (config_toml.rs:184-192).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-sandbox/src/lib.rs:55 - `std::env::vars_os()` filtre par une liste de variables preservees codee en dur ; aucune cle de configuration. context.rs:139 lit `SHELL` sans possibilite de surcharge.

##### Aucun mecanisme de compatibilite ou de migration des cles de configuration

`mineur` · `absent` · effort `S`

**Impact.** Avec 3 cles le risque est faible aujourd'hui, mais le format n'a aucun chemin de migration : le premier renommage cassera silencieusement les preferences existantes sans avertissement.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/key_aliases.rs:11-22 - table CONFIG_KEY_ALIASES mappant les cles legacy vers les canoniques, appliquee recursivement (key_aliases.rs:24-58) ; cles obsoletes conservees en no-op documente (config_toml.rs:300-306, 735-744) ; message d'erreur guidant la migration profil v1 -> v2 (loader/mod.rs:269-278).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:85-99 - la lecture compare la cle par egalite stricte `key.trim() != expected_key` ; renommer une cle rend silencieusement la valeur invisible. Aucune notion de version dans settings.toml.

#### Non applicables à Pyxis

- **Couche requirements.toml d'administration (contraintes imposees)** (mineur) : Sans deploiement d'entreprise ni administrateur distinct de l'utilisateur, une couche de contraintes imposees n'a pas de consommateur. Elle n'aurait de sens que si Pyxis visait une distribution en organisation, ce que ri
- **Table [model_providers] et cles de provider alternatives** (mineur) : Le mono-provider est une decision d'architecture assumee et documentee. Exposer une table de providers configurables contredirait ADR-11 et ajouterait une abstraction sans requirement courant.
- **Familles de cles specifiques a l'infrastructure OpenAI** (mineur) : Ces familles pilotent des backends proprietaires OpenAI, de la telemetrie interne, des surfaces desktop/Windows et du provisionnement MDM. Aucune ne correspond a un besoin d'un utilisateur de Pyxis sur Linux avec un abon

### Contexte projet injecté et prompt système

**Parité estimée : partial**

*Surface Codex.* Codex sépare trois couches. (1) Base instructions: le texte système vient des métadonnées du modèle (`codex-rs/models-manager/src/model_info.rs:17` `include_str!("../prompt.md")`, 275 lignes; variantes locales `core/gpt_5_1_prompt.md`, `core/gpt_5_2_prompt.md`, `core/gpt-5.2-codex_prompt.md`, `core/gpt_5_codex_prompt.md`, `core/prompt_with_apply_patch_instructions.md`), surchargeable par config (`core/src/config/mod.rs:3778-3788`: `model_instructions_file` -> `instructions` -> `base_instructions`, plus `developer_instructions`), et modulable par personnalité (`models-manager/src/model_info.rs:18-23,57-70`, `core/templates/personalities/`). (2) Instructions utilisateur/projet: `core/src/agents_md.rs` remonte de cwd jusqu'au project root déterminé par `project_root_markers` (défaut `.git`, `config/src/project_root_markers.rs:5`), teste par répertoire `AGENTS.override.md` puis `AGENTS.md` puis `project_doc_fallback_filenames` (`agents_md.rs:233-246`), concatène root->cwd sous `project_doc_max_bytes` (32 KiB, `config/src/config_toml.rs:68`) avec troncature + warning (`agents_md.rs:105-137`); le fichier global `~/.codex/AGENTS.md` / `AGENTS.override.md` est chargé séparément (`codex-home/src/instructions/mod.rs:9-27`) et placé en tête, séparé par `--- project-doc ---` (`agents_md.rs:41,315-338`). Le rendu final est un message `user` marqué `# AGENTS.md instructions` + `<INSTRUCTIONS>` (`core/src/context/user_instructions.rs:19-28`), rafraîchi et mis en cache par sélection d'environnement (`core/src/agents_md_manager.rs:32-45`). (3) Fragments de contexte dynamiques: un framework `WorldState` diffé par tour (`core/src/session/world_state.rs:19-140`, `core/src/context/world_state/mod.rs`) injecte AGENTS.md (avec notice de remplacement quand il change, `world_state/agents_md.rs:9-11,52-78`), permissions/sandbox rendues depuis des templates par politique (`prompts/templates/permissions/approval_policy/*.md`, `sandbox_mode/*.md`), environnements (`world_state/environment.rs:250-286`: cwd, shell, status, `current_date`, `timezone`, réseau, profil FS + workspace roots, sous-agents), skills disponibles (`core/src/context/available_skills_instructions.rs:47-62`, câblé `core/src/session/mod.rs:3380`), apps/plugins/outils, rappel d'heure (`core/src/context/current_time_reminder.rs:23-38`), changement de modèle (`core/src/context/model_switch_instructions.rs:26`), contexte additionnel externe borné à 1000 tokens (`context-fragments/src/additional_context.rs:5-52`), contexte de hooks (`core/src/context/hook_additional_context.rs`) et contexte interne d'extensions (`core/src/context/internal_model_context.rs:7-12`). S'y ajoute une mémoire persistante inter-sessions (`memories/README.md`, pipeline 2 phases, artefacts `~/.codex/memories/MEMORY.md`, injection via `ext/memories/templates/memories/read_path.md`, citations `<oai-mem-citation>`). Aucun git status n'est injecté dans le contexte modèle (grep `git` sur `core/src/context/world_state/environment.rs` et `environment_context.rs`: aucun résultat). Le TUI affiche les sources chargées (`tui/src/status/helpers.rs:37` `compose_agents_summary`) et `core/src/prompt_debug.rs:103` permet de dumper le prompt.

*Surface Pyxis.* Pyxis implémente la même forme de fil, en beaucoup plus compact, via deux fichiers. `crates/agent-cli/src/prompt.rs:20-31` sélectionne un des deux templates embarqués (`include_str!`) selon le slug: `crates/agent-cli/prompts/gpt5_generic.md` (22 lignes: spec AGENTS.md, autonomie, préambule, bloc environnement, guidance d'édition, qualité) pour tout slug générique ou inconnu, `crates/agent-cli/prompts/codex_finetuned.md` (5 lignes) si le slug contient `-codex`. Le template est resélectionné à chaque tour (`crates/agent-cli/src/interactive.rs:274-277`), puis enrichi des guidelines comportementales des outils (`interactive.rs:215-228`, source `crates/agent-tools/src/tool.rs:187`) et d'une directive d'objectif optionnelle (`interactive.rs:195-209`). Ce texte part dans `instructions` de la Responses API, jamais comme item `input[]` (`crates/agent-provider/src/chatgpt_request.rs:46-53,95`). Le contexte projet est construit une seule fois au démarrage, AVANT Landlock (`crates/agent-cli/src/main.rs:346-349`), par `crates/agent-cli/src/context.rs:26-33`: découverte des AGENTS.md de cwd jusqu'au répertoire contenant `.git` avec cap de profondeur 24 (`context.rs:20,46`), candidats `AGENTS.md` puis `CLAUDE.md` (`context.rs:17`), budget 32 000 octets aligné sur Codex (`context.rs:13`) mais alloué au plus proche d'abord (`context.rs:55-66`) avant réordonnancement parent->cwd (`context.rs:69`), enveloppe `# AGENTS.md instructions` + `<INSTRUCTIONS>cwd: …</INSTRUCTIONS>` identique au marqueur Codex (`context.rs:80`), plus un bloc `<environment>` cwd/shell/current_date/timezone (`context.rs:120-130`). Les deux messages sont poussés dans `AgentContext::context_messages` (`crates/agent-core/src/agent.rs:75-79`) et préfixés à chaque requête sans jamais être persistés dans le transcript (`agent.rs:130-136`), donc ils survivent structurellement à la compaction. Durcissement notable absent côté Codex: rejet des symlinks via `symlink_metadata` (`context.rs:93-97`), lecture plafonnée (`context.rs:107-115`), et texte explicite de déclassement de confiance dans l'enveloppe et dans les deux prompts. Aucun fichier d'instructions hors workspace n'est lu: `~/.pyxis/` ne sert qu'à `settings.toml` (`crates/agent-cli/src/settings.rs:38`), `~/.agents/skills` n'est lu que pour les NOMS du sous-menu `/skills` (`main.rs:511-528`, `interactive.rs:1049`), `~/.claude.json` uniquement pour MCP (`main.rs:404-411`).

#### Écarts pertinents

##### Selection du prompt par sous-chaine de slug, sans lien avec le catalogue modele

`majeur` · `divergent` · effort `M`

**Impact.** Un futur modele fine-tune dont le slug ne contient pas '-codex' recevra le prompt long (surspecification, tokens gaspilles, possible conflit avec ses poids); inversement un modele generique nomme avec '-codex' sera sous-specifie. Le catalogue backend deja recupere pourrait porter cette information.

**Codex.** /home/arthur/dev/codex/codex-rs/models-manager/src/model_info.rs:17,142 les base_instructions sont un champ du ModelInfo (donc pilotees par le catalogue backend, avec fallback local prompt.md), et le repo embarque au moins 5 variantes distinctes (/home/arthur/dev/codex/codex-rs/core/gpt_5_1_prompt.md, gpt_5_2_prompt.md, gpt-5.2-codex_prompt.md, gpt_5_codex_prompt.md, prompt_with_apply_patch_instructions.md)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs:29-31 uses_codex_finetuned_prompt() teste slug.contains("-codex") et prompt.rs:20-26 choisit entre deux templates seulement; /home/arthur/dev/pyxis/crates/agent-provider/src/models.rs ne porte aucun champ d'instructions (grep 'instructions|prompt' -> 0 resultat) alors que le catalogue est deja decouvert depuis le backend (commit 81a8ba7)

##### Aucun fichier d'instructions utilisateur global (~/.pyxis/AGENTS.md)

`moyen` · `absent` · effort `S`

**Impact.** L'utilisateur ne peut pas definir de preferences personnelles transverses (style de reponse, conventions, interdits) valables sur tous ses depots. Chaque projet doit dupliquer ces regles dans son AGENTS.md, ou elles sont perdues. C'est la fonction que remplit ~/.claude/CLAUDE.md ou ~/.codex/AGENTS.md chez les agents concurrents.

**Codex.** /home/arthur/dev/codex/codex-rs/codex-home/src/instructions/mod.rs:9-27 lit ~/.codex/AGENTS.override.md puis ~/.codex/AGENTS.md et les expose comme UserInstructions; /home/arthur/dev/codex/codex-rs/core/src/agents_md.rs:41,315-322 les place en tête du bloc, separees des docs projet par '--- project-doc ---'

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:17 CANDIDATES ne contient que AGENTS.md et CLAUDE.md, et la remontee part du workspace (context.rs:36-50). Greps infructueux sur crates/: 'home_dir', '.pyxis', 'AGENTS' -> seules occurrences hors workspace = /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:38 (settings.toml), main.rs:517 (~/.agents/skills, noms seulement), main.rs:406 (~/.claude.json, MCP seulement)

##### Le modele ignore le mode de permissions et la portee du sandbox

`moyen` · `partial` · effort `M`

**Impact.** En mode read-only ou ask, le modele tente des ecritures et des commandes qu'il ne peut pas executer, brule des tours sur des refus, et n'a aucune strategie d'escalade explicite. Inversement en full-access il ne sait pas qu'il peut agir sans demander. Pyxis a 5 modes de permissions dont le modele n'est jamais informe.

**Codex.** /home/arthur/dev/codex/codex-rs/prompts/templates/permissions/sandbox_mode/workspace_write.md et /home/arthur/dev/codex/codex-rs/prompts/templates/permissions/approval_policy/on_request.md decrivent au modele ce qui est lisible/inscriptible, l'etat reseau et comment demander une escalade; injectes par /home/arthur/dev/codex/codex-rs/core/src/session/world_state.rs:47-70 (PermissionsState) et re-rendus en diff quand la politique change; le bloc environnement porte aussi <filesystem> et workspace_roots (/home/arthur/dev/codex/codex-rs/core/src/context/environment_context.rs:66-78)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:120-130 le bloc <environment> se limite a cwd/shell/current_date/timezone; grep 'approval|permission|sandbox|read-only' sur /home/arthur/dev/pyxis/crates/agent-cli/prompts/*.md ne renvoie que la phrase anti-injection; /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:790-812 la commande /permissions modifie l'approbateur et settings.toml sans rien reinjecter dans le contexte modele

##### Les skills sont listes dans le TUI mais jamais decrits au modele

`moyen` · `partial` · effort `M`

**Impact.** Le modele recoit '/frontend-design' comme texte brut sans savoir ce que ce skill contient ni ou le lire, et il ne peut pas decider seul d'en invoquer un. Le mecanisme n'a de valeur que si l'utilisateur connait deja le contenu du skill.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/context/available_skills_instructions.rs:28-62 rend un fragment 'developer' contenant le catalogue de skills plus un mode d'emploi, cable dans le tour par /home/arthur/dev/codex/codex-rs/core/src/session/mod.rs:3380; le contenu d'un skill invoque est injecte par /home/arthur/dev/codex/codex-rs/core/src/session/turn.rs:671,716

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:511-528 read_skills() ne collecte que les NOMS de dossiers de ~/.agents/skills; /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1049 le sous-menu /skills se contente d'inserer le nom dans l'input; aucun SKILL.md n'est lu (grep 'SKILL.md' sur crates/ -> 0 resultat)

**Statut documentaire.** docs/ROADMAP.md:87 et :114 classent 'Skills / commands + hooks utilisateur' hors MVP, livrable Phase 2

##### Le contexte projet est un instantane de demarrage, jamais rafraichi

`mineur` · `divergent` · effort `S`

**Impact.** Un AGENTS.md cree ou modifie pendant la session (cas frequent: l'agent l'ecrit lui-meme) reste invisible jusqu'au redemarrage. Une session qui franchit minuit continue d'annoncer la veille comme current_date, ce qui pollue tout raisonnement date-sensible. La contrainte Landlock ne justifie que la remontee au-dessus du workspace, pas la relecture des fichiers internes.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/agents_md_manager.rs:32-45 refresh() reevalue la decouverte a chaque tour (cache invalide par la selection d'environnement) et /home/arthur/dev/codex/codex-rs/core/src/context/world_state/agents_md.rs:9-11,52-78 reinjecte le nouveau bloc precede de 'These AGENTS.md instructions replace all previously provided AGENTS.md instructions.'; la date est un fragment separe reemis (/home/arthur/dev/codex/codex-rs/core/src/context/current_time_reminder.rs:23-38)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:346-349 context::messages(&workspace, &context::today_utc()) est appele une seule fois avant Landlock, puis /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:286 se contente de cloner cfg.context_messages a chaque tour

##### Aucune surface pour inspecter le contexte reellement injecte

`mineur` · `absent` · effort `S`

**Impact.** Impossible de verifier quels AGENTS.md ont ete pris en compte, si la troncature a 32 Ko s'est declenchee, ou si un CLAUDE.md a masque un AGENTS.md. Diagnostic d'un comportement non conforme aux conventions du repo purement a l'aveugle.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/status/helpers.rs:37 compose_agents_summary() affiche les fichiers AGENTS.md charges (globaux puis projet) dans le statut, alimente par /home/arthur/dev/codex/codex-rs/core/src/agents_md.rs:393-404 LoadedAgentsMd::sources(); /home/arthur/dev/codex/codex-rs/core/src/prompt_debug.rs:103 permet de dumper le prompt complet

**Pyxis.** grep '"/context"|"/status"|AGENTS' sur /home/arthur/dev/pyxis/crates/agent-tui/src et /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs ne renvoie que le commentaire interactive.rs:124; la liste des commandes (interactive.rs:790 et suivantes) ne comporte ni /context ni /status

#### Écarts discutables

##### Le prompt systeme n'est surchargeable ni par config ni par fichier

`moyen` · `absent` · effort `S`

**Impact.** Changer le comportement de base de l'agent (ton, politique de verification, langue) impose de recompiler. Aucun moyen pour l'utilisateur d'evaluer un prompt alternatif ni de neutraliser une directive du template embarque.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/config/mod.rs:3778-3788 resout base_instructions depuis (dans l'ordre) l'override CLI, model_instructions_file, puis cfg.instructions; /home/arthur/dev/codex/codex-rs/models-manager/src/model_info.rs:53-56 applique la surcharge sur le ModelInfo

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs:11-14 les deux templates sont figes par include_str! et prompt.rs:20-26 ne consulte aucune config; /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs ne contient aucune cle d'instructions (grep 'instruction' sur crates/agent-cli/src/settings.rs -> 0 resultat)

##### Aucune memoire persistante entre sessions

`mineur` · `absent` · effort `XL`

**Impact.** Chaque session repart de zero: conventions decouvertes, pieges de build, decisions anterieures sont reperdus. Compense partiellement par AGENTS.md, qui reste manuel.

**Codex.** /home/arthur/dev/codex/codex-rs/memories/README.md decrit un pipeline 2 phases (extraction par rollout puis consolidation) produisant ~/.codex/memories/MEMORY.md, memory_summary.md, rollout_summaries/ et skills/; l'injection se fait via /home/arthur/dev/codex/codex-rs/ext/memories/templates/memories/read_path.md (protocole de quick memory pass + citations <oai-mem-citation>)

**Pyxis.** grep -ri 'memor' sur /home/arthur/dev/pyxis/crates/ ne renvoie que 'InMemorySession' (agent-core/src/lib.rs:158, structure de test); /home/arthur/dev/pyxis/docs/CURRENT_STATUS.md:21 liste 'Vector memory' parmi les elements differes

**Statut documentaire.** docs/ROADMAP.md:118 place la memoire vectorielle sqlite-vec en Phase 2; docs/CURRENT_STATUS.md:21 la confirme comme differee

##### Pas de fichier d'override local non versionne (AGENTS.override.md)

`mineur` · `absent` · effort `S`

**Impact.** Impossible d'ajouter des instructions locales personnelles (chemins machine, credentials de test, preferences individuelles) sans modifier un fichier versionne partage par l'equipe.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/agents_md.rs:41-42 LOCAL_AGENTS_MD_FILENAME = 'AGENTS.override.md' et agents_md.rs:233-246 le teste en priorite sur AGENTS.md dans chaque repertoire, meme logique cote home (/home/arthur/dev/codex/codex-rs/codex-home/src/instructions/mod.rs:10,26)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:17 CANDIDATES = ['AGENTS.md', 'CLAUDE.md'] : CLAUDE.md est un fallback de compatibilite, pas un override de priorite superieure

##### Budget, noms de fichiers et marqueurs de racine sont codes en dur

`mineur` · `divergent` · effort `S`

**Impact.** Un monorepo sans .git a la racine logique, un depot utilisant un autre nom de fichier de convention, ou un AGENTS.md volumineux qu'on veut plafonner plus bas ne peuvent pas etre pris en charge. Le budget de 32 Ko peut aussi representer une part importante d'une fenetre de contexte sans possibilite de le reduire.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:286-291 expose project_doc_max_bytes (defaut 32 KiB, config_toml.rs:68) et project_doc_fallback_filenames; config_toml.rs:468 expose project_root_markers (defaut ['.git'], /home/arthur/dev/codex/codex-rs/config/src/project_root_markers.rs:5), une liste vide desactivant la remontee

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:13 AGENTS_BUDGET = 32_000 en const, context.rs:17 CANDIDATES en const, context.rs:20,46 marqueur '.git' et MAX_WALK_DEPTH = 24 en dur; aucune de ces valeurs n'apparait dans /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs

##### Pas de mecanisme d'injection de contexte additionnel ni de diff par tour

`mineur` · `divergent` · effort `L`

**Impact.** Aucune voie pour qu'un composant (hook, MCP, Paneflow, sous-systeme de statut) ajoute du contexte au modele en cours de session, et aucune economie de tokens: les blocs sont renvoyes integralement a chaque tour meme inchanges. Acceptable au perimetre actuel, bloquant des que hooks ou embarquement Paneflow arrivent.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/context/world_state/mod.rs et /home/arthur/dev/codex/codex-rs/core/src/session/world_state.rs:19-140 composent N sections typees, chacune rendue seulement quand son snapshot change; /home/arthur/dev/codex/codex-rs/context-fragments/src/additional_context.rs:5-52 permet a un hote d'injecter des paires cle/valeur bornees a 1000 tokens; /home/arthur/dev/codex/codex-rs/core/src/context/hook_additional_context.rs et internal_model_context.rs:7-12 ouvrent la meme surface aux hooks et extensions

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:26-33 produit exactement deux Message::user figes; /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:130-136 les concatene tels quels a chaque requete; ephemeral_messages (agent.rs:82-84) existe mais n'est utilise que pour le message utilisateur non persiste (interactive.rs:266-271)

**Statut documentaire.** docs/ROADMAP.md:114 differe les hooks utilisateur en Phase 2; docs/ROADMAP.md:120 prevoit l'embarquement in-process Paneflow, qui consommera cette surface

##### Troncature du budget AGENTS.md silencieuse pour l'utilisateur et le modele

`mineur` · `divergent` · effort `S`

**Impact.** Sur un depot dont les AGENTS.md cumulent plus de 32 Ko, des conventions entieres disparaissent du contexte sans que l'utilisateur ni le modele n'en sachent rien, ce qui se manifeste plus tard par des violations de convention inexplicables.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/agents_md.rs:122-129 emet un tracing::warn! avec le chemin et le budget restant quand un doc projet depasse le budget

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:57-62 sort silencieusement de la boucle quand la section suivante depasse le budget, et context.rs:71-78 tronque le corps sans aucun message ni marqueur

##### Changement de modele en session non signale au nouveau modele

`mineur` · `partial` · effort `S`

**Impact.** Apres un /models, le nouveau modele herite d'un transcript produit sous un autre scaffold (par exemple prompt court fine-tune vers prompt long generique) sans savoir que les conventions de comportement ont change, ce qui produit des incoherences de style et de niveau d'autonomie sur les premiers tours.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/context/model_switch_instructions.rs:26 fragment <model_switch> injecte lors d'un changement, et /home/arthur/dev/codex/codex-rs/core/src/context_manager/updates.rs:47 build_model_instructions_update_item pousse les nouvelles instructions dans l'historique

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:274-277 le template est simplement resélectionné au tour suivant; aucun message n'informe le modele que le transcript precedent a ete produit par un autre modele avec d'autres instructions

##### Pas de couche de personnalite parametrable

`mineur` · `absent` · effort `M`

**Impact.** Le registre de reponse n'est pas ajustable, mais le ton retenu est deja explicite et coherent avec l'usage vise. Cout d'absence faible pour un outil mono-utilisateur.

**Codex.** /home/arthur/dev/codex/codex-rs/models-manager/src/model_info.rs:18-23 templates friendly/pragmatic, model_info.rs:57-70 suppression de la section '# Personality' quand Personality::None, fichiers /home/arthur/dev/codex/codex-rs/core/templates/personalities/gpt-5.2-codex_friendly.md et _pragmatic.md, fragment /home/arthur/dev/codex/codex-rs/core/src/context/personality_spec_instructions.rs

**Pyxis.** grep -i 'personal' sur /home/arthur/dev/pyxis/crates/ -> 0 resultat; le ton est fige dans /home/arthur/dev/pyxis/crates/agent-cli/prompts/gpt5_generic.md:1 ('dense and direct, with no hollow preamble')

### Extensibilité et commandes (skills, hooks, plugins, slash commands)

**Parité estimée : minimal**

*Surface Codex.* Codex expose quatre surfaces d'extensibilite distinctes. (1) Skills: unite = un dossier contenant `SKILL.md` plus un manifeste optionnel `agents/openai.yaml` (`/home/arthur/dev/codex/codex-rs/core-skills/src/loader.rs:138-142`); la decouverte parcourt des racines scopees Repo / User / System / Admin derivees de la pile de config (`core-skills/src/loader.rs:291-375`), plus `$HOME/.agents/skills`, `$CODEX_HOME/skills`, `/etc/codex/skills`, les racines `.agents/skills` remontees entre la racine projet et le cwd (`core-skills/src/loader.rs:376-420`) et les racines apportees par les plugins. Le modele recoit un catalogue rendu (nom + description + locator) avec budget de contexte (8000 chars ou 2% de la fenetre, `core-skills/src/render.rs:18-21`), un bloc d'instructions d'usage explicite avec regles de declenchement `$SkillName` et divulgation progressive (`core-skills/src/render.rs:27-45`); le corps du `SKILL.md` mentionne est injecte dans le tour sous marqueurs `<skill>...</skill>` (`core-skills/src/skill_instructions.rs:31-40`, `core-skills/src/injection.rs:26-31`). Metadonnees riches: `SkillPolicy.allow_implicit_invocation`, restriction produit, `SkillInterface` (icones, couleur, prompt par defaut), dependances outils (`skills/src/model.rs:8-92`), regles d'activation persistees (`skills/src/model.rs:100-109`). L'invocation implicite est meme detectee a posteriori quand l'agent lit un `SKILL.md` ou lance un script de `scripts/` (`core-skills/src/invocation_utils.rs:31-44`). Six skills systeme sont embarquees (`skills/src/assets/samples/`: imagegen, openai-docs, plugin-creator, review-agent, skill-creator, skill-installer). (2) Hooks: 11 evenements (`hooks/src/lib.rs:19-32`: PreToolUse, PermissionRequest, PostToolUse, PreCompact, PostCompact, SessionStart, SessionEnd, UserPromptSubmit, SubagentStart, SubagentStop, Stop), declares en `hooks.json` ou dans la config TOML (`hooks/src/engine/discovery.rs:307-311`), avec matchers par outil/trigger, statut de confiance et politique `allow_managed_hooks_only` (`hooks/src/engine/discovery.rs:56-83`). Contrat JSON bidirectionnel: entree typee par evenement, sortie typee capable de bloquer, de reecrire l'input outil et d'injecter du contexte modele (`hooks/src/events/pre_tool_use.rs:38-45` avec `should_block`, `block_reason`, `updated_input`, `additional_contexts`; `hooks/src/schema.rs:127-273`). (3) Plugins: manifeste `plugin.json` (`utils/plugins/src/plugin_namespace.rs:10-13`) empaquetant skills, serveurs MCP, apps et hooks (`plugin/src/manifest.rs:8-38`), gere par `codex plugin add|list|marketplace|remove` (`cli/src/plugin_cmd.rs:48-66`) avec marketplaces, sources npm, catalogue distant et sync au demarrage (`core-plugins/src/`). (4) Commandes: ~65 slash commands builtin (`tui/src/slash_command.rs:12-79`), chacune portant des metadonnees d'availability (args inline, disponibilite pendant un tour, disponibilite en side conversation, visibilite par OS/build: `tui/src/slash_command.rs:153-256`), plus feature-gating dynamique (`tui/src/bottom_pane/slash_commands.rs:70-82`) et commandes de service tier injectees apres `/model`. Deux sigils de mention dans le composer: `@` pour fichiers/plugins, `$` pour outils et skills (`utils/plugins/src/mention_syntax.rs:4-7`, `tui/src/bottom_pane/skill_popup.rs:20-29`). `/skills` ouvre un toggle persistant (`tui/src/bottom_pane/skills_toggle_view.rs:37-43`), `/hooks` un navigateur d'evenements et handlers (`tui/src/bottom_pane/hooks_browser_view.rs:44-50`), `/init` envoie un prompt embarque generant AGENTS.md (`tui/src/chatwidget/slash_dispatch.rs:252-255`). A noter honnetement: Codex n'expose plus de slash commands utilisateur basees sur des fichiers de prompt (`grep custom_prompts` vide dans codex-rs); les skills ont remplace ce mecanisme.

*Surface Pyxis.* Pyxis expose une surface d'extensibilite quasi nulle et une surface de commandes volontairement reduite. Les slash commands sont une table statique de 12 entrees `(nom, description, prend_un_argument)` servant a la fois le menu de completion et le dispatch (`/home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58-75`): `/help`, `/models`, `/effort`, `/permissions`, `/skills`, `/goal`, `/providers`, `/mcp`, `/resume`, `/new`, `/clear`, `/quit`. Le dispatch vit dans une seule chaine `match` (`crates/agent-cli/src/interactive.rs:682-1060`), avec un garde-fou d'execution pendant un tour pour `/goal`, `/resume`, `/new`, `/clear` (`interactive.rs:819-868`). Le composer supporte des sous-menus a fil d'Ariane multi-niveaux (`/providers subscription codex connect`, `/mcp <serveur> <action>`) que Codex ne modelise pas de facon aussi uniforme (`crates/agent-tui/src/state.rs:1500-1522`), et les mentions fichier `@chemin` existent (`state.rs:1478-1491`). Les "skills" Pyxis ne sont qu'une liste de noms de dossiers: `read_skills()` liste les sous-dossiers de `~/.agents/skills` et n'ouvre aucun fichier (`crates/agent-cli/src/main.rs:511-528`). `/skills <filtre>` ouvre un sous-menu de filtrage (`state.rs:1294-1298`) dont la validation **insere du texte** `"/<nom> "` dans le message (`state.rs:1508`, `state.rs:1556-1562`); ce token est ensuite explicitement traite comme non-commande et part tel quel au modele (`state.rs:360-364`), avec un simple surlignage visuel de "chip" (`crates/agent-tui/src/render.rs:1445-1471`). Taper `/skills` sans argument ne fait qu'afficher une notice (`interactive.rs:1049-1051`). Aucun `SKILL.md` n'est lu, aucun catalogue n'est injecte: le contexte modele se limite a AGENTS.md/CLAUDE.md plus un bloc `<environment>` (`crates/agent-cli/src/context.rs:17-33`). Il n'existe aucun moteur de hooks: `agent-tools` ne contient aucune occurrence de "hook" alors que `docs/ARCHITECTURE.md:352-360` decrit un pipeline `hooks PreToolUse -> permissions -> call() -> taint -> hooks PostToolUse`; seule la couche de rendu TUI possede `HookCell` et `TranscriptPayload::HookRun` (`crates/agent-tui/src/history_cell.rs:1522-1531`), construits uniquement depuis des tests. Aucune notion de plugin (`grep -rni plugin --include=*.rs crates/` = 0 hit) ni de marketplace. La configuration utilisateur persistee se limite a trois cles dans `~/.pyxis/settings.toml` (mode de permission, effort, modele: `crates/agent-cli/src/settings.rs:34-68`). MCP est configurable via `.mcp.json` / `~/.claude.json` mais reste de l'inspection: les outils MCP ne sont pas exposes au modele (`docs/CURRENT_STATUS.md:12`). Le report est assume et documente: `docs/ROADMAP.md:87` et `:114` placent "Skills / commands + hooks utilisateur" en Phase 2, `tasks/prd-pyxis.md:453` les liste comme differes, et `docs/codex-port-inventory.md:64` classe l'emission de hooks en `skip`.

#### Écarts pertinents

##### Aucun format de skill: Pyxis ne lit ni SKILL.md ni manifeste

`majeur` · `partial` · effort `M`

**Impact.** Un skill Pyxis n'a ni description ni instructions exploitables: l'utilisateur voit un nom de dossier et le modele ne recoit rien. Les skills partagees avec les autres CLIs via ~/.agents/skills sont inertes cote Pyxis.

**Codex.** /home/arthur/dev/codex/codex-rs/core-skills/src/loader.rs:138-142 definit SKILL.md + agents/openai.yaml comme unite de skill; skills/src/model.rs:8-92 porte name/description/short_description/interface/dependencies/policy parses depuis ces fichiers

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:513-528 read_skills() ne fait que read_dir + file_name; `grep -rn "SKILL.md" --include=*.rs crates/` = 0 hit, `grep -rni "frontmatter|serde_yaml" --include=*.rs crates/` = 1 hit non lie

**Statut documentaire.** docs/ROADMAP.md:114 et tasks/prd-pyxis.md:453 placent skills/commands/hooks en Phase 2: report assume, pas rejet

##### Le catalogue de skills n'est jamais expose au modele

`majeur` · `absent` · effort `M`

**Impact.** Le modele ne sait pas quelles skills existent, donc il ne peut jamais en declencher une de lui-meme. Toute la valeur de decouverte automatique par description est perdue.

**Codex.** /home/arthur/dev/codex/codex-rs/core-skills/src/render.rs:18-21 (budget 8000 chars / 2% de fenetre) et :27-45 (intro + regles de declenchement, `$SkillName`, divulgation progressive) rendent un catalogue nom+description+locator dans le contexte du thread

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:22-33 ne construit que le bloc AGENTS.md + <environment>; `grep -rn skills crates/agent-core/src crates/agent-session/src` = 0 hit

**Statut documentaire.** docs/ROADMAP.md:114 (Phase 2)

##### Selectionner un skill insere du texte au lieu d'injecter ses instructions

`majeur` · `divergent` · effort `M`

**Impact.** Taper `/frontend-design fais X` envoie litteralement la chaine `/frontend-design fais X` au modele, sans aucune instruction associee. Le comportement depend entierement de la chance que le modele devine le sens du token.

**Codex.** /home/arthur/dev/codex/codex-rs/core-skills/src/injection.rs:88-110 lit le SKILL.md mentionne et l'injecte; core-skills/src/skill_instructions.rs:31-40 l'encadre en <skill><name><path>...

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1508 (`Menu::Skills => format!("/{} ", item.id)`) et state.rs:360-364 (`un message qui commence par /<skill> n'est PAS une commande -> il part a l'agent`)

##### Aucun moteur de hooks: 11 evenements Codex, 0 cote Pyxis

`majeur` · `absent` · effort `L`

**Impact.** Aucun moyen pour l'utilisateur d'automatiser quoi que ce soit autour de l'agent: pas de format-on-write, pas de garde-fou maison sur les commandes, pas de notification de fin de tour. C'est le principal vecteur d'automatisation utilisateur d'un harness moderne.

**Codex.** /home/arthur/dev/codex/codex-rs/hooks/src/lib.rs:19-32 declare PreToolUse, PermissionRequest, PostToolUse, PreCompact, PostCompact, SessionStart, SessionEnd, UserPromptSubmit, SubagentStart, SubagentStop, Stop; hooks/src/engine/discovery.rs:307-311 charge hooks.json depuis chaque dossier de config

**Pyxis.** `grep -rni hook --include=*.rs crates/agent-tools crates/agent-core crates/agent-cli` = 0 hit; docs/ARCHITECTURE.md:352-360 decrit pourtant `hooks PreToolUse -> permissions -> call() -> taint -> hooks PostToolUse`; tasks/prd-pyxis.md:251 et :395 posent la meme exigence non implementee

**Statut documentaire.** docs/ROADMAP.md:114 et tasks/prd-pyxis.md:453 reportent explicitement les hooks utilisateur en Phase 2

##### MCP configurable mais non branche sur le modele: extensibilite par serveur inoperante

`majeur` · `partial` · effort `M`

**Impact.** Le seul canal d'extension de capacites reellement present dans le code (MCP) ne change rien a ce que l'agent peut faire: `/mcp` liste des outils que le modele ne peut pas appeler.

**Codex.** /home/arthur/dev/codex/codex-rs/plugin/src/manifest.rs:20-31 (mcp_servers dans le manifeste plugin) et cli/src/mcp_cmd.rs pour la gestion; les serveurs MCP fournissent des outils reellement appelables par le modele

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:688 construit le Registry avec seulement Bash/Edit/Glob/Grep/Read/Write; le McpRegistry (main.rs:755) n'est utilise que par les vues d'inspection (crates/agent-cli/src/interactive.rs:1283-1580); docs/CURRENT_STATUS.md:12 le confirme explicitement

**Statut documentaire.** docs/CURRENT_STATUS.md:19 liste `MCP tools in the agent loop` comme differe; docs/ROADMAP.md:86 le place en Phase 2

##### Une seule racine de skills, pas de scopes repo/user/system/admin

`moyen` · `partial` · effort `S`

**Impact.** Un depot ne peut pas embarquer ses propres skills versionnees avec le code, et une organisation ne peut rien imposer. Les skills sont forcement globales a la machine.

**Codex.** /home/arthur/dev/codex/codex-rs/core-skills/src/loader.rs:291-375 derive des racines Repo (config projet), User ($CODEX_HOME/skills et $HOME/.agents/skills), System (cache embarque) et Admin (/etc/codex/skills); loader.rs:376-420 ajoute les .agents/skills remontes entre racine projet et cwd

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:517 unique racine `home.join(".agents").join("skills")`, aucune racine projet ni systeme

##### 12 slash commands contre ~65 chez Codex

`moyen` · `partial` · effort `L`

**Impact.** Manques a fort impact quotidien: pas de /init pour amorcer un AGENTS.md, pas de /compact manuel alors que la compaction existe (crates/agent-core/src/compaction.rs:78-103), pas de /diff ni de /status pour inspecter l'etat, pas de /review. L'utilisateur doit sortir de l'outil pour chacune de ces operations.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/slash_command.rs:12-79 enumere ~65 variantes; absentes cote Pyxis et pertinentes hors infra OpenAI: /init (slash_dispatch.rs:252), /compact (:256), /review (:267), /diff, /status, /usage, /copy, /raw, /mention, /rename, /fork, /archive, /delete, /theme, /keymap, /vim, /statusline, /title, /skills en mode toggle, /hooks (:432), /plan, /side, /ps, /stop, /memories, /experimental

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58-75 liste exactement /help, /models, /effort, /permissions, /skills, /goal, /providers, /mcp, /resume, /new, /clear, /quit; dispatch a crates/agent-cli/src/interactive.rs:682-1060

**Statut documentaire.** tasks/prd-codex-tui-parity.md:375 exige seulement la preservation des commandes existantes, pas l'ajout des commandes Codex manquantes

##### Le rendu de hooks est deja porte mais aucun evenement ne l'alimente

`mineur` · `partial` · effort `S`

**Impact.** Surface morte assumee: cout de maintenance sans valeur utilisateur tant que le moteur n'existe pas, mais elle prouve que le chemin d'affichage est deja pret, donc l'ecart residuel est cote coeur, pas cote TUI.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/hooks_browser_view.rs:44-50 et tui/src/chatwidget/slash_dispatch.rs:432-434 (/hooks) rendent des hooks reellement executes par hooks/src/engine/dispatcher.rs

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/history_cell.rs:1522-1531 (HookCell) et crates/agent-tui/src/app_event.rs:285-290 (TranscriptPayload::HookRun) existent, mais les seules constructions sont des tests (history_cell.rs:4892-4910, :5202-5206); docs/codex-port-inventory.md:64 classe l'emission runtime en `skip`

**Statut documentaire.** docs/codex-port-inventory.md:64 documente explicitement le choix de porter le rendu sans l'emission

#### Écarts discutables

##### Pas de contrat d'entree/sortie permettant a un hook de bloquer ou reecrire une action

`moyen` · `absent` · effort `L`

**Impact.** Meme si les hooks existaient en simple notification, sans pouvoir de blocage ni de reecriture ils ne permettent pas d'imposer une politique locale (interdire un chemin, forcer un flag, reecrire une commande dangereuse).

**Codex.** /home/arthur/dev/codex/codex-rs/hooks/src/events/pre_tool_use.rs:38-45 expose should_block, block_reason, updated_input et additional_contexts; hooks/src/schema.rs:240-273 definit le wire PreToolUseHookSpecificOutput avec decisions allow/deny/ask; hooks/src/events/permission_request.rs pilote la decision d'approbation elle-meme

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/permission.rs ne consulte que les modes de permission internes; aucune source externe ne peut refuser ou modifier un appel d'outil (`grep -rn "should_block|updated_input" crates/` = 0 hit)

##### Pas d'activation/desactivation persistante des skills

`mineur` · `absent` · effort `S`

**Impact.** Impossible de museler une skill bruyante. Consequence limitee tant que rien n'est injecte, mais bloquante des que le catalogue le sera (budget de contexte).

**Codex.** /home/arthur/dev/codex/codex-rs/skills/src/model.rs:100-109 (SkillConfigRule/SkillConfigRules) et tui/src/bottom_pane/skills_toggle_view.rs:37-43 (SkillsToggleItem.enabled) exposent un toggle persiste via /skills

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:34-68: ~/.pyxis/settings.toml ne persiste que permission_mode, reasoning_effort et model; aucune cle skills

##### Pas de detection d'invocation implicite de skill

`mineur` · `absent` · effort `M`

**Impact.** Surtout de l'observabilite et du controle de politique. Faible impact utilisateur direct pour un outil mono-utilisateur.

**Codex.** /home/arthur/dev/codex/codex-rs/core-skills/src/invocation_utils.rs:31-44 detecte qu'une commande shell lit un SKILL.md ou lance un script de scripts/ pour attribuer l'usage; skills/src/model.rs:62-67 (SkillPolicy.allow_implicit_invocation) permet de l'interdire

**Pyxis.** aucun equivalent: `grep -rni "implicit" --include=*.rs crates/` sans resultat lie aux skills; crates/agent-tools/src/bash.rs ne connait pas la notion de skill

##### Aucun format de plugin empaquetant skills, hooks et serveurs MCP

`mineur` · `absent` · effort `XL`

**Impact.** Aucun moyen de distribuer d'un coup un bundle coherent (instructions + serveur MCP + automatisation). Chaque brique doit etre installee a la main dans un endroit different.

**Codex.** /home/arthur/dev/codex/codex-rs/plugin/src/manifest.rs:8-38 (PluginManifest: paths.skills, paths.mcp_servers, paths.apps, paths.hooks) et utils/plugins/src/plugin_namespace.rs:10-13 (plugin.json, schema versionne)

**Pyxis.** `grep -rni plugin --include=*.rs /home/arthur/dev/pyxis/crates/` = 0 hit; aucune mention de plugin dans docs/ROADMAP.md, docs/DECISIONS.md ni tasks/*.md

**Statut documentaire.** aucun ADR ne traite les plugins: ni adopte ni rejete, c'est un angle mort du registre de decisions

##### Metadonnees de disponibilite des commandes plus pauvres

`mineur` · `partial` · effort `S`

**Impact.** Le menu propose des commandes qui echoueront pendant un tour, et la regle de disponibilite est dupliquee entre la table et le match: source de divergence quand la liste grandira.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/slash_command.rs:153-173 (supports_inline_args), :176-187 (available_in_side_conversation), :190-246 (available_during_task pour chaque commande), :248-256 (is_visible par OS/build), et tui/src/bottom_pane/slash_commands.rs:70-82 (feature-gating dynamique)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58 declare un seul booleen takes_arg par commande; la disponibilite pendant un tour est codee ad hoc dans le dispatch (crates/agent-cli/src/interactive.rs:819-820 pour /goal, :864-868 pour /resume|/new|/clear)

##### Pas de sigil de mention pour outils et skills dans le composer

`mineur` · `absent` · effort `S`

**Impact.** Reutiliser `/` pour deux semantiques distinctes (commande executee vs texte envoye au modele) est ambigu, et le code doit d'ailleurs l'expliciter (state.rs:360-364). Un sigil dedie leverait l'ambiguite et ouvrirait la mention explicite d'outils.

**Codex.** /home/arthur/dev/codex/codex-rs/utils/plugins/src/mention_syntax.rs:4-7 definit `$` pour outils/skills et `@` pour fichiers/plugins; tui/src/bottom_pane/skill_popup.rs:20-29 fournit le popup de mention avec fuzzy match, description, categorie et rang de tri

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1478-1491 n'implemente que `@` pour les fichiers; le sous-menu skills passe par `/skills ` puis insere `/<nom> ` (state.rs:1295-1298, :1508), ce qui collisionne visuellement avec la syntaxe des commandes

#### Non applicables à Pyxis

- **Pas de modele de confiance des hooks** (mineur) : Devient pertinent seulement le jour ou Pyxis chargera des hooks depuis un depot clone: executer du code arbitraire declare dans un repo est un vecteur d'attaque direct.
- **Pas de marketplace ni de commande d'installation d'extensions** (mineur) : Distribution d'extensions inexistante. Pour un projet Linux-first mono-utilisateur sans canal de release avant la Phase 3 (docs/ROADMAP.md:94), c'est coherent avec le scope actuel.

#### Écarts réfutés en vérification

- **Slash commands utilisateur par fichiers de prompt: absentes des deux cotes** : Ce n'est pas un ecart Pyxis vs Codex: la capacite est absente des DEUX cotes, l'auditeur le dit lui-meme. Verifie cote Codex: `grep -rn "custom_prompts|CUSTOM_PROMPTS" --include=*.rs /home/arthur/dev/codex/codex-rs` = 0 hit. Verifie cote Pyxis: /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs:11-14 (GPT5_GENERIC et CODEX_FINETUNED en include_str!) et prompt.rs:22-28 select_system_prompt(). Un item symetrique ne peut pas figurer dans un inventaire d'ecarts -> refute comme ecart.
- **Commandes Codex liees a l'infra ou aux plateformes OpenAI** : Refute comme ecart: l'auditeur conclut lui-meme "aucun equivalent attendu". Les commandes citees relevent de l'infra OpenAI (/apps connectors, /feedback, /import Claude Code, codex cloud), du support macOS/Windows (/app gate a codex-rs/tui/src/slash_command.rs:252) ou des sous-agents (/agent, /subagents, enum a slash_command.rs:38 et :72-74), tous hors scope declare: /home/arthur/dev/pyxis/docs/ROADMAP.md:96 (Linux uniquement, distribution Phase 3), ROADMAP.md:87-92 (mono-provider, sous-agents exclus), tasks/prd-pyxis.md:453 (sous-agents/teams Phase 2). Aucune action a tirer -> non-applicable.

### MCP

**Parité estimée : minimal**

*Surface Codex.* Codex traite MCP comme un sous-système de premier plan réparti sur ~34k lignes: `codex-rs/rmcp-client/` (13.5k, transports + OAuth), `codex-rs/codex-mcp/` (12.8k, connection manager, catalogue d'outils, elicitation), `codex-rs/mcp-server/` (3.5k, Codex exposé EN TANT QUE serveur MCP) et `codex-rs/connectors/` (4.8k, annuaire ChatGPT Apps, infra OpenAI). Deux transports client: stdio (`codex-rs/rmcp-client/src/rmcp_client.rs:356`, avec launcher local ou via exec-server) et Streamable HTTP (`rmcp_client.rs:391`); SSE n'est pas un transport autonome mais le mode de réponse du Streamable HTTP (`codex-rs/rmcp-client/src/http_client_adapter.rs:213`, erreur `ServerDoesNotSupportSse` en `:345`). La config par serveur est très riche (`codex-rs/config/src/mcp_types.rs:156-223`): `auth` (oauth|chatgpt), `environment_id`, `enabled`, `required`, `supports_parallel_tool_calls`, `startup_timeout_sec`, `tool_timeout_sec`, `default_tools_approval_mode`, `enabled_tools`/`disabled_tools`, `scopes`, `oauth.client_id`, `oauth_resource` (RFC 8707), et `tools` (approbation par outil). OAuth complet et persistant: `codex-rs/rmcp-client/src/oauth.rs` (1458 l.), `perform_oauth_login.rs` (944 l.), store keyring avec verrou et transaction de refresh (`oauth/store_lock.rs`, `oauth/refresh_transaction.rs`), découverte du statut d'auth (`auth_status.rs:1-535`). L'exposition au modèle passe par une normalisation dédiée (`codex-rs/codex-mcp/src/tools.rs:113-214`): sanitisation Responses API, préfixe historique `mcp__` optionnel, namespace + nom séparés, dédoublonnage par suffixe SHA1 12 chars, plafond 64 octets, plus un filtre allowlist/denylist (`tools.rs:66-103`). Chaque outil MCP devient un handler du routeur d'outils (`codex-rs/core/src/tools/handlers/mcp.rs:32-120`), avec hooks pre/post tool use, parallélisme conditionné au `readOnlyHint`, et une politique d'approbation dérivée des annotations MCP croisée avec 4 modes (`codex-rs/core/src/mcp_tool_call.rs:2156-2187`). Timeouts par défaut 30s startup / 300s appel (`codex-rs/codex-mcp/src/rmcp_client.rs:88-89`), troncature des résultats pour les événements (`mcp_tool_call.rs:106,850-890`) et filtrage des contenus image selon les modalités du modèle (`mcp_tool_call.rs:815-830`). Les ressources MCP sont elles aussi des outils modèle (`codex-rs/core/src/tools/handlers/mcp_resource.rs:30-35`: list_mcp_resources, list_mcp_resource_templates, read_mcp_resource) au-dessus de `rmcp_client.rs:557-603`. L'elicitation serveur→client est implémentée et arbitrée par le guardian (`codex-rs/rmcp-client/src/elicitation_client_service.rs`, `codex-rs/codex-mcp/src/elicitation.rs`, `codex-rs/core/src/session/mcp.rs:381-505`), les notifications progress/logging sont consommées (`codex-rs/rmcp-client/src/logging_client_handler.rs:63,98`) et il existe une échappatoire notification/requête custom (`rmcp_client.rs:663,693`). Côté gestion: sous-commande `codex mcp list|get|add|remove|login|logout` (`codex-rs/cli/src/mcp_cmd.rs:54-169`), surface app-server (`codex-rs/app-server-protocol/src/protocol/common.rs:993-1017,1519,1702-1704`: oauth/login, reload, status list, resource/read, tool/call, elicitation, progress), serveurs MCP fournis par plugins via `.mcp.json` (`codex-rs/codex-mcp/src/plugin_config.rs:37-49`) et désactivation par requirements admin (`codex-rs/config/src/mcp_requirements.rs`, raison exposée en `mcp_types.rs:36-52`). Enfin Codex est lui-même un serveur MCP stdio exposant les outils `codex` et `codex-reply` (`codex-rs/mcp-server/src/lib.rs:1-58`, `codex_tool_config.rs:106-118`), avec approbations exec/patch via elicitation et gestion de `prompts/list` / `prompts/get` (`codex-rs/mcp-server/src/message_processor.rs:319-323`).

*Surface Pyxis.* Pyxis a un crate `crates/agent-mcp/` de 711 lignes de source (client 164, config 300, server 225, error 22) qui couvre exactement trois choses: parser la config, spawner un serveur stdio, lister ses outils. Le transport est stdio uniquement via `TokioChildProcess` (`crates/agent-mcp/src/client.rs:95`), avec handshake `initialize` borné à 30s et `list_tools` borné à 10s (`client.rs:41-42`), descriptions plafonnées à 2048 chars (`client.rs:46`). Le handler client rmcp est le type unité `()` (`client.rs:51,103`), donc aucune notification, elicitation, sampling ou roots n'est traité. La config lit `.mcp.json` du workspace puis les `mcpServers` de `~/.claude.json` en user-scope, avec fusion priorisée et diagnostics de shadowing (`crates/agent-mcp/src/config.rs:147-177`); toute entrée sans `command` est classée `UnsupportedTransport` et conservée en diagnostic (`config.rs:219-225`), tout comme `disabled`, commande vide et entrée invalide. Le registre est un enum d'état discriminé Disconnected/Connecting/Connected/Failed où la connexion n'existe que dans `Connected` (`crates/agent-mcp/src/server.rs:12-28`), avec transitions synchrones et récupération de l'ancienne connexion au reconnect. Côté UI, `/mcp` offre list/connect/reconnect/trust/disconnect/tools/issues (`crates/agent-cli/src/interactive.rs:1306-1387`), mais l'ensemble des actions de connexion est verrouillé derrière la variable d'environnement `PYXIS_EXPERIMENTAL_MCP_CONNECT` (`interactive.rs:1390-1392`). Point tranché sans ambiguïté: **aucun outil MCP n'est exposé à la boucle du modèle**. Le grep `call_tool|CallTool` ne renvoie rien sur tout le dépôt Pyxis; `McpConnection` n'expose que `connect`, `connect_hardened`, `list_tools`, `cancel` (`client.rs:69-155`); `McpToolInfo` conserve les schémas explicitement pour « une future exposition modèle via adapter strict » (`client.rs:54-56`); `Registry::register_dyn` existe avec le commentaire « futur outil MCP » (`crates/agent-tools/src/registry.rs:432-433`) et n'est appelé nulle part. La TUI contient même des cellules `McpToolCell`/`McpInvocation` portées de Codex (`crates/agent-tui/src/history_cell.rs:1929-1996,3512`) qu'aucun émetteur d'événement n'alimente. Les docs sont honnêtes sur cet état: `docs/CURRENT_STATUS.md:12` (« MCP tools are not yet exposed as callable model tools »), `docs/ARCHITECTURE.md:406,423-424`, `docs/ROADMAP.md:86,113`, `tasks/prd-codex-orchestration.md:345`. Deux points où Pyxis fait mieux que Codex: un gate de confiance pré-spawn qui bloque tout serveur d'origine workspace, tout serveur qui masque une config user, ou tout serveur injectant des variables d'env sensibles (PATH, LD_PRELOAD, NODE_OPTIONS, PYTHONPATH…), avec affichage de la commande, des args et des clés d'env avant confirmation, plus une re-vérification TOCTOU de la config au moment du spawn (`interactive.rs:1312-1321,1408-1424,1510-1538`); et un durcissement du sous-process réutilisant le `CommandHardener` des outils Bash avec filtrage des clés proxy pour éviter les bypass via `NO_PROXY`/`ALL_PROXY` (`client.rs:79-94,166-171`).

#### Écarts pertinents

##### Les outils MCP ne sont jamais exposés au modèle: MCP est purement diagnostic

`bloquant` · `absent` · effort `XL`

**Impact.** Un utilisateur qui configure un serveur MCP dans Pyxis voit la liste de ses outils dans `/mcp <srv> tools` mais le modèle ne peut en invoquer aucun. MCP est donc, en pratique, non fonctionnel: c'est un inspecteur de serveur, pas une intégration. C'est l'écart structurant de toute cette dimension: presque tous les autres écarts en découlent.

**Codex.** codex-rs/core/src/tools/handlers/mcp.rs:32-120 (McpHandler implémente le trait outil, spec/handle/parallélisme/hooks) et codex-rs/core/src/mcp_tool_call.rs:110 (handle_mcp_tool_call): chaque outil MCP devient un outil appelable du routeur, avec appel réel via codex-rs/codex-mcp/src/connection_manager.rs:612 (client.call_tool)

**Pyxis.** Aucun `call_tool` dans tout le dépôt Pyxis (grep -rn "call_tool\|CallTool" --include=*.rs → 0 résultat). crates/agent-mcp/src/client.rs:69-155 n'expose que connect/connect_hardened/list_tools/cancel. crates/agent-tools/src/registry.rs:432-433 a un `register_dyn` commenté « futur outil MCP » jamais appelé. docs/CURRENT_STATUS.md:12 le confirme explicitement.

**Statut documentaire.** Déjà connu et planifié: docs/CURRENT_STATUS.md:19 et docs/ROADMAP.md:86,113 le mettent en Phase 2; tasks/prd-pyxis.md:441 le classe risque n°6 « MCP absent au MVP (table-stake 2026) », bloquant pour la promo publique mais pas pour le dogfood. tasks/prd-codex-orchestration.md:345 l'exclut explicitement de son scope.

##### Toute connexion MCP est verrouillée derrière une variable d'environnement expérimentale, sans auto-connexion au démarrage

`moyen` · `divergent` · effort `M`

**Impact.** Même si les outils étaient câblés, aucun serveur ne serait connecté au début d'un tour: le modèle n'aurait jamais de tools MCP disponibles sans action manuelle de l'utilisateur avant chaque session. En headless, MCP est totalement inexistant.

**Codex.** codex-rs/codex-mcp/src/connection_manager.rs:157-460: le connection manager démarre tous les serveurs `enabled` au lancement de la session, avec statut de startup émis en événement, `required_servers` (codex-rs/codex-mcp/src/connection_manager.rs:194-199) faisant échouer `codex exec` si un serveur requis ne démarre pas (codex-rs/config/src/mcp_types.rs:172-174), et reconnexion des échecs (connection_manager.rs:452).

**Pyxis.** crates/agent-cli/src/interactive.rs:1390-1392: `fn mcp_connect_enabled()` exige `PYXIS_EXPERIMENTAL_MCP_CONNECT`; sans elle `/mcp <srv> connect` et `trust` renvoient MCP_DISABLED_NOTICE (interactive.rs:1308-1310,1325-1327). crates/agent-cli/src/main.rs:754-756 construit le registre en Disconnected pur, « la connexion se fait à la demande via /mcp ». En mode headless `-p` la config MCP n'est même pas lue (main.rs:339-343).

**Statut documentaire.** docs/CURRENT_STATUS.md:19 liste « stable connect UX » comme travail à venir, sans ADR justifiant le flag.

##### Aucun transport distant: seul stdio est supporté, les entrées HTTP sont rejetées

`moyen` · `absent` · effort `L`

**Impact.** Tous les serveurs MCP hébergés (Notion, Linear, Sentry, GitHub remote, Figma…) sont inaccessibles. En 2026 la majorité des serveurs MCP grand public sont distribués en Streamable HTTP, pas en binaire local.

**Codex.** codex-rs/config/src/mcp_types.rs:433-463 définit McpServerTransportConfig::Stdio et ::StreamableHttp (url, bearer_token_env_var, http_headers, env_http_headers); codex-rs/rmcp-client/src/rmcp_client.rs:391-428 (new_streamable_http_client) et codex-rs/rmcp-client/src/http_client_adapter.rs:213,345 gèrent la réponse SSE et la retro-compat des serveurs sans SSE, avec reprise de session (rmcp-client/src/streamable_http_retry.rs:1-251).

**Pyxis.** crates/agent-mcp/src/client.rs:95 utilise exclusivement `TokioChildProcess`; crates/agent-mcp/src/config.rs:219-225: toute entrée sans champ `command` est classée `McpConfigIssueKind::UnsupportedTransport` et écartée. crates/agent-mcp/src/lib.rs:7-8 le note comme reporté.

**Statut documentaire.** docs/ROADMAP.md:113 mentionne l'ambition MCP complète mais ne tranche pas les transports; aucun ADR ne rejette HTTP.

##### Pas d'OAuth par serveur MCP (ni bearer token, ni headers, ni stockage de credentials)

`moyen` · `absent` · effort `XL`

**Impact.** Sans OAuth, aucun serveur MCP distant authentifié n'est utilisable, y compris ceux que l'utilisateur possède déjà via Claude Code. Corollaire direct de l'absence de transport HTTP.

**Codex.** codex-rs/rmcp-client/src/oauth.rs (1458 l.) + perform_oauth_login.rs (944 l.): flow OAuth complet, découverte du serveur d'autorisation via WWW-Authenticate (rmcp-client/src/http_client_adapter/www_authenticate.rs:1-233), paramètre `resource` RFC 8707 (codex-rs/config/src/mcp_types.rs:216-218), scopes configurables (mcp_types.rs:208-210), stockage keyring verrouillé et refresh transactionnel (rmcp-client/src/oauth/store_lock.rs, oauth/refresh_transaction.rs), CLI `codex mcp login/logout` (codex-rs/cli/src/mcp_cmd.rs:158-169).

**Pyxis.** crates/agent-mcp/src/config.rs:109-120: McpServerConfig ne porte que command/args/env; aucune occurrence de `oauth`, `bearer`, `token`, `Authorization` dans crates/agent-mcp/. crates/agent-mcp/src/lib.rs:7-8 déclare l'OAuth PKCE par serveur explicitement reporté.

**Statut documentaire.** Explicitement reporté et documenté: docs/ROADMAP.md:74,78 (« OAuth PKCE par serveur MCP » = travail multi-serveur reporté après l'OAuth mono-provider ChatGPT) et docs/ROADMAP.md:88,113. Écart connu, pas un oubli.

##### Aucune normalisation des noms d'outils MCP pour l'API modèle (namespacing, sanitisation, dédoublonnage, plafond 64 octets)

`moyen` · `absent` · effort `M`

**Impact.** Prérequis technique à toute exposition modèle: sans namespacing, deux serveurs MCP exposant un outil homonyme (cas ultra-fréquent: `search`, `read`, `list`) se collisionnent, et un nom > 64 octets ou contenant des caractères non autorisés fait échouer la requête Responses API entière, pas seulement l'outil fautif.

**Codex.** codex-rs/codex-mcp/src/tools.rs:113-214 (normalize_tools_for_model_with_prefix): sanitisation Responses API, préfixe `mcp__` (tools.rs:22,228-234), séparation namespace/nom (tools.rs:58-60), détection de collisions namespace et outil avec suffixe SHA1 12 chars (tools.rs:153-195,243-263), plafond MAX_TOOL_NAME_LENGTH=64 avec troncature intelligente (tools.rs:226,269-315).

**Pyxis.** crates/agent-mcp/src/client.rs:128-144: `name` est copié tel quel depuis `original_name` sans préfixe, sans sanitisation, sans détection de collision inter-serveurs; le registre Pyxis est indexé par nom brut (crates/agent-tools/src/registry.rs:41) où deux serveurs exposant `search` s'écraseraient. crates/agent-tui/src/tool.rs:239 montre que la TUI attend pourtant déjà la forme `mcp__srv__do`.

**Statut documentaire.** docs/ARCHITECTURE.md:320 pose l'uniformité DynTool comme contrat cible mais ne traite pas le nommage ni les collisions.

##### Aucune politique d'approbation pour les appels d'outils MCP (modes, annotations, mémorisation)

`moyen` · `absent` · effort `M`

**Impact.** Quand les outils MCP seront câblés, un serveur pourra exécuter des actions destructives ou réseau sans point de contrôle utilisateur, alors que le pipeline Bash/Edit de Pyxis a le sien. C'est le trou de sécurité le plus direct dans le futur câblage.

**Codex.** codex-rs/core/src/mcp_tool_call.rs:2156-2187: `requires_mcp_tool_approval` dérive l'approbation des annotations MCP (destructiveHint, readOnlyHint, openWorldHint, avec défaut fail-closed à `true`), croisé avec 4 modes AppToolApproval (Auto/Prompt/Writes/Approve) définis en codex-rs/config/src/mcp_types.rs:19-27, configurables par serveur (`default_tools_approval_mode`, mcp_types.rs:196-198) et par outil (`tools`, mcp_types.rs:220-222); mémorisation session/persistante des décisions en mcp_tool_call.rs:1455.

**Pyxis.** Aucun code d'approbation MCP: crates/agent-tools/src/permission.rs ne référence pas MCP (grep « mcp » → 0). crates/agent-mcp/src/client.rs:64-66 capture bien les annotations mais les réduit à un booléen `annotations_untrusted` jamais consommé ailleurs (grep annotations_untrusted → seule définition). Le gate de confiance existant (crates/agent-cli/src/interactive.rs:1510-1538) porte sur le spawn du serveur, pas sur l'appel d'outil.

**Statut documentaire.** docs/ARCHITECTURE.md:424 promet `returns_untrusted() == true` pour tous les outils MCP et le taint §4.6 (DECISIONS.md:204, risque R5), mais rien n'est implémenté: le taint est une défense post-exécution, pas une politique d'approbation pré-exécution.

##### Pas de filtrage des outils exposés par serveur (enabled_tools / disabled_tools)

`mineur` · `absent` · effort `S`

**Impact.** Un serveur MCP verbeux (certains exposent 40+ outils) noierait le prompt système et le budget de tokens sans possibilité de restreindre. C'est aussi le levier de réduction de surface d'attaque le plus simple face à un serveur partiellement fiable.

**Codex.** codex-rs/codex-mcp/src/tools.rs:66-103 (ToolFilter::from_config, filter_tools) appliquant `enabled_tools` (allowlist) puis `disabled_tools` (denylist) définis en codex-rs/config/src/mcp_types.rs:200-206.

**Pyxis.** crates/agent-mcp/src/config.rs:109-120: McpServerConfig n'a aucun champ de filtrage; crates/agent-mcp/src/client.rs:128-144 retourne intégralement `list_all_tools()` sans filtre. Grep `enabled_tools|disabled_tools|allowlist` dans crates/agent-mcp/ → 0.

**Statut documentaire.** Aucun ADR Pyxis sur ce point; le cap de description 2048 chars (client.rs:46) traite le volume par outil, pas le nombre d'outils.

##### Timeouts MCP codés en dur et pas de timeout d'appel d'outil

`mineur` · `partial` · effort `S`

**Impact.** Un serveur lent à démarrer (npx/uvx qui télécharge son paquet au premier lancement dépasse couramment 30s) échoue sans recours. Le `LIST_TOOLS_TIMEOUT` de 10s est particulièrement serré pour un serveur distant.

**Codex.** codex-rs/codex-mcp/src/rmcp_client.rs:88-89: DEFAULT_STARTUP_TIMEOUT 30s et DEFAULT_TOOL_TIMEOUT 300s, tous deux surchargeables par serveur via `startup_timeout_sec`/`startup_timeout_ms` et `tool_timeout_sec` (codex-rs/config/src/mcp_types.rs:184-194), le timeout d'appel étant propagé jusqu'à call_tool (codex-rs/codex-mcp/src/connection_manager.rs:239-241,612).

**Pyxis.** crates/agent-mcp/src/client.rs:41-42: CONNECT_TIMEOUT 30s et LIST_TOOLS_TIMEOUT 10s en `const`, non configurables; aucun timeout d'appel d'outil puisqu'il n'y a pas d'appel. crates/agent-mcp/src/config.rs:109-120 n'expose aucun champ timeout.

##### Ressources MCP non exposées (list/templates/read)

`mineur` · `absent` · effort `M`

**Impact.** Les serveurs MCP orientés documentation/contexte (bases de connaissances, wikis, schémas) exposent leur valeur via `resources`, pas via `tools`. Pyxis serait aveugle à cette moitié du protocole même après le câblage des tools.

**Codex.** codex-rs/core/src/tools/handlers/mcp_resource.rs:30-35 déclare ListMcpResourcesHandler, ListMcpResourceTemplatesHandler, ReadMcpResourceHandler comme outils modèle de plein droit, avec restriction d'accès par serveur (codex-rs/core/src/session/mcp.rs:37-53) au-dessus de codex-rs/rmcp-client/src/rmcp_client.rs:557-603 (list_resources, list_resource_templates, read_resource).

**Pyxis.** Grep `resource|Resource` dans crates/agent-mcp/ → 0 résultat hors `RunningService`. crates/agent-mcp/src/client.rs n'appelle que `list_all_tools()` (client.rs:118).

**Statut documentaire.** docs/ARCHITECTURE.md:404-424 ne mentionne que les tools MCP; les resources ne sont ni prévues ni rejetées.

##### Elicitation serveur→client non supportée

`mineur` · `absent` · effort `L`

**Impact.** Les serveurs MCP modernes utilisent l'elicitation pour demander une confirmation ou un paramètre manquant en cours d'appel; sans elle, ces serveurs échouent ou se dégradent silencieusement.

**Codex.** codex-rs/rmcp-client/src/elicitation_client_service.rs:1-320 et codex-rs/codex-mcp/src/elicitation.rs:1-342 implémentent la boucle d'elicitation; l'arbitrage passe par le guardian et l'UI (codex-rs/core/src/session/mcp.rs:381-505), avec exposition protocolaire `mcpServer/elicitation/request` (codex-rs/app-server-protocol/src/protocol/common.rs:1519) et auth-elicitation dédiée (codex-rs/codex-mcp/src/auth_elicitation.rs:1-347).

**Pyxis.** crates/agent-mcp/src/client.rs:51,103: le handler client rmcp est le type unité `()`, donc l'implémentation par défaut décline toute elicitation. Grep `elicit` dans crates/ → 0 résultat côté code (seule tasks/prd-codex-tui-parity.md:321 l'évoque comme cible UI future).

**Statut documentaire.** tasks/prd-codex-tui-parity.md:321 prévoit une overlay d'approbation unifiée incluant « MCP elicitation », donc le besoin est identifié côté UI mais rien n'existe côté protocole.

##### Notifications MCP (progress, logging, tools/list_changed) non consommées

`mineur` · `absent` · effort `M`

**Impact.** Un appel MCP long (indexation, recherche distante) n'affichera aucune progression, et un serveur qui modifie dynamiquement sa liste d'outils ne sera jamais rafraîchi: la liste capturée au handshake reste figée jusqu'à un reconnect manuel.

**Codex.** codex-rs/rmcp-client/src/logging_client_handler.rs:63 (on_progress: ProgressNotificationParam) et :98 (on_logging_message: LoggingMessageNotificationParam) implémentent le ClientHandler; les progrès d'appel remontent jusqu'au client via `item/mcpToolCall/progress` (codex-rs/app-server-protocol/src/protocol/common.rs:1702), et il existe une échappatoire notification/requête custom (codex-rs/rmcp-client/src/rmcp_client.rs:663,693).

**Pyxis.** crates/agent-mcp/src/client.rs:51 `RunningService<RoleClient, ()>`: le handler client est `()`, aucune méthode de notification n'est implémentée. Grep `notification|on_progress|list_changed` dans crates/agent-mcp/ → 0.

##### Pas de gestion des serveurs MCP en CLI (add/get/remove), seulement l'édition manuelle du JSON

`mineur` · `absent` · effort `S`

**Impact.** Ajouter un serveur impose d'éditer le JSON à la main hors de l'outil, et il n'y a aucune sortie machine-lisible pour scripter la config. Impact réel modéré: le format `.mcp.json` compatible Claude Code signifie qu'un utilisateur a souvent déjà sa config.

**Codex.** codex-rs/cli/src/mcp_cmd.rs:54-169: sous-commandes list/get/add/remove/login/logout, avec sortie `--json`, args typés par transport (AddMcpStdioArgs en :108, AddMcpStreamableHttpArgs en :128) et écriture dans config.toml via codex-rs/config/src/mcp_edit.rs.

**Pyxis.** crates/agent-cli/src/main.rs:388-410 ne fait que lire `.mcp.json` et `~/.claude.json`; aucune écriture. La seule surface est le slash `/mcp` (crates/agent-tui/src/state.rs:70) en lecture + connexion. Grep `mcp` dans les définitions d'args CLI de main.rs → uniquement `read_mcp_config`.

##### Champs de config par serveur manquants: enabled explicite, required, supports_parallel_tool_calls, cwd, env_vars hérités

`mineur` · `partial` · effort `S`

**Impact.** Sans `env_vars` héritables, une clé d'API doit être écrite en clair dans `.mcp.json` (souvent versionné) au lieu d'être référencée depuis l'environnement: c'est un problème de secret, pas seulement d'ergonomie. Sans `cwd`, un serveur qui résout des chemins relatifs se comporte de façon imprévisible.

**Codex.** codex-rs/config/src/mcp_types.rs:168-178 (`enabled`, `required`, `supports_parallel_tool_calls`) et :249-258 (`cwd`, `env_vars` avec source local|remote, cf. mcp_types.rs:63-101); le parallélisme croise ce flag avec le readOnlyHint dans codex-rs/core/src/tools/handlers/mcp.rs:76.

**Pyxis.** crates/agent-mcp/src/config.rs:109-120: seuls command/args/env existent. `disabled` est bien reconnu mais uniquement pour produire un diagnostic d'exclusion (config.rs:212-218), il n'y a pas de notion de serveur requis ni de cwd, et `env` est un dictionnaire littéral sans héritage depuis l'environnement hôte (client.rs:89-94).

**Statut documentaire.** À rapprocher du gate de confiance existant (crates/agent-cli/src/interactive.rs:1516-1537) qui détecte déjà les clés d'env sensibles: la brique de conscience des secrets existe, le mécanisme d'héritage manque.

##### Pas de rafraîchissement de la config MCP à chaud ni de surface d'état structurée

`mineur` · `partial` · effort `S`

**Impact.** Éditer `.mcp.json` impose de redémarrer Pyxis. Impact limité tant que MCP reste diagnostic, mais il devient gênant dès que les outils sont dans la boucle (ajouter un serveur en cours de session est un geste courant).

**Codex.** codex-rs/app-server-protocol/src/protocol/common.rs:999-1007 (`config/mcpServer/reload`, `mcpServerStatus/list`) et :1704 (notification `mcpServer/startupStatus/updated`); côté core, le rafraîchissement à chaud est piloté par un flag dirty (codex-rs/core/src/session/mcp.rs:135 refresh_mcp_if_dirty, :254 mark_mcp_runtime_dirty, :507 refresh_mcp_servers_now).

**Pyxis.** crates/agent-cli/src/main.rs:340-343: la config MCP est lue une seule fois au démarrage, avant le sandbox, et n'est jamais rechargée. crates/agent-cli/src/interactive.rs:1479-1508 fournit `/mcp issues` (diagnostics texte) et interactive.rs:463 recalcule `mcp_metas` en mémoire, mais rien ne relit le fichier.

**Statut documentaire.** L'API de diagnostic Pyxis (McpConfigIssue, crates/agent-mcp/src/config.rs:60-106) est plus riche et plus explicite que ce que Codex expose côté raisons de désactivation (codex-rs/config/src/mcp_types.rs:36-52): c'est un point où Pyxis est en avance.

#### Écarts discutables

##### Aucune politique de troncature ni de gestion des contenus image/structurés dans les résultats MCP

`mineur` · `absent` · effort `S`

**Impact.** Un serveur MCP hostile ou simplement bavard peut retourner des mégaoctets et faire exploser le contexte et le coût du tour. À câbler en même temps que l'appel d'outils, sinon le premier serveur verbeux fera sauter la fenêtre.

**Codex.** codex-rs/core/src/mcp_tool_call.rs:106 (MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES) et :850-890 (truncate_mcp_tool_result_for_event) plafonnent les résultats; :815-830 remplacent les contenus image par un placeholder quand le modèle ne supporte pas la modalité, et :844 propage `structured_content`.

**Pyxis.** Rien à tronquer côté Pyxis puisqu'aucun appel n'existe (crates/agent-mcp/src/client.rs:69-155). Le seul plafond MCP existant est DESCRIPTION_CAP=2048 sur les descriptions d'outils (client.rs:46,136), pas sur les résultats. Pyxis a bien une politique de troncature d'outils natifs (crates/agent-tools/) mais rien de spécifique MCP.

**Statut documentaire.** tasks/prd-pyxis.md:16 identifie les « coûts runaway » comme mode d'échec majeur, ce qui rend l'absence de plafond MCP cohérente à corriger au moment du câblage.

##### Pyxis ne peut pas être exposé comme serveur MCP

`mineur` · `absent` · effort `L`

**Impact.** Impossible d'orchestrer Pyxis depuis un autre agent ou un IDE via MCP. À nuancer fortement: Pyxis est embarqué dans Paneflow et n'a pas de surface app-server, donc l'orchestration passe par d'autres voies.

**Codex.** codex-rs/mcp-server/src/lib.rs:1-58 est un serveur MCP stdio complet; codex-rs/mcp-server/src/codex_tool_config.rs:106-118 expose l'outil `codex` (et `codex-reply` en :225), avec approbations exec/patch remontées par elicitation (mcp-server/src/exec_approval.rs, patch_approval.rs) et gestion de prompts/list et prompts/get (mcp-server/src/message_processor.rs:319-323). Lancé par `codex mcp-server` (codex-rs/cli/src/main.rs:1045-1047).

**Pyxis.** Grep `ServerHandler|RoleServer|serve_stdio|mcp-server` dans crates/ → 0 résultat (seul hit: un nom de paquet dans une fixture de test, crates/agent-mcp/tests/config_load.rs:154). crates/agent-mcp/src/server.rs, malgré son nom, est le registre d'états côté client (server.rs:1-2).

**Statut documentaire.** Aucun ADR ne traite le mode serveur; docs/ARCHITECTURE.md:404-424 ne décrit MCP que comme consommation cliente (« Pyxis consomme MCP via rmcp »), ce qui suggère un choix implicite plutôt qu'un oubli.

#### Non applicables à Pyxis

- **Annuaire de connecteurs / ChatGPT Apps et catalogue d'outils distants** (mineur) : Sans objet pour Pyxis: c'est l'annuaire propriétaire d'applications ChatGPT d'OpenAI, adossé à une infra de distribution et à une auth first-party inexistantes hors du périmètre OpenAI.
- **Politique administrateur désactivant des serveurs MCP (requirements.toml, managed config)** (mineur) : Pertinent uniquement pour un déploiement d'entreprise avec administration centrale. Pyxis est un outil mono-utilisateur Linux-first sans surface de gestion de flotte.

### Persistance et gestion du contexte

**Parité estimée : partial**

*Surface Codex.* Codex éclate la persistance sur cinq crates. `codex-rs/rollout/` écrit un JSONL par thread (`~/.codex/sessions/rollout-<ts>-<uuid>.jsonl`) via un writer asynchrone à commandes (`rollout/src/recorder.rs:85-125`, `RolloutCmd::{AddItems,Persist,Flush,Shutdown}`), la première ligne étant un `SessionMeta` très riche (id thread, session, `forked_from_id`, `parent_thread_id`, cwd, originator, cli_version, source, model_provider, base_instructions, history_mode, `context_window` - `protocol/src/protocol.rs:3054-3111`) ; chaque ligne est un `RolloutLine { timestamp, ordinal, item }` où `RolloutItem` porte `ResponseItem | Compacted | TurnContext | WorldState | EventMsg | SessionMeta` (`protocol/src/protocol.rs:3183-3196, 3377-3383`), filtré par une politique de persistance explicite (`rollout/src/policy.rs:8-58`). Autour : listing paginé avec curseur, filtre cwd/source/provider et cap de scan (`rollout/src/list.rs:34-120`), recherche plein-texte cross-session par ripgrep sur tous les rollouts, archivés inclus (`rollout/src/search.rs:41-62`), index de noms de thread append-only (`rollout/src/session_index.rs:19-48`), scanner JSONL inverse pour reconstruire un suffixe borné du contexte modèle sans relire tout le fichier (`rollout/src/model_context.rs:18-60`, `rollout/src/reverse_jsonl_scanner.rs`), compression zstd en tâche de fond des rollouts froids avec lecture transparente `.jsonl`/`.jsonl.zst` (`rollout/src/compression.rs:18-58`), et une base SQLite de métadonnées interrogeables (`rollout/src/state_db.rs:28-55`). `codex-rs/thread-store/` abstrait tout ça derrière un trait avec fork préparé, archive, delete, recherche d'occurrences et pagination d'items (`thread-store/src/lib.rs:15-68`). `codex-rs/message-history/` tient un historique global de prompts `~/.codex/history.jsonl` avec écriture O_APPEND atomique, verrou consultatif + retries, cap d'octets et trim (`message-history/src/lib.rs:1-110`). Côté fenêtre : `ContextManager` normalise l'historique avant chaque prompt (appariement call/output, strip images/audio si la modalité n'est pas supportée - `core/src/context_manager/history.rs:328-343`, `normalize.rs:318-364`), estime les tokens par item y compris images et audio base64 (`history.rs:497-733`), et applique une `TruncationPolicy` à l'enregistrement (`history.rs:124`). Le statut de fenêtre est calculé dans `core/src/session/context_window.rs:1-91` (tokens actifs, scope auto-compact `Total` ou `BodyAfterPrefix`, limite pleine, tokens restants, buffer de fallback), avec baseline post-compaction serveur-observée dans `core/src/state/auto_compact_window.rs:22-115`. `TokenUsage::percent_of_context_window_remaining` normalise par un `BASELINE_TOKENS = 12000` (`protocol/src/protocol.rs:2205-2249`) et le TUI affiche « N% context left ». La compaction existe en quatre implémentations : locale inline (`core/src/compact.rs`), distante v1/v2 côté backend (`compact_remote.rs`, `compact_remote_v2.rs`), token-budget qui ouvre une fenêtre neuve sans résumer (`core/src/tasks/compact.rs:26-64`), plus un outil `new_context` exposé AU MODÈLE pour demander lui-même une fenêtre neuve (`core/src/tools/handlers/new_context_window_spec.rs:7-16`). L'historique de remplacement conserve TOUS les messages utilisateur bornés à 20k tokens, pas seulement le dernier (`core/src/compact.rs:56, 611-668`). Le CLI expose `resume` (picker, `--last`, id, `--all`), `fork`, `archive`, `unarchive`, `delete` (`cli/src/main.rs:179-192, 311-367`), et le TUI `/compact`, `/fork`, `/resume`, `/new` (`tui/src/slash_command.rs:33-95`) plus un picker avec filtre cwd/provider et recherche typée (`tui/src/resume_picker.rs:289-304`), et un rollback de N tours utilisateur (`context_manager/history.rs:225`, `thread_rollout_truncation.rs:38-59`). `codex-rs/rollout-trace/` est une couche de trace brute + reducer distincte du rollout.

*Surface Pyxis.* Pyxis concentre tout dans trois fichiers. `crates/agent-session/src/lib.rs` (1140 lignes) implémente un JSONL append-only par conversation avec des garanties de durabilité explicites et bien testées : `write_all` + `flush` + `sync_data` par entrée (`lib.rs:131-154`), verrou fichier exclusif interdisant deux writers vivants (`lib.rs:92-93`), réparation de queue tronquée au réouverture (`lib.rs:95-108`), curseur de delta pour un `sync` idempotent (`lib.rs:233-246`), et distinction fine corruption réelle vs troncature de crash (`lib.rs:382-421`). Les entrées sont discriminées `Meta { schema_version } | Message | CompactBoundary | CompactCheckpoint | EncryptedReasoningRedacted | FileHistorySnapshot | Unknown` (`crates/agent-core/src/session.rs:15-31`), avec rejet d'un `schema_version` futur (`agent-session/src/lib.rs:343-350`) et tolérance aux entrées inconnues via `#[serde(other)]`. Le reasoning chiffré n'est jamais persisté (`lib.rs:64-70, 240-241`) et une redaction rétroactive est rejouable. Le listing (`list_sessions`, `lib.rs:529-567`) scanne les `*.jsonl` d'un dossier, ignore les vides, trie par mtime et expose id/premier message user/nb messages ; `workspace_prompts` (`lib.rs:574-618`) agrège et dédoublonne les prompts utilisateur de TOUTES les sessions du workspace pour l'historique fléché, plafonné. La reprise passe par `resume_file`/`resume_dir` (rejeu complet) puis `switch_to(path, cursor)` pour rebasculer le writer à chaud (`lib.rs:199-220`), ce qui alimente `/resume` et `--resume [latest|<id>]` (`crates/agent-cli/src/main.rs:170-187, 639-648` ; `crates/agent-cli/src/interactive.rs:869-956`), avec anti-traversée de chemin (`interactive.rs:333`, testé `interactive.rs:1790-1799`). Côté fenêtre, `crates/agent-core/src/budget.rs` calcule un `ContextBudget` unique par modèle (micro 70 %, auto 80 % de `max_context - output_reserve`, `budget.rs:50-62`), rejette une géométrie invalide (`try_for_model`, `budget.rs:33-46`), consomme l'`usage` réel du stream sinon retombe sur `agent-tokenizer` (`budget.rs:92-106`), et implémente le même baseline post-compaction que Codex : `mark_compacted` puis ancrage sur le premier usage réel, seuils mesurant la CROISSANCE et non l'absolu (`budget.rs:107-134`) - parité conceptuelle avec `AutoCompactWindow`. `crates/agent-tokenizer/src/lib.rs` fournit une heuristique ~1 token/4 octets par défaut et un `TiktokenCounter` o200k derrière feature. `crates/agent-core/src/compaction.rs` implémente une cascade micro (élagage des vieux `tool_result` en placeholder, `compaction.rs:78-101`) puis full auto/reactive : strip images + thinking + reasoning chiffré avant le summarizer (`compaction.rs:266-284`), garde anti « résumé de résumé » conservant verbatim TOUS les résumés antérieurs (`compaction.rs:140-150, 210-217`), borne du résumé combiné à 32k octets en gardant la queue (`cap_tail`, `compaction.rs:247-259`), refus non destructif d'un résumé vide/tronqué/refusé qui préserve le transcript (`compaction.rs:175-204`), et circuit breaker à 3 échecs (`compaction.rs:54-72`, `agent.rs:57`). La compaction MidTurn est projetée sans muter le budget (`would_autocompact`, `budget.rs:132-134` ; `agent.rs:846-851`). Le sanitaire du transcript (marqueur `/goal`) vit dans `crates/agent-cli/src/session.rs:74-90`.

#### Écarts pertinents

##### La compaction full ne conserve que le dernier message utilisateur

`majeur` · `divergent` · effort `M`

**Impact.** Après compaction, toutes les demandes intermédiaires de l'utilisateur ne survivent que sous forme paraphrasée dans un résumé généré par le modèle. Sur une session longue avec des contraintes énoncées tour par tour (« n'utilise pas X », « garde le style Y »), ces instructions se diluent ou disparaissent, alors que Codex les garde verbatim. C'est le mode d'échec classique du « l'agent a oublié ce que je lui avais dit il y a 20 minutes ».

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/compact.rs:611-668 `build_compacted_history_with_limit` réinjecte TOUS les messages utilisateur (parcours inverse, budget `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` ligne 56, troncature du plus ancien retenu) en plus du résumé ; `collect_user_messages` ligne 525-548 les collecte en excluant seulement les résumés antérieurs.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/compaction.rs:118-123 et 219-234 : seul le dernier message si `role == User` est extrait (`trailing_user`), le transcript est vidé (`messages.clear()`, ligne 224) et remplacé par `[Summary] + last_user`. Test confirmant : `compaction.rs:416-429` (`full_compact_replaces_with_summary`, assert `messages.len() == 2`).

##### Aucune compaction manuelle (/compact) exposée à l'utilisateur

`moyen` · `absent` · effort `S`

**Impact.** L'utilisateur ne peut pas reprendre la main sur son contexte : il subit la compaction quand le seuil de 80 % tombe, souvent au pire moment (au milieu d'un enchaînement d'outils). Il ne peut pas non plus compacter volontairement avant de changer de sujet pour repartir propre, ni forcer un résumé avant une longue tâche.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/slash_command.rs:40 déclare `SlashCommand::Compact`, décrit ligne 88 « summarize conversation to prevent hitting the context limit » ; le chemin manuel est `core/src/compact.rs` (run_compact_task avec CompactionTrigger::Manual) et `core/src/tasks/compact.rs:26-44` pour la variante token-budget.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58-76 : la table `COMMANDS` complète ne contient ni `/compact` ni équivalent. Grep `compact` sur `crates/agent-cli/src/` et `crates/agent-tui/src/` ne retourne que des commentaires (`interactive.rs:193-194`) et l'affichage de notice (`agent-tui/src/state.rs:942`). `agent_core::compaction::full_compact` n'est appelé que par la boucle automatique (`agent-core/src/agent.rs:399-429`).

##### La jauge de contexte existe dans le TUI mais n'est jamais alimentée

`moyen` · `partial` · effort `M`

**Impact.** L'utilisateur pilote à l'aveugle : aucun signal avant la compaction, aucune idée du coût d'un gros `Read` ou d'un `Bash` verbeux. La compaction arrive comme une surprise (« context compacted », `agent-tui/src/state.rs:942`) sans que rien n'ait annoncé la pression. Le code d'affichage est déjà écrit, il manque uniquement le canal d'événement.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:2244 `percent_of_context_window_remaining` (normalisé par BASELINE_TOKENS=12000, ligne 2205) ; /home/arthur/dev/codex/codex-rs/tui/src/token_usage.rs:43 le réexpose et le footer affiche « N% context left » (snapshots `tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__chat_composer__tests__empty.snap:14`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:580 `pub context_pct: Option<u8>` et /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1497-1499 + `context_gauge` ligne 1572 rendent la jauge. Mais `grep -rn context_pct /home/arthur/dev/pyxis/crates/` ne trouve AUCUNE affectation hors `agent-tui/examples/{welcome,transcript,input}.rs` : en runtime réel le champ reste `None` et le segment est masqué. Cause racine : `AgentEvent` (/home/arthur/dev/pyxis/crates/agent-core/src/event.rs:13-34) n'a aucune variante portant `TokenUsage` ou l'état du budget, alors que `ContextBudget` connaît `current_input`, `max_context` et `prefill_input` (`agent-core/src/budget.rs:64-81`).

**Statut documentaire.** prd-codex-tui-parity.md:343 mentionne « Given des compteurs tokens ou usage disponibles, when le stream se termine, then le pending usage output est insere » - l'AC existe mais le canal `AgentEvent` n'a pas été créé.

##### Aucun rollback de tours (édition d'un message précédent)

`moyen` · `absent` · effort `L`

**Impact.** Une mauvaise formulation de prompt ou une réponse partie dans la mauvaise direction ne se corrige qu'en repartant sur une session neuve (`/new`), perdant tout le contexte accumulé. Codex permet de revenir N tours en arrière et de reformuler en gardant le préfixe utile.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/context_manager/history.rs:225 `drop_last_n_user_turns(num_turns)` avec sémantique documentée (lignes 205-224) ; le rollout enregistre un marqueur `EventMsg::ThreadRolledBack` rejoué au resume pour que l'indexation reflète l'historique post-rollback (/home/arthur/dev/codex/codex-rs/core/src/thread_rollout_truncation.rs:50-58) ; l'UI est `tui/src/app_backtrack.rs`.

**Pyxis.** Grep `backtrack|rollback|undo` sur /home/arthur/dev/pyxis/crates/ ne retourne que des occurrences de `scrollback` (agent-tui/src/insert_history.rs, term.rs). `SessionEntry` (/home/arthur/dev/pyxis/crates/agent-core/src/session.rs:15-31) n'a aucune variante de rollback, et `apply_entry` (/home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:314-354) ne sait que push, clear-sur-checkpoint et redact.

##### Impossible de forker une session

`mineur` · `absent` · effort `M`

**Impact.** Impossible d'explorer deux approches depuis un même état de conversation sans écraser l'historique. Sur une session de debug longue, tester une piste alternative oblige soit à polluer la session courante, soit à repartir de zéro. Le `switch_to(path, cursor)` existant fournit déjà 80 % de la mécanique.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:192 `Subcommand::Fork(ForkCommand)` ; /home/arthur/dev/codex/codex-rs/tui/src/slash_command.rs:37 `SlashCommand::Fork` (« fork the current chat », ligne 95) ; /home/arthur/dev/codex/codex-rs/tui/src/cli.rs:40-55 (`fork_picker`, `fork_last`, `fork_session_id`, `fork_show_all`) ; le store expose `PrepareForkParams`/`PreparedFork`/`ForkBoundary` (/home/arthur/dev/codex/codex-rs/thread-store/src/lib.rs:32,44-45) et `SessionMeta.forked_from_id` (`protocol/src/protocol.rs:3058`) trace la filiation ; les frontières de fork sont calculées par `fork_turn_positions_in_rollout` (`core/src/thread_rollout_truncation.rs:70`).

**Pyxis.** Grep `fork` sur /home/arthur/dev/pyxis/crates/ ne retourne qu'un commentaire sur `fork-safe` des sous-process (`agent-cli/src/main.rs:6`) et un usage figuré dans `docs/DECISIONS.md:78`. `agent-session` n'expose que `create_in`, `create_at`, `switch_to`, `resume_file`, `resume_dir`, `list_sessions`, `workspace_prompts` (/home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:163-220, 298-618). Aucun champ de filiation dans `SessionEntry::Meta` (/home/arthur/dev/pyxis/crates/agent-core/src/session.rs:16-18).

#### Écarts discutables

##### Le header de session ne porte que schema_version

`mineur` · `partial` · effort `S`

**Impact.** Une session reprise ne sait pas avec quel modèle ni depuis quel commit/branche elle a tourné, et le listing ne peut ni afficher ni filtrer sur ces axes. En pratique on reprend une session sans savoir si elle a été menée avec gpt-5-codex ou un autre modèle, ni sur quelle branche git l'agent a travaillé - information critique quand on reprend un travail 3 jours plus tard.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:3054-3111 `SessionMeta` : session_id, thread id, forked_from_id, parent_thread_id, timestamp, cwd, originator, cli_version, source, thread_source, model_provider, base_instructions, dynamic_tools, history_mode, history_base, context_window. Ces champs alimentent directement le listing (`rollout/src/list.rs:47-88` `ThreadItem` : cwd, git_branch, git_sha, git_origin_url, source, model_provider, cli_version, created_at, updated_at).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/session.rs:16-18 : `Meta { schema_version: u32 }`, c'est tout. Écrit une seule fois à la création (/home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:170-174). Conséquence directe : `SessionInfo` (/home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:517-524) ne peut exposer que `id` (nom de fichier), `summary` (1er message user), `message_count` et `modified` (mtime du fichier), et l'id lui-même n'est qu'un timestamp millis (`agent-cli/src/interactive.rs:1677-1693`).

##### Pas de recherche plein-texte dans le contenu des sessions

`mineur` · `partial` · effort `M`

**Impact.** Retrouver « la session où j'avais résolu le bug de sandbox » est impossible dès que le premier message ne le mentionne pas. Avec un id qui n'est qu'un timestamp epoch (`interactive.rs:1677`), le listing devient inexploitable au-delà de quelques dizaines de sessions.

**Codex.** /home/arthur/dev/codex/codex-rs/rollout/src/search.rs:41-62 `search_rollout_matches` : ripgrep `-l --fixed-strings --ignore-case` sur tous les `*.jsonl` de `sessions/` ou `archived_sessions/`, avec fallback de scan manuel et prise en charge des rollouts compressés, plus extraction d'un snippet de contexte (`MATCH_CONTEXT_BEFORE_CHARS`/`AFTER`, lignes 22-23). Exposé par `first_rollout_content_match_snippet` (`rollout/src/lib.rs:82`) et le picker TUI (`tui/src/resume_picker.rs:303-304`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1280-1293 : le sous-menu `/resume` filtre uniquement sur `s.id.starts_with(q) || s.label.contains(q)`, et `label` est le PREMIER message utilisateur uniquement (/home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:502-513 `scan_push_message` alimente `scan.summary` avec le premier user seulement). Aucune fonction de recherche dans /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs (API publique : `resume_dir`, `resume_file`, `list_sessions`, `workspace_prompts`).

##### Ni archivage, ni suppression, ni nommage de session

`mineur` · `absent` · effort `M`

**Impact.** `<workspace>/.pyxis/sessions/` croît sans borne et sans hygiène : le menu `/resume` se dégrade linéairement, et supprimer une session contenant des secrets capturés dans un `tool_result` demande un `rm` manuel. Le nommage manque aussi pour retrouver une session par intention plutôt que par timestamp.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:182-189 `Subcommand::{Archive, Delete, Unarchive}` avec confirmation (`DeleteCommand.force`, ligne 367) ; `ARCHIVED_SESSIONS_SUBDIR` (/home/arthur/dev/codex/codex-rs/rollout/src/lib.rs:26) ; nommage de thread append-only avec résolution du plus récent (`rollout/src/session_index.rs:29-48`, `find_thread_meta_by_name_str`) ; `thread-store/src/lib.rs:26-31` (`ArchiveThreadParams`, `DeleteThreadParams`, `DeleteThreadsParams`). Compression zstd des rollouts froids en tâche de fond (`rollout/src/compression.rs:24-30`).

**Pyxis.** Grep `archive|delete_session|rename_session` sur /home/arthur/dev/pyxis/crates/ : aucun résultat. `list_sessions` (/home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:529-567) énumère tous les `*.jsonl` du dossier sans notion d'archive, de rétention ni de plafond. Aucun `SessionEntry` de nommage (/home/arthur/dev/pyxis/crates/agent-core/src/session.rs:15-31).

##### Le resume relit et parse intégralement le fichier de session

`mineur` · `divergent` · effort `M`

**Impact.** Coût O(taille totale du dossier) à chaque ouverture du menu `/resume` : sur des sessions multi-mégaoctets (tool_results volumineux persistés verbatim), l'ouverture du menu et le resume deviennent perceptiblement lents. Un `CompactCheckpoint` rend pourtant tout le préfixe antérieur inutile - l'information pour couper est déjà dans le format, elle n'est pas exploitée.

**Codex.** /home/arthur/dev/codex/codex-rs/rollout/src/model_context.rs:18-46 `ModelContextScan` : scan inverse newest-first qui s'arrête dès qu'il a vu une `CompactedItem` avec `replacement_history` + `window_number` ET une frontière de tour utilisateur complète, retournant un suffixe borné suffisant pour reconstruire le contexte modèle ; alimenté par `rollout/src/reverse_jsonl_scanner.rs`. Le listing lui-même n'ouvre que la tête et la queue (`rollout/src/list.rs:96-113` `HeadTailSummary`, `read_head_for_summary`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:303-312 `resume_file` fait un `std::fs::read_to_string` du fichier ENTIER en mémoire, puis `resume_content` (ligne 356-380) désérialise chaque ligne. Idem au réouverture en écriture : `resume_locked_file` (ligne 123-129) relit tout. `scan_session` (ligne 423-451) rejoue aussi tout le fichier pour CHAQUE session listée, et `list_sessions` (ligne 545) le fait en boucle sur tout le dossier.

##### Le modèle ne peut pas demander une fenêtre de contexte neuve

`mineur` · `absent` · effort `M`

**Impact.** Le modèle sait souvent avant le seuil qu'il change de phase (fin d'investigation, début d'implémentation) et pourrait repartir propre sans payer un résumé. Écart réel mais de faible priorité : c'est une optimisation, pas une capacité manquante - la compaction automatique couvre le cas d'usage principal.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/new_context_window_spec.rs:7-16 : outil `new_context` exposé au modèle, « Start a new context window. Does not clear, reset, or otherwise affect environment state. » ; handler /home/arthur/dev/codex/codex-rs/core/src/tools/handlers/new_context_window.rs:35-40 appelle `session.request_new_context_window()`, consommé par `AutoCompactWindow::take_new_context_window_request` (/home/arthur/dev/codex/codex-rs/core/src/state/auto_compact_window.rs:99-104) et exécuté sans résumé par `core/src/tasks/compact.rs:66-90`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:691-697 enregistre exactement `Read, Glob, Grep, Write, Edit, Bash` - aucun outil de gestion de contexte. `agent_core::compaction` n'expose que `microcompact` et `full_compact` (/home/arthur/dev/pyxis/crates/agent-core/src/compaction.rs:78, 107), tous deux déclenchés par la boucle sur seuil (`agent-core/src/agent.rs:399-429`), jamais par le modèle.

##### Pas de politique de troncature centralisée à l'enregistrement de l'historique

`mineur` · `partial` · effort `S`

**Impact.** Un serveur MCP qui renvoie 5 Mo de JSON gonfle le transcript et le fichier de session sans garde-fou, et le budget ne le voit qu'après coup via l'usage backend. Les outils natifs sont bien bornés ; le trou est sur les outils MCP et sur l'absence de plafond unique côté cœur.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/context_manager/history.rs:124 `record_items(items, policy: TruncationPolicy)` applique la troncature au moment d'entrer dans l'historique, via `process_item` (ligne 344, avec budget de sérialisation `policy * 1.2`) et `truncate_function_output_payload` (ligne 437) ; la politique est réutilisée par la compaction (`core/src/compact.rs:45-47`, `TruncationPolicy::Tokens`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/session.rs:52 `sync(&self, messages)` persiste les messages tels quels ; /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:238-244 n'applique que la redaction du reasoning chiffré. La troncature vit uniquement par outil et en dur : `MAX_OUTPUT = 30_000` (/home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:16), `MAX_BYTES` (agent-tools/src/read.rs:101), `MAX_MATCHES`/`MAX_LINE_BYTES` (agent-tools/src/grep.rs:20,137). Grep `truncate|MAX` sur /home/arthur/dev/pyxis/crates/agent-mcp/src/ : aucun résultat, donc un résultat d'outil MCP arbitrairement gros entre non borné dans l'historique ET dans le JSONL.

##### Pas de compression des sessions froides

`mineur` · `absent` · effort `M`

**Impact.** Sur un workspace intensément utilisé, `.pyxis/sessions/` accumule des JSONL non compressés contenant des tool_results verbatim. Le problème est réel mais secondaire face à l'absence de rétention/suppression (`no-session-lifecycle-management`) : compresser des sessions qu'on ne peut de toute façon pas purger règle le mauvais bout du problème.

**Codex.** /home/arthur/dev/codex/codex-rs/rollout/src/compression.rs:18-30 : suffixe `.zst`, worker best-effort en tâche de fond avec marqueur anti-réentrance sous `codex_home` ; lecture transparente plain/compressé (`open_rollout_line_reader`, ligne 46-57) et rematérialisation avant append (`materialize_rollout_for_reference`, /home/arthur/dev/codex/codex-rs/rollout/src/lib.rs:43-47). La recherche gère aussi les rollouts compressés (`rollout/src/search.rs:60`).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:33 `pub const SESSION_FILE: &str = "session.jsonl"` et `list_sessions` ligne 537 ne reconnaît que l'extension `jsonl`. Aucune dépendance de compression ; `open_prepared` (ligne 84-121) ouvre en clair uniquement.

#### Non applicables à Pyxis

- **Les images comptent pour 0 token dans l'estimation locale** (mineur) : Le fallback tokenizer sous-estime massivement une conversation avec captures d'écran : plusieurs milliers de tokens invisibles pour le budget. Impact limité tant que l'`usage` backend arrive (chemin nominal, `budget.rs:9
- **Pas d'index de métadonnées interrogeable (SQLite / pagination / curseur)** (mineur) : À l'échelle Codex (milliers de threads, multi-clients, cloud) l'index est indispensable ; à l'échelle Pyxis (sessions d'un seul workspace, un seul utilisateur, mono-provider) il serait une couche de complexité disproport
- **Pas de compaction côté serveur (endpoint backend)** (mineur) : La compaction distante est un endpoint propriétaire du backend OpenAI, non documenté publiquement et instable (deux versions v1/v2 coexistent dans Codex). La compaction locale de Pyxis produit un résultat équivalent au p

#### Écarts réfutés en vérification

- **Historique de prompts limité au workspace et sans recherche** : Partiellement refute : le scope workspace est un CHOIX DE DESIGN documente, pas un manque. /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:568-572, commentaire de workspace_prompts : 'Agrege les prompts utilisateur de TOUTES les sessions d'un dossier (ancien -> recent), pour l'historique navigable **par dossier** (facon Claude Code)'. La deduplication move-to-end (lib.rs:602-608) et le cap (lib.rs:612-614) sont equivalents fonctionnels du trim au HISTORY_SOFT_CAP_RATIO de Codex. Reste vrai et non refute : aucune recherche interactive - grep 'history_search|Ctrl-R|ctrl_r' sur /home/arthur/dev/pyxis/crates/ = zero, seules history_prev/history_next existent (agent-tui/src/state.rs:1006-1042). Cablage unique au demarrage : agent-cli/src/interactive.rs:467-468. L'ecart residuel reel se reduit a l'absence de Ctrl-R, severite mineur, le reste (scope machine vs workspace) est une divergence assumee.

### Interface terminal

**Parité estimée : partial**

*Surface Codex.* Codex expose ~150 modules TUI sous /home/arthur/dev/codex/codex-rs/tui/src/. Le composer (bottom_pane/chat_composer.rs, 12455 l., adosse a bottom_pane/textarea.rs, 3919 l.) est un editeur multiligne complet: newline explicite (keymap.rs:963-969, Ctrl+J/Ctrl+M/Shift+Enter/Alt+Enter), motions mot (keymap.rs:973-983), kill/yank Ctrl+U/Ctrl+K/Ctrl+Y (keymap.rs:1013-1016), mode Vim complet (keymap.rs:141-212, 1019-1105), recherche d'historique inverse Ctrl+R/Ctrl+S (keymap.rs:958-959), overlay de raccourcis sur `?` (keymap.rs:955-957, bottom_pane/footer.rs:168), placeholder `[Pasted Content N chars]` pour gros collages (chat_composer.rs:1086-1090), detection de paste-burst non-bracketed (bottom_pane/paste_burst.rs, 580 l.), pieces jointes image par collage presse-papier (clipboard_paste.rs:51) ou par chemin colle/drag-and-drop (chat_composer.rs:1103-1124, normalize_pasted_path clipboard_paste.rs:251), et lignes `[Image #N]` navigables. Les mentions `@` passent par une session de recherche floue asynchrone relancee a chaque frappe (file_search.rs:16-60, bottom_pane/file_search_popup.rs). Autour: overlay pager/transcript avec tail live (pager_overlay.rs, 1612 l.), backtrack Esc-Esc qui forke avant un message utilisateur et le recharge dans le composer (app_backtrack.rs:1-22), overlay d'approbation a decisions multiples (approval_overlay.rs:810-823, keymap.rs:253-266: approve / approve_for_session / approve_for_prefix / deny / decline / cancel / fullscreen), widget de plan (history_cell/plans.rs, /plan), rendu de diff dedie (diff_render.rs, 2559 l.), indicateur de contexte restant (footer.rs:998-1009) et de rate limits (status/rate_limits.rs, chatwidget/rate_limits.rs:10-13 seuils 75/90/95), status line reconfigurable (/statusline, bottom_pane/status_line_setup.rs:1-20), ~50 slash commands (slash_command.rs:15-78) dont /diff (get_git_diff.rs), /status (status/card.rs), /usage, /copy (clipboard_copy.rs), /theme (theme_picker.rs), /keymap, /vim. Cote terminal: titre de fenetre OSC (terminal_title.rs, pose depuis chatwidget/status_surfaces.rs:246), notifications OSC 9 ou BEL selon detection (notifications/mod.rs:19-30, chatwidget/notifications.rs:6-21), sondes OSC 10/11 bornees a 100 ms (terminal_probe.rs), niveaux de couleur (terminal_palette.rs), editeur externe Ctrl+G (external_editor.rs, keymap.rs:934), mode raw scrollback Alt+R pour la selection souris (chatwidget.rs:1574-1617), reflow du scrollback au resize avec debounce 75 ms et cap de lignes par terminal (app/resize_reflow.rs, transcript_reflow.rs:19, resize_reflow_cap.rs), rendu ANSI des sorties de commande (codex-ansi-escape via exec_cell/render.rs:134). Aucune capture souris n'est activee: Codex s'appuie sur le scrollback natif.

*Surface Pyxis.* Pyxis concentre son TUI dans 20 modules (~15 000 l.) sous /home/arthur/dev/pyxis/crates/agent-tui/src/ plus la boucle d'orchestration crates/agent-cli/src/interactive.rs (1828 l.). Le feature `codex_tui_parity` est actif par defaut (crates/agent-tui/Cargo.toml:11-13) et fournit un vrai socle: viewport inline avec insertion des cellules finalisees dans le scrollback natif (term.rs:39-63, insert_history.rs, interactive.rs:589-596), ChatSurface/HistoryCell (history_cell.rs, 6387 l.), StreamController stable-prefix (streaming.rs), markdown avec coloration syntaxique syntect (markdown.rs, highlight.rs), hyperlinks OSC 8 hors mesure de largeur (terminal_hyperlinks.rs:1-22), moteur de diff partage entre transcript et dialog de permission (diff.rs, render.rs:1600-1615). Le composer est un `String` mono-ligne avec curseur en offset UTF-8 (state.rs:556-565, render.rs:1395-1400, INPUT_HEIGHT=4 render.rs:26): Entree soumet toujours (state.rs:1717-1732), les seuls raccourcis d'edition sont Ctrl+A/E/U/W (state.rs:1689-1711), fleches Haut/Bas parcourent l'historique de prompts agrege par dossier (state.rs:1766-1773, interactive.rs:467-471). Les menus de completion (slash, /models, /effort, /permissions, /skills, /resume, /providers, /mcp, mentions `@`) sont rendus inline au-dessus du composer (render.rs:594, state.rs:1212-1317) et pilotes par filtre `starts_with`/`contains`. Le reste des capacites: overlay transcript Ctrl+T avec navigation vi (state.rs:1786-1825), pill « nouveaux blocs » quand on a remonte le fil (render.rs:767), spinner shimmer avec reduced-motion (spinner.rs, render.rs:1540-1569), status line modele/effort/workspace/mode de permission (render.rs:1480-1519), dialog de permission binaire o/n avec apercu diff borne (state.rs:1623-1639, render.rs:1577-1626), assainissement integral des familles d'echappement ANSI dans tout le contenu affiche (render.rs:1264-1290), realignement du viewport inline quand le terminal grandit (term.rs:120-139). Le PRD tasks/prd-codex-tui-parity.md est marque DONE sur ses 18 stories, mais le `BottomPane` porte (bottom_pane.rs:56-161) ne recoit jamais de vue (seuls appels: interactive.rs:496/628/649) et n'est jamais rendu par render_parity (render.rs:76-160): les overlays de type ListSelectionView/approval restent de l'infrastructure morte.

#### Écarts pertinents

##### Composer mono-ligne : aucune insertion de saut de ligne

`bloquant` · `absent` · effort `L`

**Impact.** Impossible de rediger un prompt multi-paragraphes, de coller un extrait de code lisible ou de structurer une instruction longue : chaque retour a la ligne envoie le message. C'est le blocage d'usage le plus immediat du composer.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:963-969 (insert_newline = Ctrl+J, Ctrl+M, Enter, Shift+Enter, Alt+Enter) adosse a /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/textarea.rs (3919 l., buffer multiligne avec wrap et hauteur desiree)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1717-1732 : `KeyCode::Enter` soumet toujours, aucune branche d'insertion de '\n' ; /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1395-1400 : la zone de saisie fait exactement 1 ligne. Greps infructueux : `insert_newline`, `Shift.*Enter`, `alt\(KeyCode::Enter\)`, `'\\n'` dans state.rs

**Statut documentaire.** tasks/prd-codex-tui-parity.md:365-377 (US-017 « Port du composer Codex ») est marque DONE dans tasks/prd-codex-tui-parity-status.json alors que le composer Codex n'a pas ete porte ; aucune divergence n'est documentee dans docs/CURRENT_STATUS.md comme l'exigeait le dernier critere d'acceptation.

##### Saisie longue tronquee : ni wrap ni defilement horizontal

`bloquant` · `absent` · effort `M`

**Impact.** Au-dela de la largeur du terminal l'utilisateur tape a l'aveugle : le texte disparait et le curseur ne bouge plus, ce qui rend l'edition d'un prompt de plus d'une ligne d'ecran pratiquement impossible.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs (rendu via TextArea qui calcule desired_height et wrappe) et /home/arthur/dev/codex/codex-rs/tui/src/wrapping.rs (adaptive_wrap_lines)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1402-1413 : les spans sont rendus dans une `Line` unique sans Wrap ; render.rs:1416-1422 : la colonne du curseur est clampee a `inner.right()-1`, donc le curseur se fige au bord des que la saisie depasse la largeur

##### Raccourcis d'edition reduits a Ctrl+A/E/U/W

`majeur` · `partial` · effort `M`

**Impact.** Pas de deplacement par mot, pas de Ctrl+K/Ctrl+Y : corriger le milieu d'un prompt se fait fleche par fleche. Le cout d'edition est proportionnel a la longueur du texte.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:970-1017 : move_word_left/right (Alt+B/F, Alt/Ctrl+fleches), delete_forward (Ctrl+D), delete_forward_word, kill_line_end (Ctrl+K), yank (Ctrl+Y), move_left/right/up/down en Ctrl+B/F/P/N

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1689-1711 : la branche CONTROL ne gere que 'a', 'e', 'u', 'w' et retourne `InputAction::None` pour tout le reste ; grep `Char\('[kyrogl]'\)` dans state.rs ne remonte que le 'y' du dialog de permission (state.rs:1625)

##### Gros collage insere brut dans une saisie mono-ligne

`majeur` · `absent` · effort `M`

**Impact.** Coller 200 lignes de log produit une saisie invisible et non editable : l'utilisateur ne peut ni verifier ce qu'il envoie ni le corriger avant soumission.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:1086-1090 : au-dela de LARGE_PASTE_CHAR_THRESHOLD le collage devient un element `[Pasted Content N chars]` et le texte complet est garde dans `pending_pastes`, reexpanse a la soumission

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/bottom_pane.rs:110-122 (`route_paste` -> `state.insert_str`) et /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:820-824 : la chaine collee est inseree telle quelle dans le `String` mono-ligne

##### Le scrollback deja emis n'est pas reflowe au redimensionnement

`majeur` · `partial` · effort `L`

**Impact.** Elargir ou retrecir la fenetre laisse tout l'historique deja imprime wrappe a l'ancienne largeur (lignes coupees ou colonnes perdues), et un terminal qui ne repond pas a la requete de position degrade la session jusqu'au redemarrage.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/app/resize_reflow.rs:1-15 : au changement de largeur, Codex efface l'historique terminal qu'il possede et reemet le transcript depuis les `HistoryCell` ; /home/arthur/dev/codex/codex-rs/tui/src/transcript_reflow.rs:19 (debounce 75 ms) et resize_reflow_cap.rs:19-22 (caps de lignes par terminal)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/term.rs:120-139 : `sync_inline_viewport` reconstruit uniquement le `Terminal` inline et efface l'ecran visible, sans reemettre les cellules ; /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:567-575 : en cas d'echec, la synchro est desactivee pour la session avec le message « Restart Pyxis after resizing »

**Statut documentaire.** tasks/prd-codex-tui-parity.md:419 liste « Resize during stream » comme cas limite attendu (« Stream source re-renders »), mais la reemission du scrollback n'est pas implementee.

##### Mentions @ : instantane fige de 200 fichiers, filtre par sous-chaine

`moyen` · `partial` · effort `M`

**Impact.** Sur un depot de plus de 200 fichiers la liste est arbitrairement tronquee (ordre de parcours du systeme de fichiers), les fichiers crees pendant la session sont invisibles, et un `@authsvc` ne trouve pas `src/auth/service.rs` faute de matching flou.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/file_search.rs:16-60 : session `codex-file-search` relancee a chaque edition du token `@`, avec annulation de la session precedente ; /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/file_search_popup.rs:17-60 : etat `waiting` et resultats flous asynchrones

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:458-461 : `workspace_file_mentions(&root, 200)` appele UNE fois au demarrage ; interactive.rs:1610-1646 : parcours recursif borne a 200 entrees, tri alphabetique ; /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1302-1317 : filtre `path.contains(q)`, `take(20)`

##### Pas de recherche inverse dans l'historique de prompts

`moyen` · `absent` · effort `M`

**Impact.** L'historique agrege par dossier peut contenir jusqu'a 200 entrees (interactive.rs:40) : sans recherche, retrouver un prompt ancien impose autant d'appuis sur Haut.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:958-959 : history_search_previous = Ctrl+R, history_search_next = Ctrl+S ; /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:51-54 : le footer devient champ de recherche et le corps previsualise le match

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1766-1773 : seules Haut/Bas naviguent (`history_prev`/`history_next`) ; aucune branche Ctrl+R dans state.rs:1689-1711

##### Approbation binaire o/n : ni « pour cette session » ni « pour ce prefixe »

`moyen` · `partial` · effort `L`

**Impact.** Chaque occurrence de la meme commande redemande une confirmation : sur une boucle `/goal` qui relance 25 fois `cargo test`, l'utilisateur doit rester devant le terminal. Le seul contournement est de basculer globalement en mode `auto`, ce qui perd toute granularite.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/approval_overlay.rs:810-823 (Accept, AcceptForSession, Decline, Cancel) et :862-884 (raccourcis approve_for_prefix / approve_for_session) ; /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:253-266 : ApprovalKeymap expose approve, approve_for_session, approve_for_prefix, deny, decline, cancel, open_fullscreen, open_thread

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1623-1639 : seules o/y/Entree (autoriser) et n/Esc/Ctrl+C (refuser) sont traitees ; /home/arthur/dev/pyxis/crates/agent-cli/src/approver.rs:15 : le canal de permission transporte un simple `oneshot::Sender<bool>` ; render.rs:1617-1622 n'affiche que `[o] allow  [n] deny`

##### Pas de retour arriere sur un message precedent (Esc-Esc / fork)

`moyen` · `absent` · effort `XL`

**Impact.** Apres un prompt mal formule, la seule option est `/new` ou `/clear` (interactive.rs:958-991), qui detruit tout le contexte au lieu de rebrancher la conversation juste avant l'erreur.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/app_backtrack.rs:1-22 : premier Esc amorce, second Esc ouvre l'overlay transcript et surligne un message utilisateur, Entree forke avant ce tour et recharge le prompt dans le composer

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1714-1716 : Esc ne fait qu'interrompre le tour en cours ; greps infructueux sur `backtrack`, `fork`, `edit previous` dans /home/arthur/dev/pyxis/crates/ (les seuls hits `fork` concernent la sandbox)

##### Indicateur de contexte restant present mais jamais alimente

`moyen` · `partial` · effort `S`

**Impact.** En session reelle l'utilisateur ne voit jamais combien de contexte reste : la compaction (`AgentEvent::Compacted`, state.rs:942) le surprend sans avertissement, alors que le code de rendu et l'agent-tokenizer existent deja.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/footer.rs:998-1009 (`context_window_line` -> « N% context left ») et /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/status_line_setup.rs:17 (« Context usage (remaining %, used %, window size) » comme item de status line)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1497-1501 rend la jauge seulement si `state.context_pct` est `Some`, mais grep `context_pct` sur tout /home/arthur/dev/pyxis/crates/ ne remonte que des affectations dans les exemples (crates/agent-tui/examples/transcript.rs:47, examples/input.rs:51...). Aucune ecriture dans agent-cli

##### Pile de vues BottomPane portee mais jamais utilisee ni rendue

`moyen` · `partial` · effort `M`

**Impact.** Toute la mecanique d'overlay (approbation riche, elicitation MCP, pickers multi-onglets) est inaccessible a l'execution : le seul chemin reel reste le menu inline de `AppState` et le dialog o/n. Le PRD est marque DONE sur cette story alors que la surface est morte.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/mod.rs:730-751 : `handle_paste` route vers `view_stack.last_mut()` puis retombe sur le composer, et la pile porte approval_overlay.rs, list_selection_view.rs, custom_prompt_view.rs, skills_toggle_view.rs, mcp_server_elicitation.rs, feedback_view.rs, hooks_browser_view.rs

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/bottom_pane.rs:56-161 definit `BottomPane`/`push_view` et :210-400 `ListSelectionView`, mais grep `push_view` sur /home/arthur/dev/pyxis/crates/agent-cli/ ne remonte rien (seuls appels : interactive.rs:496 construction, :628 route_paste, :649 route_key) et /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:76-160 (`render_parity`) n'appelle jamais `BottomPane::render`

**Statut documentaire.** tasks/prd-codex-tui-parity.md:352-364 (US-016) est DONE dans tasks/prd-codex-tui-parity-status.json ; la story couvre bien la pile de vues mais rien ne la branche.

##### Surface slash etroite : ni /status, ni /diff, ni /usage, ni /compact

`moyen` · `partial` · effort `M`

**Impact.** Aucun moyen depuis le TUI de voir l'etat de la session (compte, tokens, limites, repertoire, branche) ni le diff cumule des modifications de l'agent avant de committer : il faut sortir vers un autre terminal.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/slash_command.rs:15-78 : ~50 commandes dont Status:50, Usage:51, Diff:48, Compact:40, Init:39, Copy:46, Raw:47, Theme:55, Keymap:18, Vim:19, Statusline:54, Title:53 ; implementations dans status/card.rs et get_git_diff.rs:1-45

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58-75 : `COMMANDS` contient exactement 12 entrees (/help, /models, /effort, /permissions, /skills, /goal, /providers, /mcp, /resume, /new, /clear, /quit) ; `/help` se contente de lister ces noms (interactive.rs:682-689). Greps infructueux sur `get_git_diff`, `git diff`, `/status` dans /home/arthur/dev/pyxis/crates/

##### Messages en file : simple Notice, sans apercu ni edition

`moyen` · `partial` · effort `M`

**Impact.** Un message envoye par erreur pendant un tour ne peut plus etre retire : il sera soumis a la fin du tour courant (interactive.rs:1182-1197) sans nouvelle confirmation.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/pending_input_preview.rs:13-22 : widget dedie affichant steers en attente, steers rejetes et messages utilisateur en file, avec un hint d'edition ; /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:952 : edit_queued_message = Alt+Up / Shift+Left

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:668-676 : le prompt est pousse dans le transcript puis un `Block::Notice("Message queued.")` est ajoute et le texte part dans une `VecDeque` ; aucune vue ni raccourci ne permet de le relire, le modifier ou l'annuler

##### Pas d'ouverture du brouillon dans $EDITOR

`mineur` · `absent` · effort `M`

**Impact.** C'est l'echappatoire habituelle a un composer limite ; combinee a l'absence de multiligne, sa disparition ferme le dernier chemin pour rediger un prompt long confortablement.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/external_editor.rs:1-20 (resolution VISUAL/EDITOR, fichier temporaire, relance) ; /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:934 : open_external_editor = Ctrl+G

**Pyxis.** Greps infructueux sur `EDITOR`, `VISUAL`, `external_editor` dans tout /home/arthur/dev/pyxis/crates/

##### Aucune notification terminal en fin de tour

`mineur` · `absent` · effort `S`

**Impact.** Un tour long ou une demande de permission bloquante passent inapercus si l'utilisateur a change de fenetre : c'est exactement le cas d'usage du mode `/goal` autonome.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/notifications/mod.rs:19-30 : choix OSC 9 ou BEL selon `terminal_info()` ; /home/arthur/dev/codex/codex-rs/tui/src/chatwidget/notifications.rs:6-21 : `notify()` appele sur les evenements de cycle de tour

**Pyxis.** Greps infructueux sur `osc9`, `\\x1b]9`, `bell`, `BEL` dans /home/arthur/dev/pyxis/crates/agent-tui/src/ et agent-cli/src/ (seuls hits : `Block::Notice`, sans rapport)

##### Titre de fenetre du terminal jamais mis a jour

`mineur` · `absent` · effort `S`

**Impact.** Avec plusieurs sessions Pyxis ouvertes (usage Paneflow multi-panes revendique), rien ne distingue les onglets : ni le workspace, ni le statut du tour.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/terminal_title.rs:1-17 (ecriture OSC avec assainissement bidi/controle) et :56 (`set_terminal_title`), appele depuis /home/arthur/dev/codex/codex-rs/tui/src/chatwidget/status_surfaces.rs:246

**Pyxis.** Greps infructueux sur `SetTitle`, `set_title`, `terminal_title` dans tout /home/arthur/dev/pyxis/crates/ ; term.rs:26-90 (`enter`) n'emet que BracketedPaste, Clear et MoveTo

##### Pas d'overlay de raccourcis ni de footer contextuel

`mineur` · `partial` · effort `S`

**Impact.** Les raccourcis reellement implementes (Ctrl+T, Ctrl+A/E/U/W, PageUp/Down, j/k/Ctrl+D dans l'overlay) ne sont decouvrables nulle part dans l'interface.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:955-957 : toggle_shortcuts = `?` ; /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/footer.rs:168-214 : FooterMode ShortcutOverlay, EscHint, QuitShortcutReminder, HistorySearch, ComposerEmpty, ComposerHasDraft

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1521-1529 : `shortcut_hint` ne renvoie que trois chaines figees autour de Ctrl+C ; grep `'?'` dans /home/arthur/dev/pyxis/crates/agent-tui/src/ ne remonte rien

##### Gestion de la molette morte dans le build par defaut

`mineur` · `divergent` · effort `S`

**Impact.** Comportement final identique a Codex (scroll natif du terminal), donc pas de regression utilisateur : c'est du code mort qui suggere une capacite inexistante. A nettoyer, pas a implementer.

**Codex.** Codex n'active jamais la capture souris (aucun hit `EnableMouseCapture` dans /home/arthur/dev/codex/codex-rs/tui/src/) et s'appuie sur le scrollback natif du terminal, alimente par app/resize_reflow.rs et insert_history.rs

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:616-623 traite `MouseEventKind::ScrollUp/ScrollDown`, mais /home/arthur/dev/pyxis/crates/agent-tui/src/term.rs:29-38 n'active `EnableMouseCapture` que sous `cfg(not(feature = "codex_tui_parity"))`, et la feature est active par defaut (crates/agent-tui/Cargo.toml:11-13)

#### Écarts discutables

##### Limites d'usage de l'abonnement jamais affichees

`moyen` · `absent` · effort `M`

**Impact.** Sur un abonnement ChatGPT contraint par des quotas hebdomadaires, l'utilisateur decouvre l'epuisement en plein tour au lieu d'etre averti a 75 %. Le canal d'information est deja parse cote provider.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/status/rate_limits.rs (RateLimitSnapshotDisplay, fenetres en heure locale, etats available/stale/missing) et /home/arthur/dev/codex/codex-rs/tui/src/chatwidget/rate_limits.rs:10-13 (avertissements a 75/90/95 %, invite de bascule de modele au-dessus de 90 %)

**Pyxis.** Les donnees existent cote provider (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_events.rs, chatgpt.rs, greps `rate_limit`) mais aucune occurrence dans /home/arthur/dev/pyxis/crates/agent-tui/ ni agent-cli ; la status line render.rs:1480-1519 n'expose que modele, effort, workspace, mode de permission

##### Aucune piece jointe image (collage presse-papier ou drag-and-drop)

`mineur` · `absent` · effort `L`

**Impact.** Impossible de montrer une capture d'ecran (erreur d'UI, graphe, maquette) a l'agent, alors que le backend Codex accepte le multimodal.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/clipboard_paste.rs:51 (`paste_image_as_png`) et :251 (`normalize_pasted_path`, gere file:// et chemins shell-echappes du drag-and-drop) ; /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/chat_composer.rs:1103-1124 (`handle_paste_image_path` -> `attach_image`) et :1704 (`attach_image`)

**Pyxis.** Greps infructueux sur `image|clipboard|drag|attach` dans /home/arthur/dev/pyxis/crates/agent-tui/src/ et /home/arthur/dev/pyxis/crates/agent-cli/src/ : les seuls hits sont des faux positifs de rendu. `route_paste` (bottom_pane.rs:110-122) ne fait qu'inserer du texte

**Statut documentaire.** docs/ROADMAP.md Phase 2 (« Multimodal canonique via `ContentBlock::Image` ») place le multimodal hors du MVP livre ; l'ecart est donc planifie, pas ignore.

##### Aucune copie de la derniere reponse vers le presse-papier

`mineur` · `absent` · effort `S`

**Impact.** Recuperer une reponse longue impose une selection souris manuelle, penible sur un transcript qui a defile et impossible sans mode raw.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/clipboard_copy.rs:1-19 : backend de copie avec arboard, OSC 52 et integration tmux en session SSH, expose via Ctrl+O et `/copy` ; /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:935

**Pyxis.** Greps infructueux sur `clipboard`, `arboard`, `OSC 52`, `/copy` dans /home/arthur/dev/pyxis/crates/ ; la liste de commandes /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58-75 ne contient pas `/copy`

##### Widget de plan present dans le code mais sans producteur

`mineur` · `partial` · effort `L`

**Impact.** Sur une tache longue (notamment la boucle `/goal`, interactive.rs:1138-1178) l'utilisateur n'a aucune vue de l'avancement structure : uniquement un spinner et un compteur d'iterations.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/history_cell/plans.rs:1-22 (StreamingPlanTailCell, ProposedPlanStreamCell) et /home/arthur/dev/codex/codex-rs/tui/src/slash_command.rs:41 (`Plan`)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/lib.rs:60-61 exporte `PlanUpdateCell`, `PlanStep`, `PlanStepStatus`, mais grep `plan` sur /home/arthur/dev/pyxis/crates/agent-tui/src/app_event.rs ne remonte que `explanation:239` (aucun mapping d'evenement plan) et /home/arthur/dev/pyxis/crates/agent-tools/src/ ne contient que bash, edit, glob, grep, read, write : aucun outil `update_plan`

**Statut documentaire.** docs/ROADMAP.md Phase 1 liste explicitement « TUI riche (arbre de plan, review par hunk) » comme HORS scope MVP, reporte en Phase 2. Ecart assume.

##### Detection terminal reduite a la variable COLORTERM

`mineur` · `partial` · effort `M`

**Impact.** Un terminal 256 couleurs sans `COLORTERM` (tmux par defaut, ssh) bascule directement en degrade monochrome, et la palette n'est jamais accordee au fond reel du terminal.

**Codex.** /home/arthur/dev/codex/codex-rs/terminal-detection/src/lib.rs (identification du terminal) ; /home/arthur/dev/codex/codex-rs/tui/src/terminal_probe.rs:1-19 : sondes OSC 10/11 bornees a 100 ms avec repli conservateur ; /home/arthur/dev/codex/codex-rs/tui/src/terminal_palette.rs:14-22 : niveaux TrueColor/Ansi256/Ansi16/Unknown via supports_color

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/term.rs:174-178 : `supports_truecolor` lit uniquement `COLORTERM` et renvoie un booleen ; aucun crate de detection terminal dans /home/arthur/dev/pyxis/crates/

##### Sortie ANSI des commandes assainie au lieu d'etre rendue

`mineur` · `divergent` · effort `L`

**Impact.** La sortie de `cargo test`, `git diff` ou `eza` perd ses couleurs, donc son signal visuel (vert/rouge, surlignage). C'est un durcissement volontaire (le module documente l'injection OSC 52 / OSC 8 comme motif) : le compromis est defendable mais coute de la lisibilite.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/exec_cell/render.rs:134 et :161 : chaque ligne de sortie passe par `codex_ansi_escape::ansi_escape_line` ; /home/arthur/dev/codex/codex-rs/ansi-escape/src/lib.rs:26-38 convertit les sequences ANSI en spans Ratatui stylisees

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1264-1290 (`sanitize`) supprime CSI, OSC, DCS/SOS/PM/APC, sequences ESC 2 octets et C1 8 bits ; teste en render.rs:2152-2159

##### Pas de mode raw pour selection et copie souris

`mineur` · `absent` · effort `M`

**Impact.** Sans copie clipboard ni mode raw, extraire un bloc de code d'une reponse rendue avec gouttiere et indentation implique de nettoyer manuellement le texte selectionne.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:939 : toggle_raw_output = Alt+R ; /home/arthur/dev/codex/codex-rs/tui/src/chatwidget.rs:1574-1617 : bascule persistee dans `config.tui_raw_output_mode` avec notice utilisateur

**Pyxis.** Greps infructueux sur `raw_output`, `raw mode` (hors `enable_raw_mode` crossterm) dans /home/arthur/dev/pyxis/crates/agent-tui/src/ ; l'overlay transcript (state.rs:1786-1825) est en lecture-scroll seule

##### Overlay transcript sans tail live ni selection

`mineur` · `partial` · effort `M`

**Impact.** Ouvrir Ctrl+T pendant un tour fige la vue sur l'historique deja commite ; on ne suit pas la reponse en cours et on ne peut rien y selectionner.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/pager_overlay.rs:1-16 : l'overlay rend les cellules finalisees plus un tail live de la cellule en cours, recalcule sur une cle (largeur, revision, tick d'animation) ; app_backtrack.rs:133-148 y branche la selection de messages

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1786-1825 : l'overlay ne gere que scroll (Haut/Bas, PageUp/Down, Home/End, j/k, Ctrl+B/F/U/D, espace) et fermeture ; /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:197-320 : rendu des lignes plus un pourcentage et des hints

##### Keymap code en dur, non configurable, sans validation de conflits

`mineur` · `absent` · effort `L`

**Impact.** Aucun rebinding possible : un utilisateur dont le terminal capte Ctrl+T ou Ctrl+W n'a pas de recours. Impact reel limite tant que la surface de raccourcis reste petite.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/keymap.rs:31-54 : `RuntimeKeymap` resolu depuis `tui.keymap.<context>` avec precedence context -> global -> defauts, validation d'unicite (`validate_conflicts`, keymap.rs:919) et messages d'erreur avec chemin de config ; picker `/keymap` dans keymap_setup.rs

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1607-1784 : un `match` en dur sur `KeyEvent` ; /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs (300 l.) ne persiste que modele, effort et mode de permission (greps `save_model`, `save_reasoning_effort`, `save_permission_mode`)

##### Onboarding limite a un flux OAuth textuel, sans ecran de confiance

`mineur` · `partial` · effort `M`

**Impact.** Le premier lancement melange sortie texte brute et TUI ; il n'y a pas d'etape explicite de confiance du repertoire. Le cadrage mono-provider reduit la portee de l'ecart (une seule methode d'auth a proposer).

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/onboarding/mod.rs:1-7 : ecrans welcome, auth, keys, trust_directory dans onboarding_screen.rs, avec animation ASCII (ascii_animation.rs, frames.rs)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:476-490 : `run_auth_onboarding` affiche des `eprintln!` avant l'entree en TUI ; /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:467-593 fournit une carte d'accueil (avec repli compact sur petit terminal) mais aucun parcours d'onboarding interactif

**Statut documentaire.** docs/DECISIONS.md ADR-11 (mono-provider, abonnement ChatGPT d'abord) rend l'ecran de choix d'auth de Codex largement sans objet ; la sandbox Landlock etant toujours active (docs/CURRENT_STATUS.md, section Shipped), l'ecran trust_directory l'est aussi.

##### Status line figee, non configurable

`mineur` · `absent` · effort `M`

**Impact.** Pas de branche git ni de compteur de tokens dans la barre, et aucun moyen d'echanger un segment contre un autre sur terminal etroit.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/bottom_pane/status_line_setup.rs:1-20 : picker `/statusline` avec selection, reordonnancement gauche/droite et apercu live sur ~10 familles d'items (modele, repertoire, branche git, permissions, contexte, limites d'usage, titre/id de thread, version)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1480-1519 : composition en dur (modele, effort, workspace, mode de permission, jauge de contexte) sans point de configuration ; crates/agent-cli/src/settings.rs ne persiste aucun reglage d'affichage

#### Non applicables à Pyxis

- **Pas de mode Vim dans le composer** (mineur) : Confort d'edition avance ; sans composer multiligne le mode Vim n'aurait de toute facon pas de substrat. A traiter apres composer-single-line, si jamais.
- **Theme fixe, sans picker ni themes personnalises** (mineur) : Sur un terminal a fond clair, le theme syntect sombre fige donne un contraste degrade et l'utilisateur n'a aucun levier.
- **Pas de detection de collage non-bracketed** (mineur) : Sur un terminal Linux moderne le bracketed paste suffit ; le cas residuel (tmux mal configure, ssh vers un terminal ancien) transformerait un collage multiligne en soumissions successives.

### Modes non interactifs et intégration (exec, headless, app-server, CI)

**Parité estimée : minimal**

*Surface Codex.* Codex expose trois surfaces non interactives distinctes. (1) `codex exec` (`codex-rs/exec/`, 5530 lignes) : prompt positionnel ou lu sur stdin avec sentinelle `-` et trois politiques d'ingestion stdin (`RequiredIfPiped`, `Forced`, `OptionalAppend` qui append un bloc `<stdin>` a cote du prompt positionnel, `exec/src/lib.rs:177-186`, `exec/src/cli.rs:81-85`), flags `--json`/`--experimental-json` (JSONL sur stdout), `-o/--output-last-message FILE`, `--output-schema FILE` (JSON Schema injecte en `text.format` strict, prouve par `exec/tests/suite/output_schema.rs:36-58`), `--ephemeral` (aucun fichier de session), `--skip-git-repo-check` (sinon refus + exit 1 hors depot git, `exec/src/lib.rs:792-798`), `--color`, `--ignore-user-config`, `--ignore-rules`, `--strict-config`, plus les `SharedCliOptions` (`utils/cli/src/shared_options.rs:10-62` : `-i/--image` multi-fichiers, `-m/--model`, `-p/--profile`, `-s/--sandbox`, `--yolo`, `-C/--cd`, `--add-dir`). Deux sous-commandes exec : `resume` (id, `--last`, `--all`, `--image`, prompt) et `review` (`--uncommitted`, `--base`, `--commit`, `--title`) - `exec/src/cli.rs:165-298`. Le flux d'evenements JSONL est un contrat type et exporte en TS (`exec/src/exec_events.rs:11-133` : `thread.started`/`turn.started`/`turn.completed`/`turn.failed`/`item.started|updated|completed`/`error`, avec `Usage` detaille et items `AgentMessage`, `Reasoning`, `CommandExecution` avec `exit_code`, `FileChange`, `McpToolCall`, `WebSearch`, `TodoList`). En mode humain, la progression va sur stderr et le message final sur stdout uniquement si l'un des deux flux est redirige (`exec/src/event_processor_with_human_output.rs:70-203, 507-513`), et l'entete imprime le `session id` (`:461-464`). Les approbations sont forcees a `AskForApproval::Never` (`exec/src/lib.rs:427`) et le process sort en 1 des qu'une erreur est vue (`exec/src/lib.rs:1064-1066`) ou qu'un serveur MCP `required` echoue (`exec/tests/suite/mcp_required_exit.rs:30-36`). (2) L'app-server JSON-RPC (`app-server-protocol/src/protocol/common.rs:491-1230`) expose une centaine de methodes (`thread/start|resume|fork|archive|delete|rollback|list|read`, `turn/start|steer|interrupt`, `review/start`, `model/list`, `skills/list`, `hooks/list`, `plugin/*`, `mcp*`, `fs/*`, `process/*`, `config/*`, `account/*`) plus ~40 notifications (`item/started`, `turn/completed`, `thread/compacted`...), sur stdio, unix socket ou websocket (`cli/src/main.rs:526-536`), avec generation de bindings TS et JSON Schema (`cli/src/main.rs:601-609`) et un client in-process reutilise par exec lui-meme (`app-server-client/src/lib.rs:433,461`). (3) Deux SDK officiels (`sdk/typescript/`, `sdk/python/`) qui spawnent la CLI et consomment le JSONL (`sdk/typescript/README.md:1-10`), avec `runStreamed()` et output schema par tour. L'arborescence CLI compte ~25 sous-commandes (`cli/src/main.rs:122-211`) : `exec`, `review`, `login`(+`status`, `--with-api-key`, `--with-access-token`, `--device-auth`), `logout`, `mcp`(list/get/add/remove/login/logout, `cli/src/mcp_cmd.rs:54-61`), `plugin`, `mcp-server`, `app-server`, `remote-control`, `completion`, `update`, `doctor`, `sandbox`, `debug`(models/app-server/prompt-input/trace-reduce), `execpolicy`, `apply`, `resume`, `fork`, `archive`, `unarchive`, `delete`, `cloud`, `exec-server`, `features`.

*Surface Pyxis.* Pyxis expose un seul mode non interactif : `-p/--print <prompt>` (ou un prompt positionnel nu), dispatche a `crates/agent-cli/src/main.rs:717-750`. Le parseur d'arguments est manuel, sans clap et sans aucune sous-commande (`main.rs:88-155`, aide `main.rs:66-82`) : les seuls flags sont `-p/--print`, `--resume [latest|<file.jsonl>]`, `--model`, `--allow <host>`, `-y/--yes`, `--no-sandbox`, `--token-budget`, `--cost-budget-micro-usd`, `--input-cost-micro-per-ktok`, `--output-cost-micro-per-ktok`, `--overload-fallback-model`, `-h/--help`. En headless, la sortie est le texte final agrege uniquement, imprime apres la fin du run (`main.rs:735-750`), l'approbateur est `AutoDeny` fail-closed et `--yes` bascule en `AcceptEdits` (`main.rs:670-686`), le contexte MCP et les settings ne sont pas charges (`main.rs:340-343, 356-366`), et une erreur ou un epuisement de budget remonte en `anyhow::bail!` donc en code de sortie 1 (`main.rs:736-740`). `--resume` fonctionne aussi en headless : une session JSONL est resolue puis rechargee avant la boucle (`main.rs:639-655`, `resolve_resume_path` `main.rs:170-188`), et une nouvelle session JSONL est toujours ecrite sous `<workspace>/.pyxis/sessions/` meme en `-p` (`main.rs:637-649`). La surface d'integration reelle n'est pas un protocole mais l'API Rust embarquable : `agent_core::run_agent(ctx, deps) -> Stream<AgentEvent>` et `run_headless` (`crates/agent-core/src/lib.rs:22`, `crates/agent-core/src/agent.rs:960-991`), avec toutes les I/O injectees par traits (`crates/agent-core/src/deps.rs:15-21`) et un `AgentEvent` deja `Serialize`/`Deserialize` (`crates/agent-core/src/event.rs:11-13`). C'est un choix assume et documente : ADR-3 rejette explicitement l'IPC/process separe au profit de l'in-process avec types partages (`docs/DECISIONS.md:77, 94`, `docs/ARCHITECTURE.md:518-526`), et l'app-server Codex est marque non-goal explicite (`docs/codex-port-inventory.md:65`). L'embarquement Paneflow lui-meme reste non livre (`docs/CURRENT_STATUS.md:20`). Aucun `--json`, `--output-schema`, `--image`, `-C/--cd`, `--add-dir`, `--output-last-message`, `--version`, aucune lecture de stdin, aucune commande `login`/`logout`/`mcp` (greps sur `json`, `stdin`, `image`, `app-server`, `jsonrpc`, `SDK`, `output-schema` dans `crates/agent-cli/src/`, `docs/` et `tasks/` : aucun resultat pertinent). Le login est un onboarding OAuth implicite declenche au premier lancement interactif (`main.rs:446-461`), et refuse en headless avec un message explicite (`main.rs:434-444`). Ce que Pyxis fait mieux : des kill-switch de budget token/cout exposes en flags et variables d'environnement (`main.rs:212-241`), absents des flags de `codex exec` (grep `token_budget|cost_budget|max_turns|budget` dans `exec/src/cli.rs` et `shared_options.rs` : aucun resultat), et un headless fail-closed par defaut sur les permissions la ou Codex force `AskForApproval::Never`.

#### Écarts pertinents

##### Aucune sortie JSONL d'evenements en mode headless

`majeur` · `partial` · effort `M`

**Impact.** Un appelant CI, un script ou Paneflow-en-sous-process ne peut rien observer d'un run Pyxis : ni les commandes executees, ni leurs codes de sortie, ni les fichiers modifies, ni la consommation de tokens. Le seul artefact est le texte final et le code de sortie. Toute integration doit soit parser du texte libre, soit relire le JSONL de session a posteriori sans en connaitre le chemin.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:63-70 (`--json`/`--experimental-json`) alimente /home/arthur/dev/codex/codex-rs/exec/src/event_processor_with_jsonl_output.rs:58-66 qui serialise le contrat type de /home/arthur/dev/codex/codex-rs/exec/src/exec_events.rs:11-133 (thread.started, turn.completed avec Usage, item.completed avec CommandExecution.exit_code, FileChange, McpToolCall, TodoList)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:735-750 : `run_headless` est appele puis seul `result.text` est imprime ; tous les evenements sont jetes. /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:960-991 : `run_headless` ne conserve que le texte et compte les evenements. /home/arthur/dev/pyxis/crates/agent-core/src/event.rs:11-13 : `AgentEvent` est deja `Serialize`/`Deserialize` avec tag `type`, donc la brique existe et n'est pas cablee. Greps `--json`, `jsonl`, `stream-json` dans crates/agent-cli/src/ et docs/ : aucun resultat hors sessions JSONL.

**Statut documentaire.** Aucun ADR ne traite ce point. ADR-3 (docs/DECISIONS.md:77) engage au contraire un `AgentEvent` structure comme frontiere de contrat versionnee, ce qui rend l'absence de sortie machine incoherente avec l'intention affichee.

##### Le mode headless n'emet rien pendant le run

`moyen` · `partial` · effort `M`

**Impact.** Sur un run long en CI, la sortie reste vide pendant plusieurs minutes puis crache un bloc : impossible de diagnostiquer un blocage, de detecter une boucle d'outil, ou d'alimenter un log de build en streaming. Les timeouts CI coupent sans aucune trace de ce que l'agent faisait.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/event_processor_with_human_output.rs:70-203 : chaque item (commande, patch, recherche web, raisonnement, erreur) est imprime sur stderr des reception ; :507-513 le message final part sur stdout des que stdout ou stderr est redirige, ce qui rend `codex exec > out.txt` scriptable sans perdre les logs.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:735-750 : `agent_core::run_headless(...).await` bloque jusqu'a la fin du tour, puis `print!("{text}")`. Aucun `println!`/`eprintln!` de progression dans le chemin headless ; les seuls `eprintln!` sont les avertissements sandbox/settings (main.rs:296-320).

##### L'identifiant de session n'est jamais restitue en headless

`moyen` · `absent` · effort `S`

**Impact.** Un pipeline multi-etapes (analyser, puis corriger, puis verifier) ne peut pas cibler deterministe la session qu'il vient de creer. Deux jobs concurrents dans le meme workspace se volent mutuellement `latest`.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/exec_events.rs:39-43 : `thread.started` porte `thread_id` documente comme « can be used to resume the thread later » ; /home/arthur/dev/codex/codex-rs/exec/src/event_processor_with_human_output.rs:461-464 imprime `session id` dans l'entete du mode humain ; /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:165-172 expose `codex exec resume <SESSION_ID>`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:637-649 : une session JSONL horodatee est bien creee en headless, mais aucun `print`/`eprintln` ne la revele. Le seul moyen de chainer est `--resume latest` (/home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:175-180) qui prend la session la plus recente du repertoire, donc racy des que deux runs coexistent.

##### Aucune lecture du prompt sur stdin

`moyen` · `absent` · effort `S`

**Impact.** `cat prompt.md | pyxis -p -` et `git diff | pyxis -p "revois ce diff"` sont impossibles. Tout prompt doit tenir dans un argv, ce qui casse sur les prompts longs, multilignes ou contenant des quotes, et interdit d'injecter un artefact de build comme contexte.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:81-85 : le prompt est lu depuis stdin s'il est absent ou vaut `-`, et un stdin pipe a cote d'un prompt positionnel est ajoute comme bloc `<stdin>` ; les trois politiques sont modelisees dans /home/arthur/dev/codex/codex-rs/exec/src/lib.rs:177-186 et testees par /home/arthur/dev/codex/codex-rs/exec/tests/suite/prompt_stdin.rs.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:88-155 : le parseur ne connait que `-p <valeur>` et un positionnel unique ; aucune occurrence de `stdin`, `IsTerminal` ou `read_to_string` dans crates/agent-cli/src/ (grep effectue).

##### Pas de sortie structuree contrainte par un JSON Schema

`moyen` · `absent` · effort `M`

**Impact.** Un appelant qui veut une decision exploitable (verdict de review, liste de fichiers a patcher, score) doit parser du texte libre produit par le modele, sans garantie de forme. C'est la primitive qui rend un agent utilisable comme etape d'un pipeline plutot que comme assistant.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:52-54 (`--output-schema FILE`) ; /home/arthur/dev/codex/codex-rs/exec/tests/suite/output_schema.rs:36-58 verifie que la requete porte `text.format = {name: codex_output_schema, type: json_schema, strict: true, schema: ...}` ; l'item `AgentMessage` est alors « a JSON string when structured output is requested » (/home/arthur/dev/codex/codex-rs/exec/src/exec_events.rs:135-140).

**Pyxis.** Aucun `text.format`, `json_schema` ou `strict` dans /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs (grep `format|json_schema|structured` : seuls les blocs `input_text`/`input_image` a :114-137). Aucun flag correspondant dans /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:66-82.

##### Le mode headless ecrit toujours un fichier de session dans le workspace

`mineur` · `absent` · effort `S`

**Impact.** Chaque invocation CI salit l'arbre de travail (`.pyxis/sessions/*.jsonl`), ce qui pollue `git status`, peut faire echouer un check de proprete de worktree, et persiste des transcripts complets (donc potentiellement du contenu sensible du repo) sur des runners partages.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:30-32 : `--ephemeral` « Run without persisting session files to disk », propage jusqu'a `ThreadStartParams.ephemeral` (/home/arthur/dev/codex/codex-rs/exec/src/lib.rs:1089).

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:637-649 : `create_dir_all(<workspace>/.pyxis/sessions)` puis `JsonlSession::create_at(...)` sont executes inconditionnellement, avant meme le branchement headless/interactif de la ligne 717. Aucun flag d'opt-out (main.rs:66-82).

#### Écarts discutables

##### Aucune arborescence de sous-commandes : pas de login/logout/mcp/version/completion scriptables

`moyen` · `partial` · effort `L`

**Impact.** Impossible de provisionner un runner CI sans terminal interactif : aucune commande pour injecter un credential, verifier un etat d'authentification, se deconnecter, ajouter un serveur MCP, ou meme afficher la version du binaire dans un log de build. Le premier `pyxis -p` sur une machine froide echoue en demandant d'ouvrir une session interactive.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:122-211 : ~25 sous-commandes clap (exec, review, login, logout, mcp, plugin, mcp-server, app-server, completion, update, doctor, sandbox, debug, apply, resume, fork, archive, unarchive, delete, cloud, features...) ; /home/arthur/dev/codex/codex-rs/cli/src/mcp_cmd.rs:54-61 detaille `mcp list|get|add|remove|login|logout` avec `--json` sur list/get ; /home/arthur/dev/codex/codex-rs/cli/src/main.rs:459-505 detaille `login --with-api-key|--with-access-token|--device-auth|status`.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:109-152 : parseur manuel, aucun `match` sur un verbe ; tout argument non reconnu commencant par `-` leve « unknown argument » (main.rs:143-145), donc `--version` echoue aussi. Le login est un onboarding interactif implicite au premier lancement (main.rs:446-461, 476-509) et est explicitement refuse en headless (main.rs:434-444). Les serveurs MCP ne se gerent que par le menu `/mcp` du TUI (/home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:70).

**Statut documentaire.** Non traite par un ADR. docs/CURRENT_STATUS.md:8 documente l'auth keyring/OAuth livree mais pas sa surface CLI.

##### Aucun point d'entree pour attacher des images, alors que le coeur et le provider les supportent

`mineur` · `partial` · effort `S`

**Impact.** Un cas d'usage courant en CI (screenshot d'un test visuel casse, capture d'un graphe de perf) est inaccessible bien que 90 % de la plomberie soit deja ecrite et payee. C'est le gap le moins cher a fermer du lot.

**Codex.** /home/arthur/dev/codex/codex-rs/utils/cli/src/shared_options.rs:10-18 : `-i/--image FILE` accepte plusieurs fichiers, herite par `codex exec` et par `codex exec resume` (/home/arthur/dev/codex/codex-rs/exec/src/cli.rs:191-199).

**Pyxis.** Le support existe en profondeur : /home/arthur/dev/pyxis/crates/agent-core/src/message.rs:68 (`ContentBlock::Image`) et /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:133-137 (emission `input_image` en data URI). Mais aucun producteur : grep `ContentBlock::Image` dans crates/agent-cli/src/ et crates/agent-tui/src/ ne renvoie que des cas de rendu (/home/arthur/dev/pyxis/crates/agent-tui/src/history_cell.rs:3376,3443), jamais de construction. Aucun flag `--image` dans /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:88-155.

##### Pas de `--cd` ni de `--add-dir` : le workspace est fige sur le cwd du process

`mineur` · `partial` · effort `M`

**Impact.** Un runner CI doit faire un `cd` shell avant chaque invocation, et un monorepo ou un projet dont les artefacts vivent hors du repo (cache, sortie de build, repertoire de fixtures partage) ne peut pas etre traite : le sandbox refusera l'ecriture sans moyen de l'elargir.

**Codex.** /home/arthur/dev/codex/codex-rs/utils/cli/src/shared_options.rs:56-62 : `-C/--cd DIR` fixe la racine de travail de l'agent et `--add-dir DIR` (repetable) ajoute des racines inscriptibles ; propagees jusqu'a `runtime_workspace_roots` dans /home/arthur/dev/codex/codex-rs/exec/src/lib.rs:1083.

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:331 : `let workspace = std::env::current_dir()?;` sans alternative ; /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:304-305 : les chemins inscriptibles Landlock sont exactement le workspace plus le fichier de settings. Aucun flag `--cd`, `-C` ou `--add-dir` dans le parseur (main.rs:109-152).

##### Pas de mode revue de code non interactif

`mineur` · `absent` · effort `M`

**Impact.** Le cas d'usage CI le plus evident d'un agent de code (revoir un diff de PR contre une base) doit etre reimplemente cote appelant : construire le diff, le passer en prompt, definir le format de sortie. Codex livre ce cadrage, y compris la selection de la plage git.

**Codex.** /home/arthur/dev/codex/codex-rs/exec/src/cli.rs:265-298 : `codex exec review` avec `--uncommitted`, `--base <BRANCH>`, `--commit <SHA>`, `--title`, mutuellement exclusifs, plus le raccourci racine `codex review` (/home/arthur/dev/codex/codex-rs/cli/src/main.rs:129, 1020-1040) ; la revue passe par `review/start` cote protocole (/home/arthur/dev/codex/codex-rs/app-server-protocol/src/protocol/common.rs:884).

**Pyxis.** Aucun mode revue : grep `review` dans /home/arthur/dev/pyxis/crates/agent-cli/src/ ne renvoie rien, et la liste des slash commands du TUI (/home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:59-74) ne contient ni `/review` ni `/diff`.

##### Aucun SDK ni crate publiee pour piloter l'agent depuis un autre langage

`mineur` · `absent` · effort `L`

**Impact.** Consequence directe et acceptee du positionnement mono-consommateur. Le cout reel est qu'aucun ecosysteme tiers ne peut se construire sur Pyxis, et que meme un script Python de dogfooding devrait passer par `-p` en aveugle. A revoir seulement si Pyxis vise d'autres integrateurs que Paneflow.

**Codex.** /home/arthur/dev/codex/sdk/typescript/README.md:1-10 (`@openai/codex-sdk` spawne la CLI et echange du JSONL sur stdin/stdout, `startThread()`, `run()`, `runStreamed()`, output schema par tour) et /home/arthur/dev/codex/sdk/python/ (docs, 15 familles d'exemples, parite sync/async).

**Pyxis.** Aucun repertoire SDK dans /home/arthur/dev/pyxis (arborescence : crates/, docs/, tasks/). L'equivalent est l'API Rust in-process /home/arthur/dev/pyxis/crates/agent-core/src/lib.rs:22-36, non publiee sur crates.io et consommable uniquement par un binaire Rust du meme workspace. docs/CURRENT_STATUS.md:20 : l'embarquement Paneflow lui-meme est encore differe.

**Statut documentaire.** ADR-1/ADR-3 (docs/DECISIONS.md:29, 77) : le partage se fait par crates Rust in-process, pas par SDK cross-langage.

#### Non applicables à Pyxis

- **Aucun protocole de controle hors process pour un integrateur tiers** (mineur) : Pour le consommateur cible (Paneflow, en Rust, meme process), l'API `Stream<AgentEvent>` est strictement superieure a du JSON-RPC : zero serialisation, types partages, annulation par drop. Le gap reel n'est donc pas le p

#### Écarts réfutés en vérification

- **Aucun garde-fou avant d'ecrire dans un repertoire non versionne** : Refute : Pyxis n a pas de check git parce qu il substitue un garde-fou strictement plus fort, applique par le noyau et non par heuristique. (1) Le sandbox Landlock est ACTIF PAR DEFAUT et pose avant le runtime tokio : crates/agent-cli/src/main.rs:369 -> :289-323 (sandbox_enforced_from_args -> agent_sandbox::enforce_process(workspace, ...)), avec desactivation explicite seulement via --no-sandbox, qui logge un avertissement (main.rs:296-301). Toute ecriture est donc confinee au cwd, quel que soit le repertoire, versionne ou non. (2) En headless SANS --yes, le mode est PermissionMode::Default (main.rs:273-287) et l approbateur est AutoDeny (main.rs:682-686) ; crates/agent-tools/src/permission.rs:140 resout toute action mutante en Resolved::Ask et :227-233 AutoDeny refuse tout : `pyxis -p "..."` ne peut donc RIEN ecrire hors --yes. (3) Meme avec --yes (AcceptEdits), permission.rs:143-150 conserve la confirmation sur les actions sensibles, et le taint force la confirmation (:159-163) - confirmations qui, sous AutoDeny, sont des refus.
Cote Codex la verification existe bien (codex-rs/exec/src/lib.rs:790-798, exit 1), mais c est une heuristique de repertoire de confiance, contournee par --yolo, et Codex en a besoin parce que son sandbox est configurable/desactivable par profil. L allegation `pyxis -p ... --yes demarre dans n importe quel repertoire` est vraie mais trompeuse : il y demarre confine au repertoire. Statut absent -> divergent, severite moyen -> mineur, applicabilite discutable.
- **Pas de `--output-last-message` pour isoler la reponse finale dans un fichier** : Refute. En headless, le stdout de Pyxis EST le fichier de dernier message : crates/agent-cli/src/main.rs:747-750 n imprime que le texte final (apres retrait du marqueur GOAL_DONE) et rien d autre. Verification exhaustive : les SEULS print!/println! du binaire sont main.rs:328 (HELP, chemin --help qui retourne immediatement, :327-330), :747 et :749. Tout le reste part sur stderr : main.rs:296-320 (sandbox), :396/:407 (mcp), :361/:564/:577/:676 (settings), :481-505 (onboarding). Donc `pyxis -p "..." > out.txt` produit exactement le contenu que `codex exec -o out.txt` produit, en gardant les diagnostics sur stderr.
Inversement, la raison d etre de -o cote Codex est structurelle : ses deux processeurs melangent progression et message final sur les flux standards (codex-rs/exec/src/event_processor_with_human_output.rs:70-203 puis :507-513), donc Codex a BESOIN d une destination separee la ou Pyxis n en a pas besoin. L ecart inverse la causalite : c est une consequence de la simplicite du mode -p, pas une capacite manquante.
- **Codes de sortie non differencies entre erreur, epuisement de budget et fin normale** : Refute par la preuve Codex elle-meme : Codex n a AUCUNE differenciation de code de sortie. Tous les chemins d echec de codex exec appellent std::process::exit(1) : /home/arthur/dev/codex/codex-rs/exec/src/lib.rs:307, 325, 480, 499, 676, 797, 1065, 1815, 1826, 1912, 1927, 1934, 1943 (grep `process::exit|ExitCode` sur codex-rs/exec/src/*.rs : aucune autre valeur que 1). Le titre de l ecart decrit donc un delta qui n existe pas.
Cote Pyxis, les deux fins anormales sont par ailleurs deja distinguables sur stderr : crates/agent-cli/src/main.rs:736-739 emet `stopped: {reason:?}` pour HeadlessEnd::Exhausted et le message d erreur brut pour HeadlessEnd::Error - exactement le critere `code 1 + message stderr identifiable` que l auditeur cite comme reference Codex (codex-rs/exec/tests/suite/mcp_required_exit.rs). Le seul avantage reel de Codex est turn.completed vs turn.failed dans le flux JSONL (codex-rs/exec/src/exec_events.rs:19-24), ce qui est deja integralement compte dans l ecart headless-no-machine-readable-event-stream : compter deux fois la meme absence est une inflation.

### Modèles, providers, authentification

**Parité estimée : partial**

*Surface Codex.* Codex sépare quatre couches. (1) Description de provider : `codex-rs/model-provider-info/src/lib.rs:89-144` définit `ModelProviderInfo` (base_url, env_key, `experimental_bearer_token`, `auth` commande externe, `aws` SigV4, wire_api, query_params, http_headers, env_http_headers, retries, timeouts stream/websocket, `requires_openai_auth`, `supports_websockets`) ; le catalogue built-in est OpenAI + Amazon Bedrock + ollama + lmstudio (`:438-464`), les providers utilisateur se déclarent dans `config.toml` sous `model_providers` (`:471-508`), et `wire_api = "chat"` est désormais un rejet explicite au deserialize (`:72-84`). (2) Catalogue de modèles : `codex-rs/models-manager/src/manager.rs:33-59` définit un `ModelsEndpointClient` + `RefreshStrategy{Online,Offline,OnlineIfUncached}`, avec cache disque TTL 300 s clé par `client_version` et ETag (`models-manager/src/cache.rs:14-80`, `manager.rs:26-27,398-407,460-464`), fallback `model_info_from_slug` (`model_info.rs:125`) et overrides config (`model_info.rs:25`). Le `ModelInfo` du wire est très riche (`codex-rs/protocol/src/openai_models.rs:370-452`) : `context_window`, `max_context_window`, `auto_compact_token_limit`, `base_instructions`, `model_messages`/personality, `shell_type`, `apply_patch_tool_type`, `truncation_policy`, `supports_parallel_tool_calls`, `support_verbosity`/`default_verbosity`, `supports_reasoning_summary_parameter`/`default_reasoning_summary`, `service_tiers`, `input_modalities`, `visibility`, `priority`, `upgrade`. `ReasoningEffort` va de `none` à `ultra` avec variante `Custom` (`:39-131`). (3) Auth : `codex-rs/login/src/auth/manager.rs:73-83` porte sept modes (`protocol/src/auth.rs:9-34` : ApiKey, Chatgpt, ChatgptAuthTokens, Headers, AgentIdentity, PersonalAccessToken, BedrockApiKey) ; le login browser PKCE fait aussi un token-exchange pour obtenir une `OPENAI_API_KEY` persistée (`login/src/server.rs:1113-1142,862-894`), le device-code existe (`login/src/device_code_auth.rs`, CLI `--device-auth` `cli/src/main.rs:484`, fallback browser `cli/src/login.rs:354`), `codex login status` et `logout` avec révocation serveur (`cli/src/login.rs:424,479`, `login/src/auth/revoke.rs`). Le refresh est déclenché si `last_refresh` > 8 jours ou si l'`exp` du JWT tombe dans 5 min (`manager.rs:182-183,2522-2532`), avec raisons discriminées Expired/Exhausted/Revoked/Other (`:185-190,230-238`) et une échelle de récupération sur 401 (`UnauthorizedRecovery`, `:1589-1703`). Le stockage a quatre modes File(0600)/Keyring/Auto/Ephemeral avec repli fichier si le keyring échoue (`login/src/auth/storage.rs:39-61,404-453,498-540`). Les claims id_token exposent email, `chatgpt_plan_type`, `chatgpt_user_id`, fedramp (`login/src/token_data.rs:29-42`, plans `protocol/src/auth.rs:88-104`). (4) Runtime : rate limits parsés depuis `x-codex-primary/secondary-used-percent|window-minutes|reset-at` + crédits (`codex-api/src/rate_limits.rs:23-100`) et rendus dans le TUI (`tui/src/status/rate_limits.rs:137-208`) ; headers `session-id`/`thread-id` (`codex-api/src/requests/headers.rs:5-14`), originator `codex_cli_rs` et User-Agent avec détection terminal (`login/src/auth/default_client.rs:40-42`). Providers locaux OSS complets : Ollama avec pull et check de version Responses (`ollama/src/lib.rs:17-60`), LM Studio (`lmstudio/src/client.rs:20-45`). Config expose `model`, `model_provider`, `model_context_window`, `model_reasoning_effort`, `model_reasoning_summary`, `model_verbosity`, `service_tier`, `chatgpt_base_url`, `oss_provider` (`config/src/config_toml.rs:152-365,510`).

*Surface Pyxis.* Pyxis est mono-provider assumé (ADR-10/ADR-11, `docs/DECISIONS.md:297-356`, `docs/CURRENT_STATUS.md:9-18`) : un seul adapter `OpenAiChatGpt` sur `chatgpt.com/backend-api/codex/responses` en SSE stateless. L'OAuth est complet et de bonne qualité : PKCE S256, `state` anti-CSRF, serveur callback local 127.0.0.1:1455 bindé AVANT l'ouverture du navigateur, 404 sur requêtes parasites et read-timeout par socket (`crates/agent-auth/src/oauth/openai_chatgpt.rs:441-529`), extraction du `chatgpt_account_id` depuis le claim `https://api.openai.com/auth` du JWT access (`:159-178`), refresh rotatif (`:411-428`), device-code flow RFC 8628 avec classification `slow_down`/`expired_token`/403-404 (`:266-301,568-654`). Les secrets ont un `Debug` expurgé partout, y compris URL, headers, callback et state (`agent-auth/src/lib.rs:42-46`, `openai_chatgpt.rs:199-206,252-264,311-332`). Le `CredentialManager` rafraîchit sous `tokio::Mutex` avec marge de 60 s avant expiration, réécrit le keyring hors runtime async et distingue un refresh rejeté 401/403 (fatal) d'une erreur transport (`crates/agent-provider/src/credential.rs:14-158`) ; `agent-core` appelle `refresh_auth()` puis retente une fois sur `Auth(Expired)` (`crates/agent-core/src/agent.rs:514-524,659-670`). Le catalogue est découvert à chaud sur `GET /models?client_version=…`, filtré sur `visibility` et trié par `priority` (`crates/agent-provider/src/models.rs:54-74`, `chatgpt.rs:239-265`), publié dans le TUI par-dessus un catalogue embarqué de 7 slugs (`crates/agent-tui/src/state.rs:160-250`), avec `/models` et `/effort` en session et persistance de `model` + `reasoning_effort` dans `~/.pyxis/settings.toml` (`crates/agent-cli/src/settings.rs:6-10`, `main.rs:552-596`). Côté wire, `store:false`, `instructions` séparé, `prompt_cache_key` clampé 64 code-points, `parallel_tool_calls`, replay des reasoning items chiffrés en option (`crates/agent-provider/src/chatgpt_request.rs:45-88`). La robustesse réseau est supérieure à ce que le périmètre exigeait : connect timeout 20 s, header timeout, watchdog idle SSE, `retry-after-ms`/`Retry-After` secondes/IMF-fixdate, marqueurs de 429 terminal non ambigus, sanitisation des corps d'erreur (`chatgpt.rs:88-104,306-333,365-381,424-520`). Le fallback `originator` `pyxis` → `codex_cli_rs` est retenté automatiquement sur 400/403 mentionnant l'originator (`chatgpt.rs:335-352,630-648`). Le stockage est keyring-only sous la clé `oauth:openai_chatgpt` (`crates/agent-auth/src/store.rs:12-57`, `chatgpt.rs:34`), le login se fait en onboarding automatique au premier lancement (`crates/agent-cli/src/main.rs:446-510`) et la déconnexion via `/providers subscription codex disconnect` supprime le keyring et invalide la credential en mémoire (`crates/agent-cli/src/interactive.rs:1012-1033`).

#### Écarts pertinents

##### Les métadonnées de modèle du backend (context_window, tools, verbosity, instructions) sont ignorées

`moyen` · `partial` · effort `M`

**Impact.** Le seuil de compaction est calculé sur une constante et non sur la fenêtre réelle du modèle sélectionné. Un modèle à 400k est compacté trop tôt (perte de contexte utile), un modèle à 128k dépasse et ne se rattrape que par le 413 réactif. Tout modèle hors famille gpt-5.* tombe dans la branche fallback qui désactive le reasoning et les parallel tool calls, même quand le backend les annonce.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/openai_models.rs:370-452 - ModelInfo porte context_window, max_context_window, auto_compact_token_limit, base_instructions, shell_type, apply_patch_tool_type, truncation_policy, supports_parallel_tool_calls, support_verbosity/default_verbosity, supports_reasoning_summary_parameter, service_tiers, input_modalities ; consommé dans /home/arthur/dev/codex/codex-rs/core/src/client.rs:826,889-906 et /home/arthur/dev/codex/codex-rs/models-manager/src/manager.rs:398-407

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/models.rs:28-49 - WireModel ne déserialise que slug, display_name, visibility, priority, default_reasoning_level, supported_reasoning_levels ; le sample réel du backend à /home/arthur/dev/pyxis/crates/agent-provider/src/models.rs:84 contient bien "context_window":272000 mais le champ est droppé. En remplacement : constante DEFAULT_MAX_CONTEXT = 256_000 (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:39) et heuristique model_profile() sur le préfixe "gpt-5." (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:59-77) qui pilote max_context, parallel_tool_calls et verbosity

##### Les en-têtes de quota d'abonnement ne sont ni lus ni affichés

`moyen` · `absent` · effort `M`

**Impact.** L'utilisateur d'un abonnement ChatGPT ne voit jamais où il en est de son quota hebdomadaire. Il découvre la limite au moment où la session se bloque sur un 429 terminal, sans avertissement préalable ni date de reset, alors que le backend renvoie l'information à chaque réponse.

**Codex.** /home/arthur/dev/codex/codex-rs/codex-api/src/rate_limits.rs:57-100 - parse x-codex-primary-used-percent / -window-minutes / -reset-at, idem secondary, plus limit-name et crédits, en RateLimitSnapshot ; injecté dans les erreurs (/home/arthur/dev/codex/codex-rs/codex-api/src/api_bridge.rs:97-119) et rendu en TUI (/home/arthur/dev/codex/codex-rs/tui/src/status/rate_limits.rs:137-208)

**Pyxis.** grep 'x-codex|used.percent|RateLimitSnapshot|reset_at' sur /home/arthur/dev/pyxis/crates/ : aucun résultat. Le seul traitement de quota est réactif : parse_retry_after_ms (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:485-510) et TERMINAL_RATE_LIMIT_MARKERS sur le corps d'un 429 (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:365-381)

##### La déconnexion ne révoque pas le refresh token côté serveur

`moyen` · `partial` · effort `S`

**Impact.** Après un /providers disconnect, le refresh token reste valide chez OpenAI. Si le blob keyring a fuité (backup, dump de session, machine partagée), la déconnexion ne coupe rien : seule la copie locale disparaît.

**Codex.** /home/arthur/dev/codex/codex-rs/login/src/auth/manager.rs:879,2477 - logout_with_revoke ; /home/arthur/dev/codex/codex-rs/login/src/auth/revoke.rs (207 lignes dédiées à l'appel de révocation) ; exposé en CLI par /home/arthur/dev/codex/codex-rs/cli/src/login.rs:479

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1012-1033 - 'subscription codex disconnect' fait agent_auth::store::delete(KEYRING_ACCOUNT) puis provider.disconnect_auth() ; /home/arthur/dev/pyxis/crates/agent-provider/src/credential.rs:90-94 - disconnect() ne fait que vider l'état mémoire. grep 'revoke' sur /home/arthur/dev/pyxis/crates/ : aucun résultat

##### reasoning.summary et text.verbosity sont figés dans le code

`mineur` · `divergent` · effort `S`

**Impact.** L'utilisateur ne peut pas demander des réponses plus détaillées (verbosity high) ni couper les résumés de raisonnement, et si le backend introduit un modèle qui rejette summary:auto, la requête part quand même avec le champ.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:347-351 - clés model_reasoning_effort, model_reasoning_summary, model_verbosity ; /home/arthur/dev/codex/codex-rs/core/src/client.rs:826 gate le paramètre summary sur model_info.supports_reasoning_summary_parameter et :889-901 gate verbosity sur support_verbosity avec repli sur default_verbosity du catalogue

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:67 - body["reasoning"] = {"effort": effort, "summary": "auto"} en dur ; /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:67,73 - text_verbosity vaut "low" dans les deux branches de model_profile, sans clé de configuration correspondante dans /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:6-10 (seuls permission_mode, reasoning_effort, model)

#### Écarts discutables

##### Stockage credential keyring-only, sans repli fichier ni mode éphémère

`mineur` · `divergent` · effort `M`

**Impact.** Sur une machine Linux sans Secret Service actif (session SSH, conteneur, CI, tmux sans D-Bus utilisateur), Pyxis ne peut ni se connecter ni relire une credential existante, et le message d'erreur oriente vers un repli qui n'existe pas. C'est le cas d'usage le plus courant pour un agent terminal Linux-first.

**Codex.** /home/arthur/dev/codex/codex-rs/login/src/auth/storage.rs:498-540 - create_auth_storage supporte File (0600, :202-219), Keyring, Auto et Ephemeral ; AutoAuthStorage retombe sur le fichier quand le keyring échoue (:427-453)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/store.rs:22-57 - save/load/delete passent uniquement par keyring::Entry, aucune autre branche. Le message d'erreur promet pourtant un repli inexistant : 'secret store unavailable: {0} (fallback: env var, see docs)' (:17). grep 'PYXIS_ACCESS_TOKEN|OPENAI_API_KEY|auth.json' sur /home/arthur/dev/pyxis/crates/ : aucune lecture de credential par variable d'environnement ou fichier

**Statut documentaire.** docs/DECISIONS.md:325 rejette explicitement le auth.json clair 0600 de Pi au nom d'US-018 ; le mode éphémère en mémoire et un repli chiffré ne sont couverts par aucun ADR

##### Le device-code flow est implémenté mais aucun chemin CLI ne l'appelle

`mineur` · `partial` · effort `S`

**Impact.** Le flow browser bind 127.0.0.1:1455 et attend un callback local. Sur une machine distante en SSH sans forwarding de port ni navigateur, la connexion initiale est impossible alors que le code capable de la faire est déjà écrit et testé dans le binaire.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:484-485 - flag --device-auth ; /home/arthur/dev/codex/codex-rs/cli/src/login.rs:306,354 - run_login_with_device_code et run_login_with_device_code_fallback_to_browser

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/oauth/openai_chatgpt.rs:568-654 - start_device / poll_device complets et testés (:744-800). grep 'start_device|poll_device' sur /home/arthur/dev/pyxis/crates/agent-cli/ et /home/arthur/dev/pyxis/crates/agent-tui/ : aucun appelant. Le seul onboarding est login_browser_with_notice (/home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:488)

##### Aucun chemin credential par clé API : le variant existe, le producteur non

`mineur` · `partial` · effort `L`

**Impact.** Si OpenAI révoque le client_id Codex emprunté (risque R1 assumé), Pyxis n'a aujourd'hui aucun chemin d'auth de secours activable : le plan de sortie documenté est du code à écrire, pas une option de configuration. C'est le scénario pire-cas décrit par ADR-11 lui-même.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/auth.rs:9-11 (AuthMode::ApiKey), /home/arthur/dev/codex/codex-rs/cli/src/main.rs:463-472 (--with-api-key, --with-access-token), /home/arthur/dev/codex/codex-rs/login/src/auth/manager.rs:842-863 (lecture OPENAI_API_KEY / CODEX_API_KEY / CODEX_ACCESS_TOKEN en env), /home/arthur/dev/codex/codex-rs/login/src/server.rs:1113-1142 (token-exchange pour dériver une clé API pendant le login OAuth)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/lib.rs:52 déclare bien Credential::ApiKey { provider, key } et :21 ProviderId::OpenAiChat, mais grep 'Credential::ApiKey' sur /home/arthur/dev/pyxis/crates/ ne trouve aucun site de construction hors définition ; aucun adapter ne le consomme (/home/arthur/dev/pyxis/crates/agent-provider/src/lib.rs:14-18 n'expose que chatgpt)

**Statut documentaire.** docs/DECISIONS.md:338-356 (ADR-11) diffère explicitement US-017 / BYOK au rang de provider futur et assume le risque ; docs/DECISIONS.md:352 nomme la mitigation comme un module isolé à ajouter le jour venu

##### Pas de cache disque du catalogue /models, et aucune découverte en headless

`mineur` · `partial` · effort `M`

**Impact.** Chaque lancement interactif refait un aller-retour réseau qui peut échouer, et le mode headless (-p) n'interroge jamais le backend : il tourne toujours sur les 7 slugs figés de BUNDLED_MODELS (/home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:161-204). Un modèle retiré par le backend produit un 400 en headless sans que l'utilisateur voie jamais la liste réelle.

**Codex.** /home/arthur/dev/codex/codex-rs/models-manager/src/cache.rs:14-80 - cache fichier models_cache.json avec TTL et validation du client_version ; /home/arthur/dev/codex/codex-rs/models-manager/src/manager.rs:26-27,53-59,398-407,460-464 - TTL 300 s, ETag, RefreshStrategy Online/Offline/OnlineIfUncached

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:601-624 - le fetch est un tokio::spawn best-effort gardé par `if !headless`, sans persistance ; /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:207-250 - set_models écrit un OnceLock une fois par process, jamais sur disque. grep 'models_cache|etag|ETag' sur /home/arthur/dev/pyxis/crates/ : aucun résultat

##### Aucune surface d'identité de compte : ni plan, ni email, ni statut de login

`mineur` · `partial` · effort `S`

**Impact.** L'utilisateur ne peut pas vérifier quel compte ChatGPT est actif ni quel plan est utilisé. Sur une machine où deux comptes ont pu être connectés successivement, rien ne permet de savoir lequel paie les tokens, et le diagnostic d'un 403 de plan insuffisant se fait à l'aveugle.

**Codex.** /home/arthur/dev/codex/codex-rs/login/src/token_data.rs:29-42 - IdTokenInfo expose email, chatgpt_plan_type, chatgpt_user_id, chatgpt_account_is_fedramp ; plans énumérés dans /home/arthur/dev/codex/codex-rs/protocol/src/auth.rs:88-104 ; commande dédiée codex login status dans /home/arthur/dev/codex/codex-rs/cli/src/login.rs:424

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/oauth/openai_chatgpt.rs:159-167 - extract_account_id lit uniquement chatgpt_account_id du claim custom et jette le reste de la payload. grep 'chatgpt_plan_type|plan_type|email' sur /home/arthur/dev/pyxis/crates/ : aucune occurrence hors le scope OAuth 'openid profile email' (:36). Le TUI n'affiche qu'un booléen connected (/home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:757)

##### client_version figé en dur : le catalogue se périme silencieusement

`mineur` · `divergent` · effort `S`

**Impact.** Le backend filtre le catalogue sur minimal_client_version. Dès qu'OpenAI publie de nouveaux slugs, ils restent invisibles jusqu'à un bump manuel du code, et rien dans le produit ne signale que la liste est tronquée. C'est une dette d'entretien récurrente, pas un bug ponctuel.

**Codex.** /home/arthur/dev/codex/codex-rs/models-manager/src/manager.rs:398,460 - client_version_to_whole() dérive la version réelle du binaire et sert aussi de clé de validité du cache

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/oauth/openai_chatgpt.rs:81-98 - CODEX_CLIENT_VERSION = "0.145.0", constante à bumper à la main à chaque release de openai/codex, override via PYXIS_CODEX_CLIENT_VERSION ; le commentaire documente la mesure (0.1.0 → 0 modèle, 0.145.0 → 8)

**Statut documentaire.** documenté honnêtement en commentaire mais non couvert par un ADR

##### Les causes d'échec de refresh ne sont pas distinguées

`mineur` · `partial` · effort `S`

**Impact.** Une rotation de refresh token ratée (blob keyring restauré depuis un backup, deux process Pyxis concurrents) produit le même message qu'une révocation par OpenAI. L'utilisateur ne sait pas s'il doit simplement se reconnecter ou si le canal d'abonnement est coupé.

**Codex.** /home/arthur/dev/codex/codex-rs/login/src/auth/manager.rs:185-190 - messages distincts pour token expiré, refresh déjà consommé (rotation ratée), token révoqué, et mismatch de compte ; classification dans RefreshTokenError::failed_reason (:230-238)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/credential.rs:141-157 - tout 401/403 sur le refresh devient un unique ProviderError::Http { message: "OAuth refresh rejected (revoked token?)" } ; en aval /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:731-732 ne discrimine que sur la présence du mot 'expired' dans le corps

##### En-têtes d'observabilité de requête absents (session-id, thread-id, User-Agent structuré)

`mineur` · `partial` · effort `S`

**Impact.** Aucune corrélation côté serveur entre les requêtes d'une même session, ce qui rend un ticket de support ou un diagnostic de 400 backend beaucoup plus difficile. Le User-Agent générique reqwest est aussi un signal d'anomalie face à un originator qui prétend être un client Codex.

**Codex.** /home/arthur/dev/codex/codex-rs/codex-api/src/requests/headers.rs:5-14 - session-id et thread-id posés sur chaque requête Responses ; /home/arthur/dev/codex/codex-rs/login/src/auth/default_client.rs:15,40-42 - User-Agent construit avec détection de terminal et originator par défaut codex_cli_rs

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/oauth/openai_chatgpt.rs:364-380 - auth_headers ne pose que Authorization, chatgpt-account-id, originator ; responses_request ajoute OpenAI-Beta, accept, content-type (:336-345). L'identité de session ne voyage que dans le corps via prompt_cache_key (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:86-88) ; le User-Agent est celui par défaut de reqwest (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:157-160)

##### Les base_instructions servies par le catalogue ne sont pas utilisées

`mineur` · `divergent` · effort `L`

**Impact.** Les modèles gpt-5.x du backend Codex sont post-entraînés avec leurs instructions de base ; les remplacer par un prompt maison est un choix produit légitime mais non instrumenté : rien ne mesure l'écart de comportement, et une évolution serveur des instructions passera inaperçue.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/openai_models.rs:389-391 - base_instructions et model_messages viennent du /models du backend ; résolus par modèle et personnalité dans :478-490

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:707-711 - prompt::select_system_prompt(&args.model) choisit un template local par slug ; /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_request.rs:23,46 - le champ instructions retombe sur DEFAULT_INSTRUCTIONS si le canonique n'en fournit pas. Le champ base_instructions n'est pas déserialisé (/home/arthur/dev/pyxis/crates/agent-provider/src/models.rs:28-44)

**Statut documentaire.** aucun ADR ne traite le choix du prompt système par rapport aux base_instructions du catalogue

##### Un seul compte stockable, pas de bascule ni de workspace forcé

`mineur` · `absent` · effort `M`

**Impact.** Se connecter avec un second compte écrase silencieusement le premier, et un utilisateur membre de plusieurs workspaces ChatGPT ne peut pas choisir lequel facture la session. Pour un usage mono-utilisateur en dogfood l'impact reste borné.

**Codex.** /home/arthur/dev/codex/codex-rs/login/src/auth/manager.rs:2267-2282 - set/forced_chatgpt_workspace_id ; :190,1734 - détection de mismatch de compte au refresh avec message dédié ; /home/arthur/dev/codex/codex-rs/chatgpt/src/workspace_settings.rs:26-38 - settings de workspace cachés par (base_url, account_id)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:34 - KEYRING_ACCOUNT est la constante unique "oauth:openai_chatgpt" ; /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:423,464 - load et save utilisent cette seule clé. Aucune notion de liste de comptes dans /home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:1319-1466 (menu /providers limité à connect/disconnect)

##### base_url, proxy sortant et CA custom non configurables

`mineur` · `partial` · effort `M`

**Impact.** Derrière un proxy MITM d'entreprise avec CA interne, le handshake TLS échoue sans recours ; pointer vers un mock local pour tester le wire impose de recompiler. Impact limité pour un usage personnel Linux, réel pour toute tentative d'usage en environnement contraint.

**Codex.** /home/arthur/dev/codex/codex-rs/config/src/config_toml.rs:365 - clé chatgpt_base_url ; /home/arthur/dev/codex/codex-rs/model-provider-info/src/lib.rs:94,113-121 - base_url, query_params, http_headers et env_http_headers par provider ; /home/arthur/dev/codex/codex-rs/http-client/src/custom_ca.rs et outbound_proxy.rs pour le TLS et le proxy d'entreprise

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-auth/src/oauth/openai_chatgpt.rs:76-79 - CHATGPT_BASE_URL, RESPONSES_PATH et MODELS_PATH sont des constantes ; le client est construit sans configuration de proxy ni de CA (/home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:157-160). Les seules variables d'environnement reconnues sont PYXIS_ORIGINATOR, PYXIS_CODEX_CLIENT_VERSION et PYXIS_IDLE_TIMEOUT_SECS

#### Non applicables à Pyxis

- **Providers alternatifs (OSS local, Bedrock, Azure, wire_api chat)** (mineur) : Sans objet pour l'utilisateur cible : le positionnement mono-provider est une décision produit explicite et datée, pas un oubli d'implémentation.
- **Modes d'auth programmatiques (AgentIdentity, PersonalAccessToken, Headers, ChatgptAuthTokens externes)** (mineur) : Ces modes existent pour l'infrastructure interne d'OpenAI (agents managés, hôtes applicatifs injectant des tokens, comptes de service). Hors scope d'un agent terminal personnel mono-utilisateur.

#### Écarts réfutés en vérification

- **Pas de refresh périodique proactif hors fenêtre d'expiration** : REFUTE : la preuve Codex est mal lue. /home/arthur/dev/codex/codex-rs/login/src/auth/manager.rs:2521-2532 n est PAS un OU. Le code fait un RETURN ANTICIPE sur l expiration du JWT (`if let Some(tokens) = ... && let Ok(Some(expires_at)) = parse_jwt_expiration(...) { return expires_at <= Utc::now() + Duration::minutes(CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES); }`), et la regle last_refresh > 8 jours (TOKEN_REFRESH_INTERVAL, manager.rs:182) n est atteinte QUE si l exp du JWT est illisible. Le mecanisme primaire de Codex est donc exactement celui de Pyxis : refresh sur fenetre avant expiration. credential.rs:14,71 `if now.saturating_add(REFRESH_MARGIN_MS) >= cred.expires_at` avec REFRESH_MARGIN_MS = 60_000. Seule divergence residuelle : marge 60 s vs 5 min, et Pyxis choisit deliberement la marge (credential.rs:12-14 documente qu il s ecarte de Pi qui vise le bord exact). Le second sous-claim est faux aussi : le recovery n est pas 'un refresh + un retry' mais une boucle bornee par config.max_retries (agent.rs:514-524 pour le stream, agent.rs:668 pour le second site). Applicabilite non-applicable : la capacite existe sous forme fonctionnellement equivalente.

### Observabilité et assurance qualité du harness

**Parité estimée : minimal**

*Surface Codex.* Codex traite l'observabilite comme un sous-systeme a part entiere. `codex-rs/otel/` (~3 400 LoC) fournit un `OtelProvider` cablant logs/traces/metriques OTLP (gRPC, HTTP binaire ou JSON) plus un exporteur in-memory pour les tests (`otel/src/config.rs:88-104`, `otel/README.md:99-145`), un emetteur d'evenements metier session-scoped `SessionTelemetry` avec ~35 points d'emission typés (turn TTFT, api_request, websocket_connect/request/event, log_sse_event, sse_event_failed, user_prompt, tool_decision, sandbox_outcome, `otel/src/events/session_telemetry.rs:250-1101`), la propagation W3C traceparent/tracestate (`otel/src/trace_context.rs`) et une suite de tests dediee (`otel/tests/suite/otlp_http_loopback.rs`, `manager_metrics.rs`, `otel_export_routing_policy.rs`). Le logging fichier est natif : le TUI ouvre `log_dir/codex-tui.log` en `0o600` avec un layer `tracing_subscriber::fmt` non bloquant filtre par `RUST_LOG` (`tui/src/lib.rs:1231-1259`), le mode headless a son propre `exec_stderr_env_filter` (`exec/src/lib.rs:232-238`). Le diagnostic dispose de trois surfaces CLI : `codex doctor` (`cli/src/doctor.rs:1-11`, `:151-171`) avec `--json` redige pour rapport de support, `codex debug models|prompt-input|app-server|trace-reduce` (`cli/src/main.rs:227-244`) dont `prompt-input` dumpe en JSON exactement ce que le modele voit, et `codex-rs/rollout-trace/` : un bundle local opt-in (`CODEX_ROLLOUT_TRACE_ROOT`) de `manifest.json` + `trace.jsonl` + `payloads/*.json`, rejoue hors ligne en graphe `state.json` par `codex debug trace-reduce` (`rollout-trace/README.md:1-40`, `:104-125`). La correlation d'incident passe par `codex-response-debug-context` qui extrait `x-request-id`/`x-oai-request-id`/`cf-ray`/`x-openai-authorization-error`/`x-error-json` des erreurs transport, consomme en 10 points de `core/src/client.rs` (`response-debug-context/src/lib.rs:19-54`), et par `codex-feedback` : un ring buffer 4 MiB capturant tous les logs a `Level::TRACE` independamment de `RUST_LOG`, plus des tags structures et des diagnostics de proxy, uploade sur consentement (`feedback/src/lib.rs:169-260`, `feedback/src/feedback_diagnostics.rs:1-31`). Cote test deterministe : 1 157 fichiers portent des tests, `core/tests/common/` est un harness partage de 6 702 LoC (`responses.rs` 1 755 lignes de constructeurs d'events SSE + mock wiremock capturant et assertant les requetes sortantes, `streaming_sse.rs` 714 lignes de serveur SSE gate chunk par chunk, `test_codex.rs` 1 273 lignes bootant un `CodexThread` reel contre le mock, `context_snapshot.rs` 787 lignes pour figer le contexte model-visible), exploite par 115 fichiers de suite (`core/tests/suite/`, 98 425 LoC) couvrant approvals, compaction, resume/fork, MCP, hooks, sandbox et OTel lui-meme (`core/tests/suite/otel.rs:36-38` avec `tracing_test::traced_test`). Le TUI ajoute 663 snapshots `insta` et un `VT100Backend` qui rejoue l'ANSI emis dans un parseur vt100 pour asserter le scrollback reel (`tui/tests/test_backend.rs:1-4`, `tui/tests/suite/vt100_history.rs:22-53`, `vt100_live_commit.rs`, `resize_reflow.rs`). Un panic hook restaure le terminal (`tui/src/tui.rs:537`, `tui/src/lib.rs:1350`). Le tout est verrouille en CI (`.github/workflows/rust-ci.yml`, `rust-ci-full-nextest-platform.yml`, `cargo-deny.yml`) et documente comme regle de contribution (`AGENTS.md:112-123` integration-first, `:180-196` workflow `cargo insta`).

*Surface Pyxis.* Pyxis n'a aucune couche d'observabilite structuree. Le grep `tracing|opentelemetry|telemetry|RUST_LOG|env_logger` sur `crates/**/*.rs` et `crates/**/Cargo.toml` ne renvoie rien : pas de crate `tracing`, pas de spans, pas de niveaux, pas de subscriber. La seule primitive est `crates/agent-tui/src/debug_log.rs:1-44`, 44 lignes ecrivant `timestamp message` dans `pyxis-tui-debug.log` sous le cwd quand `PYXIS_DEBUG_TUI` est pose, avec exactement 9 sites d'appel, tous relatifs au dimensionnement du viewport (`crates/agent-tui/src/term.rs:42`, `:67`, `:126`, `:137`, `crates/agent-cli/src/interactive.rs:571-643`). Une seconde sonde, `PYXIS_DEBUG_USAGE`, compare l'usage backend a l'estimation locale mais via `eprintln!` dans le coeur (`crates/agent-core/src/agent.rs:561-570`). Le CLI n'expose aucune sous-commande : `HELP` (`crates/agent-cli/src/main.rs:67-81`) liste 11 options de run, pas de `doctor`, pas de `debug`, pas de dump de prompt ; les 12 commandes slash (`crates/agent-tui/src/state.rs:58-75`) n'incluent ni `/status` ni `/feedback`. Les erreurs provider sont assainies (`crates/agent-provider/src/chatgpt.rs:458-476` redaction bearer/tokens/account ids, teste `:974-985`) mais aucun header de correlation n'est lu. La persistance est un JSONL de `Message | CompactBoundary | CompactCheckpoint | FileHistorySnapshot` (`crates/agent-session/src/lib.rs:1-33`) : pas de payload brut requete/reponse, donc pas d'equivalent rollout-trace. Cote tests, la densite est honnete : ~570 fonctions de test sur 38 958 LoC, dont 45 dans `crates/agent-tools/src/tests_integration.rs` (dispatch concurrent, pipeline fail-closed, 5 modes de permission, taint, sur un vrai workspace temporaire), 27 dans `crates/agent-core/src/lib.rs` pilotant la boucle complete via un `MockProvider` scriptable qui capture les requetes emises (`:68-121`, `:281`), 16 dans `crates/agent-core/src/compaction.rs` via `StubProvider` (`:297-341`), 17 tests de mapping SSE fail-closed sur payloads verbatim (`crates/agent-provider/src/chatgpt_events.rs:434-765`), 19 dans `chatgpt.rs` dont deux qui montent un vrai `TcpListener` pour valider le header timeout et le silence de flux (`:1073-1084`, `:307-344`), et 3 dans `crates/agent-sandbox/src/proxy.rs:140-199` sur de vraies sockets. Le TUI teste via `ratatui::TestBackend` avec dump plein cadre et `assert_buffer_lines` (`crates/agent-tui/src/render.rs:1644-1663`, `crates/agent-tui/src/insert_history.rs:250-290`), plus des assertions de securite sur la sanitisation ANSI (`render.rs:2156`, `history_cell.rs:5675-5684`). `crates/agent-mcp/tests/` est le seul repertoire de tests d'integration inter-crate (`config_load.rs`, `stdio_lifecycle.rs` qui spawn un vrai process stdio). Il n'existe aucun fichier `.snap`, aucune dependance `insta`, `wiremock`, `httpmock`, `mockito`, `pretty_assertions` ou `tempfile` (les seules dev-deps sont `tokio` dans 4 crates), aucun panic hook, et aucun `.github/workflows` : rien n'execute ces tests automatiquement. L'OTel et les tests VCR sont explicitement planifies en Phase 3 (`docs/ROADMAP.md:120`, `:131-132`, `docs/CURRENT_STATUS.md:24`, `docs/DECISIONS.md:138`, `:203`).

#### Écarts pertinents

##### Le scrollback inline n'est teste que sur le buffer visible, pas sur le flux ANSI reellement emis

`majeur` · `partial` · effort `M`

**Impact.** L'insertion au-dessus du viewport inline, le reflow au resize et le commit de lignes sont les zones ou le TUI corrompt l'affichage. Le commit recent 99cce34 'fix(tui): rebuild the inline viewport when the terminal grows' est exactement cette classe de bug, et aucun test de non-regression au niveau du flux ANSI ne peut la capturer aujourd'hui.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/tests/test_backend.rs:1-4 (reexport `VT100Backend`) ; /home/arthur/dev/codex/codex-rs/tui/tests/suite/vt100_history.rs:22-53 (le backend rejoue l'ANSI dans un parseur vt100 puis asserte `backend().vt100().screen().contents()`) ; suites soeurs vt100_live_commit.rs et resize_reflow.rs ; dep `vt100` a tui/Cargo.toml:163

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/insert_history.rs:250-290 utilise `ratatui::backend::TestBackend` avec `Viewport::Inline(1)` et `assert_buffer_lines` : TestBackend ne modelise pas le scrollback ni le defilement du terminal hote. Meme approche dans render.rs:1644-1663 et term.rs:183-201

**Statut documentaire.** tasks/prd-codex-tui-parity.md:80 exige des 'tests terminal pour resize, paste, streaming' ; le status JSON marque EP-005 DONE

##### Zero snapshot de rendu alors que le PRD parite TUI en exige au moins 20 et est marque DONE

`majeur` · `divergent` · effort `M`

**Impact.** Deux consequences. D'abord une regression de rendu (espacement, prefixe, couleur, troncature) passe si l'assertion `contains` reste satisfaite, ce qui est le mode d'echec normal d'un TUI. Ensuite le status JSON declare atteint un critere d'acceptation mesurable qui ne l'est pas : la doc de suivi n'est plus fiable comme source de verite sur la couverture.

**Codex.** 663 fichiers `.snap` dans /home/arthur/dev/codex/codex-rs (dont tui/src/chatwidget/snapshots/, tui/src/history_cell/snapshots/, cli/src/doctor/snapshots/codex__doctor__output__tests__doctor_human_report_environment_rows.snap) ; workflow documente a /home/arthur/dev/codex/AGENTS.md:180-196 (`cargo insta pending-snapshots`, `cargo insta show`)

**Pyxis.** `find /home/arthur/dev/pyxis -name '*.snap' -not -path './target/*'` -> aucun resultat ; aucune dep `insta` dans les Cargo.toml. Les assertions TUI sont soit `assert!(out.contains(...))` (/home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1685-1687), soit `assert_buffer_lines` inline (/home/arthur/dev/pyxis/crates/agent-tui/src/insert_history.rs:283-289). Or tasks/prd-codex-tui-parity.md:388 pose 'au moins 20 snapshots couvrent: chat idle, user message, streaming deltas, markdown code, table, exec, tool success, tool error, diff, approval, queue input, resize' et tasks/prd-codex-tui-parity-status.json marque EP-005 et le PRD entier DONE

**Statut documentaire.** tasks/prd-codex-tui-parity.md:388 et :476 ('Codex TUI flows covered by snapshots : 0 -> >=20') ; status marque DONE, donc divergence doc/code a corriger dans un sens ou dans l'autre

##### Aucune CI n'execute les ~570 tests de Pyxis

`moyen` · `absent` · effort `S`

**Impact.** Un filet de test qui n'est jamais declenche automatiquement se degrade silencieusement : une regression introduite dans agent-core ou agent-tui peut etre commitee et poussee sans qu'aucun signal ne la revele. C'est le multiplicateur qui annule la valeur du reste de la suite.

**Codex.** /home/arthur/dev/codex/.github/workflows/rust-ci.yml:1-60 (detection de paths changes + checks Cargo natifs), rust-ci-full-nextest-platform.yml, cargo-deny.yml, blocking-ci.yml ; /home/arthur/dev/codex/AGENTS.md:64-70 impose `just test -p <crate>` puis suite complete avant finalisation

**Pyxis.** `ls -la /home/arthur/dev/pyxis/.github` -> aucun fichier ou dossier ; aucun `justfile`, aucun script de CI a la racine (`ls -a /home/arthur/dev/pyxis` : assets, .cargo, Cargo.toml, .claude, clippy.toml, .codex, CONTRIBUTING.md, crates, docs, spikes, tasks). Les 570 tests reperes par `grep -rc '#\[test\]|#\[tokio::test\]' crates/` ne sont declenches que manuellement

**Statut documentaire.** docs/ROADMAP.md:131-132 place la CI VCR en Phase 3 mais ne mentionne aucune CI de base pour les tests existants

##### Aucun tracing structure : pas de crate tracing, pas de spans, pas de RUST_LOG

`moyen` · `absent` · effort `M`

**Impact.** Quand une session Pyxis se comporte mal (tour qui gele, tool call perdu, boucle de retry), il n'y a rien a lire : ni niveau, ni cible, ni correlation de tour. Le diagnostic se fait par ajout de `eprintln!` temporaires et recompilation, ce qui est incompatible avec un bug non reproductible.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/lib.rs:1246-1259 (layer fmt non bloquant, `EnvFilter::try_from_default_env()` avec defaut `codex_core=info,codex_tui=info,codex_rmcp_client=info`, span events NEW|CLOSE) ; /home/arthur/dev/codex/codex-rs/exec/src/lib.rs:232-238 (`exec_stderr_env_filter`) ; spans metier ex. `codex.exec` avec champs `thread.id`/`turn.id` a exec/src/lib.rs:225-229

**Pyxis.** `grep -rn 'tracing|opentelemetry|telemetry|tracing_subscriber|RUST_LOG' --include=*.rs --include=*.toml /home/arthur/dev/pyxis/crates/` -> aucun resultat. Seul substitut : /home/arthur/dev/pyxis/crates/agent-tui/src/debug_log.rs:29-44, une fonction `log(&str)` ecrivant `millis message` dans un fichier, avec 9 sites d'appel tous lies au viewport

##### L'adapter ChatGPT n'est jamais teste au niveau HTTP : ni faux backend, ni rejeu SSE

`moyen` · `partial` · effort `M`

**Impact.** Tout ce qui se passe entre la requete HTTP et le mapper reste hors filet : construction du body Responses, headers d'auth et originator, classification des statuts, backoff, refresh de token en cours de stream, decoupage des chunks SSE aux frontieres d'evenement. Une derive du wire format du backend ChatGPT casserait Pyxis en silence, ce qui est precisement le risque R4 identifie.

**Codex.** /home/arthur/dev/codex/codex-rs/core/tests/common/responses.rs:1-80 et 1-1755 (mock wiremock `ResponseMock` qui capture chaque requete, expose `single_request`, `saw_function_call`, `function_call_output_text`, plus les constructeurs `ev_*` et `sse()`/`sse_response()`/`mount_sse_once()`) ; core/tests/common/streaming_sse.rs:1-60 (serveur SSE gate chunk par chunk pour tester les livraisons partielles) ; 21 Cargo.toml declarent wiremock

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt_events.rs:434-765 teste le mapper SSE sur des chaines JSON inline (excellent mais purement en memoire) ; /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:1073-1084 est le seul test reseau (un `TcpListener` brut qui n'envoie jamais de headers, pour le timeout). Aucune dev-dep wiremock/httpmock/mockito : `crates/agent-provider/Cargo.toml` [dev-dependencies] ne contient que tokio

**Statut documentaire.** docs/DECISIONS.md:138 et :203 designent les tests VCR comme la mitigation obligatoire de R4 ; docs/ROADMAP.md:132 les qualifie de 'non negociable, bloquant en CI' mais en Phase 3 ; docs/CURRENT_STATUS.md:24 les liste comme non livres

##### Aucun harness d'integration bootant l'agent complet (cablage CLI + sandbox + session + outils)

`moyen` · `partial` · effort `L`

**Impact.** L'ordre critique documente en tete de main.rs (Landlock applique sur le thread principal AVANT le runtime tokio) n'est verifie par aucun test. Idem pour l'interaction resume + compaction + taint scan, ou pour la propagation d'une erreur provider jusqu'a l'affichage TUI. Ces compositions sont exactement la ou un agent casse en production.

**Codex.** /home/arthur/dev/codex/codex-rs/core/tests/common/test_codex.rs:1-70 (`test_codex()` construit un `ThreadManager`/`CodexThread` reel avec config, auth, shell, exec-server, extension registry, thread store et mock server) ; 115 fichiers dans core/tests/suite/ totalisant 98 425 LoC ; AGENTS.md:114-118 impose l'integration test pour tout changement de logique d'agent

**Pyxis.** Le test le plus profond de la boucle injecte au niveau du trait `Provider` a l'interieur du crate : /home/arthur/dev/pyxis/crates/agent-core/src/lib.rs:68-121 (`MockProvider`) et :281. Les seuls tests hors `src/` sont /home/arthur/dev/pyxis/crates/agent-mcp/tests/config_load.rs et stdio_lifecycle.rs. Le cablage reel vit dans /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs (970 lignes) dont les 18 tests ne couvrent que le parsing d'arguments et la resolution de budget (main.rs tests: `parse_args_*`, `run_config_*`, `headless_*`)

**Statut documentaire.** docs/ARCHITECTURE.md via DECISIONS.md:73 revendique un coeur 'testable sans I/O' : l'invariant est tenu, mais rien ne teste la composition

##### Aucun panic hook : un panic laisse le terminal en raw mode sans trace exploitable

`moyen` · `absent` · effort `S`

**Impact.** Un panic dans la boucle de dessin (indexation de buffer, unwrap sur une largeur, arithmetique de viewport) laisse l'utilisateur avec un shell en raw mode et alternate screen, message de panique noye ou invisible. C'est a la fois un defaut d'UX et une perte totale du signal de diagnostic sur le crash le plus grave.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/tui.rs:537-538 (`panic::take_hook()` puis `set_hook`) et /home/arthur/dev/codex/codex-rs/tui/src/lib.rs:1350-1351 (second hook au niveau run_main) ; lib.rs:1156 encapsule meme l'init OTEL dans un `catch_unwind`

**Pyxis.** `grep -rn 'set_hook|panic::|catch_unwind' --include=*.rs /home/arthur/dev/pyxis/crates/` -> aucun resultat. L'entree/sortie terminal est geree par /home/arthur/dev/pyxis/crates/agent-tui/src/term.rs:42-137 sans garde de panique

##### Impossible de dumper ce que le modele voit reellement

`mineur` · `absent` · effort `S`

**Impact.** Le prompt systeme et l'injection de contexte sont la premiere cause de comportement decevant d'un agent, et c'est precisement le diagnostic pose dans tasks/prd-codex-orchestration.md:18. Sans dump, valider une regression de contexte apres compaction ou apres changement d'AGENTS.md exige d'instrumenter le code.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/main.rs:234-235 `codex debug prompt-input` : 'Render the model-visible prompt input list as JSON', avec support d'images ; complete cote test par core/tests/common/context_snapshot.rs:14-40 (`ContextSnapshotRenderMode` Redacted/Full/KindOnly, strip des instructions de capacite et des ids d'items) pour figer le contexte model-visible en test

**Pyxis.** Pyxis construit un contexte projet non trivial (/home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:122-139 : timezone, shell, cwd, plus AGENTS.md selon tasks/prd-codex-orchestration.md:18) et un system prompt long type gpt_5_2 ; aucune option CLI ni slash ne l'expose. `grep 'PYXIS_' --include=*.rs crates/` recense 8 variables (DEBUG_TUI, DEBUG_USAGE, ORIGINATOR, CODEX_CLIENT_VERSION, IDLE_TIMEOUT_SECS, REDUCED_MOTION, HOME, budgets) : aucune ne dumpe le prompt

##### La sonde PYXIS_DEBUG_USAGE ecrit sur stderr depuis le coeur, en contradiction avec la regle du projet

`mineur` · `divergent` · effort `S`

**Impact.** Activer la sonde d'usage en mode TUI corrompt le rendu au moment meme ou l'on cherche a observer un probleme, donc l'outil de diagnostic n'est utilisable qu'en headless. Accessoirement, la regle correcte est deja ecrite dans le repo mais n'est appliquee que dans agent-tui, pas dans agent-core.

**Codex.** /home/arthur/dev/codex/codex-rs/otel/src/events/session_telemetry.rs:926 `sse_event_completed(&self, usage: &TokenUsage, ttft_ms)` : l'usage part dans un evenement tracing structure, jamais sur stderr ; le TUI Codex route tout vers le layer fichier (tui/src/lib.rs:1246) et le mode exec filtre stderr explicitement (exec/src/lib.rs:232-238)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/agent.rs:561-570 : `if std::env::var_os("PYXIS_DEBUG_USAGE").is_some() { ... eprintln!("[usage] backend input=...") }`, execute dans la boucle de streaming du coeur. Or /home/arthur/dev/pyxis/crates/agent-tui/src/debug_log.rs:1-2 pose la regle inverse : 'le terminal appartient au rendu, un eprintln! y corromprait l'affichage'

**Statut documentaire.** docs/DECISIONS.md:73 (ADR-3) : agent-core n'emet QUE des evenements structures, jamais d'ANSI ; l'eprintln contrevient a l'esprit de l'invariant

#### Écarts discutables

##### Aucun identifiant de correlation backend capture sur erreur

`mineur` · `absent` · effort `S`

**Impact.** Sur un 401/429/500 intermittent du backend ChatGPT, Pyxis ne peut ni correler deux occurrences entre elles, ni distinguer un rejet d'originator d'une expiration de token, ni fournir un identifiant a une trace cote serveur. Le cout d'implementation est de quelques lignes pour une capacite de diagnostic disproportionnee sur le seul canal reseau du produit.

**Codex.** /home/arthur/dev/codex/codex-rs/response-debug-context/src/lib.rs:5-54 extrait `x-request-id`, `x-oai-request-id`, `cf-ray`, `x-openai-authorization-error` et decode le base64 de `x-error-json` en code d'erreur ; consomme a core/src/client.rs:1033, :1475, :1494, :1677, :2052, :2174, :2326, :2392 et model-provider/src/models_endpoint.rs:199 ; retransmis en tags de feedback (feedback/src/lib.rs:58-60 `auth_request_id`, `auth_cf_ray`)

**Pyxis.** `grep -rn 'request_id|x-request-id|cf-ray|x-oai' --include=*.rs /home/arthur/dev/pyxis/crates/` ne renvoie que des occurrences de `previous_response_id` (chatgpt_request.rs:9, :307). Le traitement d'erreur HTTP a /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:326 lit `resp.text()` et l'assainit (`sanitize_error_body`, :458-476) sans jamais consulter `resp.headers()`

##### Pas de fichier de log de session : seule une trace TUI opt-in ecrite dans le workspace

`mineur` · `partial` · effort `S`

**Impact.** Le comportement par defaut est zero trace persistante : quand l'utilisateur constate un probleme, il est trop tard pour l'observer, il faut relancer avec la variable. Et ecrire un log de diagnostic dans le repertoire du projet audite le fait apparaitre dans le git status de l'utilisateur.

**Codex.** /home/arthur/dev/codex/codex-rs/tui/src/lib.rs:1231-1250 ouvre `log_dir/codex-tui.log` en append avec mode `0o600` (nom a tui/src/lib.rs:231) ; `log_dir` resolu depuis la config a core/src/config/mod.rs:4537 ; nettoyage du chemin legacy a tui/src/lib.rs:310

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/debug_log.rs:11-21 : fichier `pyxis-tui-debug.log` ecrit sous le repertoire courant, actif seulement si `PYXIS_DEBUG_TUI` est pose, sans permissions restreintes, et volontairement place dans le workspace ('seul emplacement inscriptible quand le sandbox est actif', :5-6). `PYXIS_HOME` existe pourtant deja comme racine de config (/home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:35)

##### Aucune commande de diagnostic d'installation

`mineur` · `absent` · effort `M`

**Impact.** Quand Pyxis refuse de demarrer ou de s'authentifier (keyring absent, Landlock indisponible sur le noyau, proxy d'entreprise, credential expire), il n'existe aucun moyen d'obtenir un etat consolide. Le diagnostic repose sur la lecture du code source par le mainteneur, ce qui est acceptable pour un usage solo mais bloquant des le premier utilisateur externe.

**Codex.** /home/arthur/dev/codex/codex-rs/cli/src/doctor.rs:1-11 (checks read-mostly sur installation, config, auth, terminal, chemins d'etat, sondes de joignabilite bornees, chaque check rendant une ligne redigee et serialisable) ; :151-171 options `--json --summary --all --no-color --ascii` ; :619-637 redaction du rapport JSON ; modules background.rs, git.rs, runtime.rs, system.rs, thread_inventory.rs, updates.rs

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:67-81 : la constante `HELP` liste 11 options de run et aucune sous-commande ; `parse_args_from` (:88-...) rejette tout positionnel supplementaire (test `parse_args_rejects_extra_positional`). Les 12 commandes slash (/home/arthur/dev/pyxis/crates/agent-tui/src/state.rs:58-75) n'exposent ni /status ni /doctor

##### Aucune capture des payloads bruts requete/reponse pour post-mortem

`mineur` · `absent` · effort `M`

**Impact.** Quand un tour part de travers (arguments de tool call malformes, reasoning perdu, compaction qui coupe mal), le transcript canonique montre le resultat mais pas la cause. Rejouer le probleme demande de le reproduire en live contre un backend non deterministe. La version minimale utile (garder les N derniers couples requete/reponse bruts sous PYXIS_HOME) coute peu.

**Codex.** /home/arthur/dev/codex/codex-rs/rollout-trace/README.md:1-40 et :104-125 : bundle local opt-in via `CODEX_ROLLOUT_TRACE_ROOT` contenant `manifest.json`, `trace.jsonl` (spine d'evenements ordonne par `seq`) et `payloads/*.json` (requetes, reponses, entrees/sorties d'outils, sortie terminal, donnees de compaction), reduit hors ligne en graphe `state.json` par `codex debug trace-reduce` (cli/src/main.rs:238-239) ; principe explicite 'observe first, interpret later', ecriture best-effort qui ne peut jamais faire echouer la session

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-session/src/lib.rs:1-33 et :31-34 : le JSONL de session ne contient que `Message | CompactBoundary | CompactCheckpoint | FileHistorySnapshot`, c'est-a-dire le transcript canonique deja normalise. Aucun stockage de la requete Responses emise ni du flux SSE recu ; `grep -rn 'TRACE|payload' --include=*.rs crates/agent-session/` ne remonte rien de tel

##### Pas de crate de support de test partagee : helpers reimplementes par fichier

`mineur` · `absent` · effort `M`

**Impact.** Chaque nouveau test paie le cout de son propre echafaudage, ce qui pousse mecaniquement vers des tests unitaires etroits plutot que vers les tests de composition qui manquent. C'est la cause structurelle de l'ecart no-cross-crate-integration-harness, pas seulement une question de style.

**Codex.** /home/arthur/dev/codex/codex-rs/core/tests/common/ est un crate `core-test-support` de 6 702 LoC reutilise par les 115 suites (lib.rs 717, responses.rs 1755, test_codex.rs 1273, streaming_sse.rs 714, context_snapshot.rs 787, apps_test_server.rs 773, test_environment.rs 186, tracing.rs 26) ; s'y ajoute le crate `test-binary-support` (test-binary-support/lib.rs:1-40) pour le dispatch arg0 sous test ; AGENTS.md:123 demande explicitement de reutiliser les helpers existants

**Pyxis.** Aucun crate ni module de support de test partage dans /home/arthur/dev/pyxis/crates/. Les helpers sont locaux et reecrits : /home/arthur/dev/pyxis/crates/agent-tools/src/tests_integration.rs:26-40 implemente `TempWs` a la main 'sans dependance tempfile' ; /home/arthur/dev/pyxis/crates/agent-tui/src/render.rs:1647-1663 redefinit `dump`/`draw` ; agent-core/src/lib.rs:68 et agent-core/src/compaction.rs:297 definissent deux doubles de provider distincts et non partages

#### Non applicables à Pyxis

- **Pas d'export OpenTelemetry** (mineur) : Pour un outil mono-utilisateur en dogfood, l'absence d'export est defendable. L'ecart reel n'est pas l'exporteur mais l'absence de la couche `tracing` en dessous (voir no-structured-tracing) : sans elle, ajouter OTel en 
- **Pas de tampon de logs haute fidelite pour rapport d'incident** (mineur) : La partie upload (Sentry, infra OpenAI) est hors scope, mais la partie reutilisable ne l'est pas : conserver en memoire les N derniers Mo de trace complete permet d'ecrire un rapport apres coup, sans avoir prevu d'active

### Angles morts (critique de complétude)

**Parité estimée : partial**

*Surface Codex.* Au-dela des 12 axes deja audites, Codex porte plusieurs capacites de harness dans des crates peu evidentes. `collaboration-mode-templates/templates/{plan,execute,pair_programming,default}.md` + `protocol/src/config_types.rs:676` definissent un mode de collaboration first-class qui lie ensemble un ModeKind, un modele, un reasoning effort et des developer_instructions, injecte et rediffe dans le contexte modele par `core/src/context/world_state/collaboration_mode.rs:17`. `core/src/turn_diff_tracker.rs:49`, instancie par tour a `core/src/session/turn.rs:240`, agrege un diff unifie de tout ce que l agent a modifie pendant le tour, emis en `TurnDiffEvent` (`core/src/session/turn.rs:2556`) et rejouable ailleurs via la sous-commande `codex apply` (`cli/src/main.rs:175`); `git-utils/src/baseline.rs:17` fournit en plus un depot baseline pour diffs et reset hors depot git. Le protocole expose `ExecCommandOutputDelta` (`protocol/src/protocol.rs:1387`, struct 3610) pour streamer stdout/stderr pendant une commande longue. La fidelite shell est traitee serieusement: detection du shell utilisateur (`shell-command/src/shell_detect.rs:271`), invocation en login shell `-lc` (`core/src/shell.rs:22`), snapshot de l environnement interactif reutilise par commande (`core/src/shell_snapshot.rs:43,153`), et parsing semantique des commandes pour l affichage (`shell-command/src/parse_command.rs:30`). Le git du workspace est collecte (`git-utils/src/info.rs:72,891`) puis attache aux metadonnees de tour (`core/src/turn_metadata.rs:120`) et a la barre de statut (`tui/src/chatwidget/status_surfaces.rs:550`). La config est ecrite atomiquement et sans perte de structure via toml_edit (`core/src/config/edit.rs:2,732`). `external-agent-migration/src/lib.rs:1` importe la configuration Claude Code et Cursor (MCP, hooks, fichiers memoire, subagents). Enfin `install-context/src/lib.rs:37` + `cli/src/main.rs:159` + `tui/src/updates.rs` donnent une conscience du mode d installation et une mise a jour en place, et `core/src/client.rs:11` + `core/src/session_startup_prewarm.rs:26` ajoutent un transport WebSocket avec prewarm de session.

*Surface Pyxis.* Cote Pyxis, rien de tout cela n existe. Le mode Plan est purement un filtre de permissions (`crates/agent-tools/src/permission.rs:29,125`): aucun jeu d instructions dedie, aucun modele ni effort lie au mode. `crates/agent-tui/src/diff.rs:1-9` documente explicitement un diff pur, borne, sans I/O, derive du seul `input` de l outil: il n existe aucune agregation de diff au niveau du tour (grep `turn_diff|TurnDiff|unified` sur `crates/` ne retourne rien). `AgentEvent` (`crates/agent-core/src/event.rs:13-34`) n a que des deltas Text et Reasoning; la sortie shell n arrive qu a la completion (`crates/agent-tui/src/app_event.rs:474-489`). L outil bash lance `sh -c` en dur (`crates/agent-tools/src/bash.rs:100-104`) alors que le bloc environnement annonce `$SHELL` au modele (`crates/agent-cli/src/context.rs:121,139`). Git n apparait que comme marqueur de racine pour la remontee AGENTS.md (`crates/agent-cli/src/context.rs:46`); aucune branche, HEAD, remote ou etat sale n est collecte. `crates/agent-cli/src/settings.rs:101-131` fait un read-modify-write puis `std::fs::write` sans verrou ni fichier temporaire. La lecture de config Claude Code se limite aux `mcpServers` (`crates/agent-mcp/src/config.rs:148,151`). Le parseur d arguments n a pas de `--version` (`crates/agent-cli/src/main.rs:110`) et il n existe aucune notion d installation ni de mise a jour. Le provider est SSE stateless assume (`crates/agent-provider/src/chatgpt.rs:4-6`, `docs/PROVIDERS.md:185,198`). Points verifies et juges non significatifs, donc non listes ci-dessous: `codex-file-watcher` (usages app-server seulement, deja couvert par les gaps de rafraichissement a chaud), `codex-secrets` (redaction cablee uniquement dans le produit memories), `message-history` (Pyxis a un equivalent via `agent_session::workspace_prompts`), `arg0`/`.env`, `uds`/`websocket-client`/`stdio-to-uds` (transports app-server, deja couverts), `guardian`/`agent-graph-store`/`analytics`/`connectors`/`memories` (sous-agents ou infra OpenAI, hors scope).

#### Écarts pertinents

##### Aucun mode de collaboration first-class (plan / execute / pair programming)

`majeur` · `absent` · effort `L`

**Impact.** Un mode plan qui se contente de bloquer les ecritures produit un agent qui tente quand meme d editer puis se heurte a des refus, au lieu d un agent a qui l on a dit d explorer et de proposer. Et changer de posture de travail (explorer vs executer) ne peut pas emporter un changement de modele ou d effort, alors que c est precisement l usage: plan en effort eleve, execution en effort moyen.

**Codex.** /home/arthur/dev/codex/codex-rs/collaboration-mode-templates/src/lib.rs:1-4 embarque quatre templates (plan.md, execute.md, pair_programming.md, default.md); /home/arthur/dev/codex/codex-rs/protocol/src/config_types.rs:676 definit CollaborationMode { mode: ModeKind, settings: Settings } ou Settings porte model, reasoning_effort et developer_instructions, avec apply_mask (ligne 727) et with_updates (ligne 702) pour changer de mode en session; /home/arthur/dev/codex/codex-rs/core/src/context/world_state/collaboration_mode.rs:17-27 et 50-62 injectent les instructions du mode comme fragment developer et ne les reemettent que si le mode a change

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/permission.rs:29 declare PermissionMode::Plan, mais 125-126 et 153 montrent qu il ne fait que refuser toute mutation: aucun jeu d instructions, aucun modele ni effort attache. grep -rn "Plan" sur /home/arthur/dev/pyxis/crates/agent-cli/src/prompt.rs et interactive.rs ne retourne rien: le prompt systeme est identique dans tous les modes

##### Aucun suivi du diff agrege du tour, ni baseline, ni rejeu du diff

`majeur` · `absent` · effort `L`

**Impact.** Impossible de repondre a la question la plus frequente en fin de tour: qu est-ce qui a change au total. Chaque edit est affiche isolement, les modifications faites par une commande bash sont invisibles, et il n existe aucun artefact de diff exportable. Cela bloque directement l ambition Paneflow de review par hunk.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/turn_diff_tracker.rs:49 definit TurnDiffTracker; /home/arthur/dev/codex/codex-rs/core/src/session/turn.rs:240-241 en instancie un par tour avec des display roots, 2365 marque should_emit_turn_diff, 2556 emet l evenement; /home/arthur/dev/codex/codex-rs/cli/src/main.rs:175 expose `codex apply` pour rejouer le dernier diff produit par l agent en `git apply`; /home/arthur/dev/codex/codex-rs/git-utils/src/baseline.rs:17-50 fournit un depot baseline et diff_since_latest_init pour les workspaces sans git

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/diff.rs:1-9 documente un diff derive du seul `input` de l outil, pur et sans I/O, avec des numeros de ligne relatifs aux fragments d entree faute de relire le disque. grep -rn "turn_diff|TurnDiff|unified" --include=*.rs sur /home/arthur/dev/pyxis/crates ne retourne aucune occurrence

**Statut documentaire.** docs/ROADMAP.md:118 annonce la review par hunk cote Paneflow, qui presuppose ce tracker

##### La sortie des commandes shell n est rendue qu a la completion

`majeur` · `absent` · effort `M`

**Impact.** Un `cargo build` ou une suite de tests de trois minutes laisse l ecran muet, sans moyen de distinguer une compilation en cours d un blocage. C est aussi ce qui rend l interruption utile: sans sortie live, l utilisateur ne sait pas quoi interrompre.

**Codex.** /home/arthur/dev/codex/codex-rs/protocol/src/protocol.rs:1387 declare la variante ExecCommandOutputDelta, structuree en 3610, qui achemine des fragments de stdout/stderr pendant l execution

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-core/src/event.rs:13-34: AgentEvent n a que Text et Reasoning comme deltas; ToolResult (24, 43-52) porte le contenu complet. /home/arthur/dev/pyxis/crates/agent-tui/src/app_event.rs:474-489 ne construit TranscriptPayload::ExecOutput que dans la branche TranscriptLifecycle::Completed

##### bash s execute en `sh -c` alors que le contexte annonce $SHELL au modele

`majeur` · `divergent` · effort `M`

**Impact.** Le modele recoit une information fausse: on lui dit zsh et on execute sh (dash sur beaucoup de systemes). Il produira des constructions zsh/bash valides (globs recursifs, substitution de processus, `[[ ]]`) qui echoueront, et aucun alias, fonction ou PATH du profil utilisateur n est disponible. C est un ecart de correction, pas seulement de confort.

**Codex.** /home/arthur/dev/codex/codex-rs/shell-command/src/shell_detect.rs:271 detecte le shell de connexion; /home/arthur/dev/codex/codex-rs/core/src/shell.rs:22-30 derive les arguments d exec et sait invoquer en login shell (`-lc`); /home/arthur/dev/codex/codex-rs/core/src/shell_snapshot.rs:43-45 et 153 capturent une fois l environnement interactif de l utilisateur, avec validation et retention, pour le reutiliser a chaque commande sans repayer le cout du profil

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tools/src/bash.rs:100-104 lance `Command::new("sh").arg("-c")` en dur et la description outil ligne 39 annonce `sh -c`; /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:121 et 139 injectent pourtant `<shell>` depuis `std::env::var("SHELL")`, donc typiquement /bin/zsh

##### Aucune conscience git du workspace (branche, HEAD, remote, etat sale)

`moyen` · `absent` · effort `M`

**Impact.** Le modele ignore sur quelle branche il travaille et si l arbre etait deja sale avant son intervention, ce qui change la reponse a « commit ceci » ou « qu ai-je casse ». Cote session, rien ne permet de retrouver a quel etat du depot une session correspondait.

**Codex.** /home/arthur/dev/codex/codex-rs/git-utils/src/info.rs:72 collect_git_info, 151 get_head_commit_hash, 268 get_has_changes, 320 recent_commits, 891 current_branch_name; consomme dans /home/arthur/dev/codex/codex-rs/core/src/turn_metadata.rs:120 (repo root attache aux metadonnees de tour), /home/arthur/dev/codex/codex-rs/core/src/session/session.rs:874 et /home/arthur/dev/codex/codex-rs/tui/src/chatwidget/status_surfaces.rs:445,550 (branche dans le statut)

**Pyxis.** grep -rn "git" --include=*.rs sur /home/arthur/dev/pyxis/crates ne remonte que /home/arthur/dev/pyxis/crates/agent-cli/src/context.rs:46 (`.git` comme borne de remontee AGENTS.md) et /home/arthur/dev/pyxis/crates/agent-cli/src/interactive.rs:1634 (`.git` exclu du scan de fichiers). Aucune commande git n est jamais executee, aucune branche ni SHA n est collecte

##### Persistance des preferences non atomique et sans verrou, alors que Paneflow lance N instances

`moyen` · `divergent` · effort `S`

**Impact.** Deux panes Pyxis qui changent de modele ou de mode de permission au meme moment se perdent mutuellement leurs ecritures, et une interruption en plein `write` laisse un settings.toml tronque. Le scenario est structurel pour un agent concu pour tourner en flotte dans Paneflow.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/config/edit.rs:2 importe write_atomically et 732 l applique au document TOML edite, avec un modele de document preservant commentaires et structure (edit/document_helpers.rs)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/settings.rs:104-131: lecture integrale, reconstruction ligne a ligne, puis `std::fs::write(path, contents)` direct, sans fichier temporaire, sans rename, sans verrou. Le fichier est unique et global (`~/.pyxis/settings.toml`, ligne 38)

##### Aucun import de configuration depuis Claude Code ou Cursor au-dela des serveurs MCP

`moyen` · `partial` · effort `M`

**Impact.** L ecosysteme de depart de l utilisateur est Claude Code: les CLAUDE.md, hooks et agents existants representent l essentiel de la configuration accumulee. Sans chemin d import, chaque projet doit etre reconfigure a la main pour Pyxis, ce qui est le principal frein a l adoption d un harness concurrent sur une machine deja equipee.

**Codex.** /home/arthur/dev/codex/codex-rs/external-agent-migration/src/lib.rs:1 declare des helpers de migration; les modules couvrent les hooks Claude (hooks_cla.rs) et Cursor (hooks_cur.rs), les serveurs MCP (mcp.rs, build_mcp_config_from_json_file), les fichiers memoire (memory.rs, memory_import.rs, discover_external_memory_files), les subagents (subagents.rs), les plugins (plugins.rs) et les sessions (sessions/), avec detection de source (detect/) et rapport (reporting.rs)

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-mcp/src/config.rs:1,148,151 ne lit que `.mcp.json` du workspace et les `mcpServers` de `~/.claude.json`. grep -rn "CLAUDE.md|claude/agents|claude/hooks" --include=*.rs sur /home/arthur/dev/pyxis/crates ne retourne rien: les fichiers memoire, hooks, skills et subagents Claude Code ne sont jamais importes

##### Aucune lecture semantique des commandes shell pour l affichage

`mineur` · `absent` · effort `M`

**Impact.** Un pipeline `cd sub && rg -n foo | head -50` s affiche comme une chaine tronquee au lieu de dire ce qui est reellement fait et ou. Sur un transcript long, la lisibilite de l activite outil est ce qui permet de superviser un agent sans relire chaque commande.

**Codex.** /home/arthur/dev/codex/codex-rs/shell-command/src/parse_command.rs:30 parse_command decompose une ligne de commande en segments typees, suit les `cd` pour calculer les chemins (1298-1302), scinde sur les connecteurs et deduplique les segments consecutifs, produisant un resume par etape plutot que la commande brute

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-tui/src/tool.rs:36 mappe l outil bash au libelle fixe "Run" et 42 affiche la premiere ligne de la commande tronquee a 64 caracteres (helper ligne 209)

#### Écarts discutables

##### Aucun contexte d installation, aucune version rapportee, aucune mise a jour

`mineur` · `absent` · effort `M`

**Impact.** Un binaire qui ne sait pas dire sa version rend tout rapport d incident ambigu et empeche de correler un comportement a un build. Le sujet reste secondaire tant que Pyxis se construit depuis les sources, mais devient bloquant des la premiere distribution.

**Codex.** /home/arthur/dev/codex/codex-rs/install-context/src/lib.rs:37-60 modelise InstallContext { method, package_layout } avec les variantes Standalone, Npm, Bun, Pnpm et un layout bin/resources/path; /home/arthur/dev/codex/codex-rs/cli/src/main.rs:159 expose la sous-commande `update`; /home/arthur/dev/codex/codex-rs/tui/src/updates.rs, updates_cache.rs et update_prompt.rs gerent la detection de version disponible et l invite in-app

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-cli/src/main.rs:110 ne reconnait que `-h`/`--help` parmi les commutateurs meta: aucun `--version`. grep -rn "CARGO_PKG_VERSION" --include=*.rs sur /home/arthur/dev/pyxis/crates ne retourne rien; aucun module de mise a jour n existe

**Statut documentaire.** docs/ROADMAP.md:17 prevoit la publication d un crate racine et d un binaire `pyxis`, ce qui reactive ce besoin

##### Pas de transport WebSocket Responses ni de prechauffage de session

`mineur` · `absent` · effort `XL`

**Impact.** La latence percue au premier token du premier tour reste celle d un handshake TLS complet plus un cache prompt froid. L effet est mesurable surtout sur les sessions courtes et repetees, typiques d un usage en flotte de panes.

**Codex.** /home/arthur/dev/codex/codex-rs/core/src/client.rs:11-24 decrit un ModelClientSession par tour qui met en cache une connexion WebSocket Responses, conserve le jeton x-codex-turn-state pour le routage collant, et effectue un prewarm `response.create` avec generate=false avant la premiere requete pour reutiliser la connexion; /home/arthur/dev/codex/codex-rs/core/src/session_startup_prewarm.rs:26-38 lance ce prechauffage des le demarrage de session avec timeout et resolution Ready/Unavailable

**Pyxis.** /home/arthur/dev/pyxis/crates/agent-provider/src/chatgpt.rs:1-10 et 157-160: un unique `reqwest::Client` HTTP, SSE stateless, aucune notion de connexion persistante par tour ni de requete de prechauffage. grep -rn "prewarm|websocket" --include=*.rs sur /home/arthur/dev/pyxis/crates ne retourne rien

**Statut documentaire.** docs/PROVIDERS.md:185,198 rejette explicitement l etat conversationnel serveur (previous_response_id) comme incompatible avec le transcript client-side; le prewarm Codex est justement cable sur ce chemin v2, donc l adoption suppose de rouvrir cette decision
