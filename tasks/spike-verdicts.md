# EP-001 — Verdicts de Phase 0 (spikes de dé-risquage)

> **Statut : PASSE (go pour Phase 1).** Les 5 spikes ont un verdict écrit. Le go/no-go
> d'accès provider (US-001) est tranché. Code jetable, isolé dans `spikes/` — **rien
> de ceci n'est du MVP** et rien ne doit être porté tel quel en Phase 1 (cf.
> `docs/ROADMAP.md` Phase 0 : « aucune dette à porter, aucune API à figer hormis le verdict auth »).
>
> **Note ADR-11.** Le provider set issu de ces verdicts est historique. Ollama comme premier dogfood et le duo Ollama + OpenAI Chat au token ne sont plus le scope livré ; le MVP courant livre `OpenAiChatGpt` d'abord. Les spikes restent des preuves de faisabilité, pas une promesse produit actuelle.

| Date | 2026-06-15 |
|---|---|
| Auteur | Arthur Jean |
| Toolchain | rustc/cargo 1.95.0, edition 2024, mold (link), workspace `spikes/` |
| Kernel | 7.0.12-201.fc44 — `CONFIG_SECURITY_LANDLOCK=y`, Landlock actif dans LSM |
| Ollama | local, modèles `devstral-small-2:24b` (tool-capable) + `arthurjean/mistral-trismegistus:7b-q6_K` |
| Gates | `cargo check`/`clippy --no-deps`/`fmt --check` verts ; **18/18 tests** passent |

Reproductibilité : tout part de `spikes/`. `cargo test --workspace` pour les preuves
déterministes ; les runs live sont rappelés sous chaque spike.

---

## US-001 — Accès provider (go/no-go auth) — **PASSE (avec réserve non bloquante)**

**Hypothèse.** Il existe au moins un canal d'auth par lequel l'utilisateur final parle
au modèle sans être bloqué, sur lequel bâtir le MVP.

**Exécuté.**
- **Ollama (local) — PROUVÉ EN RÉEL.** `s1-provider-access ollama` : flux reçu (615 chars),
  **aucune credential distante**, `usage` présent (`input:202 output:256`). Provider local
  **non-bloqué**, viable comme défaut MVP.
- **OpenAI (API key au token) — harness prêt, non exécuté ici.** Manque `OPENAI_API_KEY`
  dans l'env. Le runner streame `/v1/chat/completions` avec `stream_options.include_usage`
  (coût metérable) et classifie un 401 en `Auth::Invalid`. **À lancer par Arthur** :
  `OPENAI_API_KEY=sk-... cargo run -p s1-provider-access -- openai`.
