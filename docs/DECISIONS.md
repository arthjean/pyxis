# Registre des décisions d'architecture (ADR léger)

Ce registre consigne les décisions structurantes de **Pyxis** (CLI agent IA multi-provider, écrite en Rust). Commande : `pyxis`. Format par décision : **Contexte / Décision / Justification / Alternatives écartées / Conséquences & risques**. Statut du projet : implémentation MVP en cours, avec un provider livré (`OpenAiChatGpt`). Les ADR fixent les décisions ; l'état réellement livré est suivi par le code, les fichiers de statut JSON et [`docs/CURRENT_STATUS.md`](./CURRENT_STATUS.md).

Documents détaillés (versions longues des mêmes décisions) : `docs/CURRENT_STATUS.md` (état livré courant), `docs/ARCHITECTURE.md` (boucle, cœur, crates, pipeline d'outils), `docs/PROVIDERS.md` (couche multi-provider, adapters, taxonomie d'erreurs), `docs/ROADMAP.md` (phases, spike de de-risquage). Les ADR pointent vers ces fichiers là où le détail vit.

| ADR | Sujet | Statut |
|---|---|---|
| ADR-1 | Langage = Rust | Accepté |
| ADR-2 | Frontend = Ratatui + Crossterm (terminal) | Accepté |
| ADR-3 | Cœur headless + frontend client | Accepté |
| ADR-4 | Couche multi-provider maison | Accepté |
| ADR-5 | Nom = Pyxis | Accepté (2026-06-15) |
| ADR-6 | Différenciateur recentré | Accepté |
| ADR-7 | Registre des risques majeurs | Vivant |
| ADR-8 | Nommage des crates : `pyxis*` publié, `agent-*` interne | Accepté |
| ADR-9 | Taxonomie d'erreurs canonique : `ErrorClass` | Accepté |
| ADR-10 | Auth abonnement ChatGPT = `ProviderKind::OpenAiChatGpt` (Responses backend ChatGPT, WebSocket borné avec repli SSE) | Accepté (2026-06-15), transport amendé (2026-08-01) |
| ADR-11 | Scope MVP recentré : abonnement ChatGPT d'abord, Ollama retiré, autres providers différés | Accepté (2026-06-15) |
| ADR-12 | Runtime de thread `agent-runtime` au-dessus de `run_agent` | Accepté (2026-07-27) |
| ADR-13 | Sous-agent mutateur en worktree Git : NO-GO v1 | Accepté (2026-07-28) |
| ADR-14 | Garde-fou de boucle : échelle vétoiste `[3, 5, 8]`, rappel borné, clé intacte | Accepté (2026-08-20) |
| ADR-15 | Déversement de sortie d'outil : racine sous `.pyxis` du workspace, meilleur effort strict, plafond de crate | Accepté (2026-08-21) |
| ADR-16 | Frontière de survie des travaux de fond : enregistrements durables, processus non ; une reprise rapporte et n'exécute rien | Accepté (2026-08-22) |

---

## ADR-1 — Langage : Rust

**Contexte.** Le choix de langage conditionne la perf du binaire, l'accès natif aux primitives kernel (sandbox), la distribution (binaire statique vs runtime embarqué) et la vélocité de développement solo.

**Décision.** Rust, décision ferme. Workspace de crates internes (`agent-core`, `agent-provider`, `agent-tools`, `agent-mcp`, `agent-tui`, `agent-session`, `agent-sandbox`, `agent-auth`, `agent-tokenizer`, `agent-cli`). Le nommage publié/interne est tranché en **ADR-8**. Détail du workspace dans `docs/ARCHITECTURE.md`.

**Justification.** Perf native et contrôle mémoire (démarrage <100 ms, binaire statique unique), state machine de la boucle d'agent vérifiable par le compilateur (enum `Transition` exhaustif), sandbox kernel-level (Landlock) accessible nativement sans FFI.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| TypeScript / Bun (stack de Claude Code) | Distribution Node lourde, taxe de démarrage et de mémoire, pas d'accès natif à Landlock, state machine non vérifiable par le compilateur. Le seul gain (vélocité solo) ne compense pas ces pertes. |

**Conséquences & risques.**
- **Risque d'exécution N°1 assumé** : la vélocité de développement solo en Rust est plus lente qu'en TS/Bun. Mitigation : périmètre MVP serré, le dur et le risqué d'abord (cf. ADR-7 et `docs/ROADMAP.md`, Phase 0).
- Discipline ownership-first, async Tokio, jamais d'`.unwrap()` en prod : lints clippy obligatoires (`panic`/`unimplemented`/`dbg_macro` en `deny`, `unwrap_used`/`expect_used` en `warn`).
- Le coût de compilation et la complexité du workspace deviennent un poste à surveiller dès Phase 1.

---

## ADR-2 — Frontend : Ratatui + Crossterm (terminal)

**Contexte.** Pyxis doit s'ouvrir **directement dans le shell**, comme Claude Code — pas une fenêtre d'application. Le cœur émet des événements structurés (cf. ADR-3) ; le frontend n'est qu'un rendu. Il fallait choisir la technologie de rendu terminal et l'esthétique cible.

**Décision.** **Ratatui + Crossterm** pour le frontend terminal standalone (crate `agent-tui`). UI cible : **monochrome, moderne, épurée** (esthétique Rauch/Vercel), pas un TUI « à l'ancienne » (bordures doubles, couleurs criardes). Détail du découplage cœur↔TUI dans `docs/ARCHITECTURE.md`.

**Justification.**
- Ratatui rend de l'ANSI dans le terminal natif : c'est exactement ce que fait Ink chez Claude Code. **Clarification importante : Ink EST un TUI**, il n'a rien de magique. Le plafond visuel d'un terminal est identique pour Ink et pour Ratatui — c'est le **design** qui fait toute la différence, pas la lib.
- Ratatui est l'idiome Rust mature pour le rendu terminal, sans pont FFI vers un runtime JS.
- L'esthétique monochrome épurée est une décision produit (différenciateur perçu), pas une contrainte technique.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| **GPUI** pour le frontend standalone | GPUI ouvre une **fenêtre GPU** (app desktop), pas une CLI terminal. Incompatible avec l'exigence « s'ouvre dans le shell ». |
| Réimplémentation/portage d'Ink en Rust | Aucun gain : même plafond ANSI que Ratatui, coût de portage énorme, perte de l'écosystème Ratatui. |

**Conséquences & risques.**
- `agent-tui` est **découplé du core via canaux** et **n'est jamais importé par le core**. Le core n'émet jamais d'ANSI (cf. ADR-3).
- La qualité visuelle repose entièrement sur la discipline de design (tokens, sobriété), pas sur la techno : risque de dérive « TUI générique » si le design n'est pas tenu.
- Crossterm fixe le socle cross-platform du rendu/entrée terminal ; les spécificités OS (sandbox, etc.) sont traitées ailleurs.

---

## ADR-3 — Cœur headless + frontend client

**Contexte.** Pyxis doit fonctionner en mode terminal par défaut **et** rester ouverte à d'autres clients (mode headless, futurs frontends riches) sans dupliquer la logique d'agent ni casser le mode terminal.

**Décision.** `agent-core` est **découplé du frontend** et **n'émet QUE des événements structurés** (jamais d'ANSI). Le frontend Ratatui est un **simple client** qui consomme ces événements. Règle d'or : `agent-core` ne dépend **ni** de `agent-tui` **ni** de `agent-provider` (testable sans I/O, mode headless `-p` sans Ratatui). Détail dans `docs/ARCHITECTURE.md`.

**Justification.**
- Le découplage event-driven rend la boucle testable sans API ni terminal (deps injectables, mode headless `-p`).
- Conséquence clé : n'importe quel client peut embarquer `agent-core` in-process et rendre les mêmes événements. L'enrichissement futur passe par un **protocole** d'événements, **sans casser** le mode terminal par défaut.
- Un seul cœur, plusieurs rendus : pas de fork de logique.

**Deux types d'événements (frontière à ne pas confondre).** Le système manipule **deux** enums d'événements distincts, par design :

| Enum | Sens | Défini dans | Consommé par |
|---|---|---|---|
| `StreamEvent` | provider → core | `docs/PROVIDERS.md` (couche multi-provider) | `agent-core` (traduit le wire format en état canonique) |
| `AgentEvent` | core → clients | `docs/ARCHITECTURE.md` (cœur headless) | `agent-tui` et le mode `-p` |

`agent-core` consomme les `StreamEvent` d'un provider et les **traduit** en `AgentEvent` structurés vers les clients. Ce ne sont pas deux noms pour la même chose : `StreamEvent` est un détail de la couche provider (deltas de texte/reasoning/tool-call/usage), `AgentEvent` est le contrat de rendu côté frontends.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Cœur émettant directement de l'ANSI (couplé au TUI) | Fermerait tout client non-terminal et rendrait le core non testable sans terminal. |
| Frontend communiquant avec le cœur via IPC/process séparé | Casse le bénéfice in-process de Rust (cf. ADR-1) : latence, sérialisation, types non partagés. L'in-process avec types partagés est précisément l'avantage qu'on protège. |

**Conséquences & risques.**
- Le **protocole d'événements** (`AgentEvent`) devient une frontière de contrat à versionner avec soin (le TUI et le mode headless en dépendent).
- API consommateur du cœur = **stream** via `async-stream` ; communication TUI via canaux.
- Tout ajout dans le core doit rester pur (pas de dépendance TUI/HTTP) sous peine de casser l'invariant headless.

---

## ADR-4 — Couche multi-provider maison

**Contexte.** Le différenciateur central de Pyxis est le **multi-provider first-class** (tous les modèles frontier), là où Claude Code est Anthropic-only. Il fallait décider comment normaliser des wire formats hétérogènes (Anthropic, OpenAI Chat/Responses, Gemini, Ollama, OpenRouter, Bedrock/Vertex/Azure) tout en gardant la qualité Claude Code et la perf Rust. Version détaillée : `docs/PROVIDERS.md`.

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
- **OpenAI** : **trois surfaces à ne pas confondre**. `OpenAiChatGpt` (backend ChatGPT/Codex, WebSocket borné au tour avec repli SSE stateless) est le provider MVP courant depuis ADR-11. *Chat Completions* (transcript client) reste un adapter BYOK futur. *Responses API publique* (état server-side via `previous_response_id`) ne mappe pas sur le canonique et reste un mode gated sur `capabilities.server_side_state`, jamais par défaut.
- **Gemini** : function calls potentiellement **fragmentées** en stream → **réassembler côté adapter** avant `ToolCallEnd`. `systemInstruction`, context cache.
- **Ollama** : contexte historique retiré du scope par ADR-11. Le risque d'`usage` absent reste utile comme justification provider-agnostique du fallback `agent-tokenizer`.
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

## ADR-5 — Nom : Pyxis

**Contexte.** Le projet avait besoin d'un nom court, CLI-friendly, spatial, distinct de Hera Terminal, et cohérent avec un agent qui navigue dans un codebase pour lire, modifier et revenir avec un diff contrôlable.

