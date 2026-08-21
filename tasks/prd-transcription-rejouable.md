[PRD]
# PRD: Transcription rejouable

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-21 | Arthur Jean | Rédaction initiale, lot #8 du plan de portage DeepSeek Harness |
| 1.1 | 2026-08-21 | Arthur Jean | Ajout de la racine du dépôt source, de la table de correspondance dsh vers Pyxis et des numéros de ligne dans les ancres `Source dsh`, pour que la reprise des décisions soit vérifiable ligne à ligne. Correction des chemins de scénario : ils vivent sous `examples/headless-agent/tests/snapshots/`, pas à la racine de l'exemple |

## Problem Statement

`pyxis -p --output-format json` écrit sur stdout le seul contrat que consomment les intégrations tierces, et aucun test du dépôt ne lit un octet de ce que cette sortie produit. Six défaillances mesurées sur l'état du dépôt au 2026-08-21 :

1. **La fonction qui produit la transcription n'est atteinte par aucun test.** `crates/agent-cli/src/headless.rs:44` porte `pub async fn run(run: HeadlessRun<'_>)`, la seule fonction qui compose un tour headless complet : garde de hook `UserPromptSubmit`, injection de skills, ouverture de session, boucle `tokio::select!` sur les événements, `turn_diff`, `run_summary`, puis `Hook(Stop)`. Les huit tests de `crates/agent-cli/tests/e2e_headless.rs` ne l'appellent jamais : ils importent `agent_core::run_headless` (`e2e_headless.rs:34`) et le pilotent dix fois. Ils prouvent le moteur, pas l'assemblage. Tout ce que `headless.rs` ajoute au moteur, c'est-à-dire l'ordre des événements terminaux, la présence conditionnelle du `turn_diff` et le `Hook(Stop)` émis à chaque exécution réussie, n'a aucun témoin.

2. **Le rédacteur JSONL a quatorze tests et aucun ne passe par son écriture.** `crates/agent-cli/src/jsonl.rs:318` ouvre un `#[cfg(test)] mod tests` de quatorze tests dont l'assistant unique construit un `EventLine` et appelle `serde_json::to_value`, ce qui rend un `Value`, jamais des octets. La fonction qui écrit réellement est `write_line` (`jsonl.rs:310`), et elle verrouille `std::io::stdout()` en dur : aucun test ne peut l'appeler sans polluer la sortie du harnais. Le sérialiseur est donc prouvé, la ligne ne l'est pas, et c'est la ligne qui est le contrat : `#[serde(flatten)]` de `EventLine`, ordre des clés, séparateur, `\n` final.

3. **Deux sources de non-déterminisme sont câblées en dur sur le chemin qui compte.** `crates/agent-cli/src/runtime.rs:655-656` construit `Arc::new(RandomIds)` et `Arc::new(SystemClock)` sans qu'aucun paramètre ne puisse les remplacer, alors que `SessionRuntime::open` reçoit déjà un `EngineDeps` qui porte, lui, une horloge injectable (`runtime.rs:546-552`). Deux horloges coexistent donc dans la même fonction, l'une donnée et l'autre décidée sur place. Le dépôt possède pourtant les deux remplaçants : `SequentialIds` (`crates/agent-runtime/src/id.rs`, dont le doc-comment énonce « Two runs seeded the same way produce byte-identical identifiers ») est déjà utilisé dans le crate binaire lui-même, à `crates/agent-cli/src/failure_line.rs:34-38`.

4. **Le rejeu de transport existe mais ne vérifie jamais que le script a été consommé.** `crates/agent-cli/tests/fixtures/` porte cinq fichiers `.sse` rejoués à travers le vrai `CodexEventMapper`, ce qui est la bonne décision de conception et le précédent à étendre. Aucun de ces tests n'assure que le scénario a été joué en entier : un tour qui s'arrête à la moitié du script passe au vert. C'est précisément le mode d'échec que `assertConsumed()` de dsh ferme, et que `nock.isDone()`, `pendingMocks()` et `all_played` ferment dans les trois écosystèmes VCR de référence.

5. **Le contrat écrit décrit une sortie qui n'existe pas.** `docs/EVENT_SCHEMA.md` documente dix-huit types d'événements ; `crates/agent-core/src/event.rs` en porte vingt-quatre. Six sont muets, dont `hook`, que `headless.rs` émet à la fin de chaque exécution réussie. Les exemples du document utilisent les préfixes `th_`, `tu_` et `ev_` ; le code rend `thr_`, `trn_` et `evt_` (`crates/agent-runtime/src/id.rs:191,196,206`). Un intégrateur qui écrit son parseur d'après le document filtre sur un préfixe que le binaire n'émet pas. Le document énonce enfin un « ordre garanti » (événements, puis `turn_diff`, puis `run_summary` toujours en dernier) que rien ne vérifie, puisque la fonction qui le tient n'est pas testée (défaillance 1).

6. **Le dépôt tient quatre portes octet à octet et aucune ne regarde stdout.** Les schémas d'app-server, `docs/crate-graph.md`, `docs/tool-catalog.md` et `docs/config-catalog.md` sont comparés octet à octet dans `just test`. Le patron est mûr, testé, et son message d'échec nomme sa commande de régénération. La surface la plus exposée du binaire, celle qu'un script tiers parse, est la seule à en être privée.

**Why now:** le lot #7 vient de livrer le contrat d'expérience du modèle, qui décrit ce que Pyxis envoie ; il ne reste plus rien d'écrit sur ce que Pyxis rend. Les deux briques dont ce lot dépend sont déjà dans l'arbre et sous-employées : `SequentialIds` n'a qu'un seul appelant et le patron `PYXIS_UPDATE_*` en a quatre, tous documentaires. `crates/agent-doc-gates/src/gates.rs:266` va jusqu'à porter le commentaire « A third switch joins this list in the change that introduces it » : la place est réservée depuis le lot des catalogues et personne ne l'a prise. Enfin la dette de la défaillance 5 grossit à chaque variante ajoutée à `AgentEvent` : six types muets aujourd'hui, et rien de mécanique n'empêche le septième.

## Overview

Le lot fait entrer dans le dépôt un harnais de rejeu déterministe et la porte qui compare son résultat octet à octet. Le harnais est un `#[cfg(test)] mod` du crate binaire qui appelle `crates/agent-cli/src/headless.rs:44` (la vraie fonction, pas une reconstitution) avec un fournisseur scripté, un générateur d'identifiants séquentiel et une horloge gelée, et capture la transcription dans un tampon au lieu de stdout. La porte est un fichier `.jsonl` gelé par scénario, comparé octet à octet, régénéré par une bascule d'écriture que seule `just regen` peut allumer.

