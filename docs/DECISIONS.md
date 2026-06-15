# Registre des décisions d'architecture (ADR léger)

Ce registre consigne les décisions structurantes de **Numen** (CLI agent IA multi-provider, écrite en Rust, liée à Paneflow). Commande : `numen`. Format par décision : **Contexte / Décision / Justification / Alternatives écartées / Conséquences & risques**. Statut du projet : phase d'étude/design, pré-implémentation, aucun code écrit. Ces ADR sont la source de vérité ; toute proposition contradictoire doit d'abord amender un ADR.

Documents détaillés (versions longues des mêmes décisions) : `docs/ARCHITECTURE.md` (boucle, cœur, crates, pipeline d'outils), `docs/PROVIDERS.md` (couche multi-provider, adapters, taxonomie d'erreurs), `docs/ROADMAP.md` (phases, spike de de-risquage). Les ADR pointent vers ces fichiers là où le détail vit.

| ADR | Sujet | Statut |
|---|---|---|
| ADR-1 | Langage = Rust | Accepté |
| ADR-2 | Frontend = Ratatui + Crossterm (terminal) | Accepté |
| ADR-3 | Cœur headless + frontend client | Accepté |
| ADR-4 | Couche multi-provider maison | Accepté |
| ADR-5 | Nom = Numen | Accepté (2026-06-15) |
| ADR-6 | Différenciateur recentré | Accepté |
| ADR-7 | Registre des risques majeurs | Vivant |
| ADR-8 | Nommage des crates : `numen*` publié, `agent-*` interne | Accepté |
| ADR-9 | Taxonomie d'erreurs canonique : `ErrorClass` | Accepté |
| ADR-10 | Auth abonnement ChatGPT = `ProviderKind::OpenAiChatGpt` (Responses API backend ChatGPT, SSE stateless, gated) | Accepté (2026-06-15) |

---

## ADR-1 — Langage : Rust

**Contexte.** Numen est conçue pour être « ultra performante dans Paneflow ». Paneflow est bâti sur GPUI, donc en Rust. Le choix de langage de Numen conditionne sa capacité à partager du code avec Paneflow et à s'y embarquer, ainsi que la vélocité de développement solo.

**Décision.** Rust, décision ferme. Workspace de crates internes (`agent-core`, `agent-provider`, `agent-tools`, `agent-mcp`, `agent-tui`, `agent-session`, `agent-sandbox`, `agent-auth`, `agent-tokenizer`, `agent-cli`). Le nommage publié/interne est tranché en **ADR-8**. Détail du workspace dans `docs/ARCHITECTURE.md`.

**Justification.** La raison principale est l'intégration Paneflow : une CLI réutilisant la base existante = **crates partagées + cœur embarqué in-process** (pas d'IPC, types partagés). Bénéfices transverses : perf native, contrôle mémoire, state machine de la boucle d'agent vérifiable par le compilateur (enum `Transition` exhaustif), sandbox kernel-level (Landlock) accessible nativement.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| TypeScript / Bun (stack de Claude Code) | Mur **FFI/IPC** avec Paneflow (GPUI = Rust) : impossible d'embarquer le cœur in-process proprement, types non partagés. Distribution Node lourde. Le seul gain (vélocité solo) ne compense pas la perte d'intégration. |

**Conséquences & risques.**
- **Risque d'exécution N°1 assumé** : la vélocité de développement solo en Rust est plus lente qu'en TS/Bun. Mitigation : périmètre MVP serré, le dur et le risqué d'abord (cf. ADR-7 et `docs/ROADMAP.md`, Phase 0).
- Discipline ownership-first, async Tokio, jamais d'`.unwrap()` en prod : lints clippy obligatoires (`panic`/`unimplemented`/`dbg_macro` en `deny`, `unwrap_used`/`expect_used` en `warn`).
- Le coût de compilation et la complexité du workspace deviennent un poste à surveiller dès Phase 1.

---

## ADR-2 — Frontend : Ratatui + Crossterm (terminal)

**Contexte.** Numen doit s'ouvrir **directement dans le shell**, comme Claude Code — pas une fenêtre d'application. Le cœur émet des événements structurés (cf. ADR-3) ; le frontend n'est qu'un rendu. Il fallait choisir la technologie de rendu terminal et l'esthétique cible.

**Décision.** **Ratatui + Crossterm** pour le frontend terminal standalone (crate `agent-tui`). UI cible : **monochrome, moderne, épurée** (esthétique Rauch/Vercel), pas un TUI « à l'ancienne » (bordures doubles, couleurs criardes). Détail du découplage cœur↔TUI dans `docs/ARCHITECTURE.md`.