- **Anthropic (blocage outils tiers) — sonde prête, non exécutée ici.** Le runner POST
  `/v1/messages` et capture le **message exact** ; détection du blocage par sous-chaîne
  `"only authorized for use with Claude Code"` → `Auth::ThirdPartyBlocked`. **À lancer**
  avec un token API (devrait passer) et un token d'abonnement (devrait être bloqué) :
  `ANTHROPIC_API_KEY=... / ANTHROPIC_OAUTH_TOKEN=... cargo run -p s1-provider-access -- anthropic`.
  Test unitaire `third_party_block_message_is_classified` : la classification du message
  de blocage est déjà couverte (payload réel d'Anthropic).

**Verdict go/no-go (tranché).** Le MVP **ne dépend d'aucun canal subscription** (FR-11).
Chemin non-bloqué confirmé : **Ollama local (zéro auth) + OpenAI Chat au token**. Anthropic
reste **conditionnel** : son statut (first-class / dégradé / différé) dépend du run des deux
legs ci-dessus, **mais la viabilité du produit n'en dépend pas** (ADR-7 R1, PROVIDERS §6).
La réserve (2 legs à clés) ne bloque pas Phase 1 : l'archi route déjà autour.

**Premier dogfood (Open Question tranchée par le spike).** **Ollama** est le provider du
premier dogfood — gratuit, local, prouvé. OpenAI au token vient en second pour la qualité.

---

## US-002 — Provider canonique sur 1 stream SSE — **PASSE**

**Hypothèse.** La couche maison (`reqwest` + `eventsource-stream`, sans SDK) décode un flux
provider vers des `StreamEvent` canoniques (Anthropic-like, `PROVIDERS.md §2`).

**Exécuté.**
- **6 tests déterministes** (`cargo test -p s2-canonical-sse`) sur payloads OpenAI-compat :
  `TextDelta`, `Usage`, `Done{EndTurn}`, **tool calls fragmentés réassemblés** (invariant
  `args_json` complet & JSON valide à `ToolCallEnd`), **chunk malformé → `AdapterError::Json`
  sans panic** (AC3), sentinelle `[DONE]` no-op.
- **Live Ollama** : `s2-canonical-sse "Compte de 1 à 3."` → **87 `StreamEvent` décodés**
  de bout en bout (`TextDelta`*, `Done{EndTurn}`, `Usage`) depuis un vrai flux SSE.
- Flux interrompu (AC2) : la couche `stream_chat` mappe une coupure transport en
  `AdapterError::Stream(_)` (erreur typée propagée, pas de panic).

**Verdict.** Le format canonique tient sur le premier provider. `eventsource-stream` parse
proprement le SSE OpenAI-compat. **Go.** Réserve actée pour Phase 1 : Gemini fragmente les
tool calls différemment (réassemblage déjà prévu, `PROVIDERS §4.2`) — non testé ici (hors
scope Phase 0, mono-provider).

---

## US-003 — Boucle minimale stream → outil → reboucle — **PASSE**

**Hypothèse.** Une state machine à `enum Transition` exhaustif, dans sa forme réduite,
streame → exécute un `Bash` → réinjecte → reboucle jusqu'à `end_turn`, et reste robuste.

**Exécuté.**
- **4 tests** (`cargo test -p s3-agent-loop`), `Provider` **injectable** (`ScriptedProvider`,
  preuve sans API réelle — invariant « deps injectables ») :
  - `loop_runs_tool_then_ends` : tool_use → exec → réinjection → reboucle → `EndTurn` (AC1/AC3),
    sortie d'outil marquée `untrusted`.
  - `tool_timeout_does_not_freeze_loop` : un outil `sleep 5` sous timeout 200 ms est **signalé
    timeout, la boucle reprend la main et se ferme** (AC2, `kill_on_drop` tue l'orphelin).
  - `decide_transition_is_exhaustive_and_pure` : fonction pure, `match` exhaustif.
- **Live devstral** : la boucle a fait émettre un `tool_use bash{cmd:"echo bonjour depuis pyxis"}`,
  exécuté, réinjecté `bonjour depuis pyxis`, le modèle a conclu, **`EndTurn` propre en 2 tours**.

**Verdict.** La state machine se ferme proprement, les transitions sont exhaustives (vérifié
compilation). **Go.** Note : `Transition::Compact`/`Recover` (withholding, compaction) sont
**hors scope Phase 0** par décision roadmap — à introduire en US-006/US-008.

---

## US-004 — Rendu TUI streaming brut (Ratatui) — **PASSE**

**Hypothèse.** Le tube `agent-core → canal → agent-tui` rend le texte streamé token-par-token,
le cœur n'émettant que des `AgentEvent` (jamais d'ANSI).

**Exécuté.**
- **3 tests `TestBackend`** (`cargo test -p s4-tui-stream`) : accumulation token-par-token
  rendue dans le buffer (AC1, version déterministe), **resize en plein stream → reflow sans
  corruption** (AC2, aucun panic d'indices), sélection monochrome sans truecolor (AC3).
- **Dump headless** (`s4-tui-stream` hors TTY) : rendu monochrome, transcript wrappé, filet
  fin sur le champ de saisie, accent unique sur le marqueur `›`. Aucune bordure ASCII lourde.
- Le « cœur » (thread feeder) communique **exclusivement** par `mpsc<AgentEvent>` — frontière
  respectée.

**Verdict.** Le découplage cœur/TUI ne fuit pas (events, jamais d'ANSI). Rendu fluide et
épuré côté pipeline. **Go.** **Réserve subjective** : la fluidité perçue (scintillement,
cadence) se valide à l'œil — `cargo run -p s4-tui-stream` **dans un vrai terminal truecolor**
(reste à faire par Arthur, c'est le seul critère non automatisable de ce spike).

---

## US-005 — Sandbox Landlock FS + proxy réseau — **PASSE**

**Hypothèse.** Landlock confine le FS au niveau kernel ; un proxy local filtre le réseau par
hostname (Landlock ne filtre pas le réseau, ADR-7 R3).

**Exécuté (en réel sur le kernel d'Arthur).**
- **Landlock** : `s5-sandbox landlock` → ruleset **`FullyEnforced`** ; écriture sous workspace
  OK ; **écriture hors workspace REFUSÉE au kernel (`Permission denied`, os error 13)** (AC1).
- **Proxy** : `s5-sandbox proxy` → hôte `api.allowed.test` **tunnelisé (200, bannière upstream
  reçue)** ; hôte `evil.exfil.test` **bloqué (403) + journalisé** (AC2). Self-contained,
  déterministe (upstream local, DNS stubbé ; la logique de sécurité — le check d'allow-list
  sur le hostname — est identique en prod).
- 2 tests (`cargo test -p s5-sandbox`) : allow-list fail-closed, tunnel/403.

**Verdict.** **Faisabilité solo confirmée** : Landlock FS = vrai, kernel-level ; proxy CONNECT
applicatif = suffisant pour le filtrage par hostname. **Go — pas besoin de basculer sur
nftables** pour le MVP (l'alternative reste notée si le proxy s'avère fragile sous charge en
Phase 1). Dégradation non-Linux : le binaire `cfg`-gate Landlock et avertit explicitement
(AC3, Linux-first).

---

## Synthèse & décisions débloquées pour Phase 1

| Inconnue Phase 0 | Verdict | Conséquence Phase 1 |
|---|---|---|
| Canal d'auth viable (R1) | **OUI** — Ollama (réel) + OpenAI au token ; Anthropic conditionnel | MVP model-agnostic, provider set = Ollama + OpenAI Chat |
| Format canonique tient sans SDK | **OUI** | `agent-provider` : trait `Provider` + `StreamEvent` canonique (US-015) |
| State machine se ferme proprement | **OUI** | `enum Transition` exhaustif + `async-stream` (US-006) |
| Frontière cœur/TUI (events, pas d'ANSI) | **OUI** | `agent-tui` client du `Stream<AgentEvent>` (US-019) |
| Sandbox FS + réseau faisable solo | **OUI** (Landlock + proxy, sans nftables) | `agent-sandbox` Landlock FS + proxy (US-020) |

**Open Questions du PRD touchées par les spikes :**
- *Provider du premier dogfood* → **tranché : Ollama** (gratuit, local, prouvé).
- *Faisabilité proxy solo vs nftables* → **tranché : proxy solo suffit** pour le MVP.
- *Tokenizer / fallback usage* → confirmé pertinent : Ollama **a** émis `usage` ici, mais le
  fallback `agent-tokenizer` reste **non négociable** (modèles/configs sans usage), cf. US-007/016.

**Reste à la main d'Arthur (non bloquant pour le go) :**
1. `OPENAI_API_KEY=… cargo run -p s1-provider-access -- openai` → capter usage/coût réels.
2. `ANTHROPIC_OAUTH_TOKEN=… cargo run -p s1-provider-access -- anthropic` → **figer le message
   de blocage exact** (le verdict R1 est déjà tranché ; ceci ne fait que documenter la preuve).
3. `cargo run -p s4-tui-stream` dans un terminal truecolor → valider la fluidité à l'œil.

**GO Phase 1.** Aucune inconnue tueuse de projet ne subsiste. Code de Phase 0 à jeter (ne pas
porter dans le workspace MVP `agent-*`).

---

# US-005 — Enveloppe V8 et frontière de sécurité — **PASSE (in-process retenu)**

> Périmètre différent des cinq spikes ci-dessus : ce verdict appartient à
> `tasks/prd-parite-totale-codex-cli.md` (EP-002, US-005) et conditionne US-006.
> Le code de mesure vit dans `spikes/s6-code-mode-v8/` et reste jetable ; seul le
> verdict est normatif.

| Date | 2026-07-28 |
|---|---|
| Toolchain | rustc/cargo 1.97.1, edition 2024, Linux x86_64 |
| Dépendances | `v8 = "=149.2.0"` (pin exact de la baseline Codex), `deno_core_icudata = "0.77.0"` |
| V8 lié | `14.9.207.2-rusty`, binaires précompilés (pas de `V8_FROM_SOURCE`) |
| Reproduction | `cargo test -p s6-code-mode-v8` (9 tests) et `cargo run --release -p s6-code-mode-v8 -- report [--json]` |

**Hypothèse.** Un isolate in-process peut recevoir une enveloppe bornée :
interruptible de l'extérieur en moins d'une seconde, plafonné sur le heap V8 **et**
sur la mémoire native avec les deux dépassements distinguables, et joignable au
shutdown.

## Mesures (profil release, machine de référence)

| Dimension | Mesure | Budget PRD | Verdict |
|---|---|---|---|
| Taille binaire | 71,1 MiB (binaire du spike, V8 statique) | non budgété | coût acté |
| Init process (ICU + platform + `V8::initialize`) | 0,7 ms, une seule fois | non budgété | négligeable |
| Session froide prête | p50 3,9 ms / **p95 4,1 ms** / max 5,4 ms sur 100 | < 1 000 ms p95 | **OK** (245x de marge) |
| Cellule chaude | p50 3,2 ms / **p95 3,2 ms** / max 5,2 ms sur 1 000 | < 100 ms p95 | **OK** (31x de marge) |
| Heap V8 après charge | 15,6 MiB utilisés / 31,0 MiB alloués / plafond 256,0 MiB honoré | plafond 256 MiB | **OK** |
| Mémoire native | RSS 27,4 MiB, delta +22,5 MiB pour le run complet | non budgété | coût acté |
| Interruption d'une boucle infinie | **201,9 ms** pour un budget de 200 ms (dépassement 1,9 ms) | < 1 000 ms | **OK** |
| Runtime Tokio pendant le spin | 18 ticks de 10 ms observés | ne doit pas bloquer | **OK** |

## Ce que le spike prouve

- **Interruption externe.** `IsolateHandle::terminate_execution()` appelé depuis un
  thread watchdog arrête `while (true) {}` en 1,9 ms au-delà du budget. L'isolate
  vit sur un thread OS dédié, hors du runtime Tokio : une tâche Tokio de 10 ms a
  continué de tourner pendant tout le spin. `cancel_terminate_execution()` après
  la cellule rend la session réutilisable — la cellule suivante répond `2` à `1 + 1`.
- **Deux plafonds mémoire distincts.** Heap V8 saturé par des chaînes retenues →
  `v8 heap limit reached: 16777216 bytes`. Mémoire native saturée par des
  `ArrayBuffer` → `native memory budget exceeded: 9437184 bytes requested,
  8388608 bytes allowed`. Aucun processus n'est tué : rendre du headroom depuis le
  `NearHeapLimitCallback` laisse V8 dérouler la pile au lieu d'appeler
  `FatalProcessOutOfMemory`.
- **Piège découvert, à porter en US-007.** V8 traite un refus d'allocation
  d'`ArrayBuffer` comme une pression heap et **déclenche aussi** le
  `NearHeapLimitCallback`. Classer sur le drapeau du heap d'abord donnerait
  « heap » pour un dépassement natif. Le drapeau de l'allocateur est le seul non
  ambigu et doit être testé en premier.
- **Isolation.** Un contexte neuf par cellule : `globalThis.leaked` posé par une
  cellule est `undefined` pour la suivante, dans le même isolate comme dans un
  autre. L'état inter-cellules devra donc être un store explicite (US-008), jamais
  l'objet global.
- **Mode JIT figé au process.** V8 lit `--jitless` une fois. Une seconde
  initialisation demandant l'autre mode échoue explicitement
  (`v8 already initialized with jit=...`) au lieu de tourner silencieusement sous
  le premier : c'est le comportement attendu par US-007 AC2.
- **Shutdown borné.** Le worker est joint sous 2 s ; le cas non joint est
  **retourné** au lieu d'être détaché, ce dont US-007 AC4 a besoin.

## Verdict

**Architecture in-process retenue.** Aucun seuil obligatoire n'est manqué, donc la
clause de blocage d'US-005 AC4 ne s'applique pas : US-006 n'est pas bloquée et
aucune recommandation process-owned n'est activée.

Réserves consignées, non bloquantes :

1. **71 MiB de binaire** et ~22 MiB de RSS supplémentaires sont le prix de V8
   statique. La conséquence est architecturale, pas de perfomance : le moteur doit
   rester dans une crate isolée pour que les consommateurs headless de Pyxis ne le
   paient pas.
2. **L'isolate n'est pas un bac à sable de capacités.** Il sépare l'état JavaScript
   et borne CPU/heap/native ; il ne remplace ni les permissions, ni le taint, ni les
   hooks. Tout pont natif reste une allowlist explicite (US-008).
3. Les mesures sont faites sur **une** machine Linux x86_64. Elles bornent la
   faisabilité, pas la performance de tout le parc.