De DeepSeek Harness, quatre décisions structurantes se reprennent et aucune ligne ne se copie, la source étant du TypeScript sur Cordis ; la table « Correspondance dsh vers Pyxis » de la section Research Findings en détaille dix, chacune ancrée sur un fichier et une ligne du dépôt source. La première est le choix du point d'entrée : `examples/headless-agent/tests/headless.snapshot.ts` ne teste pas une fonction interne, il pilote l'exemple publié à travers son driver, parce que `docs/testing.md:31-35` pose comme règle de tester « the published artifact ». La deuxième est la forme du scénario : un répertoire par cas, portant côte à côte son entrée, son script de rejeu et sa sortie attendue, ce qui rend un cas lisible d'un seul `ls` et un ajout de cas mécanique. La troisième est le triptyque de modes replay, record, refresh, réduit ici à deux, comparer ou réécrire sous bascule, parce que le troisième mode de dsh existe pour rafraîchir contre un vrai fournisseur, ce que `AGENTS.md` interdit à toute recette. La quatrième, et c'est la plus importante, est l'assertion de consommation : le démontage échoue si le script n'a pas été joué en entier, ce qui distingue un scénario qui passe d'un scénario qui s'est arrêté tôt.

Trois divergences volontaires viennent des invariants de Pyxis. Le gel n'utilise pas insta, alors que le dépôt en dépend déjà pour le TUI : `insta-1.48.0/src/snapshot.rs:753-765` applique `.trim_end()` au contenu des deux genres de snapshot avant comparaison, donc efface le `\n` final. Ce `\n` est le contrat JSONL, une ligne par événement ; un outil qui le normalise ne peut pas prouver « identique octet à octet ». Le patron retenu est celui de `crates/agent-app-server/tests/schemas.rs`, qui coûte zéro dépendance et dont le dépôt tient déjà quatre instances. Ensuite le déterminisme s'obtient par injection et jamais par normalisation après coup : dsh remplace `sessionId` par `{{sessionId}}` et met `time` à zéro dans ses fixtures, une réécriture qui masque aussi les régressions qu'elle traverse ; ici tous les champs `*_ms` d'`AgentEvent` viennent d'une seule horloge (`crates/agent-core/src/agent.rs:1724-1729`, et `crates/agent-core/src/clock.rs:23` est le seul appel à l'horloge système du crate), donc une horloge gelée suffit à les figer sans les cacher. Enfin le fournisseur de rejeu vit en processus derrière le trait `Provider` plutôt que derrière un endpoint loopback, parce que `#[tokio::test(start_paused = true)]` et de l'E/S réseau réelle se contredisent : l'horloge virtuelle avance dès que l'exécuteur est oisif, et un socket local rend l'oisiveté imprévisible.

Le périmètre de production est étroit et il est nommé : deux coutures, et rien d'autre. `EventWriter` reçoit un puits au lieu de verrouiller `stdout()`, et `SessionRuntime::open` accepte l'horloge et le générateur d'identifiants qu'on lui passe au lieu de les décider. Le reste du lot est du test, du document et une note. La question du binaire publié, seule vraie divergence avec la règle 3 de `docs/testing.md`, n'est pas tranchée par omission : une story la mesure et l'écrit, sans construire la couture de credential qu'elle exigerait.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Scénarios rejoués produisant une transcription identique octet à octet | 4/4 | tous les scénarios ajoutés |
| Tests atteignant `crates/agent-cli/src/headless.rs:44` | ≥ 4 | ≥ 4 |
| Sources de non-déterminisme câblées en dur sur le chemin headless | 0/2 | 0 |
| Types d'`AgentEvent` non documentés dans `docs/EVENT_SCHEMA.md` | 0/6 | 0, quel que soit le nombre de variantes |
| Préfixes d'identifiants faux dans `docs/EVENT_SCHEMA.md` | 0/3 | 0 |
| Fixtures de rejeu sans assertion de consommation | 0/5 | 0 |
| Dépendances ajoutées au workspace | 0 | 0 |
| Clés de configuration utilisateur ajoutées | 0 | 0 |
| Temps mur ajouté à `just test` sur cache chaud | ≤ 3 s | ≤ 3 s |
| Recettes ajoutées au `justfile` | 0 | 0 |

## Target Users

### Intégrateur consommant la sortie `--output-format json`
- **Role:** développeur branchant un script, un orchestrateur ou une interface sur `pyxis -p`, qui parse le JSONL ligne à ligne.
- **Behaviors:** lit `docs/EVENT_SCHEMA.md`, écrit un parseur, filtre sur `type`, corréle sur `thread_id` et `turn_id`, s'arrête sur `run_summary`.
- **Pain points:** le document décrit dix-huit types sur vingt-quatre et donne trois préfixes d'identifiants que le binaire n'émet pas. Un filtre écrit d'après le document ne reconnaît pas `thr_`, et une ligne `hook` non documentée arrive à la fin de chaque exécution réussie. L'« ordre garanti » sur lequel il fonde son état de fin n'est vérifié par rien.
- **Current workaround:** lancer le binaire, capturer la sortie à la main et écrire le parseur d'après ce qu'il observe, ce qui fige dans son code des détails que rien ne promet.
- **Success looks like:** un fichier `.jsonl` gelé dans le dépôt qu'il peut lire comme exemple exécutable, et un document dont chaque affirmation est tenue par ce fichier.

### Agent de codage modifiant un événement ou le chemin headless
- **Role:** Claude Code ou Codex recevant une tâche qui ajoute une variante d'`AgentEvent`, change un champ, ou touche l'ordre des terminaux dans `headless.rs`.
- **Behaviors:** lit `AGENTS.md`, suit la table « Where new behavior goes », écrit la variante, lance `just check`.
- **Pain points:** rien ne l'informe qu'il vient de changer une sortie que des scripts tiers parsent. `cargo test --workspace` reste vert parce qu'aucun test ne lit les octets de stdout, et les quatorze tests de `jsonl.rs` passent tous par `serde_json::to_value`, donc ne voient ni l'ordre des clés rendu ni le `\n`.
- **Current workaround:** aucun. La régression sort dans la release.
- **Success looks like:** `just check` devient rouge sur un diff de transcription, le message nomme la commande de régénération, et le `git diff` du fichier gelé montre exactement la ligne qui a bougé.

### Mainteneur relisant une pull request qui touche la sortie
- **Role:** Arthur Jean relisant un changement dans `agent-core`, `agent-runtime` ou `agent-cli`.
- **Behaviors:** lit le diff, cherche l'effet observable, décide si le changement est un correctif ou une rupture de contrat.
- **Pain points:** l'effet sur la sortie n'est pas dans le diff. Un renommage de champ, un `skip_serializing_if` ajouté ou un ordre de terminaux inversé se lisent comme du nettoyage.
- **Current workaround:** reconstituer mentalement la sérialisation à partir des `derive`, ce qui échoue précisément sur les `#[serde(flatten)]` d'`EventLine`.
- **Success looks like:** le diff de la pull request contient le diff du fichier gelé, donc l'effet est visible avant l'exécution, comme il l'est déjà pour les schémas d'app-server et les trois catalogues.

## Research Findings

Constats qui ont façonné ce PRD.