**Justification.**
- Ratatui rend de l'ANSI dans le terminal natif : c'est exactement ce que fait Ink chez Claude Code. **Clarification importante : Ink EST un TUI**, il n'a rien de magique. Le plafond visuel d'un terminal est identique pour Ink et pour Ratatui — c'est le **design** qui fait toute la différence, pas la lib.
- Ratatui est l'idiome Rust mature pour le rendu terminal, sans pont FFI vers un runtime JS.
- L'esthétique monochrome épurée est une décision produit (différenciateur perçu), pas une contrainte technique.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| **GPUI** pour le frontend standalone | GPUI ouvre une **fenêtre GPU** (app desktop), pas une CLI terminal. Incompatible avec l'exigence « s'ouvre dans le shell ». **Nuance** : GPUI n'est pas perdu — il reste le canal de rendu *enrichi* côté Paneflow via le cœur embarqué (cf. ADR-3), où une fenêtre GPU est précisément le bon support. |
| Réimplémentation/portage d'Ink en Rust | Aucun gain : même plafond ANSI que Ratatui, coût de portage énorme, perte de l'écosystème Ratatui. |

**Conséquences & risques.**
- `agent-tui` est **découplé du core via canaux** et **n'est jamais importé par le core**. Le core n'émet jamais d'ANSI (cf. ADR-3).
- La qualité visuelle repose entièrement sur la discipline de design (tokens, sobriété), pas sur la techno : risque de dérive « TUI générique » si le design n'est pas tenu.
- Crossterm fixe le socle cross-platform du rendu/entrée terminal ; les spécificités OS (sandbox, etc.) sont traitées ailleurs.

---

## ADR-3 — Cœur headless + frontend client

**Contexte.** Numen doit fonctionner en mode terminal par défaut **et** pouvoir s'enrichir dans Paneflow (diffs GPU-accélérés, arbre de plan, review par hunk) sans dupliquer la logique d'agent ni casser le mode terminal.

**Décision.** `agent-core` est **découplé du frontend** et **n'émet QUE des événements structurés** (jamais d'ANSI). Le frontend Ratatui est un **simple client** qui consomme ces événements. Règle d'or : `agent-core` ne dépend **ni** de `agent-tui` **ni** de `agent-provider` (testable sans I/O, mode headless `-p` sans Ratatui). Détail dans `docs/ARCHITECTURE.md`.

**Justification.**
- Le découplage event-driven rend la boucle testable sans API ni terminal (deps injectables, mode headless `-p`).
- Conséquence clé : **Paneflow peut embarquer `agent-core` in-process** et rendre les mêmes événements via **GPUI** (diffs GPU, arbre de plan, review par hunk). L'enrichissement futur passe par un **protocole** d'événements, **sans casser** le mode terminal par défaut.
- Un seul cœur, plusieurs rendus (terminal Ratatui aujourd'hui, GPUI Paneflow demain) : pas de fork de logique.

**Deux types d'événements (frontière à ne pas confondre).** Le système manipule **deux** enums d'événements distincts, par design :

| Enum | Sens | Défini dans | Consommé par |
|---|---|---|---|
| `StreamEvent` | provider → core | `docs/PROVIDERS.md` (couche multi-provider) | `agent-core` (traduit le wire format en état canonique) |
| `AgentEvent` | core → clients | `docs/ARCHITECTURE.md` (cœur headless) | `agent-tui`, et plus tard Paneflow/GPUI |

`agent-core` consomme les `StreamEvent` d'un provider et les **traduit** en `AgentEvent` structurés vers les clients. Ce ne sont pas deux noms pour la même chose : `StreamEvent` est un détail de la couche provider (deltas de texte/reasoning/tool-call/usage), `AgentEvent` est le contrat de rendu côté frontends.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Cœur émettant directement de l'ANSI (couplé au TUI) | Rendrait l'embarquement Paneflow/GPUI impossible (GPUI ne consomme pas de l'ANSI), et le core non testable sans terminal. |
| Frontend communiquant avec le cœur via IPC/process séparé | Casse le bénéfice in-process de Rust (cf. ADR-1) : latence, sérialisation, types non partagés. L'in-process avec types partagés est précisément l'avantage qu'on protège. |

**Conséquences & risques.**
- Le **protocole d'événements** (`AgentEvent`) devient une frontière de contrat à versionner avec soin (le consommateur Paneflow et le TUI en dépendent). Le protocole d'enrichissement Paneflow est planifié en Phase 2 (`docs/ROADMAP.md`).
- API consommateur du cœur = **stream** via `async-stream` ; communication TUI via canaux.
- Tout ajout dans le core doit rester pur (pas de dépendance TUI/HTTP) sous peine de casser l'invariant headless.

---

## ADR-4 — Couche multi-provider maison

**Contexte.** Le différenciateur central de Numen est le **multi-provider first-class** (tous les modèles frontier), là où Claude Code est Anthropic-only. Il fallait décider comment normaliser des wire formats hétérogènes (Anthropic, OpenAI Chat/Responses, Gemini, Ollama, OpenRouter, Bedrock/Vertex/Azure) tout en gardant la qualité Claude Code et la perf Rust. Version détaillée : `docs/PROVIDERS.md`.

