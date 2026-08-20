# Note: ne pas partir de la base Codex CLI

Statut: implemented

## Problème

Pyxis visait la parité avec Codex CLI, et la question ouverte le 2026-07-27 était celle du chemin :
repartir de la base Codex, mûre et réellement dogfoodée, ou continuer sur une base propre. La
prémisse du fork tenait, Codex est mûr et Pyxis ne l'était pas, et rien dans le dépôt ne disait
pourquoi ce chemin n'avait pas été pris. Une option écartée sans trace se repropose.

Ce document complète [l'audit du 2026-07-27](../../../parity/audits/parity-audit-2026-07-27.md), qui énumère les
écarts ; celui-ci répond à une question différente : **par quel chemin**. Mesures prises sur les
dépôts réels, Pyxis à `0c1cf17` et Codex à `95637f7056`, tous deux clonés localement, sans modifier
une ligne de code.

La cible de parité n'est plus ce document : elle est `docs/parity/codex-baseline-matrix.json`,
générée depuis le clone Codex figé au commit `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`
(`cargo run -p agent-parity -- check`). L'état réellement livré est dans
[`docs/CURRENT_STATUS.md`](../../../CURRENT_STATUS.md), et la preuve qui l'accompagne dans
[`docs/parity/offline-suite.md`](../../../parity/offline-suite.md). Ce qui survit ici est la
décision, pas l'instantané de mesures qui l'a portée.

## Décision

**Ne pars pas de la base Codex CLI. La prémisse est juste, la conclusion ne suit pas.**

La prémisse est exacte : Codex est mûr et réellement dogfoodé, Pyxis ne l'est pas. Mais la maturité
de Codex ne vient pas de son code source, elle vient de **176 contributeurs et 707 commits sur les
30 derniers jours** (mesuré par `git log` sur le clone local). Ce n'est pas un actif transférable
par un fork : c'est un flux. Forker, c'est prendre l'instantané d'un flux et se condamner à courir
derrière.

Ce qu'il faut faire à la place, dans cet ordre :

1. **Réparer le signal de vérification** (un test non déterministe passe en CI par chance).
2. **Rendre Pyxis installable et utilisable hors de son propre dépôt.** C'est le vrai blocage : le
   dogfood n'a jamais eu lieu ailleurs, et c'est mécanique, pas culturel.
3. **Absorber quatre composants Codex, crate par crate**, là où ils sont autonomes et mûrs.
4. **Puis seulement** les écarts de comportement modèle (`apply_patch`, `update_plan`).

---

## 1. Audit de santé du code

### Métriques

| | Pyxis | Codex CLI |
|---|---|---|
| Lignes Rust | 52 093 | 1 222 552 |
| Crates | 10 | 107 |
| Commits totaux | 120 | 8 612 |
| Commits sur 30 jours | 0 (dernier : 26 juil.) | 707 |
| Contributeurs sur 90 jours | 1 | 176 |
| Tests | 835 | non mesuré |
| Snapshots de rendu | 37 | 613 (TUI seul) |
| CI | verte (fmt + clippy + tests) | non mesurée |

Répartition Pyxis : `agent-tui` 20 626 lignes (40 %), `agent-tools` 8 335, `agent-cli` 7 096,
`agent-core` 6 803, `agent-provider` 3 364, `agent-mcp` 2 556, le reste 3 313.

### Constat 1 — Le signal de vérification est faux

`cargo test --workspace` échoue localement :

```
tests_integration::call_content_appears_only_at_the_highest_verbosity FAILED
crates/agent-tools/src/tests_integration.rs:2300
attendu au niveau trace: [...] DEBUG pyxis::tools: tool permission resolved [...]
```

Le même test passe seul (`cargo test -p agent-tools --lib <nom>`) et le crate entier passe seul,
deux fois de suite. Il n'échoue que sous la charge du workspace complet. La CI lance exactement
`cargo test --workspace --no-fail-fast` (`.github/workflows/ci.yml:71`) et est verte sur les cinq
derniers runs : **elle est verte par chance, pas par preuve.**

Le mécanisme probable est que `traced_dispatch` installe un collecteur par
`tracing::subscriber::set_default`, qui est *thread-local*, alors que le dispatch d'outils est
concurrent (`buffer_unordered`). Sous forte parallélisation de tests, l'émission part sur un thread
qui ne voit pas le collecteur du test.

C'est le défaut le plus grave trouvé dans cet audit, pas parce qu'il casse une fonctionnalité, mais
parce qu'il casse le seul instrument qui dit si les autres fonctionnent. Rien d'autre ne devrait
passer avant.

### Constat 2 — Le dogfood n'a jamais eu lieu hors du dépôt

`.pyxis/sessions/` contient 20 fichiers, dont 7 substantiels (120 à 337 Ko de JSONL), datés des 24,
25 et 26 juillet. Une recherche de `.pyxis/` sur tout `$HOME` ne trouve que deux emplacements :
`~/.pyxis` (configuration et logs) et `/home/arthur/dev/pyxis/.pyxis`.

**Pyxis n'a jamais été lancé sur un autre projet que lui-même.** Trois jours d'auto-hébergement sur
son propre code, zéro session ailleurs.

La cause est mécanique et le README la nomme : « No packaged releases yet, you build from source ».
Utiliser Pyxis sur un autre dépôt demande aujourd'hui de connaître le chemin absolu vers
`target/release/pyxis`. Tant que ce frottement existe, aucune quantité de parité fonctionnelle ne
produira de maturité, parce que la maturité est un sous-produit de l'usage.

C'est ici que ton intuition est juste et que la solution proposée se trompe de levier : le problème
n'est pas la lignée du code, c'est qu'il n'y a pas de boucle d'usage.

### Constat 3 — L'effort est allé à 40 % dans la partie la plus remplaçable

`agent-tui` fait 20 626 lignes, soit deux fifths du dépôt, pour la couche qui a le moins de
propriétés vérifiables et le plus de concurrence. À titre de comparaison, `codex-rs/tui` fait
232 538 lignes avec 613 snapshots. Sur cet axe précis, la parité est hors de portée et n'est pas
l'endroit où Pyxis peut gagner.

### Constat 4 — Les actifs réels sont dans le cœur, et ils sont bons

Ce sont les seules choses que Pyxis a et que Codex n'a pas, et elles tiennent en peu de lignes :

- **Trait `Provider` réellement générique** : 7 méthodes, 5 avec implémentation par défaut
  (`crates/agent-core/src/provider.rs:408`). Un adaptateur Anthropic s'y branche sans toucher au
  cœur.
- **Taint untrusted au sens OWASP LLM01** (`crates/agent-tools/src/taint.rs`), forçant une
  approbation sur toute action destructrice ou réseau qui suit une lecture non fiable, y compris
  pour les résultats MCP.
- **Hooks fail-closed** : un hook ne peut que resserrer, `allow` se lit « pas d'objection », tout
  échec refuse. Codex laisse un hook `allow` court-circuiter une confirmation.
- **Machine à transitions typée** validée par un `Accumulator` qui fait échouer tout provider hors
  contrat (`crates/agent-core/src/transition.rs`).
- **Cœur headless strict** : aucune bibliothèque n'écrit sur une sortie de processus.

Ces cinq propriétés représentent peut-être 3 000 lignes. Elles sont la seule raison défendable pour
laquelle Pyxis existe plutôt que d'être un alias de Codex.

### Constat 5 — Deux dettes mineures mais visibles

- Le compteur de tokens par défaut reste l'heuristique `len / 4`
  (`crates/agent-tokenizer/src/lib.rs`), alors que les compteurs réels du backend arrivent
  désormais à chaque tour. Le compteur exact existe derrière la feature `tiktoken`, désactivée.

---

## Alternatives écartées

**Partir de la base Codex CLI, par fork.** C'est l'option principale, et c'est celle que cette
section démonte : cinq obstacles, du plus décisif au moins. Elle est écartée.

**Réécrire sans rien prendre de Codex.** Écartée aussi, et par le même audit : trois crates Codex
sont autonomes, mûrs et adoptables à la granularité du crate, ce que la section suivante détaille.
Refuser le fork n'oblige pas à refuser le composant.

**Continuer à piloter par le décompte d'écarts de parité.** Écartée : sur 82 écarts, environ 40
sont hors d'un périmètre défendable, et aucun des cinq points qui décident si l'outil se garde
après une semaine n'y figure. La source de priorité devient le journal d'usage réel.

### 2.1 La vélocité amont rend le fork ingérable en solo

Mesuré sur le clone local (`git log`) :

| Mois | Commits Codex |
|---|---|
| 2025-12 | 420 |
| 2026-01 | 654 |
| 2026-02 | 876 |
| 2026-03 | 850 |
| 2026-04 | 1 086 |
| 2026-05 | 925 |
| 2026-06 | 929 |
| 2026-07 (partiel) | 691 |

**707 commits sur les 30 derniers jours, 176 contributeurs distincts sur 90 jours.** Codex produit
chaque mois environ six fois l'historique total de Pyxis.

Un fork solo qui modifie le cœur (ce qu'il faudrait faire, voir 2.3) se retrouve à rebaser contre ce
flux. Deux issues : soit tu rebases, et tu passes ton temps à résoudre des conflits dans du code que
tu n'as pas écrit ; soit tu ne rebases pas, et tu maintiens seul 370 000 lignes qui ne bénéficient
plus des correctifs amont. Les deux sont pires que la situation actuelle.

### 2.2 Le volume dépasse ce qu'une personne peut tenir

Hors fichiers `*_tests.rs` et répertoires `tests/` :

| Crate Codex | Lignes hors tests |
|---|---|
| `tui` | 194 354 |
| `core` | 109 725 |
| `cli` | 21 008 |
| `protocol` | 21 278 |
| `config` | 17 299 |
| `exec` | 3 901 |

Soit ~370 000 lignes pour six crates seulement. Et le graphe n'est pas réductible : **environ 90 des
107 crates sont atteints depuis le binaire `codex`**. Le binaire dépend directement d'app-server,
app-server-daemon, cloud-tasks, chatgpt, exec, exec-server, mcp-server, responses-api-proxy, tui et
core (`codex-rs/cli/Cargo.toml:24-62`), et `core` tire à lui seul ~60 crates internes
(`codex-rs/core/Cargo.toml:26-82`).

Le point qui interdit tout découpage propre : **la TUI n'appelle plus le cœur en direct, c'est un
client JSON-RPC d'app-server** (`codex-rs/tui/src/app_server_session.rs:17-28`,
`codex_app_server_client::AppServerClient`). Prendre « la TUI et la boucle » signifie donc porter
aussi app-server, son protocole, son client et son daemon. Il n'existe pas de tranche « agent
interactif » extractible.

Le crate `config` illustre le problème de calibrage : il gère des couches de configuration
d'entreprise, du MDM, et des bundles cloud (`codex-rs/config/src/config_layer_source.rs`,
`cloud_config_bundle.rs`, `config_requirements.rs`). C'est du code correct pour OpenAI et du poids
mort pour un projet personnel.