### Contexte concurrentiel
- **insta** (Rust, dominant) : ergonomie de revue supérieure avec `cargo insta review`, déjà dans le workspace pour `agent-tui`. Écarté ici : `~/.cargo/registry/src/index.crates.io-*/insta-1.48.0/src/snapshot.rs:753-765` applique `.trim_end()` au contenu des deux genres de snapshot puis remplace `\r\n` par `\n`, ce qui rend l'égalité octet à octet inatteignable sur un format dont le `\n` terminal est le séparateur d'enregistrement.
- **expect-test, goldenfile, snapbox** : les trois rendent le même service que quarante lignes de code local et ajoutent chacun une dépendance de développement au workspace, contre un dépôt qui argumente chaque entrée de son `Cargo.toml`.
- **vcrpy, nock, rvcr** : le modèle de cassette à trois modes est un standard de fait. Les trois portent une assertion de consommation (`all_played`, `nock.isDone()`, `pendingMocks()`), ce qui confirme que `assertConsumed()` de dsh n'est pas une idiosyncrasie mais la partie non négociable du modèle.
- **Écart de marché :** aucun de ces outils ne traite le déterminisme de l'horloge et des identifiants, qui reste à la charge de l'application. C'est là que Pyxis a une avance non exploitée : le trait `Clock` et le trait `IdGenerator` existent déjà.

### Bonnes pratiques reprises
- Tester l'artefact publié plutôt qu'une reconstitution (`docs/testing.md:31-35`). Appliqué au plus près : la vraie fonction `headless::run`, atteinte par l'idiome que `crates/agent-cli/src/main.rs:31-34` documente déjà pour un crate sans cible `[lib]`.
- Un répertoire par scénario portant entrée, script et sortie attendue côte à côte (`examples/headless-agent/tests/snapshots/`, onze scénarios sur ce modèle).
- Le déterminisme par injection plutôt que par masquage : dsh tokenise `sessionId` et met `time` à zéro dans ses fixtures ; Pyxis peut geler la source, ce qui préserve la valeur comme témoin.
- `#[tokio::test(start_paused = true)]` fige `tokio::time::Instant`, donc la durée d'outil mesurée à `crates/agent-tools/src/registry.rs:446`. La documentation de tokio et deux issues connues (tokio#3108, tokio#7237) déconseillent de combiner l'horloge virtuelle avec de l'E/S réseau réelle, ce qui tranche contre un serveur mock loopback.

### Correspondance dsh vers Pyxis

Racine du dépôt source : `/home/arthur/dev/deepseek-harness`, **lecture seule**, aucune ligne copiée. Tous les chemins de la colonne « Source dsh » et de la ligne `Source dsh` de chaque story sont relatifs à cette racine. Les numéros de ligne sont ceux lus au 2026-08-21 : ils servent la navigation, l'autorité restant le contenu. Le fichier `docs/testing.md:47` est celui qui désigne le propriétaire du domaine : « `examples/headless-agent` owns the internal canonical-event JSONL snapshots and replay fixtures », ce qui corrige la colonne « Source dsh à lire » du plan de portage, laquelle ne nomme que `packages/test-support/`, qui ne porte que la mécanique.

| # | Décision reprise | Source dsh | Ce que Pyxis en fait | Story |
|---|---|---|---|---|
| 1 | Le point d'entrée testé est l'artefact publié, jamais une reconstitution | `docs/testing.md:31-35`, « "Real entry path" means the published artifact » | Appelle `crates/agent-cli/src/headless.rs:44`, la vraie fonction ; mesure et nomme ce qui reste au-dessus au lieu de le supposer couvert | US-123, US-130 |
| 2 | Un répertoire par scénario, entrée, script et attendu côte à côte | `examples/headless-agent/tests/snapshots/`, onze scénarios ; `docs/testing.md:47` | Quatre répertoires de même forme, découverts par balayage, sans ligne de code par scénario | US-126 |
| 3 | La comparaison est une égalité stricte contre le fichier lu, sans matcher flou | `examples/headless-agent/tests/headless.snapshot.ts:669`, `expect(normalized).toBe(await readFile(...))` | `assert_eq!` sur les octets, patron de `crates/agent-app-server/tests/schemas.rs`, sans `trim` ni normalisation de fin de ligne | US-124 |
| 4 | Le script de rejeu doit être prouvé consommé au démontage | `packages/test-support/llm-replay/src/index.ts:138` et `:787`, message `:799` « fixture not fully consumed [...] the scenario drove fewer model calls than recorded » | Assertion de drainage par scénario, appliquée aussi aux cinq fixtures `.sse` existantes | US-122 |
| 5 | Une requête hors script est une erreur nommée, jamais un flux vide | `packages/test-support/llm-replay/src/index.ts:774` « script exhausted [...] requested model call #N », `:768` pour la session non enregistrée | Fournisseur fail-closed nommant le scénario et le rang de la requête | US-122 |
| 6 | Seule la frontière chère ou non déterministe est simulée, tout l'aval reste réel | `docs/testing.md:21-23`, « Mock only the expensive or non-deterministic boundary (LLM adapter, network, clock); keep everything downstream real » | Rejeu derrière le trait `Provider`, avec le vrai `CodexEventMapper`, la vraie session, les vrais outils | US-122, US-123 |
| 7 | Vérifier le monde, pas l'auto-rapport | `docs/testing.md:27-29` | L'« ordre garanti » de `docs/EVENT_SCHEMA.md` devient une assertion sur les fichiers gelés au lieu d'une prose | US-128 |
| 8 | Un mode de réécriture existe, distinct du mode de comparaison | `examples/headless-agent/tests/headless.snapshot.ts:642-644`, `refreshFixtureReplacements` | Bascule `PYXIS_*` confinée à `just regen` ; le troisième mode de dsh, rafraîchir contre un vrai fournisseur, n'est pas repris, `AGENTS.md` l'interdisant à toute recette | US-124, US-125 |
| 9 | Le scénario s'ajoute ou se met à jour dans la même pull request que le changement visible | `docs/testing.md:47` | Le fichier gelé entre dans le diff de la pull request, règle déjà portée par `AGENTS.md` pour les schémas et les trois catalogues | US-124 |
| 10 | **Divergence assumée :** dsh normalise après coup, Pyxis gèle la source | `packages/test-support/acp-snapshot/src/normalize.ts:9` (`{{sessionId}}`), `:23-25` (suppression de `time` et `time0`), `:321` et `:327` (mise à zéro) | Horloge injectée et `SequentialIds` : la valeur reste un témoin au lieu d'être effacée, ce qu'un `time = 0` rend impossible | US-121, US-123 |

*Les affirmations citées ci-dessus ont été vérifiées contre le disque au 2026-08-21 ; les sources complètes sont dans les notes de session.*

## Assumptions & Constraints

### Assumptions (to validate)
- Une horloge gelée, `SequentialIds` et `start_paused` suffisent à rendre la transcription reproductible : tous les champs `*_ms` d'`AgentEvent` (`started_at_ms`, `completed_at_ms`, `duration_ms`, `delay_ms`) viennent de `deps.clock` (`crates/agent-core/src/agent.rs:1724-1729`) et `agent-core` ne contient aucun autre appel à l'horloge système que `crates/agent-core/src/clock.rs:23`. Reste à prouver qu'aucun crate en aval n'en réintroduit une. US-123 le vérifie par un balayage explicite.
- Aucun type sérialisé dans la transcription n'utilise `HashMap` : les trois charges utiles libres d'`event.rs` sont des `serde_json::Value` (`event.rs:217,303,493`), et le seul `HashMap` d'`agent-core` est local à `tools.rs:661`, hors sérialisation. À reconfirmer si une variante arrive.
- L'espace de travail temporaire du harnais n'est pas un dépôt git, donc `turn_diff` n'est pas émis, ce que `docs/EVENT_SCHEMA.md:189` énonce déjà. US-126 en fait un scénario nommé au lieu d'un effet de bord.
- La distance entre `headless::run` et le binaire publié se réduit à l'analyse d'arguments et au chargement du credential par trousseau (`crates/agent-cli/src/main.rs:1101`). US-130 la mesure au lieu de la supposer.