**Décision.** **Pyxis**. Commande : `pyxis`. Décidé le **2026-07-02** après abandon de l'ancien nom et séparation claire avec Hera Terminal. La conséquence sur le **nommage des crates** (surface publique `pyxis`, internes `agent-*`) est tranchée en **ADR-8**.

**Justification.** Pyxis renvoie à la constellation de la boussole. C'est le bon imaginaire pour un agent de code : il garde le cap dans un grand repo, localise les bons fichiers, choisit une trajectoire d'édition et ramène un résultat inspectable. Le nom est court, rapide à prononcer, naturel en commande CLI, et moins chargé que les noms de mission spatiale déjà utilisés ou collisionnés.

**Historique du sweep.**

| Candidat | Verdict |
|---|---|
| Hera | Gardé pour le terminal agent-native, pas pour l'agent de code |
| Rosetta | Déjà réservé par un autre projet maison |
| Caelum | Bon sens de sculpture, moins percutant |
| Chandra | Bon imaginaire d'observation, plus doux |
| **Pyxis** | Meilleur équilibre : spatial, court, navigational, CLI-friendly |

**Alternatives écartées.** Les candidats ci-dessus. La règle durable : éviter les noms déjà réservés par les projets maison, les collisions évidentes dans l'espace agent, et les noms trop génériques pour être cherchables.

**Conséquences & risques.**
- Vérifier crates.io, GitHub, domaines et risque marque avant publication publique.
- Le rename invalide les preuves live liées à l'ancien `originator` OpenAI. `originator=pyxis` doit être revalidé ; le fallback `codex_cli_rs` reste prêt.

---

## ADR-6 — Différenciateur recentré

**Contexte.** Plusieurs angles de différenciation ont été envisagés. Il fallait trancher sur le positionnement produit avant d'engager l'implémentation, pour éviter de bâtir autour d'un axe à TAM trop étroit ou déjà couvert par la concurrence.

**Décision.** Différenciateur = **« Qualité Claude Code, tous les providers frontier, perf Rust. »** Concrètement : full Rust natif ultra-perf + multi-provider first-class (là où Claude Code est Anthropic-only), sur un cœur headless embarquable (cf. ADR-3).

**Justification.** C'est l'intersection défendable : la qualité d'un agent de référence, ouverte à tous les modèles frontier, avec un moat technique (Rust natif, cœur headless) qu'un wrapper TS ne peut pas répliquer.

**Alternatives écartées.**

| Axe envisagé | Pourquoi écarté |
|---|---|
| **Verification-grounded / vertical Rust** | **TAM trop étroit.** |
| **Sandbox déclaratif** | **Codex 2026 le fait déjà** — pas de différenciation. |

**Conséquences & risques.**
- Le positionnement est **model-agnostic** : cohérent avec la mitigation du risque N°1 (cf. ADR-7 R1).
- « Qualité Claude Code » est un standard élevé à tenir sur N providers, pas seulement un.

---

## ADR-7 — Registre des risques majeurs

**Contexte.** Le projet porte un petit nombre de risques structurants qui conditionnent la roadmap (le principe étant : **le dur et le risqué en premier**). Ce registre les centralise avec leur mitigation et leur point de décision. La séquence des phases vit dans `docs/ROADMAP.md`.

**Décision.** Maintenir le tableau ci-dessous comme ADR vivant. Chaque risque a une mitigation décidée et, le cas échéant, un go/no-go en Phase 0.

| # | Risque | Nature | Mitigation décidée | Point de décision |
|---|---|---|---|---|
| **R1** | **Blocage Anthropic des outils tiers** s'authentifiant via abonnement Pro/Max (déployé jan 2026, durci avr 2026). Un agent tiers ne peut plus utiliser un abonnement Max. | Produit : **risque N°1** | Mitigation durable : architecture **model-agnostic** et adapters BYOK isolés. Scope actuel après ADR-11 : `OpenAiChatGpt` livré d'abord, autres providers différés ; l'ancien chemin Ollama + OpenAI token reste un verdict historique, pas le scope livré. Message « This credential is only authorized for use with Claude Code... » → classifié `Auth(ThirdPartyBlocked)` (cf. ADR-4, ADR-9). | **Spike auth Anthropic = go/no-go de Phase 0** (1 jour, dans `agent-auth`). Cf. `docs/ROADMAP.md` Phase 0 et `docs/PROVIDERS.md` §6. |
| **R2** | **Vélocité de développement Rust en solo** plus lente que TS/Bun. | Exécution — **risque N°1 d'exécution** | Coût **assumé** (cf. ADR-1). Périmètre MVP serré, dur en premier, crates découplées et testables sans I/O. | Suivi continu sur la roadmap. |
| **R3** | **Sandbox cross-platform** : Landlock filtre le FS au niveau kernel mais **ne filtre pas par hostname** ; pas d'équivalent Linux/macOS uniforme. | Sécurité / portabilité | Landlock **FS** (vrai, kernel-level) + **réseau filtré best-effort via proxy local** (PAS Landlock). macOS Seatbelt en durcissement. | Phase 0 : Landlock FS + 1 appel réseau filtré par proxy. macOS Seatbelt en Phase 3 (`docs/ROADMAP.md`). |
| **R4** | **Pas de SDK Anthropic Rust officiel** — wire format à maintenir à la main, dérive possible. | Maintenance | Couche maison (cf. ADR-4), SDK communautaires (`anthropic-sdk-rs`) comme **référence de wire format**. **Tests VCR** sur payloads providers en **CI obligatoire** = filet sans SDK officiel. | VCR en CI dès Phase 3 (durcissement). |
| **R5** | **Prompt injection** (OWASP LLM01) : output d'outil non fiable détournant l'agent. | Sécurité | **TAINT untrusted** : tout output d'outil (Bash, Read, MCP) est `untrusted` par défaut (`returns_untrusted=true`), propagé. Action destructive/réseau dans un turn contenant du taint récent → **force `Ask`**. | Intégré au pipeline d'outils dès le MVP (Phase 1, cf. `docs/ARCHITECTURE.md`). |

**Justification.** Concentrer ces risques en un seul ADR force la roadmap à attaquer le risqué d'abord (Phase 0 de de-risquage) et donne un point go/no-go explicite (R1) avant d'investir dans l'implémentation complète.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Laisser chaque risque dans le document qui le porte (`docs/ROADMAP.md`, `docs/PROVIDERS.md` §6) | Dispersés, les risques n'ordonnent plus rien : c'est leur concentration en un seul ADR qui force la roadmap à attaquer le risqué d'abord et qui donne un point go/no-go explicite sur R1. |
| Ouvrir un document de risques ad hoc hors du registre | Écartée par la conséquence méta ci-dessous : tout nouveau risque structurant s'ajoute ici. Un document ad hoc sort du champ de vision et le registre cesse d'être vivant. |