**Décision.** Couche **maison** : `reqwest` + `eventsource-stream`, **format canonique interne Anthropic-like** (content blocks). Trait `Provider { kind, capabilities, stream -> BoxStream<StreamEvent>, complete, classify_error }`, `enum StreamEvent { TextDelta, ReasoningDelta, ToolCallStart, ToolCallDelta, ToolCallEnd, Usage, Done }`, `struct Capabilities { vision, tools, prompt_caching, reasoning, server_side_state, max_context }`. **Divergences localisées dans chaque adapter.** La taxonomie d'erreurs renvoyée par `classify_error` est canonisée en **ADR-9** (`ErrorClass`).

**Justification.**
- Le canonique Anthropic-like rend l'adapter Anthropic quasi-identité et préserve la finesse (content blocks, cache, thinking).
- Contrôle total du wire format : nécessaire faute de **SDK Anthropic Rust officiel** (les SDK communautaires servent de référence de format, cf. ADR-7 R4).
- Capabilities explicites : les features non mappables (état server-side OpenAI Responses) sont **gated**, pas imposées.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| **LiteLLM** | Hop réseau supplémentaire + normalisation **lossy** (perte de features natives). |
| **Vercel AI SDK** | TS-only — inutilisable en Rust. Conservé comme **inspiration d'interface** seulement. |
| **genai** (crate) | Beta, pas assez stable pour porter le cœur du produit. |
| **OpenRouter comme stratégie** | OpenRouter = **un adapter** parmi d'autres (méta-routeur OpenAI-compat), pas la stratégie de normalisation. |

**Divergences par provider (résumé ; détail dans `docs/PROVIDERS.md` §3).**
- **Anthropic** : adapter quasi-identité. `cache_control` ephemeral TTL 1h, thinking adaptatif. Betas gated sur `kind == Anthropic`.
- **OpenAI** : **deux surfaces**. *Chat Completions* (transcript client → mappe proprement, **cible MVP**). *Responses API* (état server-side via `previous_response_id` → **ne mappe pas** sur le canonique → mode **gated** sur `capabilities.server_side_state`, **jamais par défaut**).
- **Gemini** : function calls potentiellement **fragmentées** en stream → **réassembler côté adapter** avant `ToolCallEnd`. `systemInstruction`, context cache.
- **Ollama** : OpenAI-compat, **usage souvent absent** en stream → fallback `agent-tokenizer` **obligatoire** (sinon compaction cassée — cf. ContextBudget, `docs/ARCHITECTURE.md`).
- **OpenRouter** : méta-routeur OpenAI-compat (200+ modèles, perd les features natives).
- **Bedrock / Vertex / Azure** : **pas des adapters complets** — auth injectable (SigV4 / OAuth Google / endpoint custom), réutilisent l'adapter Anthropic/OpenAI/Gemini sous-jacent. Toutes les creds via `agent-auth`.

**Transverses.**
- Retry : `classify_error -> ErrorClass` (taxonomie canonique en **ADR-9**). Backoff exponentiel + jitter ; `Overloaded(529)` = backoff agressif, honore `Retry-After`, **fallback model après 3×529** ; `Auth(Expired)` → refresh OAuth. Détail dans `docs/PROVIDERS.md` §5.1.
- Le message Anthropic « This credential is only authorized for use with Claude Code... » → classifié `Auth(ThirdPartyBlocked)`.
- Stratégie cache-hit : ordre stable (`system → tools → CLAUDE.md → historique`), blocs cacheables en tête, **jamais de contenu volatile avant un bloc cache**.
- Multimodal canonique : `ContentBlock::Image`.

**Conséquences & risques.**
- Maintenir N adapters = **dette de maintenance** proportionnelle au nombre de providers. Mitigation : tests **VCR** sur payloads providers en CI (filet sans SDK officiel, cf. ADR-7 R4), prévus en Phase 3 (`docs/ROADMAP.md`).
- Sans SDK officiel, chaque évolution de wire format (surtout Anthropic) doit être suivie manuellement.
- Le format canonique Anthropic-like privilégie Anthropic ; les providers les plus éloignés (Responses, Gemini streaming) concentrent la complexité dans leur adapter.

---

## ADR-5 — Nom : Numen

**Contexte.** Le projet avait besoin d'un nom disponible dans l'espace « agent IA » de 2026, en particulier **libre sur crates.io** (atout décisif pour un projet cargo). Le concept sémantique recherché : l'esprit-guide, l'âme de « daimon ».

**Décision.** **Numen** (latin : la présence/volonté divine qui *anime*). Commande : `numen`. Décidé le **2026-06-15** après un sweep de disponibilité. La conséquence sur le **nommage des crates** (réservées `numen*` vs internes `agent-*`) est tranchée en **ADR-8**.

**Justification.** Numen garde l'âme de « daimon » (l'esprit-guide) tout en étant le **seul survivant** d'un sweep où tous les autres candidats étaient en collision majeure. Crates **`numen`, `numen-cli`, `numen-core` confirmés LIBRES** sur crates.io — atout décisif pour réserver l'espace de nom publié.