### Hard Constraints
- Aucune dépendance ajoutée au workspace, aucune clé de configuration utilisateur ajoutée : le catalogue reste à quinze clés.
- La bascule d'écriture est une variable `PYXIS_*` hors configuration, catégorie « génération », et `crates/agent-doc-gates/src/gates.rs` doit refuser toute recette de vérification qui l'allume.
- `crates/agent-cli` n'a pas de cible `[lib]` et n'en gagne pas : le harnais vit dans un `#[cfg(test)] mod` sous `src/`, comme `config_catalog` et `tool_catalog`.
- Aucune recette ne met `PYXIS_LIVE_PARITY` : le rejeu ne touche aucun endpoint réel.
- Le dépôt `/home/arthur/dev/deepseek-harness` se lit, ne s'écrit pas et ne se copie pas : il est en TypeScript sur Cordis, seules ses décisions de conception se reprennent.
- Le comportement de production sur stdout ne change pas, `write_line` continuant à vider le tampon après chaque ligne pour un consommateur en pipe.
- Toute variable `PYXIS_*` lue sous `crates/*/src`, y compris depuis un module `#[cfg(test)]`, est balayée par `crates/agent-cli/src/config_catalog.rs:520` et doit figurer dans `docs/config-catalog.md`, sous peine de faire échouer la suite.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatage du workspace
- `cargo clippy --workspace --all-targets` - lints, sans `-D warnings` par décision documentée
- `cargo test --workspace --no-fail-fast` - suite complète, nomme tous les tests en échec
- `just check` - l'agrégat des quatre portes du CI est vert
- `cargo test -p agent-cli --bin pyxis transcript` - la porte ciblée du lot, sans bascule d'écriture
- `git status --porcelain` - vide après `just check` : aucune porte de vérification n'écrit

## Epics & User Stories

### EP-038: La couture déterministe

Les deux coutures de production et le fournisseur de rejeu qui, ensemble, rendent la vraie fonction headless exécutable en test et son résultat reproductible. Ferme les défaillances 1, 2, 3 et 4.

**Definition of Done:** un test appelle `crates/agent-cli/src/headless.rs:44`, capture la transcription en mémoire, et deux exécutions consécutives rendent des octets identiques.

#### US-120: Le rédacteur d'événements écrit dans un puits qu'on lui donne
**Description:** As a agent de codage, I want que `EventWriter` reçoive sa destination au lieu de verrouiller `stdout()` so that les octets réellement rendus soient observables sans polluer la sortie du harnais.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Source dsh:** `examples/headless-agent/tests/fixtures/headless-driver.ts:23,26` (33 lignes) pour l'écriture d'une ligne JSON par événement sur stdout, et `examples/headless-agent/tests/headless.snapshot.ts:669` pour sa capture en chaîne comparable

**Acceptance Criteria:**
- [ ] `EventWriter` porte un puits d'écriture fourni à la construction ; `crates/agent-cli/src/jsonl.rs:310` ne verrouille plus `std::io::stdout()` depuis une fonction libre.
- [ ] Given le chemin de production, when `headless::run` construit le rédacteur, then le puits est stdout et chaque ligne est suivie d'un vidage de tampon, comme aujourd'hui.
- [ ] Given un puits en mémoire, when une séquence d'événements est écrite, then le test lit des octets, pas un `serde_json::Value`.
- [ ] Given une erreur d'écriture sur le puits, when elle survient, then le comportement est celui d'aujourd'hui, l'erreur est ignorée et le tour continue, et un test le prouve sur un puits qui échoue toujours.
- [ ] Les quatorze tests existants de `jsonl.rs` restent verts sans changement de sémantique ; au moins un nouveau test assure la présence du `\n` terminal et l'absence de `\r`.
- [ ] Aucune clé de configuration, aucun drapeau et aucune variable d'environnement n'est ajouté par cette story.

#### US-121: La session accepte l'horloge et le générateur d'identifiants qu'on lui passe
**Description:** As a agent de codage, I want que `SessionRuntime::open` cesse de décider ses deux sources de non-déterminisme so that la même entrée produise les mêmes identifiants et les mêmes horodatages.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Source dsh:** `docs/testing.md:21-23`, qui nomme l'horloge parmi les frontières à simuler ; `packages/test-support/acp-snapshot/src/normalize.ts:321,327` comme contre-modèle, dsh mettant `time` à zéro faute de pouvoir geler la source

**Acceptance Criteria:**
- [ ] `crates/agent-cli/src/runtime.rs:655-656` ne construit plus `RandomIds` et `SystemClock` en dur ; les deux arrivent par l'appelant.
- [ ] Given le chemin de production, when `main.rs` ouvre une session, then il passe `RandomIds` et `SystemClock`, et le comportement observable est inchangé.
- [ ] Given `SequentialIds::starting_at(seed)` et une horloge gelée, when deux sessions sont ouvertes avec la même graine, then elles rendent les mêmes `thread_id`, `turn_id` et `event_id`.
- [ ] La duplication d'horloge est résolue : la session et le moteur (`crates/agent-cli/src/runtime.rs:546-552`) partagent une seule instance, ou le document de la story explique pourquoi deux restent nécessaires.
- [ ] Given un appel qui omettrait l'une des deux dépendances, when le code est compilé, then il ne compile pas : aucune valeur par défaut silencieuse n'est introduite.
- [ ] Aucune clé de configuration n'est ajoutée : l'injection est un paramètre de fonction, pas un réglage.

#### US-122: Le fournisseur de rejeu refuse ce que le script n'a pas prévu, et le script est prouvé consommé
**Description:** As a agent de codage, I want un fournisseur de test qui joue un script et échoue sur tout écart so that un scénario qui s'arrête tôt ou qui appelle une fois de trop devienne rouge.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Source dsh:** `packages/test-support/llm-replay/src/index.ts:138` et `:787` pour `assertConsumed`, `:799` pour son message, `:774` et `:768` pour le refus d'une requête hors script ; `examples/headless-agent/tests/snapshots/goal-tools/replay.override.json` et `ralph-loop/replay.override.json` pour la forme d'un script par scénario

