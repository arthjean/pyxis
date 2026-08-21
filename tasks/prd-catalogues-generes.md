[PRD]
# PRD: Catalogues générés

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-21 | Arthur Jean | Rédaction initiale, lot #6 du plan de portage DeepSeek Harness |
| 1.1 | 2026-08-21 | Arthur Jean | Revue d'EP-031 : le critère d'ancrage d'US-095 supposait une capacité que la porte de liens n'a pas et refuse d'avoir ; il devient un critère de lien mort |

## Problem Statement

Pyxis publie quatre artefacts générés et prouvés frais : `docs/app-server/protocol.schema.json`, `docs/app-server/protocol.d.ts` et les deux matrices de parité. Tout le reste de `docs/` est rédigé à la main, y compris les documents dont le contenu est intégralement dérivable du code. Quatre défaillances mesurées sur l'état du dépôt au 2026-08-21 :

1. **L'invariant de souillure est prescrit et inspectable nulle part.** `AGENTS.md:79-80` pose que « tool output is untrusted by default » et que le défaut du trait `Tool` est fail-closed. Le défaut est bien `true` (`crates/agent-tools/src/tool.rs:382`), et **dix** implémentations de `crates/agent-tools/src/` le redescendent à `false` : `write.rs:55`, `edit.rs:64`, `patch.rs:108`, `plan.rs:102`, `context_window.rs:70` et `:167`, `time.rs:95` et `:177`, `ask.rs:111` et `:300`. Aucun document ne les liste. Un onzième désarmement passerait en revue sans qu'un relecteur ait un endroit où constater qu'il vient de s'ajouter à une liste. Le même angle mort couvre onze autres propriétés de politique : `DynTool` en déclare quinze (`tool.rs:440-500`), `ToolSpec` en transporte trois (`registry.rs:1243-1256`), et `tools_read()` est privé (`registry.rs:153`), donc rien hors du crate ne peut énumérer les outils.
2. **La surface de configuration est documentée à moitié, et sa liste de sécurité est fausse.** `README.md:277` annonce cinq clés de sécurité ; `SECURITY_KEYS` en compte sept (`crates/agent-cli/src/settings.rs:78-86`), `web_search` et `safe_commands` s'étant ajoutées sans que la phrase suive. Sur les quinze clés de `KNOWN_KEYS` (`settings.rs:54-70`), cinq n'apparaissent dans aucun document du dépôt (`cost_budget_micro_usd`, `input_cost_micro_per_ktok`, `output_cost_micro_per_ktok`, `overload_fallback_model`, `safe_commands`) et deux (`token_budget`, `web_search`) n'apparaissent que dans des audits de parité qu'`AGENTS.md:117-119` déclare non normatifs. Enfin `README.md:274` présente la couche `environment` comme « les variables `PYXIS_*` » alors que cinq clés exactement en ont une (`main.rs:565-614`) : le tableau des couches promet une généralité que le code ne tient pas.
3. **Les deux tableaux de crates sont des affirmations exhaustives périmées.** Seize crates existent sous `crates/`. `README.md:141-150` en liste dix, `docs/ARCHITECTURE.md:64-74` en liste onze, et le graphe ASCII d'`ARCHITECTURE.md:88-105` en dessine onze. Les absents sont `agent-app-server`, `agent-code-mode`, `agent-code-mode-v8`, `agent-doc-gates`, `agent-parity`, plus `agent-runtime` pour le seul README. Le graphe omet aussi l'arête `agent-tools -> agent-code-mode`. Chaque crate ajouté depuis l'écriture de ces tableaux les a rendus un peu plus faux, sans qu'aucun signal ne se déclenche.
4. **Rien n'empêche la prochaine occurrence.** Les quatre artefacts générés du dépôt le sont parce que quelqu'un a écrit un générateur pour chacun. Aucun mécanisme ne dit qu'un document décrivant une structure doit être dérivé, et aucune porte ne rattrape un tableau exhaustif rédigé à la main. Le taux de dérive observé est de trois documents sur trois.

**Why now:** le lot #5 vient de livrer `just check`, `just regen` et la porte de non-dérive d'`agent-doc-gates` (`crates/agent-doc-gates/src/gates.rs`). Les trois briques dont un catalogue généré a besoin, un agrégat de vérification, un niveau de régénération séparé et un précédent de document lu par un test, existent depuis quatre commits et n'ont pas encore de second usage. Écrire les catalogues maintenant les fait entrer dans une mécanique déjà éprouvée ; les écrire après les lots #7 et #8, qui ajoutent chacun une porte documentaire, coûte trois migrations au lieu d'une. La dérive mesurée, elle, ne se stabilise pas : elle croît d'une ligne à chaque crate, chaque outil et chaque clé.

## Overview

Le lot ajoute trois documents dérivés sous `docs/`, chacun produit par une fonction pure et prouvé frais par une comparaison octet à octet à l'intérieur de `cargo test --workspace` : `docs/crate-graph.md` depuis les seize `crates/*/Cargo.toml`, `docs/tool-catalog.md` depuis les `DynTool` réellement enregistrés par le binaire, et `docs/config-catalog.md` depuis `crates/agent-cli/src/settings.rs`. La mécanique n'est pas inventée : `crates/agent-app-server/tests/schemas.rs:38-54` la porte déjà, avec une variable d'environnement qui bascule le même code de la vérification vers l'écriture, et le message d'échec qui nomme la commande de régénération. Le lot en produit trois instances de plus et ajoute leurs lignes à `just regen`, jamais à une recette de vérification.

De DeepSeek Harness, trois décisions se reprennent et aucune ligne ne se copie. La première est le générateur unique à deux modes : `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts:50-119` rend tout le fichier par une fonction `render()` pure, puis `--check` compare la chaîne entière et traite un fichier absent comme un fichier périmé, puisque le remède est le même. La deuxième est le garde de complétude : `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:636-645` globe le disque et échoue si un paquet d'outil manque au manifeste, parce que sans lui l'omission devient ce que le générateur produit et la porte de fraîcheur reste verte dessus. La troisième est le garde de récolte : `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:662-670` refuse qu'une entrée du manifeste contribue zéro outil, cas que le premier garde ne voit pas et qui signale un démarrage cassé plutôt qu'une section vide. La quatrième idée, le diff de la première ligne divergente (`gen-tool-catalog.ts:795-803`), est déjà couverte par `assert_eq!` sur des chaînes, que Rust rend avec un diff.

La difficulté propre à Pyxis n'est pas la porte, c'est l'accès à la donnée. Les trois catalogues ont trois sources de nature différente et cela commande où vit chaque générateur. Le graphe de crates se lit dans seize fichiers TOML, donc il tient entièrement dans `agent-doc-gates`, dont le `Cargo.toml:12-23` interdit toute dépendance et impose un parseur écrit à la main, comme pour l'arbre de notes et pour le `justfile`. Le catalogue de configuration a besoin de `KNOWN_KEYS`, `SECURITY_KEYS` et `ConfigLayer`, tous privés à `agent-cli`. Le catalogue d'outils a besoin d'instancier des `DynTool`, ce que seul le binaire sait faire : le jeu réel est câblé dans `crates/agent-cli/src/main.rs:1812-1877`, vingt-neuf sites d'enregistrement dont plusieurs exigent un handle de runtime, tandis que `agent_tools::default_registry` (`crates/agent-tools/src/lib.rs:96-118`) n'en expose que onze et n'est référencé que par des tests. Les deux derniers générateurs vivent donc dans `crates/agent-cli/src/`, sous `#[cfg(test)]` : le crate n'a qu'une cible `[[bin]]` et aucun `[lib]`, donc `crates/agent-cli/tests/` ne peut rien en importer, alors que les tests unitaires de la cible binaire s'exécutent bien, ce que les mille lignes de tests de `settings.rs` prouvent déjà.

Le corollaire du plan dsh est repris tel quel et c'est lui qui rend le lot honnête : un manifeste rédigé à la main a besoin d'un garde qui le confronte au disque. Pour les outils, le garde croise le manifeste avec les sites `.register(` de `main.rs`, un croisement textuel dont `agent-doc-gates` a déjà deux précédents. Pour la configuration, il croise la table déclarative avec `KNOWN_KEYS`, `SECURITY_KEYS` et les variables `PYXIS_*` lues par le binaire. Pour les crates, il croise le graphe avec `crates/*/Cargo.toml`. Un catalogue sans son garde documenterait exactement ce qu'on a pensé à y mettre.

Dernière décision, structurante et peu évidente : la colonne « rôle » des tableaux de crates devient dérivable parce qu'une ligne `description` entre dans chacun des seize `Cargo.toml`. Sans elle, le graphe généré ne peut porter que des noms et des arêtes, et les deux tableaux rédigés restent en place avec leur dérive. Avec elle, le rôle vit à côté du code, le générateur le lit, et les tableaux du `README.md` et d'`ARCHITECTURE.md` deviennent un lien. La colonne « dépendances interdites » d'`ARCHITECTURE.md`, elle, reste rédigée : c'est un invariant, pas un fait, et l'absence actuelle d'une arête ne dit pas qu'elle est proscrite.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Catalogues dérivés publiés et prouvés frais par `cargo test --workspace` | 3/3 | 3/3, plus ceux des lots suivants |
| Crates du workspace absents d'un tableau publié comme exhaustif | 0/16 | 0, quel que soit le nombre de crates |
| Clés de `KNOWN_KEYS` sans documentation normative | 0/15 | 0 |
| Outils enregistrés par le binaire absents du catalogue | 0 | 0 |
| Sites désarmant `returns_untrusted` lisibles dans un seul document | 10/10 | tous |
| Documents décrivant une structure et rédigés à la main | 0 des 3 visés | 0 |
| Dépendances ajoutées au workspace | 0 | 0 |
| Temps mur ajouté à `just test` sur cache chaud | ≤ 3 s | ≤ 3 s |