**Conséquences & risques (méta).**
- **R1 reste structurant** : après ADR-11, le MVP dépend volontairement du canal ChatGPT subscription pour accélérer le dogfood, mais le positionnement model-agnostic reste l'assurance. Si un canal subscription tombe, le plan de sortie est un adapter BYOK isolé, pas une refonte du cœur.
- Ce registre est **vivant** : tout nouveau risque structurant (ex. évolution de la politique d'un provider, breaking change wire format) s'ajoute ici, pas dans un document ad hoc.
- Les mitigations R3/R4 ont un coût de CI/infra (proxy réseau, harness VCR) à provisionner en Phase 3.

---

## ADR-8 — Nommage des crates : `pyxis*` publié, `agent-*` interne

**Contexte.** Deux nommages coexistaient sans être réconciliés, créant la plus grosse incohérence transversale de la doc :
- ADR-5 acte Pyxis comme identité publique. La disponibilité crates.io/GitHub reste à vérifier avant publication.
- ADR-1, `docs/ARCHITECTURE.md` et `docs/ROADMAP.md` nomment le workspace `agent-core`, `agent-cli`, `agent-tui`, etc.

Les noms réservés n'étaient jamais utilisés par les crates effectivement décrites. Il faut trancher explicitement, sinon ADR-5 et ADR-1/ARCHITECTURE se contredisent.

**Décision.** On découple **identité publiée** et **identité interne**, et on l'acte :

| Rôle | Nom | crates.io | Notes |
|---|---|---|---|
| Binaire / commande | `pyxis` | publié | C'est ce que l'utilisateur installe et exécute (`pyxis`). |
| Crate racine publiée (façade CLI) | `pyxis` (≈ l'actuel `agent-cli`) | publié | Crate de wiring, seule à dépendre de tout. Peut être déclarée `[[bin]] name = "pyxis"`. |
| Réservations de nom | `pyxis-cli`, `pyxis-core` | à vérifier/réserver | À réserver pour protéger l'espace de nom si disponibles ; servent de **placeholders/redirections** si une publication granulaire devient pertinente. Pas (encore) le nom des crates internes. |
| Crates internes du workspace | `agent-core`, `agent-provider`, `agent-tools`, `agent-mcp`, `agent-tui`, `agent-session`, `agent-sandbox`, `agent-auth`, `agent-tokenizer` | non publiées (path deps) | Restent en `agent-*` : ce sont des détails d'implémentation du workspace, non destinés à une consommation externe au MVP. |

Autrement dit : **la surface publiée porte le nom `pyxis` ; les crates internes gardent le préfixe `agent-*`.** La divergence est intentionnelle et documentée ici.

**Justification.**
- Conserver `agent-*` en interne évite un renommage massif de tout le workspace (ADR-1, ARCHITECTURE, ROADMAP) pour un gain cosmétique faible.
- Réserver `pyxis`, `pyxis-cli`, `pyxis-core` protégera l'identité produit sur crates.io si les noms sont disponibles.
- Le binaire et la façade publiée portant `pyxis` : l'utilisateur et l'écosystème ne voient que `pyxis`. Le préfixe `agent-*` n'est visible que dans le repo.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Renommer tout le workspace en `pyxis-core`/`pyxis-cli`/`pyxis-tui`/… | Churn massif sur toute la doc et le futur code, pour un gain cosmétique. Peut être fait plus tard si une publication crate-par-crate devient un objectif réel — pas maintenant. |
| Garder uniquement `agent-*` et abandonner les réservations `pyxis*` | Expose à un squat ultérieur du nom produit sur crates.io. |

**Conséquences & risques.**
- **Action à sécuriser tôt** : vérifier puis publier des stubs pour réserver `pyxis`, `pyxis-cli`, `pyxis-core` sur crates.io avant publication publique.
- Divergence publié/interne à garder lisible : un `README` du workspace doit rappeler que `pyxis` (publié) = `agent-cli` (interne) + façade.
- Si Phase 2/3 impose une publication granulaire (ex. `agent-core` réutilisable par un client externe via crates.io plutôt que path dep), réévaluer : à ce moment les réservations `pyxis-core`/`pyxis-cli` peuvent devenir les noms publiés de ces crates.

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

**Alternatives écartées.** Les trois formulations concurrentes du contexte, chacune pesée puis abandonnée.

| Option | Pourquoi écartée |
|---|---|
| Les 4 classes de la rédaction initiale (BRIEF + ADR-4), sans `RateLimited` | 429 et 529 ont des politiques de backoff distinctes (`Retry-After` contre backoff agressif) ; les fusionner perd la sémantique. |
| `Auth` non paramétrée | Le blocage produit R1 (`ThirdPartyBlocked`) serait noyé dans un `Auth` opaque, alors qu'il ne se traite pas comme un `Expired`, qui déclenche un refresh OAuth. |
| Le nom `ErrClass` de `docs/ARCHITECTURE.md` §3.4 | Un seul nom supprime la divergence entre ARCHITECTURE et PROVIDERS ; celui du document canonique gagne, la taxonomie retenue étant la sienne. |

**Conséquence sur les autres documents.** `docs/ARCHITECTURE.md` §3.4 (renommer `ErrClass` → `ErrorClass`, passer à 5 variantes), `docs/PROVIDERS.md` (déjà canonique, sert de référence) et toute mention dans `docs/ROADMAP.md` doivent référencer cette taxonomie. ADR-4 ci-dessus est aligné.

**Précision withholding (ne pas confondre avec le retry transverse).** Le mécanisme de **withholding** (`Option<PendingError>`) retient **uniquement** une erreur PTL (prompt-too-long) / max-tokens jusqu'à échec confirmé du recovery (tentative de **compaction réactive** avant propagation). Il ne s'applique **pas** aux `Overloaded`/`Retryable`/`RateLimited`, qui relèvent du **backoff transverse** de la couche provider (`docs/PROVIDERS.md` §5.1). Mettre un `PendingError` sur un 529 mélangerait les deux mécanismes : `docs/ARCHITECTURE.md` §3.4 doit ne charger `PendingError` que sur PTL/max-tokens.

**Conséquences & risques.**
- La taxonomie est un point de contrat entre `agent-provider` (qui la produit) et `agent-core` (qui décide retry/fallback/withholding) : tout ajout de variante est un changement de contrat à propager.
- Les payloads providers exotiques (Ollama, OpenRouter) doivent être mappés explicitement vers `ErrorClass` dans leur adapter, faute de quoi ils retombent en `Retryable` par défaut prudent.

---

## ADR-10 — Auth abonnement ChatGPT : `ProviderKind::OpenAiChatGpt` (Responses API backend ChatGPT)

**Contexte.** Le dogfooder principal (Arthur) veut alimenter Pyxis avec son **abonnement ChatGPT** (Plus/Pro), pas une clé API au token, et a rejeté Ollama comme défaut quotidien. L'extraction de l'implémentation de référence Pi (TypeScript, détail vérifié dans `docs/openai-subscription-auth.md`, 45/45 constantes confirmées contre le code) établit un fait structurant : **l'auth abonnement ChatGPT n'appelle PAS Chat Completions.** Elle :
- réutilise le `client_id` OAuth du **Codex CLI officiel OSS** (`app_EMoamEEZ73f0CkXaXp7hrann`), flow PKCE S256 sur `auth.openai.com` (browser callback `localhost:1455` + device-code) ;
- appelle l'inférence sur le **backend ChatGPT via la Responses API** : `https://chatgpt.com/backend-api/codex/responses` (SSE/WS), headers propriétaires `chatgpt-account-id` (claim JWT `https://api.openai.com/auth`.chatgpt_account_id) + `originator`, body `store:false` + `instructions` + `input[]` (jamais `messages[]`).

Cela entre en conflit frontal avec le cadrage antérieur (« cible Chat Completions ; Responses API hors scope ») et touche le piège **`docs/PROVIDERS.md §4.1`** (la Responses API à état server-side `previous_response_id` ne mappe pas le transcript client-side, indispensable à compaction/resume/replay).

**Décision.** Introduire un **`ProviderKind::OpenAiChatGpt`** distinct, et NON réutiliser `OpenAiChat` ni `OpenAiResponses` :
- **Surface séparée** : base `https://chatgpt.com/backend-api/codex`, endpoint `/responses`, `capabilities().server_side_state = false`.
- **Transport amendé en 2026** : le backend Codex est stateless en SSE. WebSocket peut réutiliser `previous_response_id` uniquement pour une extension stricte dans le même tour et sur la même connexion. Nouvelle session, nouveau tour, changement de propriétés non-input ou repli SSE repartent du contexte complet dans `input[]`; compaction/resume restent client-side.
- **Auth** : OAuth PKCE (réutilise le client Codex), credentials en **keyring** (jamais en clair — contrairement au `~/.pi/agent/auth.json` clair de Pi), refresh tokens rotatifs. Implémentée dans `agent-auth/src/oauth/openai_chatgpt.rs`.
- **Statut** : credential **optionnelle, gated, étiquetée « fragile »**. **Jamais en P0**, jamais le défaut, jamais en chemin critique. Le canal Chat Completions au token (BYOK) reste pur et est **clarifié** : il ne couvre pas l'abonnement ChatGPT.

**Justification.**
- L'endpoint et le wire format diffèrent totalement de Chat Completions : un `if` dans `OpenAiChat` créerait des branches conditionnelles fragiles. Adapter dédié = divergences localisées (`PROVIDERS.md §1.1`).
- `OpenAiResponses` générique cible `api.openai.com/v1/responses` (API publique au token) ; le backend ChatGPT en diverge (base URL, headers propriétaires, `store:false` forcé) → ne pas confondre les deux.
- L'invariant porte sur la propriété du transcript, pas sur le transport. Une continuation WebSocket bornée au tour conserve cet invariant tant que chaque frontière de session et chaque fallback repart du corps complet.
- Garder l'abonnement hors P0 respecte **FR-11** et le **risque R1** (ADR-7) : le MVP ne dépend d'aucun canal subscription.

**Alternatives écartées.**

| Option | Raison de l'écart |
|---|---|
| Réutiliser `OpenAiChat` (Chat Completions) pour l'abonnement | Impossible : le backend ChatGPT n'expose pas `/chat/completions` ; wire format incompatible (`input[]` vs `messages[]`, `store:false` forcé). |
| Réutiliser `OpenAiResponses` générique | Cible l'API publique `api.openai.com/v1/responses`, pas le backend ChatGPT (`chatgpt.com/backend-api/codex`) ni ses headers propriétaires. Mélange = fragilité. |
| Activer WebSocket + `previous_response_id` | État server-side connection-scoped → casse compaction / resume JSONL / replay (`PROVIDERS §4.1`). |
| En faire le provider par défaut / P0 | Viole FR-11 et le risque R1 : dépendre d'un canal subscription tiers révocable. |
| Stocker comme Pi (`auth.json` clair 0600) | Viole la règle du keyring (obligatoire, jamais en clair). |

**Conséquences & risques.**
- **ToS-grey** : réutiliser le `client_id` du Codex CLI = se faire passer pour Codex. « Sign in with ChatGPT » est gaté à Codex + IDE partenaires (issue `openai/codex#10974` fermée « not planned »). Usage perso, **révocable unilatéralement** — OpenAI peut faire sur ce client ce qu'Anthropic a fait sur Pro/Max le 4 avril 2026 (R1 s'applique aussi à OpenAI).
- Prévoir un équivalent `Auth(ThirdPartyBlocked)` côté OpenAI si le backend renvoie un message de blocage (wording inconnu à ce jour, à sonder en live comme le leg Anthropic).
- `originator` est hardcodé par client (Pi met `"pi"`) ; Pyxis met `"pyxis"` — le backend **peut** valider l'`originator` contre une liste connue. À tester au premier run ; rejet → soit zone grise totale, soit blocage.
- Dépendance dure au claim custom `https://api.openai.com/auth`.chatgpt_account_id (header `chatgpt-account-id` requis pour router) : un changement de namespace côté OpenAI casse silencieusement.
- **Conséquence sur les autres documents** : `docs/PROVIDERS.md §2` (ajouter `OpenAiChatGpt` à `ProviderKind`), canal BYOK clarifié (scope = Chat Completions au token). Détail d'implémentation : `docs/openai-subscription-auth.md`.

> **Mise à jour (ADR-11, 2026-06-15 ; transport, 2026-08-01)** : le **statut** d'ADR-10 (« gated, optionnelle, jamais en P0 ») est **superseded par ADR-11**. La décision *technique* conserve un ProviderKind distinct, le backend Responses, le transcript client-side et l'OAuth keyring. Le transport est amendé : WebSocket borné au tour avec repli SSE stateless.

---

## ADR-11 — Scope MVP recentré : abonnement ChatGPT d'abord, Ollama retiré, autres providers différés

**Statut.** Acté 2026-06-15 (directive Arthur). Supersede le **scoping** d'ADR-10 (« gated/optionnelle/jamais P0 ») et la portion « Ollama = provider non-bloqué du MVP » d'ADR-7/§6 et de `docs/PROVIDERS.md §6`. **N'altère pas** l'architecture multi-provider (trait `Provider`), qui reste l'invariant.

**Contexte.** Arthur veut dogfooder Pyxis **maintenant**, avec son abonnement ChatGPT (modèles GPT/Codex), exactement comme Pi. Décision explicite : « je ne veux pas faire le multi-provider maintenant, je veux d'abord me concentrer sur Codex et les modèles GPT du plan d'abonnement, que Pyxis fonctionne déjà parfaitement avec, et plus tard j'attaquerai d'autres providers au fur et à mesure. » Et, séparément : **Ollama est retiré du scope** (« trop instable, je ne l'implémenterai certainement jamais »).

**Décision.**
1. **Seul provider livré au MVP = `OpenAiChatGpt`** (abonnement, Responses WebSocket avec repli SSE, ADR-10 amendé). C'est désormais la cible P0 du dogfood, pas une commodité gated.
2. **Ollama supprimé** : variante `ProviderKind::Ollama` / `ProviderId::Ollama` retirée du code, le chantier Ollama **annulé**. Le fallback tokenizer reste — il est provider-agnostique (estimation pré-tour, providers futurs sans `usage`), sa justification n'est juste plus « Ollama ».
3. **OpenAI Chat Completions BYOK différé** au rang de provider futur (plus la cible MVP). Le trait provider (canonique + retry) et l'auth keyring restent et sont **satisfaits**.
4. **L'architecture reste multi-provider** : le trait `Provider` est inchangé, les autres adapters (Anthropic, OpenAI Chat, Gemini…) s'ajoutent ensuite, chacun comme un module d'`agent-provider`, sans toucher le cœur.

**Justification.** Levier dogfood maximal et immédiat (Arthur orchestre des agents toute la journée ; il veut SON modèle, tout de suite). La valeur produit se prouve par l'usage quotidien réel, pas par un tableau de 6 providers vides. Le coût d'opportunité (pas de BYOK Chat Completions au MVP) est assumé : c'est de la séquence, pas de l'abandon.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Livrer le multi-provider de front (Anthropic, OpenAI Chat, Gemini…) avant le dogfood | La valeur produit se prouve par l'usage quotidien réel, pas par un tableau de six providers vides. C'est de la séquence, pas de l'abandon : le trait `Provider` reste l'invariant. |
| Garder Ollama comme provider non bloqué du MVP | Retiré sur directive explicite (« trop instable, je ne l'implémenterai certainement jamais »). Le fallback tokenizer survit, provider-agnostique ; sa justification n'est simplement plus Ollama. |
| OpenAI Chat Completions BYOK comme cible MVP | Le dogfooder veut son abonnement, pas une clé au token. Différé au rang de provider futur, le trait provider et l'auth keyring restant satisfaits. |

**Risque accepté (explicite).** Faire d'un canal subscription tiers **révocable** la fondation *du MVP* contredit temporairement **FR-11** et le **risque R1** (OpenAI peut couper le `client_id` Codex comme Anthropic l'a fait sur Pro/Max le 4 avril 2026 — cf. [[research-subscription-auth-pyxis]]). **Mitigation structurelle** : le pari n'est pas architectural mais de *séquencement*. Le trait `Provider` garde la porte ouverte ; le jour où OpenAI coupe, ajouter un adapter BYOK (Chat Completions ou Anthropic) est un module isolé, pas une refonte. La thèse model-agnostic survit comme **assurance**, même si le premier (et seul) adapter livré est l'abonnement.

**Pire scénario.** OpenAI révoque le client Codex → l'unique provider livré tombe → Pyxis est temporairement inutilisable jusqu'à l'ajout d'un adapter BYOK. Probabilité moyenne, impact élevé mais borné (le code adapter + auth BYOK est petit, le cœur intact). C'est le prix conscient de la vélocité dogfood.

**Conséquences sur les autres documents.** `docs/PROVIDERS.md §6` (Ollama n'est plus le provider non-bloqué du MVP — l'abonnement ChatGPT est la cible ; la stratégie model-agnostic reste l'assurance), tableau §3 (colonne Ollama caduque), `ARCHITECTURE.md` invariant 7 (le fallback tokenizer n'est plus motivé par Ollama mais reste valide). Mise à jour incrémentale ; non bloquant pour le code.

---

## ADR-12 — Runtime de thread `agent-runtime` au-dessus de `run_agent`

**Statut.** Accepté 2026-07-27. Conclusion du spike de runtime d'orchestration durable. N'altère ni ADR-3 (cœur headless) ni le trait `Provider` (ADR-4/ADR-11).

**Contexte.** Pyxis possédait une boucle d'agent, un transcript durable, des outils, un sandbox, MCP, des skills et deux clients, mais aucun objet durable ne possédait une conversation. `agent-cli` relançait un `run_agent` par tour et gardait l'activité dans un `ActiveTurn` process-local : un redémarrage restaurait des messages, pas l'état d'un thread, d'un turn ou d'une opération de contrôle. Le risque numéro un était qu'un nouveau module réimplémente progressivement la boucle modèle-outils et laisse deux orchestrateurs vivants.

**Décision.** Un crate `agent-runtime` entre `agent-core` et les clients, avec quatre responsabilités **disjointes** :

1. **Runtime de thread** (`agent-runtime`) : identité durable (`ThreadId`, `TurnId`, `StepId`, `EventId`, `AgentId`), mailbox de contrôle bornée, cycle de vie des turns, persistance des opérations d'orchestration, arbre d'annulation et shutdown. Un thread est possédé par **un** actor ; les clients ne touchent ni `JoinHandle` ni writer JSONL.
2. **Moteur de turn** (`agent-core::run_agent`) : reste l'**unique** boucle modèle-outils. Retry, compaction, withholding et dispatch d'outils lui appartiennent. Le runtime l'atteint par la seule interface `TurnRunner`, forwarde ses `AgentEvent` sans les modifier et lit exactement un `TurnOutcome`.
3. **Contexte par requête** : la fabrique de contexte injectée dans `RunAgentRunner` est le point où `TurnContext` et `StepContext` se branchent. Le runtime ne construit aucune requête modèle.
4. **Clients** (TUI, headless) : consomment `ThreadHandle` (soumettre, observer, lire le dernier état, arrêter) et le flux `RuntimeEvent`.

Décisions dérivées prises dans le même spike :

- **Store.** Le trait `ThreadStore` et l'adapter mémoire vivent dans `agent-runtime`, qui ne touche pas au disque ; l'adapter JSONL vit dans `agent-session` (dépendance à sens unique `agent-session` → `agent-runtime` → `agent-core`). Les deux adapters passent le **même** test de contrat.
- **Format.** Les événements d'orchestration sont des entrées JSONL **additives** dans le fichier de session existant (`thread_meta`, `thread_event`). `SESSION_SCHEMA_VERSION` n'est **pas** incrémenté : un lecteur v1 les mappe sur `SessionEntry::Unknown` et les ignore, alors qu'un bump rendrait tout fichier neuf illisible par un binaire antérieur qui refuse `schema_version > 1`. Poursuivre une session v1 laisse son préfixe byte-identique.
- **Identifiants.** Pas de dépendance `uuid` : charge opaque de 128 bits rendue `<prefixe>_<32 hex>`, produite par un `IdGenerator` injecté (`rand` en production, compteur déterministe en test). Le préfixe est le discriminant de type au parsing, ce qui fait échouer un `TurnId` reçu là où un `ThreadId` est attendu, y compris après un aller-retour par un fichier.
- **Annulation.** `tokio-util` devient une dépendance directe : `CancellationToken` donne l'arbre parent-enfant que le `CancelToken` maison ne savait pas exprimer, `TaskTracker` compte les tâches dynamiques. *Mis à jour ensuite :* le token maison est **supprimé**. `agent-core::Deps::cancel` porte désormais le `CancellationToken` lui-même, `RunAgentRunner` passe le token du turn tel quel et le point de traduction a disparu avec lui. Ce qui reste du cœur est sa **discipline** d'arrêt (frontières sûres, réconciliation avant terminal), exposée par `agent_core::cancel::guard`, qui poll le futur en premier là où `run_until_cancelled` perdrait la course. Un seul arbre de tokens, un seul mécanisme.
- **Événements live.** `broadcast` borné à 256 plutôt que `mpsc`, contrairement à la recommandation non normative des *Technical Considerations* du cadrage : un client lent ou déconnecté ne doit jamais bloquer l'actor, et perdre un événement **live** est récupérable puisque l'état durable est le store (`RecvError::Lagged` dit au client de relire). Le dernier état voyage bien par `watch`.
- **Exhausted.** `AgentEvent::Exhausted` est mappé sur l'état terminal `failed` avec une cause préfixée, jamais sur `completed` : un arrêt par garde-fou n'a pas terminé le travail et ne doit pas être lu comme un succès.
- **Limites v1.** Mailbox 64, flux live 256, inputs pending 16, grâce d'abort 2 s, shutdown 3 s sont des **constantes** de `agent-runtime::thread`. Zéro clé de configuration publique (FR-20).

**Verdict du spike.** Le prototype pilote `run_agent` avec un provider et un store factices sans réécrire une seconde boucle : dans `crates/agent-runtime/tests/turn_seam.rs`, un échec provider en milieu de flux est **retenté par le moteur**, un outil est dispatché par le pipeline injecté et le turn se termine sur un unique événement terminal, alors que le code du runner ne contient ni retry, ni compaction, ni dispatch. La réconciliation des tool calls sans résultat reste celle du cœur : une annulation par token hiérarchique produit `Interrupted` après réconciliation. **Les chantiers dépendants ne sont donc pas bloquées.**

**Alternatives écartées.**

| Option | Raison de l'écart |
|---|---|
| Mettre le runtime dans `agent-core` | Trois appelants réels (TUI, headless, sous-agents) et un besoin d'état durable ; `agent-core` doit rester le moteur de turn headless sans possession de conversation. |
| Extraire un `AgentRun` pilotable en remplacement de `run_agent` | Refonte du cœur pour un besoin que le seam `TurnRunner` couvre déjà. À rouvrir seulement s'il est prouvé qu'un contexte par step est impossible via la fabrique injectée. |
| Remplacer `SessionEntry` par un nouveau format v2 | Casse la contrainte « toute session v1 reste lisible et poursuivable » et interdit un fork par copie de préfixe. |
| Garder le `CancelToken` maison comme unique mécanisme | Il n'a ni filiation ni comptage de tâches : impossible de garantir « aucun processus orphelin » ni « stragglers abortés puis drainés ». |
| Ajouter `uuid` (UUIDv7) | Coût de dépendance pour une propriété (ordonnancement temporel) dont aucune acceptance n'a besoin ; `seq` par thread ordonne déjà le log. |
| Store SQLite ou fork référencé copy-on-write | Hors scope v1 explicite ; le trait `ThreadStore` laisse la porte ouverte sans changer l'interface. |

**Conséquences & risques.**
- **Risque principal** : `agent-runtime` peut dériver vers une seconde boucle. Garde-fou : `TurnRunner` reste le seul seam, et toute logique de retry, compaction ou dispatch qui y apparaîtrait invalide ADR-12.
- **Risque de double orchestration** pendant la migration : `ActiveTurn`, son compteur `u64`, son `JoinHandle` direct et la FIFO de prompts legacy vivent encore dans `crates/agent-cli/src/interactive.rs`. La migration doit les supprimer, pas les faire cohabiter.
- Un store qui bloque indéfiniment bloque l'actor dans `on_submit` et échappe au budget de shutdown. Acceptable en v1 (écritures fichier locales) ; à revoir si un adapter distant apparaît.
- `agent-session` dépend désormais d'`agent-runtime`. L'inverse est interdit : c'est ce qui garde le runtime sans accès disque.
- **Conséquence sur les autres documents** : `docs/ARCHITECTURE.md` (nouveau crate et sa place dans le graphe) et `docs/EVENT_SCHEMA.md` (entrées `thread_meta` / `thread_event`) sont mis à jour à la livraison, avec la relecture documentaire qui l'accompagne.

### Mesures — reprise et fork

Mesurées le 2026-07-27 sur la machine de référence, build release, adapter JSONL (`cargo test --release -p agent-session -- --ignored --nocapture`).

| Opération | Volume | Mesure | Cible |
|---|---|---|---|
| Reprise (`ThreadStore::read`) | 10 000 événements, 2,5 Mo | **p95 = 12,6 ms** sur 20 runs | < 500 ms p95 |
| Fork matérialisé (`ThreadStore::fork`) | 10 000 événements | **23,3 ms**, 2 527 919 octets copiés | aucune, à consigner |

**Question ouverte tranchée.** Un store référencé (copy-on-write entre fichiers) n'est **pas** justifié en v1 : copier le préfixe entier d'une session de 10 000 événements coûte 23 ms et 2,5 Mo, soit deux ordres de grandeur sous le budget de reprise. La matérialisation reste le choix par défaut ; elle est aussi ce qui rend une branche lisible après suppression de sa source. Le trait `ThreadStore` garde la porte ouverte si une session dogfood dépasse un ordre de grandeur de plus.

**Un index ou un checkpoint de reprise** n'est pas nécessaire non plus : le scan linéaire est 40 fois sous la cible.

**Décisions dérivées de ces mesures.**

- **Identité d'une session v1.** `ThreadId` dérivé du hash du chemin canonique **et** de la première ligne durable, puis matérialisé une seule fois dans la ligne `thread_meta`. La dépendance au chemin ne dure donc que jusqu'au premier append ; après, déplacer ou renommer la session ne change plus rien. Deux copies du même transcript dans deux répertoires restent deux threads distincts, ce qui est le comportement voulu.
- **Provenance d'une branche.** Portée par la **dernière** ligne `thread_meta` du fichier enfant, écrite après le préfixe hérité. Le préfixe est donc copié **verbatim**, sans réécriture d'octets, et l'ancienne liaison du parent qu'il contient est simplement dépassée par celle de l'enfant.
- **Fermeture des turns au resume.** Tout turn laissé non terminal par un arrêt (`queued`, `running`, `needs_input`) est fermé en `interrupted` par un unique événement de recovery, après réconciliation des tool calls sans résultat. Aucun turn ne reste ouvert pour toujours (FR-04), et un transcript repris ne porte jamais de `tool_use` orphelin.
- **Configuration du turn durable.** Le `TurnContext` capturé voyage avec la transition qui démarre le turn, plutôt que dans un second événement : un thread repris sait sous quel modèle, quel mode de permission et quel sandbox son dernier turn a tourné, sans coût d'écriture supplémentaire.
- **Idempotence.** La table `client_message_id -> (TurnId, EventId)` est reconstruite depuis le log au resume, donc un retry client qui traverse un redémarrage est toujours dédupliqué.

---

## ADR-13 — Sous-agent mutateur en worktree Git : **NO-GO v1**

**Statut.** Accepté 2026-07-28. Conclusion du spike sous-agents. N'altère ni ADR-12 (runtime de thread) ni la politique de confinement. Aucune surface de production n'est exposée par cette décision.

**Contexte.** Les sous-agents livrés sont **lecture seule** : autorité = intersection parent ∩ demande, aucun outil mutant exposé par défaut. La question ouverte était de savoir si un enfant **mutateur** pouvait travailler dans un worktree Git temporaire sans élargir le sandbox global ni exposer `.git` au modèle. Le spike devait trancher avant qu'un chantier ultérieur ne s'appuie sur une hypothèse de sécurité non vérifiée.

**Mesuré.** `crates/agent-sandbox/tests/worktree_spike.rs`, machine de référence, kernel avec Landlock actif (`SandboxStatus::Enforced`). Le process enfant applique `enforce_process(workspace = dépôt de fixture, writable_roots = [$TMPDIR])` puis manipule un worktree réel.

| Question | Mesure |
|---|---|
| Création d'un worktree dans `$TMPDIR` sous confinement | **PASSE** |
| Écriture de l'enfant dans sa copie sans toucher au worktree parent | **PASSE** |
| Écritures internes exigées par `git worktree add` | `<dépôt>/.git/worktrees/<nom>/` (`gitdir`, `HEAD`, `commondir`) **et** `<worktree>/.git` (pointeur `gitdir:`) |
| Suppression et nettoyage de l'administration | **PASSE** (`git worktree remove --force`) |
| Confinement toujours effectif hors racines accordées | **PASSE** (écriture refusée par le noyau) |
| **Isolation noyau parent ← enfant** | **ABSENTE** : depuis l'enfant, l'écriture dans le worktree parent réussit |

Cas dégradés, chacun avec sa procédure :

| Cas | Verdict | Récupération |
|---|---|---|
| Dépôt sale | PASSE | aucune ; la branche part du HEAD commité |
| Worktree verrouillé | REFUS de `remove` | `git worktree unlock <chemin>` puis `remove` |
| Répertoire effacé sous git (cleanup en échec) | administration orpheline persistante | `git worktree prune` |
| Hors de tout dépôt Git | REFUS | refuser l'enfant mutateur |
| Répertoire non-Git **imbriqué** dans un dépôt | `git` **remonte** jusqu'au dépôt englobant | ancrer l'enfant sur une racine de dépôt explicite, **jamais** sur un `cwd` |

**Décision. NO-GO.** La capacité mutatrice n'est pas livrée et aucun chantier de suivi n'est ouvert comme livré.

**Justification.** La mécanique n'est pas le blocage : elle passe intégralement, sans élargir Landlock d'un octet. Le blocage est structurel. Les sous-agents de Pyxis sont des **tâches du même process** que leur parent (ADR-12 : un `ThreadHandle` par enfant, pas un process par enfant), or `landlock::restrict_self` est **process-wide et irréversible**. Un enfant mutateur partagerait donc exactement le périmètre d'écriture de son parent : son isolation ne serait qu'une **convention d'outillage**, pas une garantie du noyau. Le spike le mesure directement, en écrivant depuis l'enfant dans le worktree parent avec succès.

Deuxième raison, subordonnée à la première : créer puis nettoyer un worktree exige d'écrire dans `<dépôt>/.git/worktrees/`, c'est-à-dire précisément ce que `PROTECTED_SUBPATHS` soustrait au niveau des outils. C'est tenable **si le runtime le fait lui-même** et que le modèle ne reçoit jamais cette capacité, mais cela ne vaut d'être construit qu'une fois l'isolation réelle acquise.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| Livrer l'enfant mutateur avec l'isolation telle qu'elle est mesurée | `landlock::restrict_self` est process-wide et irréversible, et un enfant est une tâche du même process (ADR-12) : son isolation ne serait qu'une convention d'outillage. Le spike l'écrit noir sur blanc en modifiant le worktree parent depuis l'enfant. |
| Exposer la capacité worktree au modèle plutôt qu'au runtime | Créer puis nettoyer un worktree écrit dans `<dépôt>/.git/worktrees/`, précisément ce que `PROTECTED_SUBPATHS` soustrait au niveau des outils. Tenable si le runtime le fait lui-même, jamais si le modèle reçoit la capacité. |
| Un process OS par enfant mutateur, avec son propre `restrict_self` | Non écartée sur le fond : c'est la seule voie qui donne une isolation du noyau, et c'est une autre architecture que celle retenue ici. Elle est consignée ci-dessous comme condition d'un go ultérieur, aucune n'étant engagée. |

**Conditions d'un go ultérieur** (aucune n'est engagée ici) : un **process OS par enfant mutateur**, avec son propre `restrict_self` restreint à son worktree et une frontière d'événements entre parent et enfant. C'est une autre architecture que celle retenue ici, et les non-goals en vigueur (pas d'app-server, pas de merge/commit automatique) restent applicables.

**Ce qui est acquis malgré le no-go.** Le harnais de mesure reste dans l'arbre : il est ignoré par défaut pour sa moitié confinée, il ne crée aucune surface produit, et il rejoue le verdict en une commande le jour où l'architecture change. L'assertion `parent_writable` du spike est volontairement **positive** : si elle tombe, c'est que le modèle de confinement a changé et qu'ADR-13 doit être rejouée.

---

## ADR-14 : Garde-fou de boucle, échelle vétoiste `[3, 5, 8]`

**Statut.** Accepté 2026-08-20. Lot #3 du plan de portage DeepSeek Harness ([`docs/deepseek-harness-porting-plan.md`](./deepseek-harness-porting-plan.md)). Aucune surface de configuration publique n'est ouverte, aucun type de protocole ne change. Décrit du côté architecture en [`docs/ARCHITECTURE.md`](./ARCHITECTURE.md) §3.6.

**Contexte.** `crates/agent-core/src/guardrail.rs` détectait déjà le batch d'outils identique répété, et refusait de l'exécuter au troisième. Le garde était donc **vétoiste**, plus strict que le rappel consultatif de l'implémentation de référence, mais il n'avait qu'un cran : `Signal` quand le compte atteignait le seuil, `Abort` au compte suivant. Cinq défauts mesurés sur cet état :

1. **Le tour mourait au cran suivant.** Un modèle qui recevait le signal, ne le comprenait pas et retentait une fois tuait la session par `ExhaustReason::ToolLoop`. Un seul message, dans un registre unique, précédait la mort, alors que le cas courant est un modèle qui corrige au second rappel.
2. **Le garde imbriqué de Code Mode écrasait les deux décisions.** `crates/agent-code-mode/src/nested.rs` testait `loop_decision != LoopDecision::Proceed` et rendait le même message dans les deux cas ; le cran terminal n'y faisait rien de plus que le cran de signal, l'appel suivant du même code de cellule repartant au dispatcher.
3. **Une répétition de part et d'autre d'une intervention humaine comptait comme une boucle.** `LoopGuard::reset()` existait sans aucun appelant sur ce déclencheur : le point sûr de steering (US-007) vidait la file d'entrées utilisateur sans toucher le garde, donc un « oui, relance exactement ce test » était vétoé.
4. **Un appel exempt blanchissait une boucle.** Un batch entièrement exempt remettait le compteur à zéro, si bien que la série `read(x)`, `wait`, `read(x)`, `wait` ne déclenchait jamais rien, alors que `wait` et `write_stdin` non terminant sont exactement les outils qu'un modèle bloqué sur une session d'exécution intercale.
5. **Rien n'enregistrait ni ne bornait ces décisions.** `LoopGuard::new` remplaçait silencieusement un seuil de 0 par 1, aucun ADR ne gouvernait le garde-fou, et le commentaire de module renvoyait à une section d'architecture inexistante.

**Décision.** Le garde reste vétoiste et devient une **échelle à trois crans**, portée par le type et non par les sites d'appel.

- `LOOP_GUARD_THRESHOLDS: [u32; 3] = [3, 5, 8]` est une **constante de crate**, validée à la compilation par une `const fn` et un bloc `const _: () = { assert!(...) };`. Une échelle non strictement croissante, ou dont le premier cran est inférieur à 2, ne compile pas. `LoopGuard::new` ne répare plus rien : un `debug_assert!` remplace l'ancien `threshold.max(1)`, et le champ `RunConfig::loop_guard_threshold` est retiré, un `u32` ne pouvant pas porter une échelle.
- **En dessous de 3, exécution normale. À partir de 3, le batch fautif n'est exécuté à aucun cran.** Ce qui escalade est le **registre** du message : `LoopRegister::Gentle` à 3 et 4, `LoopRegister::Detailed` à 5, 6 et 7, arrêt déterministe à 8. Le garde parle sur des **plages** et non sur des comptes exacts, parce qu'un garde qui n'exécute pas doit rendre un `tool_result` par `tool_use` à chaque batch et ne peut donc pas se taire entre deux crans.
- Le rappel doux ne cite **aucun** argument, ce qui garde son coût constant. Le rappel détaillé nomme l'outil, la longueur de la série et les arguments canoniques, sous un plafond de `LOOP_GUARD_ARGS_PREVIEW_BYTES = 500` octets, coupé sur une **frontière de caractère**, avec un suffixe qui dit combien d'octets ont été retirés. C'est le premier message du cœur qui cite le contenu d'un appel d'outil.
- **La clé de détection n'est jamais tronquée.** Elle reste la chaîne canonique complète `nom\0json`, triée puis jointe sur le batch.
- **Deux déclencheurs de remise à zéro, et deux seulement** : un tour neuf, et une entrée de steering qui entre effectivement dans le transcript au point sûr de `run_agent`. L'interjection se propage au garde imbriqué par `ToolDispatch::steering_input_accepted`, méthode de trait à défaut no-op.
- **Un appel exempt est transparent** : il ne compte pas et ne remet pas à zéro. C'est l'exact inverse du comportement précédent, et c'est ce qui ferme le blanchiment du défaut 4.
- **Le site imbriqué se verrouille au cran terminal.** `NestedLoopGuard` retient le message terminal et le rend à tout appel ultérieur du tour, quel que soit l'outil, sans jamais atteindre le dispatcher. Le verrou survit à la fin d'une cellule et tombe au tour suivant ou sur interjection.

**Justification.**

- **Reculer l'arrêt de 4 à 8 ne coûte que des allers-retours modèle.** Le batch n'est exécuté à aucun cran, donc le surcoût de l'échelle est en jetons, jamais en effets. `iter_cap` et `UsageBudget` restent les bornes externes. Le faux positif, lui, est plus cher chez Pyxis que chez un garde consultatif puisqu'il remplace un résultat réel : c'est précisément pour cela que l'arrêt recule.
- **Fail-loud plutôt que repli silencieux.** Un seuil invalide remplacé par un défaut n'efface pas la faute, il la déplace en aval. Les seuils étant des constantes, la validation remonte à la compilation, un cran plus tôt que la validation au chargement de la référence.
- **Borner le rappel, jamais la clé.** Tronquer la clé transformerait le garde en générateur de faux positifs sur les gros arguments : deux corps de `write` d'un mégaoctet ne différant qu'au-delà du plafond deviendraient une boucle. Le bornage est fait **à la construction** du message plutôt que par `bound_feedback`, qui ne s'applique qu'aux sorties de dispatch : la construction sous plafond est strictement plus forte, aucun message non borné n'existant même transitoirement, et elle n'a pas besoin du tokenizer. 500 octets restent très en dessous de `MAX_MODEL_TOOL_RESULT_BYTES`, qui vaut 64 Kio.
- **Une intervention humaine n'est pas la continuation d'une boucle.** Un utilisateur qui demande explicitement de refaire l'appel qui vient d'échouer exprime une intention, pas un spin. Le point sûr de steering est le seul endroit où cette information entre dans le tour, et rien n'y passe entre un `tool_use` et son résultat.
- **Un appel exempt ne prouve aucun progrès.** L'exemption dit « répéter cet appel est le protocole », pas « la série précédente est close ». Remettre à zéro sur un `wait` donnait à un outil de bookkeeping le pouvoir d'annuler la détection, ce qui est exactement l'inverse de son rôle.
- **Le verrou imbriqué existe parce qu'une cellule est du JavaScript.** Le modèle externe ne peut pas ignorer un `Exhausted` ; un code de cellule peut, lui, attraper l'erreur et retenter, et alterner les outils pour continuer à produire des effets. Le verrou est donc vérifié **avant** l'échelle et quel que soit l'outil.
- **Une seule échelle pour les deux sites.** `NestedLoopGuard` partage `LoopGuard` : deux échelles divergentes seraient une seconde source de vérité pour la même décision.
- **Un ADR, pas une note.** La règle de frontière d'`AGENTS.md` tranche seule : une pull request touchant `guardrail.rs` peut violer l'échelle, la transparence des exemptions et le bornage. Ce qui relève de l'arbre de notes est ce qu'aucun crate ne peut contredire, ce qui n'est pas le cas ici. Un ADR n'a pas de note miroir.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| **Détection sensible au résultat** (triplet nom, arguments, résultat, stable sur toute la fenêtre) | C'est la réponse correcte au faux positif sur une sortie qui change, et c'est reconnu comme telle. Elle exige de replier le résultat du batch précédent dans l'observation suivante, donc un état et un chemin de données nouveaux dans le cœur, pour un défaut que le premier cran rend récupérable puisqu'il est un message et non une mort. Reportée, à rouvrir sur un seul faux positif reproductible. |
| **Fenêtre glissante et détection non consécutive** (hachage des N derniers appels) | Détecte l'alternance A, B, A, B que le garde consécutif laisse passer, au prix d'un état par appel au lieu d'une signature et d'un compteur, et de faux positifs sur les séries légitimes. Le coût mémoire et le taux de faux positifs ne sont pas justifiés par un cas observé. |
| **Clé de configuration publique exposant les seuils** | Interdite par l'invariant 15 : chaque limite d'orchestration est une constante de crate. C'est aussi ce que la référence configure par plugin et que Pyxis refuse, la configurabilité déplaçant la validation du compilateur vers l'exécution. |
| **Garde consultatif : exécuter le batch puis rappeler** (la forme de toutes les implémentations relevées, dont celle de référence) | Rend le faux positif indolore, mais laisse un modèle en boucle produire N fois le même effet avant qu'on ne l'arrête. Le veto est la position plus stricte, assumée, et l'échelle est ce qui en paie le coût. |
| **Tronquer aussi la clé de détection**, pour n'avoir qu'un seul chemin de bornage | Deux arguments d'un Mio ne différant qu'au-delà du plafond deviendraient une boucle. Borner la clé transforme le garde en générateur de faux positifs sur exactement le scénario où il compte. |
| **Garder le repli silencieux `threshold.max(1)`** | Un seuil de 0 est une faute de programmation, pas une valeur à réparer. Le repli la déplace en aval, là où elle se lit comme un garde-fou hystérique et non comme un mauvais paramètre. |
| **Garder la remise à zéro sur batch exempt** | C'est le défaut 4 : `read(x)`, `wait`, `read(x)`, `wait` ne déclenche alors jamais rien, et les outils qui blanchissent sont précisément ceux qu'un modèle bloqué sur une session d'exécution intercale. |
| **Faire terminer le tour externe par le verrou imbriqué** | Une cellule verrouillée rend des erreurs et se termine ; le tour continue. Le site externe a sa propre échelle et arrêtera le tour si le modèle relance la même cellule. Laissée ouverte plutôt que tranchée par anticipation. |
| **Un seizième invariant dans `docs/ARCHITECTURE.md`** | Un invariant redondant avec un ADR est une seconde source de vérité. La décision est portée ici, la partie 3 la décrit et la lie. |
| **Une note miroir dans `docs/notes/`** | Interdite par la règle de frontière : une décision qu'une pull request sur `crates/` peut violer est un ADR, et un ADR n'a pas de miroir. |

**Conséquences & risques.**

- **Coût assumé.** Une série qui va jusqu'à l'arrêt dépense au plus cinq allers-retours modèle de plus qu'avant, et **zéro** exécution d'outil supplémentaire. Le message du garde reste sous `LOOP_GUARD_ARGS_PREVIEW_BYTES` plus un préambule constant, quelle que soit la taille des arguments.
- **La transparence des exemptions peut vétoer une relecture légitime** d'un journal qui grossit. Mitigation : le premier cran est un message, pas une mort, et le modèle peut relire avec un décalage. La correction de principe est la détection sensible au résultat, écartée ci-dessus avec sa condition de réouverture.
- **`ToolDispatch` s'élargit** d'une méthode `steering_input_accepted` à défaut no-op. Le défaut est le choix conservateur : ne pas remettre à zéro est plus strict que remettre à zéro, donc un dispatcher qui ignore la méthode ne devient jamais silencieusement plus permissif. L'alternative, un invariant valable sur un site sur deux, est pire.
- **Le garde vit le temps d'un `run_agent`.** Un tour repris après compaction ou après redémarrage repart sur une chaîne neuve. C'est une limite héritée et assumée : la compaction ne remet pas la chaîne à zéro dans le tour courant, mais une reprise en crée une nouvelle.
- **La détection reste une correspondance exacte** sur la chaîne canonique. Un argument qui change d'un octet à chaque itération n'est pas une boucle pour ce garde.
- **La canonicalisation est une hypothèse sur une dépendance.** `serde_json::Map` sans `preserve_order` trie ses clés, ce qui rend la signature stable ; une activation transitive de la feature casserait la détection en silence. Convertie en test nommé (`the_signature_is_blind_to_the_order_of_the_json_keys`), donc en échec de suite plutôt qu'en dérive muette.
- **L'ordre d'appel est ce qui rend le comptage des refus gratuit.** `observe` est invoqué **en amont** de la dispatch, et les refus de permission sont produits à l'intérieur de `Registry::dispatch` : un appel refusé compte donc comme un appel ordinaire, et un modèle qui martèle un appel refusé est arrêté. Un déplacement futur d'`observe` sous la dispatch casserait la propriété, ce qu'un test nommé rend impossible.
- **`RunConfig::loop_guard_threshold` disparaît.** Aucun appelant hors du workspace ne le construisait, vérifié par recherche sur `crates/`.

---

## ADR-15 : Déversement de sortie d'outil

**Statut.** Accepté 2026-08-21. Lot #4 du plan de portage DeepSeek Harness ([`docs/deepseek-harness-porting-plan.md`](./deepseek-harness-porting-plan.md)). Aucune clé de configuration n'est ouverte, aucune variante d'`AgentEvent` n'est ajoutée, aucun outil n'entre au registre. Décrit du côté architecture en [`docs/ARCHITECTURE.md`](./ARCHITECTURE.md) §4.7, et du côté contrat de fil en [`docs/EVENT_SCHEMA.md`](./EVENT_SCHEMA.md).

**Contexte.** Une sortie d'outil trop grosse n'était pas déplacée hors du contexte, elle était **détruite**. `truncate_tail` gardait 30 000 octets de queue, `bash` draînait ses octets à mesure de l'acquisition (le début, où se trouve la première erreur, n'existait donc jamais en mémoire), et `ToolResultTruncation.continuation_hint` portait la chaîne fixe « relancez l'outil avec une requête plus étroite », seul recours proposé au modèle et précisément indisponible quand il compte : build non reproductible, réponse réseau, session d'exécution déjà consommée. L'endroit qui décidait de tronquer, `bound_feedback` dans `agent-core`, ne pouvait rien écrire : le crate n'a aucune I/O par construction. Il n'existait donc aucun endroit du dépôt où une sortie d'outil pouvait être sauvegardée.

**Décision.** Au-delà de `MAX_TOOL_OUTPUT_BYTES` (30 000 octets, le plafond que toute sortie d'outil partageait déjà), la sortie complète est **écrite sur disque avant toute réduction**, et ce que le modèle lit devient un aperçu tête et queue suivi d'une notice portant un localisateur.

- **Trois modules, trois responsabilités.** `crates/agent-tools/src/spill.rs` persiste et rend un localisateur, ne décide rien. `crates/agent-tools/src/spill_policy.rs` construit le remplacement borné, ne stocke rien. `run_one_inner` de `crates/agent-tools/src/registry.rs` décide **quand**, parce que c'est le seul endroit où le texte complet existe encore et où l'identifiant d'appel est connu.
- **La racine est `<workspace>/.pyxis/spill/<12 hex du sha256 de l'identifiant de run>`**, créée par le binaire avant l'application du sandbox et déclarée dans les répertoires d'état inscriptibles. Répertoire en `0700`, fichiers en `0600`, ouverture `create_new` qui échoue sur tout chemin préexistant, lien symbolique compris. Le nom de fichier est `<6 octets hexadécimaux aléatoires>-<nom d'outil encodé>-<identifiant d'appel>.txt`, l'encodage étant injectif sur toute chaîne Rust et neutralisant `../`, les chemins absolus, le NUL et les séparateurs.
- **Le localisateur est relatif au workspace** et voyage dans le champ qui existe déjà, `ToolResultTruncation.continuation_hint`. Aucun chemin absolu n'est sérialisé dans le fil JSONL ni servi à l'app-server.
- **Meilleur effort strict.** Absence de stockage, échec d'écriture, ou notice qui ne tient pas elle-même sous le plafond : chacun journalise à l'échelon avertissement et rend le résultat **original**, octet pour octet. Un échec de déversement ne met jamais `is_error` à vrai et ne remonte rien au modèle.
- **Deux exclusions.** `read` n'est jamais déversé, faute de quoi la boucle « lire, déverser, lire » se referme. Un résultat portant un bloc non textuel reste intact.
- **`bash` déverse au fil de l'acquisition**, un fichier par flux, parce que ses deux lecteurs tournent en parallèle et qu'il ne détient jamais sa propre sortie complète.
- **La relecture est celle qui existe déjà.** `read` pagine par `offset` et `limit` jusqu'au dernier octet, `grep` nomme les fichiers qu'il a sautés, et les parcours récursifs de `grep` et `glob` n'entrent plus dans `.pyxis` ; `confine`, lui, continue d'accepter ces chemins, donc un localisateur passé explicitement reste lisible.
- **La racine est bornée par une constante de crate**, `MAX_SPILL_ROOT_BYTES = 256 Mio`. Au démarrage d'un thread, les répertoires de threads les plus anciens sont supprimés jusqu'à repasser dessous ; le répertoire du thread courant n'est jamais candidat, et la suppression refuse avant l'appel système tout chemin qui n'est pas un enfant direct de la racine.

**Justification.**

- **Trois contraintes forcent l'emplacement, et elles se composent.** D'abord, `confine` (`crates/agent-tools/src/path.rs`) refuse à `read` et à `grep` tout chemin hors du workspace : une racine ailleurs produirait un artefact que le modèle ne pourrait ni relire ni chercher, c'est-à-dire un localisateur inutilisable. Ensuite, sous une politique confinante, l'écriture Landlock est limitée aux répertoires **déclarés** : une racine hors du workspace devrait être ajoutée aux racines inscriptibles, ce qui élargirait le sandbox pour tous les outils et pas seulement pour le déversement. Enfin, `.pyxis` est dans `PROTECTED_SUBPATHS`, donc **aucun outil, `bash` compris, ne peut y écrire** : ce que la racine perd en imprévisibilité par rapport à un `mkdtemp` système, elle le regagne par une propriété plus forte, l'impossibilité pour un outil de planter un lien symbolique dans la racine avant l'ouverture.
- **Une panne de stockage n'est pas un problème du modèle.** Permissions, disque plein, nom déjà pris : aucune de ces causes n'est imputable à l'appel en cours, et aucune ne doit coûter au modèle la sortie qu'il a demandée. Faire remonter la panne transformerait un appel réussi en erreur et pousserait le modèle à relancer une commande qui, elle, a bien tourné. Le journal est le bon destinataire : l'humain peut agir sur un disque plein, le modèle non.
- **Le champ de continuation existait déjà et portait une constante.** Lui donner le localisateur ne coûte aucune surface : c'est exactement ce que son nom promettait, et les consommateurs qui l'affichaient sans le parser continuent de le faire.
- **Aperçu tête et queue, contre la queue seule.** La queue seule est bonne quand seul le verdict final compte et que rien n'est récupérable. Une fois le fichier complet sur disque, l'aperçu ne sert plus à contenir l'information mais à décider s'il faut relire : la tête, où se trouve la première erreur d'une compilation, redevient la moitié la plus utile.
- **Décider en `run_one_inner` plutôt qu'en `bound_feedback`.** Le cœur n'a pas d'I/O, par une décision de graphe que ce lot ne rouvre pas. Le hook `PostToolUse` qui suit le point retenu observe sans réécrire, par décision explicite (US-019), donc il n'est pas candidat non plus. Le remplacement tient très en dessous de `MAX_MODEL_TOOL_RESULT_BYTES` (64 Kio), si bien que `bound_feedback` n'a plus rien à tronquer et que le localisateur traverse intact.
- **Le déversement ne blanchit rien.** Un artefact déversé reste une sortie d'outil : sa relecture par `read` est non fiable comme toute lecture, `returns_untrusted` valant `true`, et l'invariant 3 continue de s'appliquer. Le disque n'est pas une frontière de confiance.
- **Éviction par thread, pas par fichier.** Une reprise trouve les artefacts d'un thread entiers ou absents, et un localisateur évincé échoue en fichier manquant, ce qui se lit. Évincer fichier par fichier laisserait un thread à moitié relisable, état que rien dans le transcript ne pourrait expliquer.
- **Un ADR, pas une note.** La règle de frontière d'`AGENTS.md` tranche seule : une pull request sur `crates/` peut déplacer la racine, faire remonter une panne au modèle ou retirer le plafond. Ce qui relève de l'arbre de notes est ce qu'aucun crate ne peut contredire, ce qui n'est pas le cas ici. Un ADR n'a pas de note miroir.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| **Racine sous le répertoire temporaire du système** (`mkdtemp`, la forme de l'implémentation de référence) | Gagne un suffixe imprévisible et perd le localisateur : `confine` refuse à `read` et à `grep` tout chemin hors du workspace, donc le modèle recevrait l'adresse d'un fichier qu'il ne peut pas ouvrir. L'imprévisibilité est remplacée par une propriété plus forte, `.pyxis` étant inscriptible par aucun outil. |
| **Outil de relecture dédié, visible du modèle** | Coûterait une place permanente dans le prompt système pour dupliquer `read` et `grep`, qui suffisent une fois la pagination livrée. La référence l'exclut également. |
| **Couture de stockage abstraite** (trait `SpillStore`, implémentation locale plus implémentation distante) | Une seule implémentation existe et aucun stockage distant n'est requis. Le trait rendrait de surcroît le « meilleur effort » plus difficile à prouver, la panne à absorber étant exactement celle de l'écriture. À introduire le jour où un second stockage existe, pas avant. |
| **Clé de configuration pour le seuil ou pour la racine** | Interdite par l'invariant 15 : une limite d'orchestration est une constante de crate. Le seuil est déjà `MAX_TOOL_OUTPUT_BYTES`, partagé par toutes les sorties d'outils ; en ouvrir une copie configurable créerait deux vérités pour la même borne. |
| **Faire remonter l'échec de déversement au modèle**, par `is_error` ou par une notice d'échec | Transformerait un appel réussi en erreur pour une cause que le modèle ne peut pas corriger, et l'inciterait à relancer une commande qui a tourné. L'échec va au journal, où un humain peut en faire quelque chose. |
| **Déverser aussi `read`** | Referme la boucle « lire, déverser, relire » : un outil dont le rôle est de paginer un fichier offre déjà `offset` et `limit`, qui sont exactement la reprise que la notice suggérerait. |
| **Politique de rétention par âge, ou suppression en fin de session** | Une session persistée, reprise ou dérivée peut encore référencer un localisateur. Seul un plafond de taille est retenu, qui n'évince jamais le thread courant. |
| **Supprimer le fichier orphelin quand la notice ne tient pas sous le plafond** | Échangerait un reliquat borné, que le plafond de la racine finira par évincer, contre un second chemin d'échec sur le même appel. |
| **Déverser aussi la sortie des sessions d'exécution persistantes** | `exec_session` a son propre plafond et son propre compteur d'omission. L'étendre est un lot distinct, à rouvrir sur un usage réel qui dépasse ce plafond de façon répétée. |
| **Un seizième invariant dans `docs/ARCHITECTURE.md`** | Un invariant redondant avec un ADR est une seconde source de vérité. La décision est portée ici, la partie 4 la décrit et la lie. |
| **Une note miroir dans `docs/notes/`** | Interdite par la règle de frontière : une décision qu'une pull request sur `crates/` peut violer est un ADR, et un ADR n'a pas de miroir. |

**Conséquences & risques.**

- **Le déversement écrit sous le workspace**, donc potentiellement dans un dépôt Git de l'utilisateur. La racine est sous `.pyxis`, déjà exclue par convention et déjà utilisée pour les sessions, et retirée du parcours récursif de `grep` et de `glob`. Si `.pyxis` n'est pas ignoré par Git chez un utilisateur, le problème préexiste à ce lot.
- **Un artefact référencé par un transcript peut être évincé** par le plafond de la racine. L'éviction porte sur les répertoires de threads les plus anciens, jamais sur le thread courant, et journalise à l'échelon information ce qu'elle supprime : une suppression silencieuse rendrait indiagnosticable l'erreur de fichier manquant qui suit.
- **Le taint traverse le disque.** Une sortie non fiable devient un fichier du workspace, dont la relecture reste non fiable. Le déversement ne blanchit rien, et l'invariant 3 s'applique à la relecture comme à la lecture d'origine.
- **Deux mécanismes de bornage se superposent**, `bound_feedback` et le remplacement. Le remplacement tient sous `MAX_TOOL_OUTPUT_BYTES`, très en dessous des 64 Kio de `MAX_MODEL_TOOL_RESULT_BYTES`, donc le cœur n'a plus rien à tronquer sur un résultat déversé.
- **Le coût est une écriture séquentielle** de la taille de la sortie, sans aucune relecture pendant l'appel. En deçà du plafond, aucun fichier n'est ouvert et le coût est nul.
- **Le localisateur apparaît dans le fil JSONL et chez l'app-server.** Relatif au workspace, donc partageable et stable d'une machine à l'autre ; `docs/EVENT_SCHEMA.md` le documente, et un client le transporte sans le découper.
- **Une question reste ouverte** : un modèle qui reconstruit un chemin absolu à partir du localisateur relatif échouerait sur son `read`. Observable dès les premiers tours réels ; un seul échec reproductible rouvrirait le choix du relatif.

---

## ADR-16 : Frontière de survie des travaux de fond

**Statut.** Accepté 2026-08-22. Lot #9 du plan de portage DeepSeek Harness ([`docs/deepseek-harness-porting-plan.md`](./deepseek-harness-porting-plan.md)). Aucune clé de configuration n'est ouverte, aucun outil n'entre au registre, `SESSION_SCHEMA_VERSION` reste à 1 et aucune migration n'est écrite. N'altère pas ADR-12, qui fixe la façon dont `agent-runtime` atteint `run_agent`, ni ADR-13, dont la portée est disjointe (voir plus bas). Le registre qu'elle borne ne traverse ni `docs/ARCHITECTURE.md`, dont la section 4 s'arrête au déversement d'ADR-15, ni `docs/EVENT_SCHEMA.md`, qui décrit le flux JSONL sans tête et non le journal de fil où vivent les entrées de travail : les surfaces que cet ADR contraint sont `crates/agent-runtime/src/jobs.rs`, `crates/agent-runtime/src/thread.rs` et la notice de reprise d'`agent-cli`.

**Contexte.** Le registre de travaux de fond livré par ce lot rend durable l'existence d'un travail, son état terminal et le fait que son résultat ait atteint le modèle. Rien de tout cela ne rend durable le **processus**. `Drop for Session` (`crates/agent-tools/src/exec_session.rs:256`) tue le groupe de processus et `shutdown()` le fait pour tous : un arrêt propre ne laisse rien tourner. Un arrêt brutal, lui, laissait jusqu'ici des processus dont le journal du fil ne portait aucune trace, et un fil rouvert lisait `running` sur des travaux dont le propriétaire n'existait plus.

La question posée par le plan est de savoir si « un redémarrage le retrouve » signifie relancer, ré-attacher, ou rapporter. Le fichier que le plan cite comme source ne tranche pas en faveur des deux premiers : `packages/jobs/jobs-local/src/index.ts:1-10` énonce que tout est gardé en mémoire, donc DeepSeek Harness ne fait survivre aucun travail à un redémarrage. La clause se renégocie sur les invariants de Pyxis, elle ne se comble pas par emprunt.

**Décision.** Les **enregistrements** sont durables, les **processus** ne le sont pas. Une reprise rapporte et n'exécute rien.

- **Tout travail que le journal laisse actif devient terminal à la réouverture**, en `JobStatus::Killed` portant la cause durable `RESTART_CAUSE` (`crates/agent-runtime/src/jobs.rs`), sur le patron exact de `AgentGraph::restore` (`crates/agent-runtime/src/agent.rs:512-525`). La transition est écrite dans le journal du fil **avant** que le registre ne soit reconstruit, donc ce que le registre charge est déjà réconcilié.
- **La réconciliation est elle-même durable.** Un second redémarrage lit un enregistrement terminal et n'a plus rien à réparer : le coût est d'une entrée par travail et par redémarrage, jamais d'une entrée par lecture.
- **Aucun processus n'est relancé et aucun pid n'est ré-attaché.** Le lanceur n'est atteint depuis aucun chemin de reprise, ce qu'un test prouve par un compteur d'appels resté à zéro.
- **Le drapeau de rapport n'est pas touché.** Un travail terminé mais non rapporté au moment de l'arrêt brutal reste non rapporté, et sera annoncé une fois, exactement une. C'est l'écart délibéré avec `AgentSupervisor::restore` (`crates/agent-runtime/src/supervisor.rs:344-367`), qui marque `delivered: true` sur tout relais restauré.
- **Ce que la reprise a fermé est dit à l'humain et au modèle.** L'humain le lit en ouvrant la session, une ligne par travail nommant sa commande tronquée et sa cause ; le modèle le lit par `list_jobs`, sur les mêmes enregistrements. La notice ne nomme que ce que **cette** reprise a fermé : la cause étant durable, la déduire de l'enregistrement ferait répéter la même interruption à chaque réouverture du journal, donc le registre retient la liste de sa propre réconciliation.

**Justification.**

- **Un état `running` sans propriétaire est un mensonge, pas une information manquante.** Le drapeau lu par l'humain et par le modèle affirmerait qu'un processus tourne alors que le seul objet qui pouvait l'arrêter, le nœud d'annulation du fil, est mort avec le processus précédent. Fermer l'enregistrement avec une cause est la seule lecture qui reste vraie sans rien exécuter.
- **La survie réelle a un coût nommable, et ce lot ne le paie pas.** Elle demanderait trois choses simultanément, et aucune n'est un détail d'implémentation. D'abord **un puits fichier sous `.pyxis/`** : un travail qui survit à son parent ne peut pas garder un tube, puisqu'une écriture dans un tube sans lecteur lève `SIGPIPE` ou `EPIPE` ([`pipe(7)`](https://man7.org/linux/man-pages/man7/pipe.7.html)), donc la sortie doit aller dans un fichier dont le chemin est durable. `.pyxis` est déjà le bon emplacement, protégé par `PROTECTED_SUBPATHS` (`crates/agent-tools/src/path.rs:33`) contre l'écriture de tout outil, `bash` compris. Ensuite **un groupe de processus détaché**, par `setsid`, sans quoi le travail meurt avec la session qui l'a lancé et la durabilité du puits ne sert à rien. Enfin **une ré-attache par pid**, qui n'a **aucune garantie d'identité** : un pid est recyclé, et rien dans l'enregistrement ne prouve que le processus portant ce numéro au redémarrage est celui qui a été lancé. Les deux premiers sont du travail ; le troisième est un trou de correction qu'aucune quantité de travail ne referme sans un mécanisme d'identité que le dépôt n'a pas.
- **Rapporter est complet, ressusciter est partiel.** Un rapport est vrai dans tous les cas : le processus est mort, ou il tourne encore orphelin, et dans les deux cas l'enregistrement dit correctement que ce fil ne le tient plus. Une ré-attache est vraie seulement quand le pid n'a pas été recyclé, et fausse silencieusement sinon, ce qui donnerait au modèle un identifiant qu'il pourrait tuer.
- **Ce n'est pas ADR-13.** ADR-13 refuse le sous-agent mutateur en worktree Git parce que l'isolation noyau d'un enfant mutateur ne se compose pas : `landlock::restrict_self` est irréversible et propre au processus, donc un parent déjà restreint ne peut pas accorder à un enfant une racine qu'il s'est retirée. C'est une décision sur la **portée des droits** d'un enfant. Celle-ci porte sur sa **durée de vie**, et un travail de fond ne demande aucun droit que le processus qui l'a lancé n'avait pas. Les deux se lisent ensemble sans que l'une implique l'autre : ADR-13 dirait non à un travail de fond plus privilégié que son parent, elle ne dit rien d'un travail de fond qui lui survivrait.
- **Un ADR, pas une note.** La règle de frontière d'`AGENTS.md` tranche seule : une pull request sur `crates/` peut ajouter une ré-attache par pid, un `setsid` ou un relancement à la reprise. Ce qui relève de l'arbre de notes est ce qu'aucun crate ne peut contredire. Un ADR n'a pas de note miroir ; la note de ce lot consigne l'écart au plan, ce qui est une autre question, et c'est elle qui lie cet ADR.

**Alternatives écartées.**

| Option | Pourquoi écartée |
|---|---|
| **Ré-attacher par pid à la reprise** | Un pid est recyclé : rien dans l'enregistrement ne prouve que le processus portant ce numéro est celui qui a été lancé. Le mode d'échec n'est pas une erreur mais un faux positif silencieux, où le modèle reçoit l'identifiant d'un processus étranger qu'il peut tuer. Une garantie d'identité demanderait un mécanisme que le dépôt n'a pas. |
| **Relancer la commande à la reprise** | Ré-exécute un effet de bord que personne n'a redemandé. Une commande de fond est très exactement le genre qu'on ne rejoue pas à l'aveugle : un déploiement, une migration, un `rm`. La durabilité du dépôt porte sur les décisions acceptées, jamais sur leur ré-exécution. |
| **Détacher par `setsid` et garder un puits fichier**, sans ré-attache | La moitié coûteuse de la survie sans la moitié utile : le travail continue, sa sortie s'accumule, et aucun fil ne peut plus ni le lister comme actif ni l'arrêter. Le registre affirmerait un travail terminé pendant qu'un processus vivant écrit sur le disque. |
| **Laisser l'enregistrement en `running` et laisser le lecteur conclure** | Reporte le mensonge sur chaque lecteur au lieu de le corriger une fois. `list_jobs`, la notice de reprise et `/status` devraient chacun réinventer la règle « actif dans un journal rouvert veut dire mort », et la première surface qui l'oublierait afficherait un travail fantôme. |
| **Un sixième état `Interrupted` dans `JobStatus`** | FR-01 ferme le vocabulaire à cinq valeurs et EP-041 est livré sur cette fermeture. `Killed` est déjà défini comme « arrêté par son propriétaire, un arrêt ou une reprise » ; ce qui distingue les trois est la cause durable, donc la cause est une constante nommée et non une sixième valeur. |
| **Marquer `delivered: true` comme `AgentSupervisor::restore`** | Un relais restauré est un message que le parent a déjà eu l'occasion de lire dans le tour qui l'a produit ; un résultat de travail de fond est le cas inverse, puisque la raison d'être du registre est que personne ne regardait quand il s'est réglé. Le marquer rapporté ici produirait le seul mode de défaillance qu'EP-044 existe pour empêcher, un résultat annoncé zéro fois. |
| **Écrire aussi `JobReported` pendant la réconciliation** | Coûterait une seconde entrée durable par travail à chaque redémarrage, pour un travail que personne n'a lu. La NFR de ce lot autorise une entrée par transition de réconciliation, pas deux. |
| **Une clé de configuration pour choisir entre rapporter et ressusciter** | Interdite par FR-16 et par l'invariant 15, et pire, ferait de la frontière une préférence. Une frontière qu'une variable d'environnement déplace n'est pas opposable à une pull request. |

**Conséquences & risques.**

- **Un arrêt brutal peut laisser des processus orphelins que Pyxis ne tient plus.** Le registre dit correctement que ce fil ne les tient pas ; il ne prétend pas qu'ils sont morts. C'est le prix explicite de la frontière, et le seul recours reste celui du système d'exploitation.
- **Le coût durable de la reprise est d'une entrée de journal par travail actif.** Borné par `MAX_ACTIVE_JOBS`, donc au plus quatre par redémarrage, et nul pour un journal dont tous les travaux sont déjà terminaux.
- **Un journal écrit avant ce lot se rouvre sans travail et sans erreur.** Aucune entrée de travail n'y existe, donc la reconstruction en trouve zéro et n'écrit rien ; `SESSION_SCHEMA_VERSION` reste à 1 et aucune migration n'est nécessaire.
- **La notice de reprise nomme une commande composée par le modèle.** Elle est neutralisée de ses caractères de contrôle et tronquée sur frontière de caractère avant d'atteindre l'écran, sur le même chemin que la notice de fin de tour.
- **Le jour où la survie réelle deviendrait un besoin**, les trois coûts nommés plus haut sont le devis, et le trou d'identité par pid est la question à résoudre en premier. Rouvrir cet ADR est le passage obligé ; l'ajouter par une pull request ne l'est pas.