**Acceptance Criteria:**
- [ ] Un `Provider` de rejeu vit dans le crate binaire sous `#[cfg(test)]`, rejoue les fixtures `.sse` à travers le vrai `CodexEventMapper`, comme le fait déjà `crates/agent-cli/tests/e2e_headless.rs`.
- [ ] Given une requête au-delà de ce que le script contient, when elle arrive, then le fournisseur rend une erreur nommant le scénario et le rang de la requête, jamais un flux vide.
- [ ] Given un script partiellement joué, when le harnais se démonte, then l'assertion de consommation échoue en nommant les entrées restantes.
- [ ] Given un script entièrement joué, when le harnais se démonte, then l'assertion passe silencieusement.
- [ ] Le fournisseur n'ouvre aucun socket et n'atteint aucun endpoint : `harness_needs_no_credentials_terminal_or_keyring` reste vrai pour le nouveau harnais.
- [ ] L'assertion de consommation est appliquée aux cinq fixtures `.sse` existantes ou une ligne de la story explique pour chacune pourquoi elle ne s'y prête pas.

#### US-123: Le chemin headless réel tourne en test et rend une transcription sans octet volatile
**Description:** As a mainteneur, I want que le harnais appelle `headless::run` et non une reconstitution so that l'ordre des terminaux, la présence du `turn_diff` et le `Hook(Stop)` final soient prouvés par la fonction qui les produit.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-120, US-121, US-122
**Source dsh:** `examples/headless-agent/tests/headless.snapshot.ts` et `examples/headless-agent/tests/fixtures/headless-driver.ts:10` pour le pilotage du chemin publié ; `docs/testing.md:31-35` pour la règle « "Real entry path" means the published artifact »

**Acceptance Criteria:**
- [ ] Un `#[cfg(test)] mod transcript` sous `crates/agent-cli/src/` appelle `crates/agent-cli/src/headless.rs:44` avec un `HeadlessRun` complet, sous `#[tokio::test(start_paused = true)]`.
- [ ] Le module suit l'idiome que `crates/agent-cli/src/main.rs:31-34` documente : il est déclaré `#[cfg(test)]` dans `main.rs`, et sa raison d'être y est écrite comme celles de `config_catalog` et `tool_catalog`.
- [ ] Given deux exécutions consécutives du même scénario, when leurs sorties sont comparées, then elles sont identiques octet à octet.
- [ ] Given la transcription produite, when elle est balayée, then elle ne contient aucun chemin absolu, aucun horodatage mur, et aucune valeur variant d'une exécution à l'autre ; le balayage est une assertion du test, pas une inspection manuelle.
- [ ] Given un espace de travail temporaire hors git, when le tour se termine, then aucun `turn_diff` n'est émis, conformément à `docs/EVENT_SCHEMA.md:189`, et le test l'assure explicitement au lieu de le subir.
- [ ] Le `session_id` de la transcription est déterministe ou justifié : s'il ne peut pas l'être, la story dit pourquoi et le rend stable par la graine de US-121.
- [ ] Le test ne lit aucun credential et ne touche pas au trousseau.

---

### EP-039: La transcription gelée et la porte qui la compare

Le fichier attendu, la comparaison octet à octet, l'isolation de sa bascule d'écriture et la couverture des quatre chemins terminaux. Ferme la défaillance 6.

**Definition of Done:** quatre fichiers `.jsonl` sont dans le dépôt, `just test` échoue sur un octet de différence en nommant sa commande de régénération, et `crates/agent-doc-gates` refuse toute recette de vérification qui allumerait la bascule.

#### US-124: La transcription est gelée dans un fichier et comparée octet à octet
**Description:** As a mainteneur, I want que la sortie attendue soit un fichier versionné comparé sans normalisation so that le diff d'une pull request montre l'effet du changement sur ce que les intégrations parsent.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-123
**Source dsh:** `examples/headless-agent/tests/snapshots/advanced-toolchain/stream-json.expected.jsonl` pour le nom et la forme du fichier attendu ; `examples/headless-agent/tests/headless.snapshot.ts:669` pour l'égalité stricte contre le contenu lu

**Acceptance Criteria:**
- [ ] Le fichier attendu est un `.jsonl` brut, une ligne par événement, terminé par `\n`, lisible sans outil.
- [ ] La comparaison est un `assert_eq!` sur les octets, sur le patron de `crates/agent-app-server/tests/schemas.rs`, sans `trim`, sans normalisation de fin de ligne, sans réordonnancement.
- [ ] Given un fichier attendu absent, when la porte tourne sans bascule, then elle échoue en nommant la commande de régénération, et ne crée aucun fichier.
- [ ] Given un octet de différence, when la porte tourne, then le message d'échec nomme le scénario, le chemin du fichier et la commande de régénération.
- [ ] Given la bascule d'écriture allumée, when la porte tourne, then elle écrit le fichier et sort sans comparer, exactement comme `PYXIS_UPDATE_SCHEMAS`.
- [ ] Une entrée `.gitattributes` fixe les fichiers gelés en `-text` pour qu'aucun checkout ne réécrive leurs fins de ligne, et un test assure que le dernier octet du fichier est `\n` et qu'aucun `\r` n'est présent.
- [ ] Aucune dépendance n'est ajoutée : ni insta, ni expect-test, ni goldenfile, ni snapbox.

#### US-125: La bascule d'écriture est isolée comme les deux autres
**Description:** As a mainteneur, I want que la troisième bascule d'écriture suive le régime des deux premières so that aucune recette de vérification ne puisse rendre la porte complaisante.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-124
**Source dsh:** aucune pour l'isolation, règle propre à Pyxis, `crates/agent-doc-gates/src/gates.rs:266` réservant explicitement la place ; `examples/headless-agent/tests/headless.snapshot.ts:642-644` pour le mode de réécriture dont la bascule est l'équivalent

**Acceptance Criteria:**
- [ ] La bascule est ajoutée à `WRITE_SWITCHES` dans `crates/agent-doc-gates/src/gates.rs`, et le commentaire annonçant la troisième est mis à jour ou retiré.
- [ ] Une ligne est ajoutée à la recette `regen` du `justfile`, et à elle seule.
- [ ] Given une recette de vérification qui allumerait la bascule, when `cargo test -p agent-doc-gates` tourne, then il échoue en nommant la recette fautive.
- [ ] Given une recette qui atteindrait `regen` par dépendance, when la porte d'isolation tourne, then elle échoue de la même façon.
- [ ] La variable est déclarée dans `crates/agent-cli/src/config_catalog.rs`, catégorie « génération », puis `docs/config-catalog.md` est régénéré et non édité à la main.
- [ ] Given la variable lue depuis un module `#[cfg(test)]` sous `crates/*/src`, when `scan_variables` (`crates/agent-cli/src/config_catalog.rs:520`) balaye l'arbre, then elle est trouvée et classée, et la suite reste verte.
- [ ] Le nombre de clés de configuration utilisateur reste quinze ; la porte de catalogue le prouve.

#### US-126: Quatre scénarios couvrent le tour nu, l'outil, l'interruption et l'erreur
**Description:** As a intégrateur, I want que les quatre fins possibles d'un tour soient gelées so that mon parseur ait un exemple exécutable de chaque état terminal.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-124
**Source dsh:** `examples/headless-agent/tests/snapshots/` pour le répertoire par scénario, onze au 2026-08-21 ; `advanced-toolchain/`, `provider-retry/`, `startup-activation-error/` et `invalid-credential/` pour la variété des chemins couverts ; `docs/testing.md:47` pour la règle qui rend le scénario obligatoire dans la même pull request