## Target Users

### Agent de codage éditant le dépôt
- **Role:** Claude Code, Codex ou tout agent recevant une tâche sur Pyxis avec un contexte vierge.
- **Behaviors:** lit `AGENTS.md` puis `docs/ARCHITECTURE.md` en entrée de session, cherche l'outil ou la clé la plus proche de sa tâche, et prend le premier tableau trouvé pour la vérité.
- **Pain points:** le tableau de crates d'`ARCHITECTURE.md` ne mentionne ni `agent-app-server` ni les deux crates Code Mode, donc l'agent qui cherche où poser une méthode de protocole ne trouve pas le crate qui l'accueille et invente un emplacement. Le tableau des couches de configuration du `README.md` promet une variable d'environnement par clé, donc l'agent propose `PYXIS_MODEL`, qui n'est lu nulle part.
- **Current workaround:** `ls crates/`, `grep` sur `KNOWN_KEYS`, lecture de sept cents lignes de `main.rs`. Aucun de ces gestes n'est prescrit, donc il n'est fait que par l'agent qui doute déjà.
- **Success looks like:** trois documents dont l'en-tête dit qu'ils sont générés, dont la fraîcheur est une assertion de la suite de tests, et qu'`AGENTS.md` nomme dans sa table « Where to read more ».

### Relecteur de la surface de sécurité
- **Role:** Arthur Jean relisant une pull request qui ajoute ou modifie un outil.
- **Behaviors:** lit le diff, vérifie que les défauts fail-closed du trait sont respectés ou argumentés, et cherche à savoir si le changement rejoint une liste connue.
- **Pain points:** un diff qui écrit `fn returns_untrusted(&self) -> bool { false }` est trois lignes ; savoir qu'il porte le compte de dix à onze demande un `grep` sur tout le workspace, et savoir si c'est cohérent avec les dix autres demande de les ouvrir. Même chose pour `is_deferrable`, `is_sensitive`, `is_read_only` et le `timeout`.
- **Current workaround:** aucun. La revue porte sur le diff, pas sur la population.
- **Success looks like:** le diff de la pull request contient la ligne du catalogue qui change, donc la revue voit le désarmement dans son tableau, à côté de ses pairs, sans quitter le diff.

### Contributeur externe découvrant le dépôt
- **Role:** développeur Rust arrivant par le `README.md`.
- **Behaviors:** lit la section « Files and configuration », essaie de régler Pyxis par `~/.pyxis/settings.toml`, et suppose que les clés listées sont les clés existantes.
- **Pain points:** cinq clés réelles ne sont nommées nulle part, donc les budgets de coût et le modèle de repli sur surcharge sont inatteignables autrement qu'en lisant `settings.rs`. Et le paragraphe des clés de sécurité en annonce cinq quand le code en refuse sept, donc un `-c web_search=true` échoue sans que le document ait prévenu.
- **Current workaround:** lire `crates/agent-cli/src/settings.rs`.
- **Success looks like:** un tableau exhaustif par construction, où chaque clé porte sa couche, sa précédence, son drapeau, sa variable d'environnement, son défaut et son caractère de sécurité.

## Research Findings

### Contexte concurrentiel

- **DeepSeek Harness**, la source du lot, génère six documents de ce type. Trois nous concernent, et ce ne sont pas ceux que la table du plan de portage désigne : `/home/arthur/dev/deepseek-harness/scripts/gen-doc-graphs.ts` (1 478 lignes) rend les coutures de capacité, le cycle de vie, le pipeline d'outils et un index, tandis que les trois catalogues cités dans la même cellule viennent de `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts` (814 l.), `/home/arthur/dev/deepseek-harness/scripts/gen-config-catalog.ts` (880 l.) et `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts` (119 l.). L'écart de taille est le renseignement : le graphe de modules, qui lit des manifestes, tient en 119 lignes ; le catalogue d'outils, qui doit démarrer un runtime, en coûte sept fois plus. La même hiérarchie de coût s'applique ici et commande l'ordre des epics.
- **Le générateur unique à deux modes est le motif dominant**, et pas seulement chez dsh : `tfplugindocs` a `generate` et `validate`, `expect-test` a `UPDATE_EXPECT=1`, `insta` a `cargo insta review`, `go test -update` fait de même. Le motif qui échoue est toujours le même, un vérificateur écrit séparément du générateur, qui dérive de lui. Le lot n'invente donc rien : `PYXIS_UPDATE_SCHEMAS` est déjà cette forme dans le dépôt.
- **La fraîcheur se prouve de deux façons dans l'état de l'art.** `tfplugindocs`, `terraform-docs`, `helm-docs` et les scripts `verify-*.sh` de Kubernetes régénèrent puis échouent sur un `git diff` sale ; `expect-test` et `insta` comparent dans le test. La seconde forme est retenue parce que le dépôt la porte déjà et parce qu'elle survit à un environnement sans `git`.
- **Le meilleur précédent de garde de complétude est `cargo dev update_lints` de rust-clippy** : il scanne les fichiers source à la recherche de `declare_clippy_lint!` et reconstruit la liste d'enregistrement, plutôt que de faire confiance à un manifeste, et le CI le lance en mode vérification. C'est exactement la forme d'US-098, qui croise le manifeste avec les sites `.register(` du binaire. La recherche n'a rien trouvé de plus proche : le cas où le générateur lui-même est incomplet reste peu traité dans l'outillage courant, et les projets qui s'en préoccupent ajoutent des assertions de non-vacuité explicites, faute de quoi un régénère-puis-diffe est aveugle à une source qui a rendu zéro élément.
- **La convention Go du fichier généré** (`// Code generated ... DO NOT EDIT.` en première ligne, `linguist-generated=true` dans `.gitattributes`) porte un piège pour ce lot : GitHub replie le diff d'un fichier marqué généré. Or ici le diff EST l'artefact de revue, puisque le bénéfice d'EP-032 est qu'un désarmement de `returns_untrusted` se voie dans la pull request. Les trois catalogues portent donc leur en-tête de fichier généré et ne sont PAS marqués `linguist-generated`.
- **Écarts et limites relevés.** `tfplugindocs` dérive d'un schéma qui est la vérité, donc son problème de complétude n'existe pas ; ses griefs récurrents sont la friction de migration et l'opacité des gabarits, ce qui plaide pour un rendu écrit à la main plutôt qu'un moteur de templates. Le piège de fraîcheur documenté par `oasdiff`, une spécification générée éditée à la main, est celui que la comparaison octet à octet ferme. Enfin le mode d'échec « frais mais inutile », un document qui répète le code sans en dire le sens, est la raison pour laquelle US-092 fait entrer une `description` rédigée dans chaque manifeste plutôt que de se contenter des noms et des arêtes.
- **Côté Rust, deux registres à l'édition des liens ont été considérés et écartés.** `inventory` et `linkme` servent à collecter des éléments dispersés entre crates ; Pyxis a un `Registry::register` explicite, donc l'énumération directe suffit et évite une dépendance. `cargo_metadata` donnerait le graphe résolu, mais suppose de lancer `cargo`, ce que l'hermétisme interdit, et le manifeste d'`agent-doc-gates` interdit la dépendance. Son argument le plus sérieux est que l'analyse à la main casse sur les dépendances héritées du workspace : c'est précisément la forme qu'emploie ce dépôt (`agent-*.workspace = true`), donc le parseur d'US-093 est fermé sur elle et échoue sur toute autre.

### Sources dsh, ancrées et vérifiées

Convention de chemins, valable dans tout ce document : **une ancre dsh est absolue et commence par `/home/arthur/dev/deepseek-harness/`, un chemin Pyxis est relatif à la racine du dépôt.** La règle est mécanique parce que les deux dépôts ont des chemins homonymes : `docs/tool-catalog.md` désigne la cible à produire ici et un document existant là-bas. Les numéros de ligne ci-dessous ont été vérifiés sur disque le 2026-08-21 et valent pour l'état du dépôt source à cette date ; ils sont un point d'entrée de lecture, pas un contrat, donc une ligne qui a bougé se retrouve par le nom du symbole cité. `dsh` est en TypeScript sur Cordis, Pyxis est en Rust : aucune ligne ne se transpose, seule une décision se reprend, et le dépôt source se lit sans jamais s'écrire.

