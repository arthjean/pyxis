# Couche multi-provider

> La couche multi-provider est le cœur de Numen. C'est elle qui justifie le projet : « qualité Claude Code, **tous** les modèles frontier ». Là où Claude Code est Anthropic-only, Numen est multi-provider first-class. Ce document est la source de vérité de cette couche.

**Docs liées.** Décisions : [`docs/DECISIONS.md`](./DECISIONS.md) (ADR-4 = couche provider, ADR-7 = roadmap/risque). Architecture transverse : [`docs/ARCHITECTURE.md`](./ARCHITECTURE.md). Plan d'exécution : [`docs/ROADMAP.md`](./ROADMAP.md). Ce document est la version détaillée d'ADR-4 ; toute divergence de signature entre ADR-4 et ce fichier se résout en faveur de ce fichier (taxonomie d'erreurs notamment, cf. §5.1).

---

## 0. Note de nommage (crates)

Le brief réserve les crates publiables `numen`, `numen-cli`, `numen-core` (confirmés libres sur crates.io — atout décisif cargo). Le **workspace interne** est nommé en `agent-*` (`agent-core`, `agent-provider`, `agent-tui`, …) pour décrire la fonction de chaque crate sans préfixe de marque. Convention retenue, à acter dans ADR-1/ADR-5 :

- Crate racine publiée + binaire installé = **`numen`** (réexporte le wiring d'`agent-cli`).
- Crates internes du workspace = **`agent-*`** (non destinées à une publication standalone à ce stade).

Ce document emploie les noms internes `agent-*`. La réservation `numen*` couvre la façade publique et le binaire.

---

## 1. Décision : couche maison + format canonique Anthropic-like

### 1.1 La décision

Numen implémente sa **propre couche provider**, en Rust natif, sur `reqwest` + `eventsource-stream`. Pas d'abstraction tierce au centre. Le format interne canonique est **Anthropic-like** (content blocks : `text`, `tool_use`, `tool_result`, `thinking`, `image` — soit `ContentBlock::Text/ToolUse/ToolResult/Thinking/Image`). Chaque provider possède un **adapter** qui traduit son wire format vers/depuis ce canonique. Toutes les divergences sont **localisées dans l'adapter** — le reste du système (`agent-core`, `agent-tools`, `agent-session`) ne connaît que le canonique.

```
agent-core  ──(canonique)──►  agent-provider  ──(adapter)──►  wire format provider
   ▲                              trait Provider                     │
   └──────────(StreamEvent canonique)◄───────────────────────────────┘
```

**Pourquoi Anthropic-like comme canonique ?** Parce que le content-block model d'Anthropic est le plus expressif du marché (thinking blocks, `tool_use`/`tool_result` first-class, `cache_control` granulaire, multimodal). Mapper *vers* un modèle riche est plus simple que de mapper vers le plus petit dénominateur commun (Chat Completions). L'adapter Anthropic devient une quasi-identité ; les autres adapters font la traduction. Les SDK Anthropic communautaires (`anthropic-sdk-rs`) servent de **référence de wire format**, jamais de dépendance.

### 1.2 Pourquoi pas une abstraction tierce comme stratégie centrale

| Option écartée | Nature | Raison de l'écart |
|---|---|---|
| **LiteLLM** | Proxy de normalisation (Python) | Hop réseau supplémentaire + normalisation **lossy** (perte de thinking, `cache_control`, usage détaillé). On hérite de leur surface de bugs et de leur cadence. Inacceptable pour une CLI « ultra-perf » in-process. |
| **Vercel AI SDK** | SDK TS | TS-only — incompatible avec un cœur Rust. **Bonne inspiration d'interface** (le design de `streamText` / parts), rien de plus. On reprend l'idée, pas le code. |
| **genai** (crate Rust) | Lib multi-provider Rust | Beta, surface instable, abstractions qui ne couvrent pas thinking/caching/taint comme on en a besoin. On veut contrôler le canonique nous-mêmes. |
| **OpenRouter** | Méta-routeur OpenAI-compat | **C'est un adapter, pas la stratégie.** On l'intègre *comme un provider parmi d'autres*, on ne construit pas dessus. Il perd les features natives (cache Anthropic, thinking Gemini). |

Conséquence : on assume d'écrire et de maintenir ~6 adapters. Le prix est réel mais c'est exactement le différenciateur du produit — on ne le sous-traite pas à une couche qui nous coûterait en perte de fidélité et en latence.

---

## 2. Le contrat : `Provider`, `StreamEvent`, `Capabilities`

`agent-provider` expose un trait unique. `agent-core` ne dépend **jamais** d'un adapter concret — il consomme `dyn Provider` (object-safe) et le flux canonique `StreamEvent`. Le type de requête échangé est `CanonicalRequest` : c'est exactement ce que renvoie `ctx.request()` dans `agent-core` (cf. `ARCHITECTURE.md §3.4`, appel `deps.provider.stream(&ctx.request())`).

```rust
/// Implémenté par chaque adapter. Object-safe (consommé en dyn Provider).
#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> &Capabilities;

    /// Le chemin chaud : un flux d'événements canoniques, jamais d'ANSI,
    /// jamais de wire format brut. L'adapter réassemble/normalise en amont.
    async fn stream(&self, req: CanonicalRequest)
        -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;

    /// Variante non-stream (utilitaire : titres de session, classification interne).
    async fn complete(&self, req: CanonicalRequest)
        -> Result<CanonicalResponse, ProviderError>;

    /// Traduit une erreur transport/HTTP/wire en classe d'erreur canonique.
    /// Source de vérité du retry (voir §5.1).
    fn classify_error(&self, err: &ProviderError) -> ErrorClass;
}

pub enum ProviderKind {
    Anthropic,
    OpenAiChat,        // Chat Completions  — CIBLE MVP
    OpenAiResponses,   // Responses API     — gated, opt-in
    Gemini,
    Ollama,
    OpenRouter,
    // Bedrock / Vertex / Azure ne sont PAS des kinds : ce sont des
    // injections d'auth/endpoint au-dessus d'Anthropic/OpenAi/Gemini.
}
```

```rust
/// Le seul vocabulaire de streaming que agent-core connaît.
/// Tout adapter doit produire CETTE séquence, quelle que soit la source.
pub enum StreamEvent {
    TextDelta      { text: String },
    ReasoningDelta { text: String },                       // thinking / reasoning
    ToolCallStart  { id: ToolCallId, name: String },
    ToolCallDelta  { id: ToolCallId, args_json: String },  // arguments incrémentaux
    ToolCallEnd    { id: ToolCallId },                      // call complet & parseable
    Usage          { usage: TokenUsage },                  // peut ne jamais arriver (Ollama)
    Done           { stop: StopReason },
}

/// Invariant fort : à ToolCallEnd, args_json concaténé DOIT être un JSON
/// complet et valide. Les adapters qui fragmentent (Gemini) ou bufferisent
/// (OpenAI tool_calls par index) réassemblent AVANT d'émettre ToolCallEnd.
```

```rust
/// Déclaré statiquement par chaque adapter. Lu par agent-core pour gater
/// les comportements (caching, thinking, server-side state, multimodal).
pub struct Capabilities {
    pub vision: bool,
    pub tools: bool,
    pub prompt_caching: bool,
    pub reasoning: bool,            // thinking / reasoning natif
    pub server_side_state: bool,    // false sauf OpenAI Responses (previous_response_id)
    pub max_context: u32,
}

pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
}
```

### 2.1 Frontière `StreamEvent` ↔ `AgentEvent`

Deux enums d'événements coexistent dans le système, **distincts par design** — ne pas les confondre :

| Enum | Producteur → consommateur | Rôle |
|---|---|---|
| `StreamEvent` | adapter `agent-provider` → `agent-core` | vocabulaire **provider-vers-cœur** : deltas de texte/reasoning, cycle de vie des tool calls, usage, stop. Normalisé en amont par l'adapter. |
| `AgentEvent` | `agent-core` → clients (TUI, `-p` headless, Paneflow) | vocabulaire **cœur-vers-clients** : événements structurés de plus haut niveau (transition d'état, message persisté, hunk de diff, demande de permission). Jamais d'ANSI. Défini dans `ARCHITECTURE.md §10`. |

`agent-core` **consomme** `StreamEvent`, fait avancer sa state machine, et **émet** `AgentEvent` vers les clients. La frontière est nette : un adapter ne produit jamais d'`AgentEvent`, le cœur ne réémet jamais de `StreamEvent` brut. C'est ce qui permet à Paneflow d'embarquer `agent-core` in-process et de rendre les `AgentEvent` via GPUI sans toucher à la couche provider.

### 2.2 Taxonomie d'erreurs canonique (`ErrorClass`)

`ErrorClass` est le type **canonique** de classification, nommé `ErrorClass` partout (et non `ErrClass`). Le brief décrivait une forme minimale à 4 classes (`Retryable | Overloaded(529) | Auth | InvalidRequest`) ; ce document fixe la forme **complète à 5 classes** avec `Auth` paramétré, qui est la version canonique à propager dans ADR-4, `ROADMAP.md` et `ARCHITECTURE.md §3.4`. La variante `Overloaded` **porte le code HTTP** (`Overloaded(u16)`, instancié `Overloaded(529)`) — signature identique dans tous les docs.

```rust
pub enum ErrorClass {
    Retryable,                 // 5xx transitoires, timeouts réseau
    Overloaded(u16),           // 529 (porte le code) — backoff agressif
    RateLimited,               // 429 — honore Retry-After
    Auth(AuthError),           // 401/403 — refresh, ou go/no-go
    InvalidRequest,            // 4xx non-retryable (400, 422)
}

pub enum AuthError {
    Expired,                   // token expiré → refresh OAuth
    ThirdPartyBlocked,         // blocage Anthropic des outils tiers (voir §5.2 / §6)
    Invalid,                   // creds invalides → erreur utilisateur
}
```

> **Réconciliation cross-docs (à acter).** Trois formulations préexistaient : (a) brief / ADR-4 / ROADMAP : 4 classes, `Auth` nu ; (b) ce fichier : 5 classes + `Auth(AuthError)` ; (c) `ARCHITECTURE.md §3.4` : `ErrClass` (nom divergent), 4 variantes. La forme canonique est **(b)**. Action : mettre à jour ADR-4 et ROADMAP pour ajouter `RateLimited` + `Auth(AuthError)`, et renommer `ErrClass` → `ErrorClass` dans `ARCHITECTURE.md`, en harmonisant la signature `Overloaded(u16)`.

**Règle d'architecture.** `agent-core` ne dépend ni de `agent-tui` ni d'un `agent-provider` *concret*. Il dépend du **trait** `Provider` (injecté). Le `ContextBudget` est calculé **une fois par modèle** à partir de `capabilities().max_context` — source unique de vérité pour la compaction.

---

## 3. Grand tableau de divergences par provider

Chaque cellule décrit ce que l'**adapter** doit faire pour ramener le provider au canonique Anthropic-like. Tout ce qui n'est pas « identité » est du code localisé dans l'adapter.

| Axe | Anthropic | OpenAI **Chat** (MVP) | OpenAI **Responses** (gated) | Gemini | Ollama | OpenRouter | Bedrock / Vertex / Azure |
|---|---|---|---|---|---|---|---|
| **Surface** | Messages API | `/chat/completions` | `/responses` | `generateContent` (stream) | OpenAI-compat | OpenAI-compat (méta) | Réutilise l'adapter sous-jacent |
| **Tool schema** | `tools[].input_schema` (JSON Schema) — **canonique** | `tools[].function.parameters` | idem Chat mais sous `tools` Responses | `functionDeclarations[].parameters` (sous-ensemble OpenAPI) | comme Chat | comme Chat | hérité de l'adapter de base |
| **Tool args en stream** | deltas dans `input_json_delta` → mappe direct | `tool_calls[idx].function.arguments` par **index**, à bufferiser | idem Chat | **fragmentés** : un function call peut arriver en plusieurs morceaux → **réassembler** (voir §4.2) | comme Chat | comme Chat | hérité |
| **Tool result (renvoi)** | bloc `tool_result` (canonique) | message `role:"tool"` + `tool_call_id` | `function_call_output` via input items | `functionResponse` part | message `role:"tool"` | message `role:"tool"` | hérité |
| **System prompt** | champ `system` top-level (cacheable) | message `role:"system"` (ou `developer`) | `instructions` | `systemInstruction` (objet séparé) | message `role:"system"` | message `role:"system"` | hérité |
| **Stop reason** | `stop_reason` (`end_turn`/`tool_use`/`max_tokens`) | `finish_reason` (`stop`/`tool_calls`/`length`) | `status` + `incomplete_details` | `finishReason` (`STOP`/`MAX_TOKENS`/`SAFETY`) | `finish_reason` (souvent partiel) | `finish_reason` | hérité |
| **Usage timing** | dans le stream (`message_delta.usage`), fiable | `usage` en fin si `stream_options.include_usage` | dans l'event `response.completed` | `usageMetadata` en fin de stream | **souvent ABSENT** → fallback tokenizer (§4.3) | dépend du modèle routé, **non garanti** → fallback | hérité (+ Bedrock facture différemment) |
| **Caching** | `cache_control: ephemeral`, TTL 1h, explicite | implicite (prefix auto, non contrôlable) | implicite + `previous_response_id` | `cachedContent` (contexte explicite, API séparée) | aucun | dépend du modèle routé | hérité (Bedrock : prompt caching propre) |
| **Thinking / reasoning** | `thinking` blocks, budget adaptatif | `reasoning_effort` (o-series), reasoning **non visible** | reasoning items (résumés) | `thinkingConfig` / `thinkingBudget` | rarement | dépend du modèle | hérité |
| **État conversation** | client-side (transcript) | **client-side** (transcript) → mappe proprement | **server-side** (`previous_response_id`) → **ne mappe pas** (§4.1) | client-side | client-side | client-side | hérité |
| **Auth** | API key **ou** OAuth (go/no-go §6) | API key | API key | API key Google | aucune (local) | API key OpenRouter | **SigV4** / **OAuth Google** / **endpoint custom** injectés via `agent-auth` |
| **Multimodal** | `image` content block | `image_url` parts | input items image | `inlineData` parts | selon modèle | selon modèle | hérité |
| **Betas** | en-têtes `anthropic-beta` (gated `kind==Anthropic`) | — | — | — | — | — | n/a |

**Lecture clé.** L'adapter Anthropic est une quasi-identité. OpenAI Chat est la **cible MVP** parce que le mapping est propre (état client-side, tool result trivial). Bedrock/Vertex/Azure ne sont **pas des adapters complets** : ce sont des couches d'auth/endpoint au-dessus d'Anthropic/OpenAI/Gemini — toutes les creds passent par `agent-auth`.

---

## 4. Pièges explicites

### 4.1 OpenAI Responses API ne mappe pas sur le canonique

La Responses API maintient l'**état conversationnel côté serveur** (`previous_response_id`). Notre canonique repose sur un **transcript client-side** (on reconstruit l'historique à chaque tour, indispensable pour la compaction, le resume JSONL, le replay des sessions). Les deux modèles sont **incompatibles par design**.

Décision :
- **Chat Completions = cible MVP** d'OpenAI. Transcript client-side → mapping propre vers le canonique.
- **Responses API = mode gated optionnel**, exposé via `capabilities().server_side_state == true`. **Jamais le défaut.** `agent-core` ne l'active que sur opt-in explicite, et la compaction / le resume sont alors dégradés (l'état vit côté OpenAI).

```rust
// agent-core, avant d'envoyer : ne PAS supposer le server-side state.
if provider.capabilities().server_side_state {
    // chemin gated, opt-in : previous_response_id, compaction limitée
} else {
    // chemin par défaut : transcript complet reconstruit côté client
}
```

### 4.2 Gemini : function calls fragmentées en stream

Gemini **peut** émettre un function call en plusieurs morceaux sur le stream (le `name` et des parties de `args` arrivent séparément). Émettre un `ToolCallEnd` prématuré casserait l'invariant « `args_json` complet à `ToolCallEnd` ».

L'adapter Gemini **bufferise et réassemble** le function call complet avant d'émettre `ToolCallEnd` :

```rust
// adapter Gemini : accumulateur par call, flush à la complétion détectée.
let mut pending: HashMap<ToolCallId, PartialCall> = HashMap::new();
// ... pour chaque part du stream :
//   - functionCall.name présent  → ToolCallStart + init accumulateur
//   - fragment d'args            → ToolCallDelta + append à pending[id]
//   - call complet (part close)  → valider JSON, puis ToolCallEnd(id)
// Ne JAMAIS émettre ToolCallEnd tant que pending[id] n'est pas un JSON valide.
```

### 4.3 Ollama : usage absent → fallback tokenizer obligatoire

Ollama (OpenAI-compat) **n'émet souvent pas** d'`usage` dans le stream. Sans comptage de tokens, la compaction est cassée (`ContextBudget` ne sait pas quand déclencher microcompact/autocompact).

`agent-core::update_budget` lit le `Usage` du stream **si présent**, sinon **retombe sur `agent-tokenizer`** (tiktoken-rs / tokenizers) pour compter localement input + output. C'est non négociable pour tout provider sans usage fiable (Ollama, certains modèles via OpenRouter).

```rust
fn update_budget(&mut self, ev: &StreamEvent, transcript: &Transcript) {
    match ev {
        StreamEvent::Usage { usage } => self.budget.apply(usage),   // chemin nominal
        StreamEvent::Done { .. } if !self.saw_usage => {
            // fallback : comptage local, sinon compaction cassée sur Ollama
            let counted = self.tokenizer.count(transcript);
            self.budget.apply_estimated(counted);
        }
        _ => {}
    }
}
```

---

## 5. Sous-systèmes transverses

### 5.1 Retry & classification d'erreurs

`classify_error` (par adapter) est la **source de vérité** du retry. La logique de backoff vit dans `agent-provider`, au-dessus des adapters. La taxonomie est `ErrorClass` (§2.2).

| Classe | Déclencheur | Politique |
|---|---|---|
| `Retryable` | 5xx transitoire, timeout réseau | backoff exponentiel + jitter |
| `Overloaded(529)` | **529** | backoff **agressif** ; après **3× 529 consécutifs → fallback model** |
| `RateLimited` | 429 | honore **`Retry-After`** (header) avant retry |
| `Auth(Expired)` | 401 token expiré | **refresh OAuth** puis 1 retry |
| `Auth(ThirdPartyBlocked)` | message Anthropic (§5.2) | **pas de retry** — remonte le go/no-go |
| `Auth(Invalid)` | 401/403 creds invalides | pas de retry, erreur utilisateur |
| `InvalidRequest` | 400/422 | **pas de retry** (bug client) |

```rust
match provider.classify_error(&err) {
    ErrorClass::Overloaded(_code) => {
        backoff.aggressive();
        if consecutive_529 >= 3 { switch_to_fallback_model(); }
    }
    ErrorClass::RateLimited => backoff.honor_retry_after(&err),
    ErrorClass::Auth(AuthError::Expired) => { auth.refresh().await?; retry_once(); }
    ErrorClass::Auth(AuthError::ThirdPartyBlocked) => return Err(BlockedThirdParty),
    ErrorClass::Retryable => backoff.exponential_jitter(),
    ErrorClass::InvalidRequest | ErrorClass::Auth(_) => return Err(err.into()),
}
```

**Le backoff transverse n'est PAS le withholding.** Les classes `Overloaded(529)` / `Retryable` / `RateLimited` relèvent du **retry transverse** ci-dessus (backoff dans `agent-provider`). Le **withholding** est un mécanisme distinct de la boucle d'agent : il retient une erreur **PTL / max-tokens** dans un `Option<PendingError>` jusqu'à **échec confirmé du recovery** (tentative de compaction réactive), pour ne pas avorter un tour qui pouvait être sauvé par une compaction. Ne pas confondre les deux : un 529 ne peuple jamais `PendingError`, il déclenche le backoff ; seules les erreurs PTL/max-tokens transitent par le withholding. Voir la boucle d'agent dans `ARCHITECTURE.md §3.4`.

### 5.2 Classification du blocage Anthropic des outils tiers

Anthropic renvoie, pour un agent tiers s'authentifiant via abonnement Pro/Max, un message du type :

> `This credential is only authorized for use with Claude Code...`

L'adapter Anthropic **classifie ce cas en `Auth(AuthError::ThirdPartyBlocked)`** — distinct d'un 401 « token expiré ». Aucun retry, aucun refresh : c'est un signal **go/no-go** remonté tel quel (voir §6). C'est le verdict du spike auth de Phase 0 (`ROADMAP.md`, Phase 0).

### 5.3 Stratégie cache-hit : ordre stable

Le cache (Anthropic explicite, OpenAI/autres implicites par préfixe) ne fonctionne que si le **préfixe est byte-stable** entre les tours. Règle d'ordre canonique :

```
system  →  tools  →  CLAUDE.md  →  historique  →  [volatile]
└──────────── blocs cacheables, en tête ────────────┘
```

- Les blocs cacheables sont **en tête**, dans un ordre **déterministe**.
- **Jamais** de contenu volatile (timestamp, état git, sortie d'outil fraîche) **avant** un bloc caché — il invaliderait tout le préfixe.
- Anthropic : `cache_control: ephemeral` posé sur les frontières (fin de `system`, fin de `tools`, fin de `CLAUDE.md`), **TTL 1h** (la valeur 1h est une beta Anthropic ; le défaut ephemeral est 5 min — Numen pose explicitement le TTL 1h).
- Les autres providers profitent du même ordre stable via leur caching de préfixe implicite.

### 5.4 Multimodal

Le canonique porte un `ContentBlock::Image`. Chaque adapter le traduit (`image_url` OpenAI, `inlineData` Gemini, `image` Anthropic). Gated sur `capabilities().vision`. À la compaction **full**, les images sont **strippées** (l'agent forké repart en mode resume sans payload image).

### 5.5 Betas Anthropic gated

Les en-têtes `anthropic-beta` (et autres features propriétaires) ne sont émis **que si `kind == ProviderKind::Anthropic`**. Aucune fuite de spécificité Anthropic vers un autre adapter.

---

## 6. Le blocage Anthropic des outils tiers & la stratégie model-agnostic

### 6.1 Le risque (RISQUE N°1 PRODUIT)

Déployé en **janvier 2026**, **durci en avril 2026** : Anthropic bloque les outils tiers qui s'authentifient via un **abonnement Pro/Max**. Un agent tiers (= Numen) ne peut **plus** utiliser un abonnement Max d'un utilisateur pour appeler Claude. C'est une menace existentielle si le produit en dépend.

### 6.2 La mitigation : model-agnostic by design

C'est précisément **pourquoi la couche multi-provider est le cœur du projet**. Le positionnement est **model-agnostic** :

- **Provider MVP non-bloqué** : **Ollama local** (aucune auth, aucun blocage possible) + **OpenAI Chat Completions au token** (API key, hors abonnement). Anthropic est **conditionnel**, jamais requis.
- Si Anthropic bloque, Numen **fonctionne quand même** — il bascule sur les autres providers. La valeur du produit ne repose sur aucun provider unique.

### 6.3 Le go/no-go : spike auth de Phase 0

Le **spike auth Anthropic** (`agent-auth`, ~1 jour) est le **go/no-go de Phase 0** (voir [`docs/ROADMAP.md`](./ROADMAP.md), Phase 0) : il détermine si Numen peut s'authentifier contre Anthropic dans des conditions acceptables. Quelle que soit son issue, le MVP avance — parce que le chemin non-bloqué (Ollama + OpenAI) ne dépend pas de ce verdict. Le résultat du spike décide seulement du **statut d'Anthropic** comme provider (first-class, dégradé, ou différé), pas de la viabilité du produit.

> En une phrase : la couche multi-provider n'est pas une feature, c'est l'**assurance** contre le risque N°1. Tant qu'un seul provider frontier reste accessible, Numen tourne.