**Acceptance Criteria:**
- [ ] Chaque scénario est un répertoire portant son entrée, son script de rejeu et sa transcription attendue côte à côte, nommés de façon uniforme.
- [ ] Le scénario « tour nu » couvre texte puis `end_turn`, et sa transcription se termine par `run_summary`.
- [ ] Le scénario « outil » couvre `tool_call` puis `tool_result` puis `end_turn`, et sa transcription porte une durée d'outil figée par `start_paused`, pas une mesure mur.
- [ ] Le scénario « interruption » couvre un tour interrompu : `run_summary` reste la dernière ligne et porte la raison.
- [ ] Le scénario « erreur » couvre un flux SSE malformé : l'erreur de contrat du fournisseur apparaît dans la transcription et `run_summary` reste la dernière ligne.
- [ ] Given un scénario ajouté après ce lot, when il est déposé dans un nouveau répertoire, then il est découvert sans modifier le code de la porte.
- [ ] La somme des fichiers gelés reste sous 100 Kio.
- [ ] Chaque scénario porte son assertion de consommation de US-122 et elle passe.

---

### EP-040: Le contrat écrit rattrapé par sa preuve

Le document d'événements corrigé et tenu par les fichiers gelés, l'ordre garanti transformé en assertion, la note qui consigne les arbitrages. Ferme la défaillance 5.

**Definition of Done:** `docs/EVENT_SCHEMA.md` décrit les vingt-quatre types avec les bons préfixes, l'ordre qu'il énonce est assuré par un test, et une note de décision consigne pourquoi ce n'est pas insta et où s'arrête le chemin réel.

#### US-127: Le schéma d'événements documente les six types muets et corrige ses préfixes
**Description:** As a intégrateur, I want que le document décrive tous les types et les vrais préfixes so that un parseur écrit d'après lui reconnaisse ce que le binaire émet.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-126
**Source dsh:** `examples/headless-agent/tests/snapshots/provider-retry/stream-json.expected.jsonl` pour le principe d'un exemple tiré de la sortie réelle plutôt que rédigé

**Acceptance Criteria:**
- [ ] Les six types absents sont documentés : `hook`, `plan`, `response_metadata`, `response_item`, `provider_extension`, `unmapped_response_item`.
- [ ] Les préfixes des exemples deviennent `thr_`, `trn_` et `evt_`, conformes à `crates/agent-runtime/src/id.rs:191,196,206`, et le document dit que la partie qui suit est trente-deux caractères hexadécimaux minuscules.
- [ ] Le document dit que `hook` est émis à la fin de chaque exécution réussie, ce que `crates/agent-cli/src/headless.rs` fait aujourd'hui sans que rien ne l'écrive.
- [ ] Le document dit que `event_id` est absent des lignes headless, ce qui est le comportement observé.
- [ ] Given une variante ajoutée à `AgentEvent` sans entrée dans le document, when `cargo test` tourne, then une assertion compare le nombre de variantes au nombre de types documentés et échoue en nommant l'écart.
- [ ] Given un exemple du document, when il est comparé à une transcription gelée, then sa forme correspond ; les exemples sont tirés des fichiers de US-126, pas rédigés.

#### US-128: L'ordre garanti cesse d'être une prose et devient une assertion
**Description:** As a intégrateur, I want que la règle d'ordre du document soit tenue par un test so that mon parseur puisse s'arrêter sur `run_summary` sans risque.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-126
**Source dsh:** `docs/testing.md:27-29`, règle « Verify the world, not the self-report »

**Acceptance Criteria:**
- [ ] Un test assure que `run_summary` est la dernière ligne de chacune des quatre transcriptions gelées.
- [ ] Un test assure que `turn_diff`, quand il est présent, précède `run_summary`.
- [ ] Given un scénario hors git, when la transcription est lue, then `turn_diff` est absent et le test le constate au lieu de sauter la vérification.
- [ ] Given une inversion de l'ordre des terminaux dans `headless.rs`, when la suite tourne, then au moins deux tests échouent : la porte octet à octet et l'assertion d'ordre.
- [ ] L'assertion lit les fichiers gelés, donc elle reste vraie sans réexécuter le harnais.

#### US-129: La note de décision consigne pourquoi ce n'est pas insta et où s'arrête le chemin réel
**Description:** As a mainteneur, I want que les trois arbitrages du lot soient écrits so that le prochain lecteur ne repose pas la question de la dépendance de snapshot.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-125, US-128
**Source dsh:** `docs/testing.md:47` pour l'obligation de scénario dans la même pull request, et `docs/testing.md:21-35` pour les trois règles que la note arbitre ; aucune ligne copiée

**Acceptance Criteria:**
- [ ] Une note sous `docs/notes/` suit le format que `docs/notes/README.md` impose et passe la porte d'`agent-doc-gates`.
- [ ] La note consigne le rejet d'insta en citant `insta-1.48.0/src/snapshot.rs:753-765` et l'effet de `trim_end` sur le `\n` terminal, et dit que la conclusion vaut pour toute version tant que cette normalisation existe.
- [ ] La note consigne le rejet du serveur mock loopback et le conflit entre `start_paused` et l'E/S réelle.
- [ ] La note consigne le déterminisme par injection contre le masquage après coup, avec la conséquence : un horodatage figé reste un témoin, un horodatage remis à zéro n'en est plus un.
- [ ] La note consigne la frontière du lot et renvoie au résultat de US-130.
- [ ] Given une décision de la note qu'une pull request pourrait violer dans `crates/`, when elle est identifiée, then elle est un ADR de `docs/DECISIONS.md` et pas une note, conformément à la règle d'`AGENTS.md`, ou la note explique pourquoi aucun crate ne peut la contredire.

#### US-130: La distance entre `headless::run` et le binaire publié est mesurée et nommée
**Description:** As a mainteneur, I want savoir exactement ce que le harnais ne couvre pas so that le choix de ne pas lancer le binaire publié soit un arbitrage écrit et non un oubli.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** None
**Source dsh:** `docs/testing.md:31-35`, « "Real entry path" means the published artifact : a package `bin` runs built `lib/bin.js` under plain `node` », règle que ce lot ne tient qu'en partie

**Acceptance Criteria:**
- [ ] Le travail énumère chaque instruction de `crates/agent-cli/src/main.rs` située entre l'analyse d'arguments et l'appel à `headless::run` que le harnais ne traverse pas.
- [ ] Le chargement du credential par trousseau (`crates/agent-cli/src/main.rs:1101`) et la construction du fournisseur (`main.rs:1510`) sont nommés comme les deux obstacles au lancement de `CARGO_BIN_EXE_pyxis` sans trousseau.
- [ ] Le travail dit, pour chaque instruction non couverte, si un test existant la couvre ailleurs, et lequel.
- [ ] Given une couture qui rendrait le binaire testable, when elle est évaluée, then le coût est écrit en termes de surface de production ajoutée, et la story conclut par une recommandation, pas par une implémentation.
- [ ] Aucune couture de credential et aucune variable d'environnement permettant de choisir un fournisseur n'est ajoutée par cette story.
- [ ] Le résultat entre dans la note de US-129 ou dans un document que celle-ci cite.