| Ancre | Ce qui s'y lit | Reprise |
|---|---|---|
| `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts:50-97` | `render(pkgs)` pur : rend le fichier entier comme une chaîne, en-tête compris, sans lire ni écrire | Repris tel quel : chaque générateur du lot est une fonction pure d'un état collecté vers une `String` |
| `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts:79-80` | En-tête `<!-- Generated by … do not edit by hand. Run … to regenerate. -->` | Repris : chaque catalogue nomme son générateur et sa recette de régénération dans ses deux premières lignes |
| `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts:101-116` | `--check` compare la chaîne entière ; un fichier illisible ou absent est traité comme périmé, le remède étant le même | Repris : `std::fs::read_to_string(&path).unwrap_or_default()` produit déjà ce comportement dans `schemas.rs:46` |
| `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts:53` | Les arêtes viennent des `peerDependencies`, désignées comme le signal canonique de dépendance runtime | Transposé : les arêtes viennent des entrées `agent-*.workspace = true` de la section `[dependencies]`, à l'exclusion de `[dev-dependencies]` |
| `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:636-645` | `assertManifestComplete` : globe `packages/*/tool-*` et échoue en nommant chaque paquet absent du manifeste | Repris, transposé au croisement du manifeste avec les sites `.register(` de `main.rs` (US-097) et de la table de configuration avec `KNOWN_KEYS` (US-100) |
| `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:662-670` | `assertToolsHarvested` : une entrée qui récolte zéro outil est un démarrage cassé, pas une section vide, et le message dit quoi comparer | Repris (US-097) : une entrée de manifeste qui ne produit aucun `DynTool` échoue |
| `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:1-7` | Le générateur DÉMARRE chaque plugin sur un contexte réel, parce qu'un schéma d'outil n'est pas connaissable statiquement | Repris dans son principe : le catalogue instancie les outils plutôt que de lire leur source, `input_schema()` étant une méthode |
| `/home/arthur/dev/deepseek-harness/docs/tool-catalog.md:8-10` | Le document déclare la configuration sous laquelle il a été produit, et une note par paquet dit quelle branche il montre quand un champ requis n'a pas de défaut | Repris (US-096) : le catalogue déclare le mode de permission, le mode de bac à sable et les capacités du fournisseur sous lesquels il est rendu |
| `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:774-806` | `--check` imprime la première ligne divergente, `JSON.stringify` des deux côtés | Écarté : `assert_eq!` sur deux `String` rend déjà un diff, et `schemas.rs:47-52` en fait un précédent |
| `/home/arthur/dev/deepseek-harness/scripts/gen-config-catalog.ts:36` | Tout paquet doit se classer : `config`, `no-config`, `seam` ou `library`. Il n'y a pas de silence | Repris (US-100) : toute variable `PYXIS_*` lue par le binaire est soit une clé du catalogue, soit explicitement classée hors configuration |
| `/home/arthur/dev/deepseek-harness/scripts/gen-config-catalog.ts:756-762` | Le schéma runtime est croisé avec le type déclaré : une clé que le loader accepte mais que le type ignore échoue la porte, « the catalog paste would hide a loader-accepted field » | Repris dans son intention (US-100) : une clé acceptée par `settings.rs` et absente de la table déclarative échoue, sinon le catalogue documenterait moins que ce que le binaire accepte |
| `/home/arthur/dev/deepseek-harness/scripts/gen-config-catalog.ts:92-99, 455, 496` | Une expression que la marche statique ne sait pas lire échoue la porte au lieu d'être ignorée | Repris comme principe de conception : un cas non couvert échoue, il ne se rend pas en cellule vide |
| `/home/arthur/dev/deepseek-harness/.agents/notes/implemented/process/2026-07-02-tool-schema-catalog.md` | La note d'origine du catalogue d'outils : pourquoi le démarrage réel, pourquoi le garde de complétude | Contexte, et modèle de la note de décision d'US-105 |
| `/home/arthur/dev/deepseek-harness/.agents/notes/implemented/process/2026-07-04-doc-tiers-and-budgets.md` | Les niveaux de documentation et leurs budgets de taille | Repris en NFR : un catalogue est borné, pour que son diff reste relisible |
| `/home/arthur/dev/deepseek-harness/scripts/gen-doc-graphs.ts` | Les coutures de capacité, le cycle de vie, le pipeline d'outils, l'index | Écarté : le plan de portage le désigne comme source du lot, à tort, et Pyxis n'a pas d'équivalent des soixante-dix coutures de capacité |

### Sources Pyxis, ancrées et vérifiées