**Historique du sweep.**

| Candidat | Verdict |
|---|---|
| daimon | Collision majeure (espace agent IA 2026) |
| sigil | Collision majeure |
| pneuma | Collision majeure |
| eidolon | Collision majeure |
| glyph | Collision majeure |
| **numen** | **Survivant — crates libres** |

**Alternatives écartées.** Les cinq candidats ci-dessus, tous écartés pour collision majeure dans l'espace agent IA 2026.

**Conséquences & risques.**
- **Seule ombre mineure** : « Numen Voice Control » (niche accessibilité Linux sur SourceHut) — **segment différent**, risque de confusion faible.
- Le nom est verrouillé ; toute renomination invaliderait la réservation des crates (à sécuriser tôt). Réserver `numen`, `numen-cli`, `numen-core` sur crates.io avant Phase 1.

---

## ADR-6 — Différenciateur recentré

**Contexte.** Plusieurs angles de différenciation ont été envisagés. Il fallait trancher sur le positionnement produit avant d'engager l'implémentation, pour éviter de bâtir autour d'un axe à TAM trop étroit ou déjà couvert par la concurrence.

**Décision.** Différenciateur = **« Qualité Claude Code, tous les providers frontier, perf Rust + intégration profonde avec Paneflow. »** Concrètement : full Rust natif ultra-perf + multi-provider first-class (là où Claude Code est Anthropic-only) + **cœur partagé avec Paneflow** permettant une intégration profonde (in-process, cf. ADR-3).

**Justification.** C'est l'intersection défendable : la qualité d'un agent de référence, ouverte à tous les modèles frontier, avec un moat technique (Rust + cœur partagé Paneflow) qu'un wrapper TS ne peut pas répliquer.

**Alternatives écartées.**

| Axe envisagé | Pourquoi écarté |
|---|---|
| **Verification-grounded / vertical Rust** | **TAM trop étroit.** |
| **Sandbox déclaratif** | **Codex 2026 le fait déjà** — pas de différenciation. |

**Conséquences & risques.**
- Le positionnement est **model-agnostic** : cohérent avec la mitigation du risque N°1 (cf. ADR-7 R1).
- La promesse « intégration profonde Paneflow » crée une **dépendance de roadmap** : le protocole d'enrichissement (Phase 2, `docs/ROADMAP.md`) doit suivre, sinon le différenciateur reste théorique.
- « Qualité Claude Code » est un standard élevé à tenir sur N providers, pas seulement un.

---

## ADR-7 — Registre des risques majeurs

**Contexte.** Le projet porte un petit nombre de risques structurants qui conditionnent la roadmap (le principe étant : **le dur et le risqué en premier**). Ce registre les centralise avec leur mitigation et leur point de décision. La séquence des phases vit dans `docs/ROADMAP.md`.

**Décision.** Maintenir le tableau ci-dessous comme ADR vivant. Chaque risque a une mitigation décidée et, le cas échéant, un go/no-go en Phase 0.

| # | Risque | Nature | Mitigation décidée | Point de décision |
|---|---|---|---|---|
| **R1** | **Blocage Anthropic des outils tiers** s'authentifiant via abonnement Pro/Max (déployé jan 2026, durci avr 2026). Un agent tiers ne peut plus utiliser un abonnement Max. | Produit — **risque N°1** | **Provider MVP non-bloqué** (Ollama local + OpenAI au token), positionnement **model-agnostic**. Anthropic conditionnel. Message « This credential is only authorized for use with Claude Code... » → classifié `Auth(ThirdPartyBlocked)` (cf. ADR-4, ADR-9). | **Spike auth Anthropic = go/no-go de Phase 0** (1 jour, dans `agent-auth`). Cf. `docs/ROADMAP.md` Phase 0 et `docs/PROVIDERS.md` §6. |
| **R2** | **Vélocité de développement Rust en solo** plus lente que TS/Bun. | Exécution — **risque N°1 d'exécution** | Coût **assumé** (cf. ADR-1). Périmètre MVP serré, dur en premier, crates découplées et testables sans I/O. | Suivi continu sur la roadmap. |
| **R3** | **Sandbox cross-platform** : Landlock filtre le FS au niveau kernel mais **ne filtre pas par hostname** ; pas d'équivalent Linux/macOS uniforme. | Sécurité / portabilité | Landlock **FS** (vrai, kernel-level) + **réseau filtré best-effort via proxy local** (PAS Landlock). macOS Seatbelt en durcissement. | Phase 0 : Landlock FS + 1 appel réseau filtré par proxy. macOS Seatbelt en Phase 3 (`docs/ROADMAP.md`). |
| **R4** | **Pas de SDK Anthropic Rust officiel** — wire format à maintenir à la main, dérive possible. | Maintenance | Couche maison (cf. ADR-4), SDK communautaires (`anthropic-sdk-rs`) comme **référence de wire format**. **Tests VCR** sur payloads providers en **CI obligatoire** = filet sans SDK officiel. | VCR en CI dès Phase 3 (durcissement). |
| **R5** | **Prompt injection** (OWASP LLM01) : output d'outil non fiable détournant l'agent. | Sécurité | **TAINT untrusted** : tout output d'outil (Bash, Read, MCP) est `untrusted` par défaut (`returns_untrusted=true`), propagé. Action destructive/réseau dans un turn contenant du taint récent → **force `Ask`**. | Intégré au pipeline d'outils dès le MVP (Phase 1, cf. `docs/ARCHITECTURE.md`). |