### 2.3 Le cœur Codex est couplé à l'API Responses, ce qui tue le différenciateur

Codex a bien un trait `ModelProvider` (`codex-rs/model-provider/src/provider.rs:101-216`), mais il
abstrait l'authentification, les capabilities et le catalogue de modèles, **pas le wire**. La preuve
tient en une ligne : `enum WireApi` n'a **qu'un seul variant, `Responses`**
(`codex-rs/model-provider-info/src/lib.rs:57-61`), et la chaîne `chat/completions` a **zéro
occurrence dans tout `codex-rs`**.

Le cœur suit : `ModelClient` importe `ResponsesClient`, `ResponsesApiRequest`,
`ResponsesWebsocketClient`, `CompactClient` et `MemoriesClient` depuis `codex-api`
(`codex-rs/core/src/client.rs:33-63`) et assume `previous_response_id`, un état de tour collant et
un préchauffage WebSocket (`client.rs:11-24`). La compaction et les mémoires sont des *endpoints
backend OpenAI*, pas des algorithmes locaux.

Le plafond est démontré par le seul provider non-OpenAI existant : `AmazonBedrockModelProvider`
(`provider.rs:20,236-241`) sert des modèles `openai.gpt-5.6-*` à travers le même wire Responses.
Autrement dit, **Codex n'a aucun exemple de fournisseur qui ne parle pas Responses**. Brancher
Anthropic (API Messages) demanderait de réécrire `codex-api` en entier, transport et types SSE/WS
compris, plus la moitié de `core/src/client.rs`. Et il n'y a pas de registry de providers pour
amortir ça : `create_model_provider` est un `match` codé en dur à deux branches
(`provider.rs:232-241`).