## Functional Requirements

- FR-01: Le système doit produire, pour une même entrée et un même script de rejeu, une transcription JSONL identique octet à octet d'une exécution à l'autre.
- FR-02: Le système doit comparer la transcription produite au fichier gelé sans aucune normalisation, y compris sur le `\n` terminal et les fins de ligne.
- FR-03: Le système doit, en cas de différence, nommer le scénario, le chemin du fichier attendu et la commande exacte de régénération.
- FR-04: Le système doit échouer quand le script de rejeu n'a pas été consommé en entier au démontage du scénario.
- FR-05: Le système doit rendre une erreur nommée, jamais un flux vide, quand une requête dépasse ce que le script contient.
- FR-06: Le système ne doit écrire un fichier gelé que lorsque la bascule d'écriture dédiée est présente dans l'environnement.
- FR-07: Le système doit refuser qu'une recette de vérification, directement ou par dépendance, allume cette bascule.
- FR-08: Le système doit exercer `crates/agent-cli/src/headless.rs:44` lui-même, et non une reconstitution de son assemblage.
- FR-09: Le système ne doit lire aucun credential, n'ouvrir aucun socket et n'atteindre aucun endpoint pendant le rejeu.
- FR-10: Le système ne doit pas modifier le comportement observable de la sortie de production : une ligne, un vidage de tampon.
- FR-11: Le système doit échouer quand une variante d'`AgentEvent` n'a pas d'entrée correspondante dans `docs/EVENT_SCHEMA.md`.

## Non-Functional Requirements

- **Performance:** la porte de transcription ajoute au plus 3 s de temps mur à `just test` sur cache chaud, mesuré sur les quatre scénarios ; aucun scénario n'attend une durée réelle, `start_paused` rendant toute temporisation virtuelle.
- **Reproductibilité:** zéro octet de différence entre deux exécutions consécutives sur la même machine, et entre une exécution locale et une exécution CI ; c'est la définition d'échec de la porte.
- **Empreinte:** 0 dépendance ajoutée au `Cargo.toml` du workspace, 0 clé de configuration utilisateur ajoutée, exactement 1 variable `PYXIS_*` ajoutée, catégorie « génération », 0 recette ajoutée au `justfile`.
- **Taille:** somme des fichiers gelés sous 100 Kio ; un scénario individuel sous 25 Kio.
- **Sécurité:** aucun credential lu, aucune connexion sortante, aucune variable d'environnement permettant de substituer un fournisseur sur le chemin de production ; le rejeu est confiné à `#[cfg(test)]` et disparaît du binaire publié.
- **Maintenabilité:** ajouter un scénario coûte un répertoire et zéro ligne dans le code de la porte ; la découverte est par balayage.
- **Fiabilité:** 0 test dépendant de l'horloge mur, du réseau, du système de fichiers utilisateur ou de l'état git de la machine ; la porte tourne dans un espace de travail temporaire.

## Edge Cases & Error States

| # | Scénario | Déclencheur | Comportement attendu | Message |
|---|----------|-------------|----------------------|---------|
| 1 | Fichier gelé absent | Nouveau scénario, première exécution sans bascule | La porte échoue, ne crée rien | « transcription absente pour `<scénario>` ; régénérer avec `<commande>` » |
| 2 | Un octet de différence | Champ renommé, ordre changé, `skip_serializing_if` ajouté | La porte échoue en nommant le fichier | « `<chemin>` est périmé ; régénérer avec `<commande>` » |
| 3 | Script non consommé | Le tour s'arrête avant la fin du script | Le démontage échoue en nommant les entrées restantes | « script `<scénario>` : `<n>` entrées non jouées » |
| 4 | Requête hors script | Le tour appelle le fournisseur une fois de trop | Erreur nommée, jamais un flux vide | « requête `<n>` hors script pour `<scénario>` » |
| 5 | Espace de travail hors git | Répertoire temporaire du harnais | Aucun `turn_diff` émis, et le test l'assure | sans objet |
| 6 | Fin de ligne réécrite au checkout | `.gitattributes` absent ou mal réglé | La porte échoue sur le `\r` | « la transcription contient un `\r` ; vérifier `.gitattributes` » |
| 7 | Temporisation de reprise | Le scénario porte un `retry_scheduled` | `delay_ms` est la valeur figée du script, pas une mesure mur | sans objet |
| 8 | Tour interrompu | Scénario d'interruption | `run_summary` reste la dernière ligne et porte la raison | sans objet |
| 9 | Flux SSE malformé | Scénario d'erreur | L'erreur de contrat apparaît, `run_summary` reste dernière | sans objet |
| 10 | Nouvelle variante d'`AgentEvent` non documentée | Une pull request ajoute un type | L'assertion de comptage échoue | « `<n>` variantes, `<m>` types documentés » |
| 11 | Bascule allumée dans une recette de vérification | Une pull request ajoute la variable à `check` | `agent-doc-gates` échoue en nommant la recette | « `<recette>` allume `<bascule>` ; seule `regen` le peut » |
| 12 | Puits d'écriture en échec | Sortie fermée par le consommateur | Le tour continue, comme aujourd'hui ; un test le prouve | sans objet |

## Risks & Mitigations

| # | Risque | Probabilité | Impact | Mitigation |
|---|--------|-------------|--------|------------|
| 1 | Le fichier gelé devient un tampon : régénéré sans être lu | Haute | Haut | La bascule ne vit que dans `regen`, `AGENTS.md` prescrit déjà la lecture du `git diff` après, et le message d'échec nomme la commande plutôt que de la suggérer. US-128 double la porte d'assertions structurelles qu'une régénération aveugle ne rend pas vertes silencieusement. |
| 2 | Non-déterminisme résiduel découvert tard : chemin temporaire, locale, flottant | Moyenne | Haut | US-123 porte un critère de balayage explicite de la transcription et l'assertion des deux exécutions consécutives ; la découverte arrive avant le gel, pas après. |
| 3 | La couture d'injection élargit une API de production pour le test seul | Moyenne | Moyen | Les deux coutures sont des paramètres de fonction, jamais des clés de configuration, ce que l'invariant d'`AGENTS.md` sur les limites d'orchestration impose déjà. Aucune valeur par défaut silencieuse : un appel incomplet ne compile pas. |
| 4 | Dérive de périmètre vers le binaire publié | Moyenne | Moyen | US-130 mesure au lieu de construire, la section Non-Goals l'énonce, et le critère de la story interdit explicitement d'ajouter la couture de credential. |
| 5 | Le refactor du puits change le comportement de vidage vu par un consommateur en pipe | Basse | Haut | US-120 porte un critère de vidage par ligne sur le chemin de production et un test sur un puits en échec. |
| 6 | Un scénario devient lent par une temporisation réelle non gelée | Basse | Moyen | `start_paused` sur tous les tests du harnais, et la contrainte de 3 s mesurée comme critère de la NFR de performance. |
| 7 | La correction de `docs/EVENT_SCHEMA.md` révèle une rupture de contrat déjà livrée | Basse | Moyen | Les préfixes documentés sont faux mais jamais tenus par le binaire : la correction aligne le document sur le comportement, aucune sortie ne change. La note de US-129 le consigne. |