**Justification.** Concentrer ces risques en un seul ADR force la roadmap à attaquer le risqué d'abord (Phase 0 de de-risquage) et donne un point go/no-go explicite (R1) avant d'investir dans l'implémentation complète.

**Conséquences & risques (méta).**
- **R1 est bloquant** : si le spike auth échoue, le positionnement model-agnostic (Ollama + OpenAI) reste valide mais Anthropic devient indisponible via abonnement — d'où l'importance que le **MVP ne dépende pas d'Anthropic**.
- Ce registre est **vivant** : tout nouveau risque structurant (ex. évolution de la politique d'un provider, breaking change wire format) s'ajoute ici, pas dans un document ad hoc.
- Les mitigations R3/R4 ont un coût de CI/infra (proxy réseau, harness VCR) à provisionner en Phase 3.

---

## ADR-8 — Nommage des crates : `numen*` publié, `agent-*` interne

**Contexte.** Deux nommages coexistaient sans être réconciliés, créant la plus grosse incohérence transversale de la doc :
- ADR-5 (et le sweep de disponibilité) réserve `numen`, `numen-cli`, `numen-core` comme **libres sur crates.io** — atout décisif présenté comme tel.
- ADR-1, `docs/ARCHITECTURE.md` et `docs/ROADMAP.md` nomment le workspace `agent-core`, `agent-cli`, `agent-tui`, etc.

Les noms réservés n'étaient jamais utilisés par les crates effectivement décrites. Il faut trancher explicitement, sinon ADR-5 et ADR-1/ARCHITECTURE se contredisent.

**Décision.** On découple **identité publiée** et **identité interne**, et on l'acte :

| Rôle | Nom | crates.io | Notes |
|---|---|---|---|
| Binaire / commande | `numen` | publié | C'est ce que l'utilisateur installe et exécute (`numen`). |
| Crate racine publiée (façade CLI) | `numen` (≈ l'actuel `agent-cli`) | publié | Crate de wiring, seule à dépendre de tout. Peut être déclarée `[[bin]] name = "numen"`. |
| Réservations de nom | `numen-cli`, `numen-core` | réservées | Réservées pour protéger l'espace de nom ; servent de **placeholders/redirections** si une publication granulaire devient pertinente. Pas (encore) le nom des crates internes. |
| Crates internes du workspace | `agent-core`, `agent-provider`, `agent-tools`, `agent-mcp`, `agent-tui`, `agent-session`, `agent-sandbox`, `agent-auth`, `agent-tokenizer` | non publiées (path deps) | Restent en `agent-*` : ce sont des détails d'implémentation du workspace, non destinés à une consommation externe au MVP. |

Autrement dit : **la surface publiée porte le nom `numen` ; les crates internes gardent le préfixe `agent-*`.** La divergence est intentionnelle et documentée ici.

**Justification.**
- Conserver `agent-*` en interne évite un renommage massif de tout le workspace (ADR-1, ARCHITECTURE, ROADMAP) à un stade pré-implémentation où la valeur d'un tel churn est nulle.
- Réserver `numen`, `numen-cli`, `numen-core` protège l'identité produit sur crates.io (l'atout décisif d'ADR-5 reste exploité : c'est la **réservation** qui compte, pas l'alignement 1:1 des noms internes).
- Le binaire et la façade publiée portant `numen` : l'utilisateur et l'écosystème ne voient que `numen`. Le préfixe `agent-*` n'est visible que dans le repo.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Renommer tout le workspace en `numen-core`/`numen-cli`/`numen-tui`/… | Churn massif sur toute la doc et le futur code, pour un gain cosmétique. Peut être fait plus tard si une publication crate-par-crate devient un objectif réel — pas maintenant. |
| Garder uniquement `agent-*` et abandonner les réservations `numen*` | Gâche l'atout décisif d'ADR-5 (espace de nom libre) et expose à un squat ultérieur du nom produit sur crates.io. |

**Conséquences & risques.**
- **Action à sécuriser tôt** : publier des stubs pour réserver `numen`, `numen-cli`, `numen-core` sur crates.io avant Phase 1 (cohérent avec ADR-5).
- Divergence publié/interne à garder lisible : un `README` du workspace doit rappeler que `numen` (publié) = `agent-cli` (interne) + façade.
- Si Phase 2/3 impose une publication granulaire (ex. `agent-core` réutilisable par Paneflow via crates.io plutôt que path dep), réévaluer : à ce moment les réservations `numen-core`/`numen-cli` peuvent devenir les noms publiés de ces crates.

---

## ADR-9 — Taxonomie d'erreurs canonique : `ErrorClass`

**Contexte.** Trois formulations de la classification d'erreurs providers coexistaient, source d'incohérence #2 :
- BRIEF + ADR-4 (rédaction initiale) : 4 classes `Retryable | Overloaded(529) | Auth | InvalidRequest`, `Auth` non paramétrée.
- `docs/PROVIDERS.md` : 5 classes (ajoute `RateLimited` pour 429 / `Retry-After`) + `Auth(AuthError)` **paramétré** (`Expired` / `ThirdPartyBlocked` / `Invalid`).
- `docs/ARCHITECTURE.md` §3.4 : type nommé `ErrClass`, 4 variantes.

Le **nom** du type divergeait (`ErrClass` vs `ErrorClass`) et la signature d'`Overloaded` (avec/sans payload 529) flottait.

**Décision.** Une seule taxonomie canonique, la plus complète (celle de `docs/PROVIDERS.md`), nommée **`ErrorClass`** partout. `classify_error` retourne `ErrorClass`. Signature figée :

```rust
enum ErrorClass {
    Retryable,                 // transitoires réseau / 5xx non-529
    RateLimited,               // 429 ; honore Retry-After
    Overloaded(u16),           // 529 (payload = status) ; backoff agressif
    Auth(AuthError),
    InvalidRequest,            // 4xx non-auth : non-retryable
}

enum AuthError {
    Expired,                   // 401 -> refresh OAuth (agent-auth)
    ThirdPartyBlocked,         // « only authorized for use with Claude Code... » (R1)
    Invalid,                   // creds invalides / révoquées
}
```

Règles de retry associées (détail `docs/PROVIDERS.md` §5.1) : `Retryable` → backoff exponentiel + jitter ; `RateLimited` → respecte `Retry-After` ; `Overloaded(529)` → backoff agressif + `Retry-After`, **fallback model après 3×529** ; `Auth(Expired)` → refresh OAuth puis retry ; `Auth(ThirdPartyBlocked | Invalid)` et `InvalidRequest` → **non-retryable**, propagés.

**Justification.**
- `RateLimited` (429) et `Overloaded` (529) ont des politiques de backoff distinctes : les fusionner perdrait la sémantique `Retry-After` vs backoff agressif.
- `Auth(AuthError)` paramétré est indispensable pour distinguer le **refresh** (`Expired`) du **blocage produit R1** (`ThirdPartyBlocked`) — ce dernier est le cœur du risque N°1, il ne peut pas être noyé dans un `Auth` opaque.
- Un nom unique (`ErrorClass`) supprime la divergence `ErrClass`/`ErrorClass` entre ARCHITECTURE et PROVIDERS.

**Conséquence sur les autres documents.** `docs/ARCHITECTURE.md` §3.4 (renommer `ErrClass` → `ErrorClass`, passer à 5 variantes), `docs/PROVIDERS.md` (déjà canonique, sert de référence) et toute mention dans `docs/ROADMAP.md` doivent référencer cette taxonomie. ADR-4 ci-dessus est aligné.

**Précision withholding (ne pas confondre avec le retry transverse).** Le mécanisme de **withholding** (`Option<PendingError>`) retient **uniquement** une erreur PTL (prompt-too-long) / max-tokens jusqu'à échec confirmé du recovery (tentative de **compaction réactive** avant propagation). Il ne s'applique **pas** aux `Overloaded`/`Retryable`/`RateLimited`, qui relèvent du **backoff transverse** de la couche provider (`docs/PROVIDERS.md` §5.1). Mettre un `PendingError` sur un 529 mélangerait les deux mécanismes : `docs/ARCHITECTURE.md` §3.4 doit ne charger `PendingError` que sur PTL/max-tokens.

**Conséquences & risques.**
- La taxonomie est un point de contrat entre `agent-provider` (qui la produit) et `agent-core` (qui décide retry/fallback/withholding) : tout ajout de variante est un changement de contrat à propager.
- Les payloads providers exotiques (Ollama, OpenRouter) doivent être mappés explicitement vers `ErrorClass` dans leur adapter, faute de quoi ils retombent en `Retryable` par défaut prudent.

---

## ADR-10 — Auth abonnement ChatGPT : `ProviderKind::OpenAiChatGpt` (Responses API backend ChatGPT, SSE stateless, gated)

**Contexte.** Le dogfooder principal (Arthur) veut alimenter Numen avec son **abonnement ChatGPT** (Plus/Pro), pas une clé API au token, et a rejeté Ollama comme défaut quotidien. L'extraction de l'implémentation de référence Pi (TypeScript, détail vérifié dans `docs/openai-subscription-auth.md`, 45/45 constantes confirmées contre le code) établit un fait structurant : **l'auth abonnement ChatGPT n'appelle PAS Chat Completions.** Elle :
- réutilise le `client_id` OAuth du **Codex CLI officiel OSS** (`app_EMoamEEZ73f0CkXaXp7hrann`), flow PKCE S256 sur `auth.openai.com` (browser callback `localhost:1455` + device-code) ;
- appelle l'inférence sur le **backend ChatGPT via la Responses API** : `https://chatgpt.com/backend-api/codex/responses` (SSE/WS), headers propriétaires `chatgpt-account-id` (claim JWT `https://api.openai.com/auth`.chatgpt_account_id) + `originator`, body `store:false` + `instructions` + `input[]` (jamais `messages[]`).

Cela entre en conflit frontal avec **US-017** (« cible Chat Completions ; Responses API hors scope ») et touche le piège **`docs/PROVIDERS.md §4.1`** (la Responses API à état server-side `previous_response_id` ne mappe pas le transcript client-side, indispensable à compaction/resume/replay).

**Décision.** Introduire un **`ProviderKind::OpenAiChatGpt`** distinct, et NON réutiliser `OpenAiChat` ni `OpenAiResponses` :
- **Surface séparée** : base `https://chatgpt.com/backend-api/codex`, endpoint `/responses`, `capabilities().server_side_state = false`.
- **SSE stateless uniquement** : le backend Codex est stateless en SSE (contexte complet dans `input[]` à chaque tour) → mappe proprement le canonique client-side. Le mode **WebSocket + `previous_response_id` est explicitement refusé** pour le MVP (il casserait compaction/resume).
- **Auth** : OAuth PKCE (réutilise le client Codex), credentials en **keyring** (US-018, jamais en clair — contrairement au `~/.pi/agent/auth.json` clair de Pi), refresh tokens rotatifs. Implémentée dans `agent-auth/src/oauth/openai_chatgpt.rs`.
- **Statut** : credential **optionnelle, gated, étiquetée « fragile »**. **Jamais en P0**, jamais le défaut, jamais en chemin critique. US-017 reste pur (Chat Completions au token, BYOK) et est **clarifié** : il ne couvre pas l'abonnement ChatGPT.

**Justification.**
- L'endpoint et le wire format diffèrent totalement de Chat Completions : un `if` dans `OpenAiChat` créerait des branches conditionnelles fragiles. Adapter dédié = divergences localisées (`PROVIDERS.md §1.1`).
- `OpenAiResponses` générique cible `api.openai.com/v1/responses` (API publique au token) ; le backend ChatGPT en diverge (base URL, headers propriétaires, `store:false` forcé) → ne pas confondre les deux.
- Le mode SSE stateless est le seul qui préserve l'invariant transcript client-side. Renoncer au WS coûte la continuation optimisée mais garde un canonique sain — arbitrage assumé.
- Garder l'abonnement hors P0 respecte **FR-11** et le **risque R1** (ADR-7) : le MVP ne dépend d'aucun canal subscription.

**Alternatives écartées.**

| Option | Raison de l'écart |
|---|---|
| Réutiliser `OpenAiChat` (Chat Completions) pour l'abonnement | Impossible : le backend ChatGPT n'expose pas `/chat/completions` ; wire format incompatible (`input[]` vs `messages[]`, `store:false` forcé). |
| Réutiliser `OpenAiResponses` générique | Cible l'API publique `api.openai.com/v1/responses`, pas le backend ChatGPT (`chatgpt.com/backend-api/codex`) ni ses headers propriétaires. Mélange = fragilité. |
| Activer WebSocket + `previous_response_id` | État server-side connection-scoped → casse compaction / resume JSONL / replay (`PROVIDERS §4.1`). |
| En faire le provider par défaut / P0 | Viole FR-11 et le risque R1 : dépendre d'un canal subscription tiers révocable. |
| Stocker comme Pi (`auth.json` clair 0600) | Viole US-018 (keyring obligatoire, jamais en clair). |

**Conséquences & risques.**
- **ToS-grey** : réutiliser le `client_id` du Codex CLI = se faire passer pour Codex. « Sign in with ChatGPT » est gaté à Codex + IDE partenaires (issue `openai/codex#10974` fermée « not planned »). Usage perso, **révocable unilatéralement** — OpenAI peut faire sur ce client ce qu'Anthropic a fait sur Pro/Max le 4 avril 2026 (R1 s'applique aussi à OpenAI).
- Prévoir un équivalent `Auth(ThirdPartyBlocked)` côté OpenAI si le backend renvoie un message de blocage (wording inconnu à ce jour, à sonder en live comme le leg Anthropic de US-001).
- `originator` est hardcodé par client (Pi met `"pi"`) ; Numen met `"numen"` — le backend **peut** valider l'`originator` contre une liste connue. À tester au premier run ; rejet → soit zone grise totale, soit blocage.
- Dépendance dure au claim custom `https://api.openai.com/auth`.chatgpt_account_id (header `chatgpt-account-id` requis pour router) : un changement de namespace côté OpenAI casse silencieusement.
- **Conséquence sur les autres documents** : `docs/PROVIDERS.md §2` (ajouter `OpenAiChatGpt` à `ProviderKind`), US-017 clarifié (scope = Chat Completions au token). Détail d'implémentation : `docs/openai-subscription-auth.md`.

> **Mise à jour (ADR-11, 2026-06-15)** : le **statut** d'ADR-10 (« gated, optionnelle, jamais en P0 ») est **superseded par ADR-11**. La décision *technique* (ProviderKind distinct, Responses backend, SSE stateless, OAuth keyring) reste intégralement valide ; seul le positionnement produit change (l'abonnement devient la cible MVP livrée en premier).