Or ADR-4 pose le multi-provider comme différenciateur central de Pyxis, et le trait `Provider`
actuel le tient. **Partir de la base Codex, c'est échanger l'unique thèse du projet contre une
avance de fonctionnalités.** Si la thèse ne vaut plus, la bonne décision n'est pas de forker Codex :
c'est d'utiliser Codex et d'arrêter Pyxis. Cette option mérite d'être posée honnêtement, mais elle
n'est pas ce que tu demandes.

### 2.4 La licence autorise, mais à sens unique et avec des obligations

Codex est sous Apache-2.0 pour tout le dépôt, `codex-rs` compris, sans sous-répertoire sous licence
distincte (`/home/arthur/dev/codex/LICENSE` ; https://github.com/openai/codex). Le `NOTICE` racine
porte « OpenAI Codex, Copyright 2025 OpenAI » et l'attribution Ratatui (MIT).

La compatibilité est établie et unidirectionnelle : la FSF
(https://www.gnu.org/licenses/license-list.html#apache2) et l'ASF
(https://www.apache.org/licenses/GPL-compatibility.html) confirment qu'Apache-2.0 entre dans un
projet GPLv3 (pas GPLv2 seule ; le « or-later » de Pyxis couvre v3). Le combiné se distribue sous
GPLv3, **jamais l'inverse**.

Obligations, du texte Apache-2.0 : conserver LICENSE, copyrights et NOTICE, et **signaler les
fichiers modifiés** (§4). La marque « Codex » n'est pas concédée (§6) : un fork public devrait être
renommé. `NOTICE-CODEX.md` décrit déjà cette procédure correctement.

Côté posture, OpenAI est ouvert aux forks : un mainteneur écrit explicitement « you're welcome to
fork the repo » (https://github.com/openai/codex/discussions/8338), et aucune action juridique
contre un fork n'est connue. La licence n'est donc pas l'obstacle. L'obstacle est que tu ne pourras
jamais remonter un correctif upstream, donc chaque divergence est une dette définitive. Cela
renforce 2.1.

### 2.5 Le fork hérite d'une maintenance qui n'est pas la tienne

Deux charges non évidentes, mesurées sur le dépôt.

**La télémétrie n'est derrière aucun feature flag cargo.** Tout est dépendance inconditionnelle, le
gating est uniquement au runtime. Trois endpoints codés en dur dans le graphe minimal :

- OTLP Statsig vers `https://ab.chatgpt.com/otlp/v1/metrics`, clé d'API en dur, **actif par défaut
  en build release** (résolu à `None` seulement sous `debug_assertions`,
  `codex-rs/otel/src/config.rs:9-32`).
- Analytics vers `{chatgpt_base_url}/codex/analytics-events/events`
  (`codex-rs/analytics/src/client.rs:116`), dépendance directe de `core`.
- DSN Sentry en dur `o33249.ingest.us.sentry.io` (`codex-rs/feedback/src/lib.rs:41-42`), dépendance
  de `core` et de `tui`.

S'y ajoutent `backend-client`, `cloud-config`, `connectors` et `ext/git-attribution`. Il faudrait
neutraliser 4 à 6 points, par suppression de clés en dur et non par features, **et refaire ce
travail à chaque rebase**. Sur un projet qui met la confidentialité et le fail-closed en avant, ce
n'est pas un détail cosmétique.

**Codex maintient ses propres forks de dépendances.** `[patch.crates-io]` pointe vers quatre forks
git : `crossterm`, `ratatui`, `tokio-tungstenite`, `tungstenite`
(`codex-rs/Cargo.toml:562-568`), plus `nucleo` en git (l. 354). Forker Codex, c'est hériter de la
maintenance de forks de `ratatui` et `crossterm`.

Bonne nouvelle en revanche sur le build : **cargo suffit**. Bazel coexiste mais n'est requis que
pour des macrobenchs. Les `build.rs` sont triviaux, il n'y a ni répertoire `vendor` ni `patches/`
appliqués, et la toolchain est épinglée à 1.95.0 (`rust-toolchain.toml:2`), la même que Pyxis. Le
`package.json` ne sert qu'au packaging npm.

---

## 3. Ce qu'il faut réellement prendre de Codex

L'inverse du fork : de l'adoption ciblée, à la granularité du crate, là où le composant est autonome
et mûr. Vérifié par comptage des dépendances internes `codex-*` dans chaque `Cargo.toml`.

**Correction du 2026-07-27, après lecture des `Cargo.toml`.** Le décompte des dépendances *internes*
`codex-*` est trompeur : ce sont les dépendances **externes** qui décident du coût réel. Deux
composants que j'avais recommandés au vendoring ne doivent pas être pris tels quels.

| Composant Codex | Lignes | Deps internes | Dépendance externe décisive | Verdict |
|---|---|---|---|---|
| `codex-execpolicy` | 1 916 | 1 | **`starlark`** : un interpréteur de langage complet, pour classer des commandes shell | **Ne pas vendorer.** Prendre le modèle de décision (`Decision { Allow, Prompt, Forbidden }`, `decision.rs:9-16`) et exprimer les règles en TOML, format déjà dans le graphe de Pyxis |
| `codex-file-search` | 1 312 | 0 | `nucleo` en **dépendance git épinglée** (`codex-rs/Cargo.toml:354`), pas la version crates.io | **Prendre, avec `nucleo` depuis crates.io.** Ne pas hériter du rev épinglé de Codex |
| `codex-apply-patch` | 4 553 | 4 (3 utilitaires) | `codex-exec-server` | Format d'édition natif des modèles `*-codex`, grammaire `apply_patch.lark` incluse. À conditionner au journal d'usage |
| Concept `ConfigLayerSource` | ~60 utiles | n/a | Précédence de configuration explicite et nommée. **Prendre l'idée, pas le crate** : le crate `config` complet pèse 17 299 lignes d'outillage entreprise | `S` |
| Concept `ExtensionRegistryBuilder` | ~40 utiles | n/a | Le meilleur design de Codex, et il est neutre vis-à-vis d'OpenAI. Voir ci-dessous | `M` |

Les trois premiers s'intègrent par vendoring sous `NOTICE-CODEX.md`, avec inscription dans
`docs/codex-port-inventory.md`. Le workspace Codex est en `version = "0.0.0"` et non publié : la
dépendance par `path` ou `git` est la seule voie, ce qui plaide pour un vendoring figé plutôt qu'un
suivi de branche.

**Sur la registry d'extensions.** C'est la trouvaille la plus réutilisable de cet audit.
`codex_extension_api::ExtensionRegistryBuilder` (utilisée `codex-rs/cli/src/main.rs:2023`,
réexportée `core-api/src/lib.rs:67`) expose des traits contributeurs dans
`ext/extension-api/src/contributors.rs` : `ToolContributor` (:273), `McpServerContributor` (:65),
`ContextContributor` (:80), `ThreadLifecycleContributor` (:123), `TurnLifecycleContributor` (:166),
`ConfigContributor` (:222), `ApprovalReviewContributor` (:310). Les extensions first-party de Codex
(web-search, skills, memories, mcp) s'installent toutes par cette voie. Le mécanisme n'a **aucun
couplage OpenAI** et il répond exactement à ce que Pyxis assemble aujourd'hui à la main dans
`crates/agent-cli/src/main.rs` (outils, MCP, skills, contexte, hooks branchés un par un). À prendre
comme forme, pas comme code.

Ironie utile : cette registry existe pour les **outils**, jamais pour les **providers**. Sur l'axe
que Pyxis a choisi, Codex n'a rien à offrir.

Ne pas prendre : `tui` (194 k lignes pour une esthétique que tu ne veux pas), `core` (couplé
Responses), `config` (calibré entreprise), `app-server`, `cloud-tasks`, `analytics`, `otel`,
`feedback`.

---

## 4. Ce que « vraiment bonne CLI » veut dire, et qui n'est pas de la parité

L'audit de parité compte 82 écarts. Aucun des cinq points ci-dessous n'y figure, et ce sont eux qui
décident si un outil se garde après une semaine.

1. **Être installable.** `cargo install --path`, un binaire nommé sur le `PATH`, et à terme une
   release taguée. Sans cela, il n'y a pas d'usage, donc pas de maturité.
2. **Démarrer sans friction dans un dépôt inconnu.** Un `/init` qui écrit un `AGENTS.md`, et un
   premier lancement qui ne demande pas de configurer trois choses.
3. **Ne pas mentir sur ce qu'il fait.** Cet axe est déjà bon (diff de tour, jauge de contexte
   alimentée par le backend, quotas réels) et c'est un avantage réel sur beaucoup d'agents.
4. **Ne pas interrompre pour rien.** Fermé par la classification de commandes et la mémorisation
   d'approbation. À vérifier en usage réel, pas en test.
5. **Ne pas perdre le travail.** Sessions JSONL, resume, réconciliation à l'annulation : déjà en
   place, jamais éprouvé sur une session longue dans un dépôt tiers.

---

## 5. Plan séquencé

### Palier 0 — Réparer l'instrument (avant tout le reste)

Rendre `call_content_appears_only_at_the_highest_verbosity` déterministe. La correction de fond est
que le dispatch concurrent ne peut pas être observé par un collecteur thread-local : soit le test
sérialise le dispatch, soit la collecte passe par un collecteur partagé entre threads. Ajouter
ensuite une exécution de la suite sous `--test-threads` élevé dans la CI pour que la classe de
défaut ne repasse pas.

**Critère de sortie** : cinq exécutions consécutives de `cargo test --workspace` vertes.

### Palier 1 — Créer la boucle d'usage (le vrai déblocage)

- `cargo install --path crates/agent-cli` documenté et vérifié, binaire `pyxis` sur le `PATH`.
- `--sandbox-mode` (`read-only` / `workspace-write` / `danger-full-access`), `--permission-mode`,
  et `-c cle=valeur` pour surcharger une clé sans éditer un fichier.
- Allow-list réseau par suffixe de domaine (`crates/agent-sandbox/src/proxy.rs:30` compare
  aujourd'hui en égalité stricte, donc `--allow github.com` ne couvre pas `api.github.com`, ce qui
  pousse l'utilisateur vers `--no-sandbox`).
- `/init` qui génère un `AGENTS.md`.
- Une page de référence de la configuration : les 10 clés ne sont décrites qu'en prose dans
  `docs/CURRENT_STATUS.md`.

**Critère de sortie** : deux semaines d'usage exclusif de Pyxis sur un projet **autre** que Pyxis,
avec un journal des frottements. Ce journal remplace toute liste de parité comme source de
priorités pour la suite.

### Palier 2 — Absorber les composants Codex mûrs

Dans l'ordre de rapport valeur/effort : `codex-file-search`, puis `codex-execpolicy`, puis le
concept de couches de configuration. Chacun sous le protocole de `NOTICE-CODEX.md` avec inscription
dans `docs/codex-port-inventory.md`.

### Palier 3 — Parité de comportement modèle

`apply_patch` (via `codex-apply-patch`) et `update_plan`. Ce sont les deux seuls outils manquants
dont l'absence change ce que le modèle *fait* plutôt que ce qu'il *peut faire*, parce que les
fine-tunes `*-codex` sont entraînés dessus. À décider explicitement au vu du journal du palier 1 :
si le journal ne les fait pas remonter, ne pas les faire.

### Ce qu'il faut arrêter de suivre

Le décompte d'écarts de parité comme métrique de pilotage. Il produit 82 items dont ~40 hors
périmètre défendable, et il n'a fait remonter aucun des cinq points de la section 4. À partir du
palier 1, la source de priorité est le journal d'usage.

---

## Conséquences

Le trait `Provider` générique de Pyxis est conservé plutôt qu'échangé contre une base mono-wire :
c'est ce qui rend le dépôt survivable si le canal d'abonnement OpenAI se ferme, et cet argument est
indépendant du chemin choisi. Ce que Pyxis prend de Codex entre crate par crate, sous
[`NOTICE-CODEX.md`](../../../../NOTICE-CODEX.md) et avec inscription dans
[`docs/codex-port-inventory.md`](../../../codex-port-inventory.md), jamais par suivi de branche
amont : le workspace Codex n'est pas publié, donc un vendoring figé est la seule voie honnête.

Le coût assumé est de ne pas hériter du dogfood de Codex. Il se paie en construisant la boucle
d'usage soi-même, et c'est pour cela que l'installabilité passe avant la parité de comportement.
La parité, elle, cesse d'être pilotée par ce document : elle l'est par
[`docs/parity/codex-baseline-matrix.json`](../../../parity/codex-baseline-matrix.json), générée et
empreintée, dont la commande de vérification est décrite dans
[`docs/parity/README.md`](../../../parity/README.md).

## Confiance et limites

**Élevée** sur tout ce qui est mesuré localement : volumes, vélocité amont, dépendances internes des
crates Codex, échec de test reproduit, absence de sessions hors dépôt, couplage Responses dans le
cœur Codex. La vélocité amont est mesurée sur le clone local, donc sur l'état de sa branche par
défaut au moment du clone.

**Moyenne** sur la cause exacte du test non déterministe : le mécanisme thread-local est l'hypothèse
la plus probable au vu du symptôme et du dispatch concurrent, mais elle n'a pas été prouvée par
instrumentation.

**Le risque d'abonnement est confirmé et daté.** OpenAI n'a jamais tranché publiquement si un client
tiers utilisant « Sign in with ChatGPT » est conforme : la question est posée explicitement dans
https://github.com/openai/codex/discussions/8338 et le mainteneur renvoie aux Terms of Use sans
répondre (« get advice from a legal expert »). Des bannissements de comptes Codex sans explication
sont rapportés par des utilisateurs
(https://community.openai.com/t/codex-chatgpt-pro-account-banned-with-no-warning-no-explanation-18-month-subscriber/1381906).

Le précédent Anthropic donne la trajectoire : blocage technique silencieux, puis interdiction
officielle des tokens OAuth Pro/Max dans les outils tiers, enforcement en avril 2026, OAuth réservé
à Claude.ai et Claude Code
(https://alternativeto.net/news/2026/2/anthropic-officially-bans-using-subscription-authentication-for-third-party-claude-use).
Ces dates viennent de sources secondaires, pas du texte d'Anthropic : à revérifier avant tout usage
contractuel.

Conclusion sur ce point : **la tolérance actuelle d'OpenAI n'est pas un droit acquis, et le canal
peut être fermé sans préavis.** C'est le seul facteur qui peut invalider tout le plan, il est
indépendant du chemin choisi, et il constitue le meilleur argument *pour* conserver le trait
`Provider` générique de Pyxis plutôt que d'adopter une base mono-wire.