## Non-Goals

Frontières explicites de cette version.

- **Lancer `CARGO_BIN_EXE_pyxis` sans trousseau.** La règle 3 de `docs/testing.md` demande le chemin publié ; l'atteindre exigerait une couture de credential et un moyen de substituer le fournisseur depuis l'environnement, c'est-à-dire deux surfaces de production ajoutées pour le test seul, dont l'une est une surface de sécurité. US-130 mesure ce que cela coûterait et ce que cela couvrirait de plus ; la décision se prend sur cette mesure, pas ici.
- **Inscrire la porte dans `docs/parity/offline-suite.md`.** Cette table s'organise par domaine de contrat Codex, et `crates/agent-cli/tests/e2e_headless.rs` n'y figure pas : le JSONL headless est un contrat propre à Pyxis, documenté par `docs/EVENT_SCHEMA.md`. La porte suit le même régime que le précédent qui lui ressemble le plus.
- **Un troisième mode de rafraîchissement contre un vrai fournisseur.** dsh en a un ; `AGENTS.md` interdit à toute recette de mettre `PYXIS_LIVE_PARITY`, donc le mode n'aurait aucun appelant licite.
- **Geler la sortie du TUI ou celle d'app-server.** Le TUI a ses instantanés `insta` revus par `cargo insta review`, app-server a ses schémas comparés octet à octet. Ce lot ne touche ni l'un ni l'autre.
- **Geler la sortie texte de `--output-format text`.** Elle est destinée à un humain, pas à un parseur ; elle n'a pas de contrat écrit et ce lot ne lui en donne pas un.
- **Ajouter un outil de revue interactif pour les transcriptions.** `git diff` sur un `.jsonl` d'une ligne par événement est lisible ; un équivalent de `cargo insta review` serait une dépendance pour un service que le format rend déjà.

## Files NOT to Modify

- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés et empreintés, jamais édités à la main.
- `docs/crate-graph.md`, `docs/tool-catalog.md`, `docs/config-catalog.md` : rendus depuis le code. La ligne de US-125 s'ajoute dans `crates/agent-cli/src/config_catalog.rs` puis le document se régénère ; une édition directe est perdue à la régénération suivante.
- `crates/agent-core/src/agent.rs` : `run_agent` est le seul moteur modèle-outils ; ce lot l'observe et ne le modifie pas.
- Le clone Codex résolu par `$PYXIS_CODEX_BASELINE` : lecture seule, sans exception.
- `spikes/` : espace jetable exclu de la Phase 0.
- `.github/workflows/ci.yml` : aucune recette n'est ajoutée, donc aucune étape ne l'est ; toute modification ici déclencherait la porte d'inventaire de recettes.

## Technical Considerations

Formulé comme des questions pour l'ingénierie, non comme des mandats.

- **Forme de la couture d'injection:** `SessionRuntime::open` gagne deux paramètres, ou un constructeur `#[cfg(test)]` parallèle. Recommandé : deux paramètres, parce qu'un second constructeur laisserait le chemin de production et le chemin de test diverger sans que rien ne le signale. À confirmer contre la longueur de la signature actuelle.
- **Forme du puits d'`EventWriter`:** un `Box<dyn Write + Send>` ou un paramètre de type générique. Recommandé : le trait-objet, `EventWriter` étant construit une fois par exécution et jamais dans une boucle chaude. Compromis : une indirection contre une signature qui ne se propage pas dans `HeadlessRun`.
- **Portée de la déclaration de la bascule:** le nom suit-il `PYXIS_UPDATE_SCHEMAS` et `PYXIS_UPDATE_CATALOGS`, ce qui suggère `PYXIS_UPDATE_TRANSCRIPTS` ? La question est de savoir si la porte gèlera un jour autre chose que des transcriptions ; si oui, un nom plus large évite une quatrième bascule.
- **Découverte des scénarios:** balayage d'un répertoire au moment du test, ou liste explicite dans le code. Recommandé : le balayage, pour que l'ajout d'un scénario ne touche pas le code. Compromis : un répertoire mal formé devient une erreur d'exécution au lieu d'une erreur de compilation.
- **Détermination du `session_id`:** dérive-t-il de la graine de US-121, ou reste-t-il fourni par l'appelant du harnais ? La seconde option est plus simple et suffit tant que le harnais est le seul appelant de test.
- **Migration:** aucune. Le format JSONL ne change pas, `SCHEMA_VERSION` reste à 1, et les corrections de `docs/EVENT_SCHEMA.md` alignent le document sur un comportement déjà livré. Pas de retour arrière à prévoir.

## Success Metrics

| Metric | Baseline (actuel) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|--------------|
| Tests atteignant `crates/agent-cli/src/headless.rs:44` | 0 | ≥ 4 | Month-1 | `grep` des appelants dans `crates/agent-cli/` |
| Scénarios gelés comparés octet à octet | 0 | 4 | Month-1 | nombre de répertoires de scénario dans l'arbre de test |
| Portes octet à octet du dépôt | 4 | 5 | Month-1 | inventaire des `assert_eq!` sous bascule `PYXIS_UPDATE_*` |
| Sources de non-déterminisme en dur sur le chemin headless | 2 (`runtime.rs:655-656`) | 0 | Month-1 | lecture de `SessionRuntime::open` |
| Types d'`AgentEvent` documentés | 18/24 | 24/24 | Month-1 | assertion de comptage de US-127 |
| Fixtures de rejeu avec assertion de consommation | 0/5 | 5/5 | Month-1 | lecture de `crates/agent-cli/tests/` et du harnais |
| Différence d'octets entre deux exécutions consécutives | non mesurée | 0 | Month-1 | assertion de US-123 |
| Différence d'octets entre exécution locale et CI | non mesurée | 0 | Month-6 | échec ou succès de `just check` en CI |
| Temps mur ajouté à `just test` | 0 s | ≤ 3 s | Month-1 | `cargo test -p agent-cli --bin pyxis transcript` chronométré sur cache chaud |
| Dépendances ajoutées au workspace | non applicable | 0 | Month-6 | `git diff` du `Cargo.toml` racine |

## Open Questions

- Le binaire publié doit-il finir par être lancé sans trousseau ? Arthur tranche sur la mesure de US-130, avant la clôture du lot. Décide si un lot ultérieur ajoute une couture de credential ou si la règle 3 de `docs/testing.md` reste tenue au niveau de `headless::run`.
- La bascule doit-elle se nommer d'après les transcriptions ou d'après un service plus large ? Arthur tranche à l'implémentation de US-125. Un nom trop étroit coûte une quatrième bascule au prochain gel non documentaire.
- Le `session_id` doit-il devenir dérivable de la graine d'identifiants ? Question ouverte à US-121 et US-123 ; sans réponse, le harnais le fournit, ce qui suffit tant qu'il est seul appelant.
- Les cinq fixtures `.sse` existantes gagnent-elles toutes l'assertion de consommation, ou certaines la refusent-elles par construction ? À constater à US-122 ; le critère demande une réponse écrite pour chacune, pas un silence.
[/PRD]