---

## ADR-11 — Scope MVP recentré : abonnement ChatGPT d'abord, Ollama retiré, autres providers différés

**Statut.** Acté 2026-06-15 (directive Arthur). Supersede le **scoping** d'ADR-10 (« gated/optionnelle/jamais P0 ») et la portion « Ollama = provider non-bloqué du MVP » d'ADR-7/§6 et de `docs/PROVIDERS.md §6`. **N'altère pas** l'architecture multi-provider (trait `Provider`), qui reste l'invariant.

**Contexte.** Arthur veut dogfooder Numen **maintenant**, avec son abonnement ChatGPT (modèles GPT/Codex), exactement comme Pi. Décision explicite : « je ne veux pas faire le multi-provider maintenant, je veux d'abord me concentrer sur Codex et les modèles GPT du plan d'abonnement, que Numen fonctionne déjà parfaitement avec, et plus tard j'attaquerai d'autres providers au fur et à mesure. » Et, séparément : **Ollama est retiré du scope** (« trop instable, je ne l'implémenterai certainement jamais »).

**Décision.**
1. **Seul provider livré au MVP = `OpenAiChatGpt`** (abonnement, Responses API SSE stateless, ADR-10). C'est désormais la cible P0 du dogfood, pas une commodité gated.
2. **Ollama supprimé** : variante `ProviderKind::Ollama` / `ProviderId::Ollama` retirée du code, US-016 **annulée**. Le fallback tokenizer (US-007) reste — il est provider-agnostique (estimation pré-tour US-014, providers futurs sans `usage`), sa justification n'est juste plus « Ollama ».
3. **US-017 (OpenAI Chat Completions BYOK) différée** au rang de provider futur (plus la cible MVP). US-015 (trait + canonique + retry) et US-018 (auth keyring) restent et sont **satisfaites**.
4. **L'architecture reste multi-provider** : le trait `Provider` est inchangé, les autres adapters (Anthropic, OpenAI Chat, Gemini…) s'ajoutent ensuite, chacun comme un module d'`agent-provider`, sans toucher le cœur.

**Justification.** Levier dogfood maximal et immédiat (Arthur orchestre des agents toute la journée ; il veut SON modèle, tout de suite). La valeur produit se prouve par l'usage quotidien réel, pas par un tableau de 6 providers vides. Le coût d'opportunité (pas de BYOK Chat Completions au MVP) est assumé : c'est de la séquence, pas de l'abandon.

**Risque accepté (explicite).** Faire d'un canal subscription tiers **révocable** la fondation *du MVP* contredit temporairement **FR-11** et le **risque R1** (OpenAI peut couper le `client_id` Codex comme Anthropic l'a fait sur Pro/Max le 4 avril 2026 — cf. [[research-subscription-auth-numen]]). **Mitigation structurelle** : le pari n'est pas architectural mais de *séquencement*. Le trait `Provider` garde la porte ouverte ; le jour où OpenAI coupe, ajouter un adapter BYOK (Chat Completions ou Anthropic) est un module isolé, pas une refonte. La thèse model-agnostic survit comme **assurance**, même si le premier (et seul) adapter livré est l'abonnement.

**Pire scénario.** OpenAI révoque le client Codex → l'unique provider livré tombe → Numen est temporairement inutilisable jusqu'à l'ajout d'un adapter BYOK. Probabilité moyenne, impact élevé mais borné (le code adapter + auth BYOK est petit, le cœur intact). C'est le prix conscient de la vélocité dogfood.

**Conséquences sur les autres documents.** `docs/PROVIDERS.md §6` (Ollama n'est plus le provider non-bloqué du MVP — l'abonnement ChatGPT est la cible ; la stratégie model-agnostic reste l'assurance), tableau §3 (colonne Ollama caduque), `tasks/prd-numen.md` (Goals/FR-02/US-016/edge #9 : Ollama hors scope), `ARCHITECTURE.md` invariant 7 (le fallback tokenizer n'est plus motivé par Ollama mais reste valide). Mise à jour incrémentale ; non bloquant pour le code.