| Ancre | Ce qui s'y lit | Rôle dans le lot |
|---|---|---|
| `crates/agent-app-server/tests/schemas.rs:38-54` | La porte modèle : `PYXIS_UPDATE_SCHEMAS` bascule l'écriture, sinon `assert_eq!` sur les octets, et le message nomme la commande | Forme exacte des trois portes du lot, avec `PYXIS_UPDATE_CATALOGS` à la place |
| `crates/agent-app-server/src/schema.rs:79-94` | `sorted()` réécrit récursivement chaque objet en ordre `BTreeMap` : le déterminisme est construit, pas espéré | Repris : aucun `HashMap` n'atteint un rendu, tout ordre est explicite |
| `crates/agent-app-server/src/schema.rs:35-38, 189-195` | `Degraded { nodes }` : le générateur REFUSE plutôt que de rendre un nœud qu'il ne sait pas nommer | Repris comme principe : un cas non couvert échoue, il ne se rend pas en cellule vide |
| `justfile:59-63` | `regen` porte les trois régénérations qui écrivent, et aucune recette de vérification ne les lance | Les trois lignes du lot s'y ajoutent (US-103) |
| `crates/agent-doc-gates/Cargo.toml:12-23` | `[dependencies]` vide, argumenté ; parseur écrit à la main ; aucun crate Pyxis importable | Contraint le graphe de crates à y vivre et les deux autres catalogues à en sortir |
| `crates/agent-doc-gates/src/links.rs:25-71` | La porte de liens lit les `.md` de la racine plus tout `docs/` | Les trois catalogues y entrent d'office : leurs liens relatifs doivent résoudre dès la première génération |
| `crates/agent-doc-gates/src/gates.rs:568` | `PROSE_DOCUMENTS` est fermé à `AGENTS.md` et `CONTRIBUTING.md` | Un catalogue peut donc nommer sa commande de régénération dans son en-tête sans déclencher la porte de prose |
| `crates/agent-tools/src/tool.rs:440-500` | `DynTool` : quinze méthodes de politique, dont `timeout(ctx)` et `loop_guard_exempt(raw)` qui dépendent d'une entrée | Matière du catalogue d'outils, et raison pour laquelle deux colonnes demandent un contexte déclaré |
| `crates/agent-tools/src/registry.rs:237-260, 1243-1256` | `tool_specs()` applique le report puis le regroupement par espace de noms ; `specs_from_tools` ne construit que nom, description tronquée à 2 048 octets et `kind` | Le catalogue ne peut pas passer par `ToolSpec` : il lui faut un accès aux `DynTool` (US-095) |
| `crates/agent-cli/src/main.rs:1812-1877` | Vingt-neuf sites `.register(` / `.register_dyn(`, dont six outils multi-agents, trois outils de ressources MCP et deux outils Code Mode conditionnels | Source réelle du jeu d'outils, et cible du garde de complétude |
| `crates/agent-cli/Cargo.toml:19-22` | `[[bin]]` seul, aucun `[lib]` | `crates/agent-cli/tests/` ne peut rien importer : les deux générateurs y vivent sous `#[cfg(test)]` |
| `crates/agent-cli/src/settings.rs:54-86, 116-134` | `KNOWN_KEYS` (15), `SECURITY_KEYS` (7), `ConfigLayer::precedence()` et `label()` en `match` exhaustifs | Matière du catalogue de configuration |
| `crates/agent-cli/src/main.rs:543-614` | Le triplet (drapeau, variable d'environnement, clé) recâblé appel par appel, cinq fois | Ce que la table déclarative d'US-099 remplace comme source de documentation |

*Sources : lecture directe des dépôts `/home/arthur/dev/pyxis` et `/home/arthur/dev/deepseek-harness` le 2026-08-21. Contexte concurrentiel relevé par recherche web le 2026-08-21 : `hashicorp/terraform-plugin-docs`, `oasdiff/oasdiff`, le livre de rust-clippy sur `cargo dev update_lints`, `rust-analyzer/expect-test`, `go.dev/s/generatedcode`, `dtolnay/inventory`, `linkme`, `cargo_metadata`, et l'AI Agent Security Cheat Sheet de l'OWASP.*

## Assumptions & Constraints

### Assumptions (to validate)
- Les vingt-sept outils natifs peuvent être instanciés hors d'une session réelle pour lire leurs métadonnées. Onze sont des structures unitaires, cinq demandent un handle de runtime (`SpawnAgent`, `SendMessage`, `FollowupTask`, `ListAgents`, `WaitAgent`, `InterruptAgent`), trois un registre de ressources MCP, deux un handle Code Mode. À valider par US-095 : si un handle ne peut pas être fabriqué à vide, la ligne de cet outil déclare la substitution employée plutôt que d'être omise.
- `input_schema()` ne dépend d'aucun état de session pour les vingt-sept outils. Contre-exemple possible : un outil dont le schéma varie avec les capacités du fournisseur. Le catalogue déclare sa configuration de rendu (US-096), ce qui rend l'hypothèse falsifiable plutôt que tacite.
- Ajouter `description` aux seize `Cargo.toml` n'a aucun effet de construction, tous les crates portant `publish = false`. À valider au premier `cargo build --workspace` d'US-092.
- Le coût réel dépasse l'estimation « 2 à 3 jours » du plan de portage, qui n'a anticipé ni l'absence d'accesseur public sur les `DynTool`, ni l'absence de cible `[lib]` dans `agent-cli`, ni les trois gardes de complétude. Estimation révisée : 42 points, soit 5 à 6 jours.

### Hard Constraints
- Zéro dépendance ajoutée, ni au `Cargo.toml` du workspace, ni aux crates touchés. `agent-doc-gates` l'interdit explicitement dans son propre manifeste et impose un parseur écrit à la main.
- `agent-doc-gates` n'importe aucun crate Pyxis et n'entre dans le graphe d'aucun binaire. Le graphe de crates s'y conforme ; les deux autres catalogues n'y entrent donc pas.
- Aucune porte du lot ne lance de processus, n'ouvre de socket ni ne lit une variable d'environnement autre que le commutateur d'écriture. C'est la condition pour vivre dans `cargo test --workspace`.
- Les trois régénérations sont des lignes de `just regen` et d'aucune recette de vérification, `AGENTS.md:51` posant que `regen` écrit et ne se lance jamais depuis une vérification.
- `.github/workflows/ci.yml` ne gagne aucune étape : les trois portes sont des tests, donc déjà couvertes par l'étape `Tests`.
- Le clone résolu par `$PYXIS_CODEX_BASELINE` reste en lecture seule, et aucune recette ni test du lot ne le touche.
- Aucune porte du lot ne met `PYXIS_LIVE_PARITY`.
- `spikes/` reste hors périmètre : le graphe de crates ne couvre que `crates/*`.
- Les trois catalogues sont sous `docs/`, donc en français pour leur prose de cadrage. Les données citées du code (noms d'outils, descriptions destinées au modèle, clés, schémas JSON) restent en anglais, verbatim, sans traduction : traduire une `description()` en ferait une copie divergente.
- Le dépôt `/home/arthur/dev/deepseek-harness` se lit, ne s'écrit pas et ne se copie pas.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatage du workspace
- `cargo clippy --workspace --all-targets` - lints, sans `-D warnings` par décision documentée
- `cargo test --workspace --no-fail-fast` - suite complète, nomme tous les tests en échec
- `just check` - l'agrégat des quatre portes du CI est vert
- `git status --porcelain docs/` - vide après `just check` : une vérification n'écrit jamais un catalogue

## Epics & User Stories

### EP-031: Le graphe de crates dérivé du workspace

Le plus simple des trois catalogues, et celui qui éprouve la mécanique complète (rendu pur, porte octet à octet, garde de complétude) sur une source triviale. Il ferme la dérive la plus visible du dépôt : trois tableaux exhaustifs faux sur trois.

**Definition of Done:** `docs/crate-graph.md` est généré, prouvé frais par `cargo test --workspace`, couvre les seize crates, et les tableaux du `README.md` et d'`ARCHITECTURE.md` ne prétendent plus être exhaustifs.

#### US-092: Le rôle de chaque crate vit dans son manifeste
**Description:** As a agent de codage, I want lire le rôle d'un crate dans son propre `Cargo.toml` so that la colonne « rôle » cesse d'être une prose dupliquée dans deux documents qui divergent.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Chacun des seize `crates/*/Cargo.toml` porte un champ `description` d'une phrase, en anglais comme le reste des manifestes.
- [ ] La description reprend le rôle déjà écrit dans `README.md:141-150` ou `docs/ARCHITECTURE.md:64-74` quand il y en a un, et en formule un pour les six crates qui n'y figurent pas.
- [ ] Given `cargo build --workspace`, when il est lancé après l'ajout, then il réussit et aucun avertissement n'apparaît : `publish = false` rend le champ purement documentaire.
- [ ] Given un `Cargo.toml` de `crates/*` sans `description`, when la porte d'US-094 s'exécute, then elle échoue en nommant le crate.
- [ ] Aucun crate hors de `crates/` n'est touché ; `spikes/` reste hors périmètre.

#### US-093: Le graphe de crates est rendu par une fonction pure
**Description:** As a agent de codage, I want un `docs/crate-graph.md` dérivé des seize manifestes so that les crates et leurs arêtes cessent d'être une affirmation rédigée.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-092

**Acceptance Criteria:**
- [ ] `agent-doc-gates` expose une fonction pure qui prend le contenu des manifestes et rend le fichier entier comme une `String`, en-tête compris, sans lire ni écrire.
- [ ] Les deux premières lignes du rendu sont un commentaire HTML nommant le générateur et la recette de régénération, sur le modèle de `/home/arthur/dev/deepseek-harness/scripts/gen-module-graph.ts:79-80`.
- [ ] Le fichier porte un diagramme Mermaid des arêtes internes au workspace, puis un tableau `| Crate | Rôle | Dépend de |` trié par nom de crate.
- [ ] Les arêtes viennent des entrées `agent-*` de `[dependencies]` uniquement ; `[dev-dependencies]` et `[build-dependencies]` sont exclues, et le document le dit.
- [ ] Given un crate sans aucune dépendance interne, when le rendu s'exécute, then sa cellule « Dépend de » porte un marqueur d'absence explicite et le nœud apparaît quand même dans le diagramme.
- [ ] Given deux exécutions consécutives sur le même arbre, when leurs sorties sont comparées, then elles sont identiques octet pour octet : aucun `HashMap` n'atteint le rendu, aucun horodatage, aucun chemin absolu, aucune dépendance à la locale.
- [ ] Given une entrée `[dependencies]` que le parseur ne sait pas lire, when le rendu s'exécute, then il échoue en nommant le fichier et la ligne, plutôt que de rendre une cellule vide.
- [ ] Le générateur n'ajoute aucune dépendance à `agent-doc-gates` : le TOML est parsé à la main, comme le `justfile` l'est déjà.

#### US-094: La fraîcheur du graphe est une assertion, et rien n'y échappe
**Description:** As a mainteneur, I want que `cargo test --workspace` échoue sur un graphe périmé ou incomplet so that un crate ajouté ne puisse pas rester hors du document.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-093

**Acceptance Criteria:**
- [ ] Un test d'intégration d'`agent-doc-gates` compare `docs/crate-graph.md` au rendu, octet pour octet.
- [ ] `PYXIS_UPDATE_CATALOGS` défini bascule le même test en écriture, sur le modèle de `crates/agent-app-server/tests/schemas.rs:40-45`.
- [ ] Given le fichier absent, when le test s'exécute sans la variable, then il échoue comme sur un fichier périmé, le remède étant le même.
- [ ] Given un fichier périmé, when le test échoue, then le message nomme le chemin et la commande exacte de régénération.
- [ ] Given un répertoire `crates/<nom>/` contenant un `Cargo.toml` absent du graphe rendu, when le test s'exécute, then il échoue en nommant ce crate : le garde de complétude confronte le rendu au disque, sans quoi l'omission serait ce que le générateur produit.
- [ ] Given `crates/` vide ou illisible, when le garde s'exécute, then il échoue plutôt que de valider un graphe vide.
- [ ] Le test ne lance aucun processus, n'ouvre aucun socket et ne lit aucune variable d'environnement hors `PYXIS_UPDATE_CATALOGS`.

#### US-095: Les tableaux rédigés cessent de prétendre à l'exhaustivité
**Description:** As a contributeur externe, I want que `README.md` et `docs/ARCHITECTURE.md` renvoient au graphe généré so that je ne lise plus un tableau qui omet six crates sans le dire.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-094

**Acceptance Criteria:**
- [ ] Le tableau de crates de `README.md:141-150` est remplacé par un lien vers `docs/crate-graph.md`, le diagramme ASCII d'orientation restant en place.
- [ ] Le tableau de `docs/ARCHITECTURE.md:64-74` conserve sa seule colonne non dérivable, « Dépendances interdites », qui énonce un invariant et non un fait, et renvoie au graphe généré pour la liste et les arêtes.
- [ ] Le diagramme ASCII d'`ARCHITECTURE.md:88-105` est explicitement qualifié de simplification éditoriale et renvoie au graphe exhaustif ; il n'est pas supprimé, ses annotations n'étant pas dérivables.
- [ ] Given un lecteur suivant le lien depuis l'un des deux documents, when la porte de liens d'`agent-doc-gates` s'exécute, then elle résout la cible : `crates/agent-doc-gates/src/links.rs:25-42` lit déjà les `.md` de la racine et tout `docs/`.
- [ ] Given un lien mort vers le graphe généré, when la porte de liens s'exécute, then elle échoue en nommant le document source, la ligne et la cible. Les ancrages restent hors de sa portée : `crates/agent-doc-gates/src/links.rs:12-13` les laisse tomber par décision écrite, prouvée par `a_link_to_an_anchor_of_an_existing_file_is_checked_on_the_file_alone`, et aucun lien inter-fichiers du dépôt n'en porte.
- [ ] `AGENTS.md` gagne une ligne « Graphe de crates » dans sa table « Where to read more ».
- [ ] Given le graphe régénéré après l'ajout d'un crate, when le diff est lu, then aucun tableau du `README.md` ni d'`ARCHITECTURE.md` n'a besoin d'être touché.

---

### EP-032: Le catalogue d'outils et l'invariant de souillure rendu inspectable

Le catalogue qui porte le bénéfice non évident du lot : les dix désarmements de `returns_untrusted` deviennent une population lisible plutôt que dix décisions locales invisibles. C'est aussi le plus coûteux, parce que la donnée n'a aujourd'hui aucune sortie publique.

**Definition of Done:** `docs/tool-catalog.md` liste tous les outils que le binaire enregistre avec leurs propriétés de politique, il est prouvé frais, et un outil ajouté à `main.rs` sans entrer au catalogue fait échouer la suite.

#### US-096: Les métadonnées de politique des outils sortent du registre
**Description:** As a relecteur de sécurité, I want une API qui énumère les `DynTool` avec leurs propriétés so that le catalogue puisse être dérivé au lieu d'être recopié.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] `crates/agent-tools/src/registry.rs` expose une entrée par outil portant au minimum : `name`, `description` non tronquée, `input_schema`, `kind`, `is_concurrency_safe`, `is_read_only`, `is_sensitive`, `is_taint_sensitive`, `returns_untrusted`, `is_deferrable`, `namespace`.
- [ ] La description exposée n'est PAS tronquée à `MAX_DESCRIPTION` : la troncature de `registry.rs:1250` sert le fil, pas la documentation, et le catalogue doit montrer ce que le modèle reçoit.
- [ ] `timeout` et `loop_guard_exempt`, qui dépendent respectivement d'un `ToolCtx` et d'une entrée, sont soit exclus de l'entrée, soit accompagnés du contexte de référence sous lequel ils sont évalués. Le choix est argumenté en commentaire.
- [ ] L'API n'expose aucune capacité d'appel : elle lit des métadonnées et ne permet pas d'invoquer un outil.
- [ ] Given un `Registry` vide, when l'énumération est appelée, then elle rend une collection vide sans paniquer.
- [ ] L'ordre rendu est déterministe et explicite, jamais l'ordre d'itération de la `HashMap` de `registry.rs:153`.
- [ ] Aucune signature publique existante n'est modifiée : `tool_specs()` et `ToolSpec` restent ce qu'ils sont, le fil du modèle n'ayant pas à changer pour un besoin documentaire.

#### US-097: Le catalogue d'outils est rendu depuis les outils que le binaire enregistre
**Description:** As a agent de codage, I want un `docs/tool-catalog.md` construit depuis le câblage réel so that le document ne décrive pas un jeu d'outils qui n'existe dans aucune session.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-096

**Acceptance Criteria:**
- [ ] Un manifeste déclaré dans `crates/agent-cli/src/` nomme chaque outil que `main.rs:1812-1877` enregistre et sait l'instancier, y compris ceux qui exigent un handle de runtime, de ressources MCP ou de Code Mode.
- [ ] Une fonction pure rend le fichier entier comme une `String` depuis les entrées collectées, en-tête compris.
- [ ] Les deux premières lignes sont un commentaire HTML nommant le générateur et la recette de régénération.
- [ ] Le fichier porte un tableau de synthèse une ligne par outil, avec au minimum : nom, espace de noms, `kind`, lecture seule, sûreté de concurrence, sensible, sensible à la souillure, rend de la sortie non fiable, différable ; puis une section par outil avec sa description intégrale et son schéma d'entrée en JSON.
- [ ] Le document déclare la configuration sous laquelle il est rendu (mode de permission, mode de bac à sable, capacités du fournisseur, présence de Code Mode), sur le modèle de `/home/arthur/dev/deepseek-harness/docs/tool-catalog.md:8-10` chez dsh, faute de quoi les colonnes conditionnelles seraient invérifiables.
- [ ] Given un outil qu'un handle substitué empêche d'instancier, when le rendu s'exécute, then il échoue en nommant l'outil, plutôt que de l'omettre.
- [ ] Given deux exécutions consécutives, when les sorties sont comparées, then elles sont identiques octet pour octet.
- [ ] Le générateur vit sous `#[cfg(test)]` dans `crates/agent-cli/src/`, `agent-cli` n'ayant pas de cible `[lib]` que `crates/agent-cli/tests/` pourrait importer.
- [ ] Given un nom d'outil, un espace de noms ou une description contenant un caractère significatif en Markdown (`|`, backtick, saut de ligne), when la cellule est rendue, then elle est échappée et le tableau reste valide.

#### US-098: Le manifeste des outils est confronté au câblage et à sa récolte
**Description:** As a mainteneur, I want qu'un outil ajouté au binaire mais absent du manifeste fasse échouer la suite so that le catalogue ne documente pas seulement ce qu'on a pensé à y mettre.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-097

**Acceptance Criteria:**
- [ ] Un garde de complétude extrait les invocations `.register(` et `.register_dyn(` de `crates/agent-cli/src/main.rs` et échoue en nommant chaque outil enregistré qui n'est pas dans le manifeste, sur le modèle de `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:636-645`.
- [ ] Le garde échoue aussi dans l'autre sens : une entrée du manifeste qui ne correspond à aucun site d'enregistrement est nommée.
- [ ] Un garde de récolte échoue quand une entrée du manifeste produit zéro `DynTool`, avec un message disant quoi comparer, sur le modèle de `/home/arthur/dev/deepseek-harness/scripts/gen-tool-catalog.ts:662-670`. Le garde de complétude ne voit pas ce cas.
- [ ] Les outils MCP dynamiques, dont le nombre dépend des serveurs connectés, sont explicitement hors périmètre du catalogue, et le document le dit en une phrase plutôt que de les omettre en silence.
- [ ] Given un site d'enregistrement conditionnel (Code Mode, multi-agents), when le garde s'exécute, then il le compte comme un outil enregistré et le catalogue déclare la condition.
- [ ] Given `main.rs` illisible ou vide, when le garde s'exécute, then il échoue plutôt que de valider un manifeste face à zéro site.
- [ ] Chaque section du catalogue est assertée non vide : une comparaison octet à octet seule est aveugle à une source qui a rendu zéro élément, et c'est le mode d'échec que les scripts de vérification de Kubernetes ont dû combler après coup.
- [ ] Le garde lit du texte et ne compile rien : il rend le même verdict sur toute machine.

#### US-099: La fraîcheur du catalogue d'outils est une assertion
**Description:** As a relecteur de sécurité, I want que le diff d'une pull request contienne la ligne de catalogue qu'elle change so that un désarmement de `returns_untrusted` se voie à côté des dix autres.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-098

**Acceptance Criteria:**
- [ ] Un test compare `docs/tool-catalog.md` au rendu, octet pour octet, et `PYXIS_UPDATE_CATALOGS` bascule l'écriture.
- [ ] Given le fichier absent, when le test s'exécute sans la variable, then il échoue comme sur un fichier périmé.
- [ ] Given un fichier périmé, when le test échoue, then le message nomme le chemin et la commande de régénération.
- [ ] Given un outil dont `returns_untrusted` passe de `true` à `false`, when le catalogue est régénéré, then la ligne de ce seul outil change et le diff la montre.
- [ ] Le tableau de synthèse rend visible en une lecture le compte d'outils rendant de la sortie non fiable et celui des outils qui ne le font pas.
- [ ] Aucun des trois catalogues n'est marqué `linguist-generated` dans `.gitattributes` : GitHub replierait leur diff, or ce diff est l'artefact de revue que le lot existe pour produire.
- [ ] Le test ne lance aucun processus et n'ouvre aucun socket.

---

### EP-033: Le catalogue de configuration et le triplet déclaré

La clé, son drapeau et sa variable d'environnement n'existent aujourd'hui reliés nulle part : `main.rs:543-614` les recâble appel par appel. L'epic les déclare en un endroit, en dérive le catalogue, et corrige la dérive de sécurité mesurée dans le `README.md`.

**Definition of Done:** `docs/config-catalog.md` couvre les quinze clés avec leur couche, leur précédence, leur drapeau, leur variable d'environnement, leur défaut et leur caractère de sécurité ; il est prouvé frais ; et aucune clé acceptée par `settings.rs` ne peut en manquer.

#### US-100: Le triplet clé, drapeau, variable d'environnement est déclaré une fois
**Description:** As a agent de codage, I want une table qui relie chaque clé à son drapeau et à sa variable so that le lien cesse d'exister uniquement sous forme de cinq appels dans `main.rs`.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Une table déclarative dans `crates/agent-cli/src/` porte, pour chacune des quinze clés de `KNOWN_KEYS` : la clé, son type, son défaut, sa couche la plus basse admise, son drapeau de ligne de commande s'il existe, sa variable d'environnement si elle existe, et son caractère de sécurité.
- [ ] La table est la source du catalogue et ne duplique pas `KNOWN_KEYS` ni `SECURITY_KEYS` : elle les référence, et US-101 prouve qu'elle les couvre.
- [ ] Given une clé sans drapeau et sans variable, when sa ligne est rendue, then les deux cellules portent un marqueur d'absence explicite et non une cellule vide.
- [ ] `ConfigLayer::precedence()` et `label()` sont lus, jamais recopiés : leur `match` exhaustif reste la source des cinq couches.
- [ ] Aucun comportement du binaire ne change : la table décrit le câblage existant et ne le remplace pas dans ce lot.
- [ ] Given une couche ajoutée à `ConfigLayer`, when le projet compile, then le `match` exhaustif la force à déclarer sa précédence, et le catalogue la rend sans modification du générateur.

#### US-101: Aucune clé acceptée par le binaire ne peut manquer au catalogue
**Description:** As a contributeur externe, I want que la porte échoue sur une clé documentée nulle part so that le catalogue ne puisse pas documenter moins que ce que le loader accepte.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-100

**Acceptance Criteria:**
- [ ] Un garde échoue en nommant toute clé de `KNOWN_KEYS` absente de la table déclarative, reprenant l'intention de `/home/arthur/dev/deepseek-harness/scripts/gen-config-catalog.ts:756-762` : une clé acceptée mais non cataloguée cacherait un champ que le loader honore.
- [ ] Le garde échoue aussi sur toute clé de `SECURITY_KEYS` que la table ne marque pas comme telle, et sur toute clé de la table absente de `KNOWN_KEYS`.
- [ ] Toute variable `PYXIS_*` lue par le binaire est soit rattachée à une clé du catalogue, soit explicitement classée hors configuration (journalisation, chemins, tests, parité), sur le modèle de la classification obligatoire de `/home/arthur/dev/deepseek-harness/scripts/gen-config-catalog.ts:36`. Une variable non classée échoue.
- [ ] Given une clé ajoutée à `KNOWN_KEYS` sans ligne de table, when la suite s'exécute, then elle échoue en nommant la clé.
- [ ] Given une variable `PYXIS_*` ajoutée au binaire sans classement, when la suite s'exécute, then elle échoue en nommant la variable.
- [ ] Le garde lit du texte et des constantes, ne lance aucun processus et ne lit aucune variable d'environnement hors le commutateur d'écriture.

#### US-102: Le catalogue de configuration est rendu et prouvé frais
**Description:** As a contributeur externe, I want un `docs/config-catalog.md` exhaustif par construction so that je n'aie plus à lire `settings.rs` pour connaître les clés existantes.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-101

**Acceptance Criteria:**
- [ ] Une fonction pure rend le fichier entier comme une `String`, en-tête compris, depuis la table déclarative et les constantes de `settings.rs`.
- [ ] Les deux premières lignes sont un commentaire HTML nommant le générateur et la recette de régénération.
- [ ] Le fichier porte le tableau des cinq couches avec leur précédence lue depuis `ConfigLayer::precedence()`, puis un tableau une ligne par clé : clé, type, défaut, couche la plus basse admise, drapeau, variable d'environnement, clé de sécurité.
- [ ] Le document énonce en prose la règle que `settings.rs:445` et `:491` appliquent : une clé de sécurité est refusée depuis un fichier d'espace de travail et depuis `-c`, un argument pouvant venir d'un script du dépôt.
- [ ] Un test compare le fichier au rendu octet pour octet, et `PYXIS_UPDATE_CATALOGS` bascule l'écriture.
- [ ] Given le fichier absent, when le test s'exécute sans la variable, then il échoue comme sur un fichier périmé.
- [ ] Given deux exécutions consécutives, when les sorties sont comparées, then elles sont identiques octet pour octet.

#### US-103: La dérive de sécurité mesurée dans le README est fermée
**Description:** As a contributeur externe, I want que le `README.md` cesse d'annoncer cinq clés de sécurité quand le code en refuse sept so that un refus ne me surprenne pas.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-102

**Acceptance Criteria:**
- [ ] La phrase de `README.md:277` ne nomme plus une liste de clés de sécurité : elle énonce la règle et renvoie au catalogue, qui porte la liste dérivée.
- [ ] La ligne « environnement | 25 | `PYXIS_*` variables » du tableau de `README.md:274` cesse de promettre une variable par clé et renvoie au catalogue pour les cinq clés qui en ont une.
- [ ] La ligne `~/.pyxis/settings.toml` du tableau de `README.md:258` cesse d'énumérer partiellement les clés et renvoie au catalogue.
- [ ] Given une phrase du `README.md` qui réintroduirait une liste de clés à la main, when la relecture du diff a lieu, then elle est refusée : la seule liste publiée est celle que le catalogue dérive.
- [ ] `AGENTS.md` gagne une ligne « Catalogue de configuration » dans sa table « Where to read more ».
- [ ] Given la porte de liens d'`agent-doc-gates`, when elle s'exécute, then les liens ajoutés au `README.md` résolvent.
- [ ] Given une sixième clé de sécurité ajoutée plus tard, when le catalogue est régénéré, then aucune phrase du `README.md` n'a besoin d'être touchée.

---

### EP-034: L'intégration et la preuve du critère de succès

Les trois catalogues n'ont de valeur que régénérables par une commande nommée et cités par les documents d'entrée. L'epic ferme la boucle et mesure le critère de succès de la colonne « Signal de vérification » du plan de portage.

**Definition of Done:** `just regen` régénère les trois catalogues, aucune recette de vérification ne les écrit, `AGENTS.md` les nomme, et une note de décision consigne la mesure avant et après.

#### US-104: Les trois régénérations entrent dans le niveau qui écrit
**Description:** As a mainteneur, I want régénérer les trois catalogues par `just regen` so that l'écriture reste dans la seule recette qui l'assume.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-094, US-099, US-102

**Acceptance Criteria:**
- [ ] `just regen` porte les trois lignes de régénération, à la suite des trois existantes de `justfile:60-63`.
- [ ] Le commentaire de documentation de `regen` reste exact : il annonce désormais schémas, instantanés, matrice de parité et catalogues.
- [ ] Aucune recette de vérification (`fmt`, `lint`, `build-tests`, `test`, `check`, `parity`, `drift`, `check-local`) ne met `PYXIS_UPDATE_CATALOGS`.
- [ ] Given `just check` lancé sur un arbre propre, when il termine, then `git status --porcelain docs/` est vide.
- [ ] Given `just regen` lancé sur un arbre à jour, when il termine, then `git diff` est vide : la régénération est idempotente.
- [ ] La porte de non-dérive `justfile` / `ci.yml` d'`agent-doc-gates` reste verte : `regen` n'est pas une étape du CI et ne porte pas de marqueur `# ci-step:`.
- [ ] Given le CI, when il s'exécute, then il ne gagne aucune étape : les trois portes vivent dans l'étape `Tests` existante.

#### US-105: Les documents d'entrée nomment les catalogues
**Description:** As a agent de codage, I want qu'`AGENTS.md` dise quels documents sont générés et par quelle commande so that je ne modifie pas à la main un fichier qu'un test réécrit.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-104

**Acceptance Criteria:**
- [ ] La table « Targeted verification signals » d'`AGENTS.md:56-63` gagne une ligne par catalogue, chacune nommant sa commande de régénération et disant qu'elle est une ligne de `just regen`.
- [ ] La table « Where to read more » d'`AGENTS.md:154-163` gagne les trois documents.
- [ ] La section « Source of truth » d'`AGENTS.md:130-132` étend sa mention des artefacts générés et jamais édités à la main aux trois catalogues.
- [ ] Given la porte de prose d'`agent-doc-gates`, when elle lit `AGENTS.md`, then elle reste verte : `PROSE_DOCUMENTS` est fermé à `AGENTS.md` et `CONTRIBUTING.md`, et les commandes ajoutées ne partagent leur tête de trois jetons avec aucune porte du `justfile`.
- [ ] Given une commande de régénération ajoutée à `AGENTS.md` dont la tête de trois jetons entre en collision avec une porte du `justfile`, when `cargo test -p agent-doc-gates` s'exécute, then il échoue en nommant la ligne.
- [ ] `CONTRIBUTING.md` dit en une phrase qu'un document généré se régénère et ne s'édite pas.
- [ ] Given la porte de liens, when elle s'exécute, then tous les liens ajoutés résolvent.

#### US-106: La note de décision consigne la mesure
**Description:** As a mainteneur, I want une note qui dise ce que les catalogues ont fermé, chiffres à l'appui so that la valeur du lot soit constatable et non postulée.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-105

**Acceptance Criteria:**
- [ ] Une note sous `docs/notes/implemented/process/` suit le format que `docs/notes/README.md` impose et que `cargo test -p agent-doc-gates` vérifie.
- [ ] La note porte la mesure avant et après pour les quatre métriques du problème : crates absents des tableaux publiés, clés sans documentation normative, écart entre les cinq clés de sécurité annoncées et les sept refusées, sites de désarmement de `returns_untrusted` listés nulle part.
- [ ] La note argumente les trois décisions de conception que le plan de portage n'avait pas anticipées : la répartition des générateurs entre `agent-doc-gates` et `agent-cli`, le manifeste d'outils gardé plutôt que `default_registry`, et le passage du rôle des crates dans les `Cargo.toml`.
- [ ] La note dit ce qui a été écarté et pourquoi : `gen-doc-graphs.ts` comme source, l'empreinte SHA-256 du modèle `agent-parity`, le diff de première ligne divergente, les outils MCP dynamiques.
- [ ] La note ne devient pas un ADR : rien dans `crates/` ne peut violer « un document de structure est dérivé », qui est une pratique et non un invariant de code. Le critère est celui d'`AGENTS.md:121-128`.
- [ ] Given `cargo test -p agent-doc-gates`, when il s'exécute, then l'arbre de notes et le format de la note passent.

## Functional Requirements

- FR-01: Le dépôt doit publier `docs/crate-graph.md`, `docs/tool-catalog.md` et `docs/config-catalog.md`, chacun rendu par une fonction pure de son état collecté vers une `String`.
- FR-02: Chaque catalogue doit porter, dans ses deux premières lignes, un commentaire nommant son générateur et sa commande de régénération.
- FR-03: Chaque catalogue doit être comparé octet pour octet à son rendu par un test que `cargo test --workspace` exécute.
- FR-04: `PYXIS_UPDATE_CATALOGS` doit basculer les trois tests de la comparaison vers l'écriture, sans autre changement de code.
- FR-05: Un fichier de catalogue absent doit être traité comme périmé, le remède étant identique.
- FR-06: Chaque catalogue doit être accompagné d'un garde de complétude qui le confronte à sa source de vérité : `crates/*/Cargo.toml` pour le graphe, les sites `.register(` de `main.rs` pour les outils, `KNOWN_KEYS` et `SECURITY_KEYS` pour la configuration.
- FR-07: Le manifeste d'outils doit échouer quand une de ses entrées ne produit aucun `DynTool`.
- FR-08: Le système ne doit ajouter aucune étape à `.github/workflows/ci.yml`.
- FR-09: Aucune recette de vérification du `justfile` ne doit écrire dans `docs/`.
- FR-10: Le catalogue d'outils doit déclarer la configuration sous laquelle il a été rendu.
- FR-11: Un cas qu'un générateur ne sait pas traiter doit faire échouer le rendu en nommant la source, et ne jamais produire une cellule vide.
- FR-12: Le système ne doit modifier aucun comportement d'exécution du binaire `pyxis`.

## Non-Functional Requirements

- **Déterminisme:** dix exécutions consécutives d'un même générateur sur un même arbre produisent des octets identiques. Aucun `HashMap` n'atteint un rendu, aucun horodatage, aucun chemin absolu, aucune dépendance à la locale ou au fuseau.
- **Performance:** les trois portes ajoutent au plus 3 s de temps mur à `just test` sur cache chaud, mesurées et consignées dans la note d'US-106. Référence : la suite complète mesurait 19 s à la clôture du lot #5.
- **Encodage:** UTF-8 sans BOM, fins de ligne LF, une fin de ligne finale, sur les trois fichiers, pour que la comparaison octet à octet ne dépende pas de la plateforme.
- **Taille:** chaque catalogue reste sous 500 Ko, borne au-delà de laquelle le diff d'une régénération cesse d'être relisible en revue. Le catalogue d'outils, qui porte des schémas JSON, est le seul qui puisse s'en approcher : dsh mesure 2 221 lignes pour un jeu plus large.
- **Hermétisme:** zéro processus lancé, zéro socket ouvert, zéro accès réseau, zéro lecture de `$PYXIS_CODEX_BASELINE`, une seule variable d'environnement lue (`PYXIS_UPDATE_CATALOGS`). Un runner sans `just`, sans clone Codex et sans réseau rend le même verdict.
- **Dépendances:** zéro dépendance ajoutée au workspace. `cargo tree -p agent-doc-gates` ne montre toujours aucun autre crate `agent-*`.
- **Sécurité:** les catalogues ne publient aucun secret, aucun chemin de machine et aucune valeur de configuration effective. Ils décrivent des schémas, des noms et des défauts, tous déjà présents dans les sources publiées sous GPL-3.0-or-later.
- **Lisibilité du diff:** un changement d'une propriété d'un outil produit un diff d'au plus trois lignes dans `docs/tool-catalog.md`, ce qui conditionne le rendu une ligne par outil dans le tableau de synthèse.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Catalogue absent | Première génération, ou fichier supprimé | Traité comme périmé, jamais comme vide | « `docs/<fichier>` is stale; regenerate with `PYXIS_UPDATE_CATALOGS=1 …` » |
| 2 | Crate ajouté hors du graphe | Un `crates/<nom>/Cargo.toml` neuf | Le garde de complétude échoue en nommant le crate | « `<crate>` a un `Cargo.toml` et n'apparaît pas dans le graphe rendu » |
| 3 | Crate sans `description` | Manifeste créé sans le champ | Le rendu échoue avant la comparaison | « `crates/<nom>/Cargo.toml` : `description` absente ; le rôle du crate vit dans son manifeste » |
| 4 | Outil enregistré hors manifeste | Un `.register(` ajouté à `main.rs` | Le garde de complétude échoue en nommant l'outil | « `<outil>` est enregistré par `main.rs` et absent du manifeste de catalogue » |
| 5 | Entrée de manifeste stérile | Un constructeur qui ne produit aucun `DynTool` | Le garde de récolte échoue, distinct du cas 4 | « `<entrée>` n'a produit aucun outil ; comparer son constructeur au site d'enregistrement » |
| 6 | Clé acceptée non cataloguée | Une clé ajoutée à `KNOWN_KEYS` | Le garde échoue en nommant la clé | « `<clé>` est acceptée par `settings.rs` et absente de la table du catalogue » |
| 7 | Variable d'environnement non classée | Une `PYXIS_*` neuve dans le binaire | Le garde échoue : il n'y a pas de silence | « `<variable>` n'est ni rattachée à une clé ni classée hors configuration » |
| 8 | Crate sans arête interne | `agent-tokenizer`, `agent-doc-gates` | Nœud rendu, cellule « Dépend de » à marqueur d'absence | - |
| 9 | Caractère Markdown dans une donnée | Une description d'outil contenant `|` ou un saut de ligne | La cellule est échappée, le tableau reste valide | - |
| 10 | Propriété dépendant d'un contexte | `timeout(ctx)`, `loop_guard_exempt(raw)` | Exclue, ou rendue avec son contexte de référence déclaré | - |
| 11 | Régénération concurrente sur deux branches | Deux pull requests touchant chacune un outil | Conflit de fusion sur le catalogue, résolu par une régénération | « regénérer plutôt que résoudre à la main » (`CONTRIBUTING.md`, US-105) |
| 12 | `PYXIS_UPDATE_CATALOGS` défini pendant une vérification | Variable exportée dans le shell du contributeur | Le test écrit au lieu de comparer, donc passe ; `git status` le révèle | La porte d'US-104 sur `git status --porcelain docs/` est le filet |
| 13 | Outils MCP dynamiques | Un serveur MCP connecté en session | Hors périmètre, dit en une phrase dans le document | - |
| 14 | `crates/` vide ou illisible | Arbre tronqué, permissions | Le garde échoue plutôt que de valider un graphe vide | « `crates/` illisible ou vide » |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Le manifeste d'outils devient un second câblage à maintenir, qui diverge de `main.rs` | High | High | C'est le risque central du lot, et le garde d'US-098 est sa seule réponse. Il croise dans les deux sens : outil enregistré hors manifeste, entrée de manifeste sans site d'enregistrement. Sans ce garde bidirectionnel, l'epic ne vaut pas d'être livré. |
| 2 | Un outil exigeant un handle de runtime ne peut pas être instancié hors session | Med | High | US-096 le traite en amont ; US-097 impose l'échec nommé plutôt que l'omission. Repli : la ligne de l'outil déclare la substitution employée, ce qui rend la limite lisible dans le document lui-même. |
| 3 | Le catalogue d'outils grossit au point que son diff n'est plus relu | Med | Med | Rendu une ligne par outil dans le tableau de synthèse (NFR de lisibilité), borne de 500 Ko, et sections détaillées séparées du tableau pour qu'un changement de propriété ne déplace pas les schémas. |
| 4 | Le rendu se révèle non déterministe en CI et non en local | Low | High | `HashMap` interdit dans un chemin de rendu, ordre explicite exigé par US-093, US-097 et US-102, et critère de dix exécutions identiques. Le précédent `schema.rs:79-94` montre que le déterminisme se construit. |
| 5 | Le parseur TOML écrit à la main casse sur une forme légitime | Med | Med | Le périmètre est fermé : les seize manifestes du dépôt, qui emploient tous la forme héritée `agent-*.workspace = true`, celle-là même sur laquelle l'analyse à la main casse quand elle n'est pas prévue. Un cas non couvert échoue en nommant fichier et ligne (US-093), il ne rend pas une cellule vide. Précédent : le parseur du `justfile` d'`agent-doc-gates`. |
| 6 | `description` dans les `Cargo.toml` a un effet de construction inattendu | Low | Low | `publish = false` sur les seize crates ; validé au premier `cargo build --workspace` d'US-092. |
| 7 | Le lot dépasse son estimation et empiète sur les lots #7 et #8 | Med | Med | Les epics sont ordonnés par coût croissant et EP-031 éprouve la mécanique complète sur la source triviale. Un arrêt après EP-031 laisse un lot cohérent et une dérive fermée sur trois. |
| 8 | La prose française et les données anglaises se mélangent mal dans un même tableau | Med | Low | Règle posée en contrainte dure : en-têtes et cadrage en français, données citées du code en anglais verbatim. Traduire une `description()` en ferait une copie divergente. |
| 9 | Le catalogue d'outils publie la cartographie des outils qui désarment la souillure | Low | Low | Les sources sont publiées sous GPL-3.0-or-later : l'information est déjà lisible, seulement dispersée, et la littérature sur la surface d'attaque agentique (OWASP AI Agent Security) porte sur des métadonnées d'outils que le dépôt expose déjà. Le vrai risque est l'inverse : un catalogue périmé lu comme une documentation de sûreté qui fait autorité, ce que la porte de fraîcheur est exactement là pour empêcher. |

## Non-Goals

- **Générer les six documents de dsh.** Le graphe de coutures de capacité trace pour chacune des soixante-dix environ le propriétaire, le fournisseur et les consommateurs. Pyxis n'a pas d'équivalent de ce concept, et l'inventer pour porter le générateur serait porter la solution avant le problème.
- **Générer `docs/ARCHITECTURE.md`, `docs/DECISIONS.md` ou `docs/EVENT_SCHEMA.md`.** Ces documents énoncent des invariants et des décisions, pas une structure. Un invariant est une intention et ne se dérive pas du code qui le respecte aujourd'hui.
- **Cataloguer les outils MCP dynamiques.** Leur nombre dépend des serveurs connectés en session ; un catalogue ne peut en publier qu'un instantané trompeur. Le document dit qu'ils sont hors périmètre.
- **Remplacer les diagrammes ASCII du `README.md` et d'`ARCHITECTURE.md`.** Ils portent des annotations éditoriales non dérivables. Ils sont requalifiés, pas supprimés.
- **Fingerprinter les catalogues.** L'empreinte SHA-256 d'`agent-parity` existe parce que sa matrice est consommée comme donnée. Pour trois documents Markdown, la comparaison octet à octet couvre déjà l'édition à la main.
- **Refactorer le câblage de configuration de `main.rs`.** La table d'US-100 décrit le triplet, elle ne le remplace pas comme mécanisme d'exécution. Substituer la table au câblage est un changement de comportement, hors du périmètre d'un lot documentaire.
- **Étendre le catalogue à `spikes/`.** Espace jetable et exclu du workspace.
- **Publier les catalogues ailleurs que dans le dépôt.** Aucun site de documentation, aucune projection : le dépôt est le système de référence.

## Files NOT to Modify

- `crates/agent-app-server/src/schema.rs` et `crates/agent-app-server/tests/schemas.rs` : le modèle du lot, pas sa cible. Les trois portes le copient dans leur forme, elles ne le touchent pas.
- `docs/app-server/protocol.schema.json` et `docs/app-server/protocol.d.ts` : générés par un autre générateur, avec leur propre commutateur.
- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés, fingerprintés, jamais édités à la main, et gouvernés par deux pins indépendants.
- `crates/agent-parity/src/lib.rs` et `crates/agent-parity/src/client_model.rs` : déplacer un `BASELINE_COMMIT` est une décision explicite, étrangère à ce lot.
- Le clone résolu par `$PYXIS_CODEX_BASELINE` : lecture seule, jamais de commit, checkout, fetch ni écriture.
- `.github/workflows/ci.yml` : les trois portes sont des tests ; l'étape `Tests` les couvre déjà, et la porte de non-dérive du lot #5 sanctionnerait une divergence.
- `spikes/` : espace Phase 0 jetable et exclu.
- `crates/agent-tools/src/registry.rs` dans ses signatures existantes : `tool_specs()` et `ToolSpec` servent le fil du modèle ; US-096 ajoute, ne remplace pas.
- `/home/arthur/dev/deepseek-harness` : se lit, ne s'écrit pas, ne se copie pas.

## Technical Considerations

- **Répartition des générateurs:** recommandé, `agent-doc-gates` pour le graphe de crates (texte pur, zéro dépendance, précédent du `justfile`) et `crates/agent-cli/src/` sous `#[cfg(test)]` pour les deux autres, `agent-cli` n'ayant qu'une cible `[[bin]]`. L'alternative, ouvrir une cible `[lib]` sur `agent-cli` pour que `crates/agent-cli/tests/` puisse importer, expose la surface interne du binaire pour un besoin documentaire : à trancher en revue d'US-097 si la forme `#[cfg(test)]` s'avère étroite.
- **Accès aux `DynTool`:** recommandé, une structure d'entrée en lecture seule construite depuis le registre, sans capacité d'appel. L'alternative, rendre `tools_read()` public, expose le verrou et les `Arc` : plus large que le besoin.
- **Format du graphe:** recommandé, Mermaid plus un tableau. Le dépôt n'utilise Mermaid nulle part et emploie des boîtes ASCII, mais générer une disposition ASCII déterministe est un problème de mise en page, pas de rendu. À confirmer que le rendu GitHub du dépôt suffit, aucun site de documentation n'étant prévu.
- **Nom du commutateur d'écriture:** recommandé, `PYXIS_UPDATE_CATALOGS`, distinct de `PYXIS_UPDATE_SCHEMAS`. Réutiliser l'existant éviterait un concept mais nommerait mal un graphe de crates. À réexaminer si un troisième commutateur apparaît : trois variables pour une même sémantique appelleraient une unification.
- **Granularité des tests:** recommandé, un test par catalogue plutôt qu'un test qui boucle sur trois, pour qu'un échec nomme le catalogue dans le nom du test. `schemas.rs` boucle sur deux fichiers d'un même générateur, ce qui n'est pas le cas ici.
- **Emplacement des fichiers:** recommandé, la racine de `docs/`, en nommage minuscule-tiret comme les documents non normatifs existants. L'alternative, un répertoire `docs/catalogs/` avec son `README.md` comme `docs/parity/` et `docs/app-server/`, ajoute un répertoire et un document pour trois fichiers.
- **Parsing des `Cargo.toml`:** recommandé, un parseur de lignes fermé aux formes du dépôt. `cargo metadata` donnerait le graphe résolu mais suppose de lancer un processus, ce que l'hermétisme interdit ; une crate TOML est interdite par le manifeste d'`agent-doc-gates`.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Catalogues dérivés publiés et prouvés frais | 0 | 3 | Month-1 | `ls docs/*catalog*.md docs/crate-graph.md`, et les trois tests dans `cargo test --workspace` |
| Crates absents du tableau du `README.md` | 6/16 | 0/16 | Month-1 | Comparaison du tableau publié à `ls crates/` |
| Crates absents du tableau d'`ARCHITECTURE.md` | 5/16 | 0/16 | Month-1 | Idem |
| Clés de `KNOWN_KEYS` sans documentation normative | 7/15 | 0/15 | Month-1 | `grep` de chaque clé dans `README.md` et `docs/`, hors `docs/parity/audits/` déclaré non normatif |
| Écart entre clés de sécurité annoncées et refusées | 5 annoncées contre 7 refusées | 0 | Month-1 | Comparaison du `README.md` à `SECURITY_KEYS` |
| Sites désarmant `returns_untrusted` listés dans un document | 0/10 | 10/10 | Month-1 | `grep -c "fn returns_untrusted"` dans `crates/agent-tools/src/`, croisé au catalogue |
| Outils enregistrés par `main.rs` absents du catalogue | non mesurable aujourd'hui | 0 | Month-1 | Le garde d'US-098 est lui-même la mesure |
| Temps mur ajouté à `just test` sur cache chaud | 0 s (référence : 19 s) | ≤ 3 s | Month-1 | Trois mesures avant, trois après, consignées dans la note d'US-106 |
| Dépendances ajoutées au workspace | 0 | 0 | Month-6 | `git diff` sur le `Cargo.toml` du workspace, et `cargo tree -p agent-doc-gates` |
| Documents de structure rédigés à la main dans `docs/` | 3 | 0 | Month-6 | Revue de la table « Where to read more » d'`AGENTS.md` |

## Open Questions

- Faut-il ouvrir une cible `[lib]` sur `agent-cli` ? La forme `#[cfg(test)]` retenue est la moins invasive, mais elle rend le générateur inaccessible depuis un futur sous-commande `pyxis --emit-catalogs`, symétrique de `pyxis app-server --emit-schemas`. À trancher par Arthur en revue d'US-097, avant que la forme ne se fige.
- Les schémas d'entrée doivent-ils être rendus intégralement dans le `docs/tool-catalog.md` de Pyxis ? dsh le fait et paie 2 221 lignes dans `/home/arthur/dev/deepseek-harness/docs/tool-catalog.md`. Un rendu de la liste des propriétés avec leur type coûterait un cinquième et perdrait les contraintes. À trancher en revue d'US-097 ; la borne de 500 Ko est le garde-fou d'ici là.
- La table déclarative d'US-100 doit-elle devenir la source d'exécution du câblage de `main.rs:543-614`, dans un lot ultérieur ? Ce serait le prolongement naturel, et cela transformerait la documentation en source de vérité plutôt qu'en reflet. Hors périmètre ici parce que c'est un changement de comportement.
- Faut-il une quatrième porte comparant le nombre de tests d'une exécution à l'autre, pour détecter un catalogue qui cesserait silencieusement d'être testé ? Question ouverte au-delà de ce lot, applicable aussi aux six portes existantes.
[/PRD]
