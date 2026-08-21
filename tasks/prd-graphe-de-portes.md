[PRD]
# PRD: Graphe de portes

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-21 | Arthur Jean | Rédaction initiale, lot #5 du plan de portage DeepSeek Harness |

## Problem Statement

Pyxis possède six vérifications mécaniques et aucune commande qui les nomme. L'inventaire vit dans `.github/workflows/ci.yml`, en YAML, et rien ne le reproduit ailleurs. Quatre défaillances mesurées sur l'état actuel du dépôt :

1. **Deux portes documentées ne sont exécutées par rien.** `cargo run -p agent-parity -- check` et `cargo run -p agent-parity -- drift` sont prescrites par `AGENTS.md:53-54` et publiées comme recette normative dans `docs/parity/offline-suite.md`. Ni le CI ni `cargo test --workspace` ne les lance. `check` exige le clone Codex épinglé (`crates/agent-parity/src/lib.rs`, erreurs `MissingClone`, `NotAGitRepository`, `CommitMismatch`, `DirtyTracked`) donc est structurellement inexécutable sur un runner GitHub ; `drift` sort non nul par conception quand l'amont a bougé, donc ne peut jamais être une étape bloquante. Les deux sont restées orphelines faute d'un endroit où les déclarer.
2. **La même porte est écrite de trois façons.** `AGENTS.md:45` et `.github/workflows/ci.yml:84` disent `cargo clippy --workspace --all-targets`. `CONTRIBUTING.md:17` dit `cargo clippy --workspace --no-deps`. Un contributeur qui suit `CONTRIBUTING.md` ne lance pas la porte du CI : `--no-deps` ne compile pas les cibles de test, donc un lint dans un `#[cfg(test)]` passe en local et casse en CI.
3. **Rien ne reproduit le CI en local.** Le dépôt n'a ni `justfile`, ni `Makefile`, ni `scripts/`, ni hook git (`.git/hooks` ne contient que les échantillons, `core.hooksPath` n'est pas défini). Un contributeur enchaîne quatre commandes à la main, dans le bon ordre, ou n'en lance aucune. La note `docs/notes/implemented/process/2026-08-20-arbre-de-notes-de-decision.md` a déjà tranché contre les hooks : la porte s'exécute par `cargo test --workspace`, jamais par un hook contournable par `--no-verify`. Cette décision ferme une voie mais n'en ouvre aucune.
4. **Trois régénérations sont dispersées et invisibles.** `PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas`, `cargo insta review` et `cargo run -p agent-parity -- generate` mutent l'arbre de travail. Elles vivent dans deux documents différents (`AGENTS.md`, `docs/parity/README.md`), sans distinction typographique d'avec les commandes de vérification, alors que la différence est catégorique : les unes constatent, les autres modifient.

**Why now:** les lots 6, 7 et 8 du plan de portage ajoutent chacun une vérification. Le lot 6 produit des catalogues comparés octet à octet, le lot 7 une porte sur les README model-facing, le lot 8 un rejeu JSONL. Sans agrégat nommé, chacune se copie en étape de `ci.yml` et rejoint les deux portes de parité déjà orphelines : le taux d'exécution locale reste nul et l'inventaire YAML grossit de trois entrées que personne ne lit. Le coût d'ajouter l'agrégat après les trois lots est de trois migrations au lieu d'une.

## Overview

Le lot crée un `justfile` à la racine : un fichier de recettes nommées qui devient l'inventaire lisible des portes du dépôt. Quatre recettes feuilles (`fmt`, `lint`, `build-tests`, `test`) reprennent exactement les quatre commandes `cargo` du CI, dans le même ordre ; `just check` les compose. La recette par défaut est `just --list`, donc la commande sans argument catalogue les portes au lieu d'en exécuter une.

De `scripts/run-gates.ts` de DeepSeek Harness, une seule idée est reprise, et elle ne demande pas de code : la distinction entre une porte qui doit **réussir** avant la suivante et une porte qui doit seulement avoir **retombé**. `just` porte les deux nativement. Une ligne de recette qui sort non nul avorte la recette, ce qui donne `needs` sans rien écrire : `just check` s'arrête sur `fmt` et ne compile jamais les tests. Le sigil `-` en tête de ligne ignore l'échec et poursuit, ce qui donne l'`allowFailure` dont `agent-parity drift` a besoin, lui qui sort non nul par conception quand l'amont a bougé. L'ordonnanceur, les 14 modes et la validation de graphe de `run-gates.ts` sont écartés : la suite complète mesure 19 secondes sur cache chaud, 1 949 tests sur 74 binaires, et il n'y a rien à paralléliser ni de cycle possible dans une liste de quatre commandes.

Le CI, lui, n'appelle pas `just`. C'est la décision structurante du lot et elle va contre la lecture naïve du plan. `.github/workflows/ci.yml` conserve ses étapes verbatim, avec leurs `timeout` par étape, le `tee` vers `cargo-test.log`, le filtre de flux et le résumé `GITHUB_STEP_SUMMARY`, tous justifiés par un commentaire écrit dans le fichier (lignes 86 à 115, 141 à 142) : un job annulé par `timeout-minutes` n'archive aucun log, donc les étapes doivent échouer d'elles-mêmes et lisiblement. Envelopper cela dans `just check` détruirait ces propriétés ; le remplacer étape par étape ajouterait une installation de `just` sur le runner et un écart de version (Ubuntu 24.04 empaquette `just 1.21.0`, Fedora 44 empaquette `1.57.0`) pour zéro gain, puisque la recette n'ajoute rien à une invocation `cargo` d'une ligne.

Le risque devient donc la dérive entre les deux fichiers, et c'est ce risque qui est traité mécaniquement. Un module dans `agent-doc-gates` extrait la liste ordonnée des invocations `cargo` de chaque côté et un test prouve leur égalité. Le dépôt a déjà deux précédents de document parsé par un test, `crates/agent-parity/tests/offline_suite.rs` qui lit un tableau Markdown et `agent-doc-gates` lui-même qui lit l'arbre de notes ; celui-ci est le troisième, sans nouvelle dépendance, avec un parseur écrit à la main comme le crate l'exige déjà dans son propre `Cargo.toml`. Le critère de succès « `just check` reproduit le CI » cesse d'être une intention et devient une assertion qui échoue rouge.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Portes du dépôt atteignables par une commande nommée | 6/6 | 6/6, plus celles des lots 6 à 8 |
| Portes documentées jamais exécutées | 0 | 0 |
| Formulations divergentes d'une même porte dans les documents prescriptifs | 1 | 1 |
| Dérive `justfile` / `ci.yml` détectée mécaniquement | oui, par `cargo test --workspace` | oui, sans régression |
| Temps mur de `just check` sur cache chaud | mesuré et consigné, ≤ 60 s | ≤ 60 s malgré les lots 6 à 8 |
| Dépendances ajoutées au workspace | 0 | 0 |

## Target Users

### Agent de codage éditant le dépôt
- **Role:** Claude Code, Codex ou tout agent recevant une tâche sur Pyxis avec un contexte vierge.
- **Behaviors:** lit `AGENTS.md` en entrée de session, exécute la plus étroite vérification qu'il croit suffisante, ne relit pas `.github/workflows/ci.yml` avant de livrer.
- **Pain points:** la table « Build and verify » d'`AGENTS.md` liste huit commandes sans dire lesquelles constituent le verdict et lesquelles régénèrent des fichiers ; l'agent choisit au jugé et livre un diff que le CI refuse.
- **Current workaround:** lancer `cargo test --workspace` seul, ce qui rate `fmt` et `clippy`, les deux portes les moins chères.
- **Success looks like:** une commande unique, `just check`, dont l'échec nomme la porte, et un `just --list` qui catalogue le reste sans ouvrir un document.

### Contributeur externe envoyant une pull request
- **Role:** développeur Rust découvrant le dépôt par `CONTRIBUTING.md`.
- **Behaviors:** clone, lance ce que `CONTRIBUTING.md` prescrit, pousse, découvre le verdict dans l'onglet Actions.
- **Pain points:** `CONTRIBUTING.md:17` prescrit `cargo clippy --workspace --no-deps`, qui ne compile pas les cibles de test ; le CI lance `--all-targets`. Le contributeur reçoit un rouge sur une commande qu'il a lancée verte.
- **Current workaround:** aucun. La divergence n'est visible qu'en lisant le YAML.
- **Success looks like:** `CONTRIBUTING.md` nomme une recette, la recette porte la commande, et il n'y a plus deux sources.

### Mainteneur du dépôt
- **Role:** Arthur Jean, seul détenteur du clone Codex épinglé par `$PYXIS_CODEX_BASELINE`.
- **Behaviors:** touche la surface de contrat Codex, doit alors lancer `agent-parity check`, et veut savoir périodiquement si l'amont a bougé.
- **Pain points:** les deux commandes de parité ne sont dans aucun agrégat ; les lancer suppose de se souvenir qu'elles existent. `drift` sort non nul par conception, donc l'enchaîner naïvement casse la séquence.
- **Current workaround:** les taper à la main, quand il y pense.
- **Success looks like:** `just check-local` fait tout ce que `just check` fait, plus la parité, avec `drift` en signal non bloquant.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- **DeepSeek Harness (`scripts/run-gates.ts`, 967 lignes)** : un ordonnanceur en processus, graphe validé avant tout démarrage (graphe vide, identifiant dupliqué, dépendance inconnue, cycle), concurrence bornée, sortie attribuable par porte. Ce qui compte ici est le contrat de l'interface `Gate` : `needs` exige un prédécesseur `passed`, `after` exige seulement qu'il soit retombé, `allowFailure` rend une porte visible sans la rendre bloquante. Le reste ne se transpose pas : `dsh` a 142 scripts et 14 modes nommés, Pyxis a quatre commandes.
- **`lefthook.yml` de `dsh`** : la moitié « hooks » de la source du lot. Elle est écartée d'avance, `docs/notes/implemented/process/2026-08-20-arbre-de-notes-de-decision.md` ayant déjà rejeté une porte en hook comme contournable par `--no-verify`. `dsh` a d'ailleurs lui-même dégonflé ses hooks depuis, sa note `2026-07-22-fast-local-git-hooks.md` concluant que le CI seul porte la couverture exhaustive.
- **`cargo-make`** : lanceur de tâches Rust-natif, dépendances de tâches déclarées en TOML, déduplication des dépendances communes, connaissance de Cargo. Écarté : il s'installe par `cargo install cargo-make`, donc se compile, et `Makefile.toml` est verbeux pour quatre commandes.
- **`cargo-xtask`** : la voie idiomatique Rust. Écartée : plus de code que le problème n'en demande, et un crate `agent-xtask` entrerait dans `crates/*` alors qu'ADR-8 réserve le préfixe aux crates produit.
- **Écart de marché comblé :** aucun de ces outils ne résout la duplication d'inventaire entre le fichier de tâches et le YAML de CI. `dsh` la résout par convention (le YAML appelle un agrégat nommé et ne connaît aucun nom de porte). Ce PRD la résout par un test, ce qui tient même quand le YAML doit garder sa propre logique.

### Best Practices Applied
- `just` avorte une recette dès qu'une ligne sort non nul ; le sigil `-` en tête de ligne poursuit malgré l'échec (`just.systems/man/en/sigils.html`). Les deux sémantiques du lot sont donc natives, sans une ligne de code d'ordonnancement.
- `just` n'est pas préinstallé sur les runners `ubuntu-latest` ; l'installer suppose soit `apt-get install just` (Ubuntu 24.04 universe, version 1.21.0-1), soit un snap, soit une action tierce. `.github/workflows/ci.yml:20-26` prend une position explicite contre les actions tierces à tag mutable. Ne pas rendre le CI dépendant de `just` évite d'avoir à arbitrer.
- Une porte qui saute silencieusement n'est pas une porte : le test de non-dérive lit deux fichiers texte et rend le même verdict sur toute machine, sans binaire externe ni variable d'environnement.

### Sources dsh, ancrées et vérifiées

Racine du dépôt source : `/home/arthur/dev/deepseek-harness`. Les chemins ci-dessous lui sont relatifs et leurs numéros de ligne ont été vérifiés sur disque le 2026-08-21. Aucun fichier ne se copie : `dsh` est en TypeScript sur Cordis, Pyxis est en Rust. Ce qui se reprend est une décision de conception, et la colonne de droite dit ce qu'il advient de chacune.

| Ancre | Ce qui s'y lit | Reprise |
|---|---|---|
| `scripts/run-gates.ts:42-56` | Interface `Gate` : `id`, `label`, `displayCommand`, `command`, `args`, `needs`, `after`, `env`, `allowFailure`, `streamOutput` | `needs` et `allowFailure` repris ; `after`, `env`, `streamOutput` écartés |
| `scripts/run-gates.ts:845-852` | `predecessorsReady` et `gateSettled` : `needs` exige un prédécesseur `passed`, `after` exige seulement qu'il soit `passed`, `failed` ou `skipped` | Sémantique reprise, code non transposé : une ligne de recette `just` qui sort non nul avorte la recette, ce qui donne `needs` gratuitement |
| `scripts/run-gates.ts:106` | Verdict de l'agrégat : une porte `failed` ou `skipped` le fait échouer, sauf `allowFailure === true` | Repris tel quel : c'est le rôle du sigil `-` devant `agent-parity drift` en US-083 |
| `scripts/run-gates.ts:481` | La seule porte du dépôt marquée `allowFailure: true` | Précédent du signal informationnel qui s'affiche sans conditionner le verdict |
| `scripts/run-gates.ts:956` | Le rapport final préfixe `NON-BLOCKING ` la disposition d'une porte non bloquante | Repris en esprit : le commentaire de documentation de la recette dit qu'elle ne bloque pas |
| `scripts/run-gates.ts:718-742` | `validateGateGraph` : graphe vide, identifiant dupliqué, dépendance inconnue, cycle, tous rejetés avant le moindre `spawn` | Écarté : une liste de quatre commandes n'a ni identifiant dupliqué ni cycle possible |
| `scripts/run-gates.ts:743-782` | `findDependencyCycle`, un DFS qui rend le chemin du cycle | Écarté, même raison |
| `scripts/run-gates.ts:783-844` | `runGates` : ordonnanceur, concurrence bornée, chemin `skipped` avec l'erreur `dependency failed or skipped:` en ligne 821 | Écarté : la suite complète mesure 19 s sur cache chaud, il n'y a rien à ordonnancer |
| `scripts/run-gates.ts:142` | `defaultConcurrency` : les modes locaux sont plafonnés parce que chaque porte de documentation construit un `ts.Program` complet | Écarté : sans parallélisme, sans objet |
| `.github/workflows/ci.yml:116, 178, 263, 570, 599` | Le YAML appelle des agrégats nommés (`pnpm run check:ci:static`, `check:ci:coverage`, `check:ci:consumers`, `check:ci`) et ne contient aucun nom de porte | Convention comprise, appliquée à l'envers : Pyxis garde ses étapes pour ne pas perdre ses diagnostics, et prouve l'égalité des inventaires par un test (EP-028) |
| `lefthook.yml:5-50` | Jobs `pre-commit` : appariement des traductions, notes archivées, oxlint sur l'index, régénération des notices avec `git add`, `git diff --cached --check`, manifeste vendor | Écarté : la note du lot 2 a déjà rejeté la porte en hook comme contournable par `--no-verify` |
| `lefthook.yml:52-55` | `pre-push` ne lance que `pnpm run typecheck` : aucune porte du graphe n'est dans le hook | Confirme l'écart : `dsh` lui-même ne met pas ses portes dans ses hooks |
| `.agents/notes/implemented/process/2026-06-11-quality-gates.md` | « Mechanical quality gates over prose guidelines » : un agent suit une porte appliquée bien plus fidèlement qu'une convention en prose | Repris : c'est la justification de fond du lot, et la raison pour laquelle EP-028 est un test et non une consigne |
| `.agents/notes/implemented/process/2026-07-06-parallel-pre-push-gates.md` | Rationale d'origine du graphe et ses alternatives écartées : garder les agrégats en série, un job CI par porte feuille, des sous-commandes en arrière-plan dans le shell, `stdio` hérité par porte | Contexte : trois de ces quatre alternatives sont sans objet à quatre portes, la quatrième (série) est celle que Pyxis retient |
| `.agents/notes/implemented/process/2026-07-22-fast-local-git-hooks.md` | Supersède la partie locale de la note précédente : hooks minimaux, l'agent lance la vérification la plus étroite, le CI porte seul la couverture exhaustive | Repris : aucun hook dans ce lot |
| `.agents/notes/implemented/process/2026-07-22-tsconfig-solution-root-two-aggregates.md` | « One solution root, two check units » ; le `pre-push` ne lançait qu'un des deux agrégats, donc la casse côté client passait le point de contrôle local et ressortait en CI | Précédent direct du double niveau `just check` et `just check-local` : un agrégat unique qui ne couvre pas tout crée exactement cette asymétrie |

*Sources : `just.systems/man/en`, `github.com/casey/just`, `packages.ubuntu.com/just`, `sagiegurari.github.io/cargo-make`, plus lecture directe de `/home/arthur/dev/deepseek-harness/scripts/run-gates.ts` et `lefthook.yml`.*

## Assumptions & Constraints

### Assumptions (to validate)
- Le coût réel du lot dépasse l'estimation « 1 à 2 jours » du plan de portage, la porte de non-dérive n'ayant pas été anticipée par celui-ci. Estimation révisée : 23 points, soit 3 à 4 jours. À valider par le premier epic livré.
- La syntaxe employée dans le `justfile` (recettes, dépendances, sigil `-`, commentaires de documentation) est disponible depuis `just` 1.0. Aucun attribut (`[group]`, `[doc]`, `[script]`), aucun `set unstable`. Hypothèse : les deux versions empaquetées observées, 1.21.0 sur Ubuntu 24.04 et 1.57.0 sur Fedora 44, exécutent le fichier à l'identique. À valider par un lancement sur chaque.
- L'ensemble des invocations `cargo` du CI est stable sur la durée du lot. Si `ci.yml` change pendant l'implémentation, le test de non-dérive le signalera : c'est son rôle.

### Hard Constraints
- Zéro dépendance ajoutée : ni au `Cargo.toml` du workspace, ni à `agent-doc-gates`, dont le `Cargo.toml` argumente déjà l'interdiction et impose un parseur écrit à la main.
- Le clone résolu par `$PYXIS_CODEX_BASELINE` est en lecture seule. Aucune recette ne peut y écrire, y committer, y checkouter ou y fetcher.
- Aucune recette ne lance `PYXIS_LIVE_PARITY=1` : cette variable dépense l'abonnement du mainteneur contre un endpoint réel.
- Les corps de shell des étapes `Tests` et `Report failing tests` de `ci.yml` sont préservés tels quels. Seuls les commentaires peuvent changer.
- La porte de non-dérive n'introduit aucun crate : elle vit dans `agent-doc-gates`, qui n'importe aucun crate Pyxis et n'entre dans le graphe d'aucun binaire.
- Le dépôt source `/home/arthur/dev/deepseek-harness` se lit, ne s'écrit pas et ne se copie pas. Il est en TypeScript sur Cordis ; aucune ligne n'en est transposée. Les ancres exactes sont dans « Sources dsh, ancrées et vérifiées ».
- Le `justfile` est en anglais, comme le code et les commentaires. Ce PRD et la note de décision sont en français, comme les documents de `docs/`.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatage du workspace
- `cargo clippy --workspace --all-targets` - lints, sans `-D warnings` par décision documentée
- `cargo test --workspace --no-fail-fast` - suite complète, nomme tous les tests en échec
- `just --list` - le `justfile` parse et catalogue ses recettes, à partir de US-082
- `just check` - l'agrégat lui-même est vert, à partir de US-082

## Epics & User Stories

### EP-027: Agrégats de portes nommés

Créer le `justfile` racine : les quatre portes du CI en recettes feuilles, l'agrégat qui les compose, le niveau local qui ajoute la parité, et le niveau de régénération tenu à l'écart des deux premiers.

**Definition of Done:** `just --list` catalogue toutes les recettes, `just check` enchaîne les quatre portes du CI dans l'ordre et s'arrête à la première qui échoue, `just check-local` ajoute la parité sans que `drift` puisse rendre le verdict rouge, et aucune recette de vérification ne mute l'arbre de travail.

#### US-082: Les quatre portes du CI en recettes nommées
**Description:** As a agent de codage, I want lancer `just check` so that j'exécute exactement les quatre portes du CI, dans leur ordre, sans lire le YAML.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Un `justfile` existe à la racine et porte quatre recettes feuilles : `fmt` (`cargo fmt --all -- --check`), `lint` (`cargo clippy --workspace --all-targets`), `build-tests` (`cargo test --workspace --no-run`), `test` (`cargo test --workspace --no-fail-fast`).
- [ ] Chaque recette feuille porte un commentaire de documentation d'une ligne, restitué par `just --list`.
- [ ] La recette `check` compose les quatre dans cet ordre exact.
- [ ] La recette par défaut du fichier est l'équivalent de `just --list` : `just` sans argument catalogue et n'exécute aucune porte.
- [ ] Given un fichier Rust mal formaté, when `just check` est lancé, then la commande sort non nul, le message nomme la recette `fmt`, et aucun processus `cargo test` n'est lancé.
- [ ] Given un `justfile` employant un attribut ou `set unstable`, when la revue le constate, then le fichier est corrigé : seule la syntaxe disponible depuis `just` 1.0 est admise.
- [ ] Aucune recette n'écrit dans le dépôt ni ne lit `$PYXIS_CODEX_BASELINE`.

#### US-083: Le niveau local ajoute les deux portes de parité
**Description:** As a mainteneur, I want lancer `just check-local` so that les deux commandes de parité orphelines s'exécutent avec le reste, `drift` sans pouvoir rendre le verdict rouge.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-082

**Acceptance Criteria:**
- [ ] `check-local` dépend de `check` puis lance `cargo run -p agent-parity -- check`.
- [ ] `check-local` lance ensuite `cargo run -p agent-parity -- drift` préfixé du sigil `-`, donc un amont qui a bougé s'affiche sans faire sortir la recette non nul.
- [ ] Given `$PYXIS_CODEX_BASELINE` non défini ou pointant un clone au mauvais commit, when `just check-local` est lancé, then la recette sort non nul en relayant l'erreur typée d'`agent-parity` et n'écrit rien dans le clone.
- [ ] Given `$PYXIS_CODEX_BASELINE` non défini, when `just check` est lancé, then la commande réussit : le niveau CI ne dépend d'aucun artefact local.
- [ ] Le commentaire de documentation de `check-local` dit explicitement qu'elle exige le clone épinglé et qu'elle n'est jamais lancée par le CI.

#### US-084: Les régénérations sont un niveau séparé
**Description:** As a agent de codage, I want une recette distincte pour les commandes qui modifient l'arbre so that je ne confonde jamais constater et régénérer.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-082

**Acceptance Criteria:**
- [ ] Une recette `regen` réunit `PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas`, l'acceptation des instantanés `insta` et `cargo run -p agent-parity -- generate`.
- [ ] `regen` n'est dépendance d'aucune recette de vérification, et aucune recette de vérification ne la lance.
- [ ] Son commentaire de documentation annonce qu'elle écrit dans le dépôt et qu'un `git diff` doit être relu ensuite.
- [ ] Given `regen` vient d'être lancée sur un dépôt propre, when `just check` est lancé, then la commande est verte.
- [ ] Given `agent-parity generate` échoue faute de clone épinglé, when `just regen` est lancée, then la recette sort non nul et les régénérations déjà appliquées restent visibles dans `git status`, sans annulation implicite.

---

### EP-028: Porte de non-dérive entre le justfile et le CI

Prouver mécaniquement que les recettes du `justfile` et les étapes de `.github/workflows/ci.yml` portent les mêmes invocations `cargo`, dans le même ordre. C'est ce qui transforme le critère de succès du lot en assertion.

**Definition of Done:** un test de `cargo test --workspace` échoue dès qu'une porte est ajoutée, retirée, réordonnée ou modifiée d'un côté sans l'autre, et son message nomme le fichier, la porte et la commande attendue.

#### US-085: Extraction des invocations de portes des deux fichiers
**Description:** As a mainteneur, I want un module qui extrait la liste ordonnée des invocations `cargo` du `justfile` et de `ci.yml` so that les deux inventaires deviennent comparables.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-082

**Acceptance Criteria:**
- [ ] Un module `gates` dans `crates/agent-doc-gates/src/` expose une extraction pour chaque fichier, retournant une liste ordonnée de `(nom de porte, argv cargo)`.
- [ ] L'appariement entre une recette et une étape passe par un marqueur explicite porté en commentaire au-dessus de la recette, les noms d'étapes du CI contenant des espaces (`Build tests`).
- [ ] Côté CI, un préfixe `timeout` avec ses options est retiré avant comparaison ; tout autre préfixe inconnu fait échouer l'extraction avec un message qui le nomme, plutôt que d'être ignoré.
- [ ] Le parseur est écrit à la main, sans dépendance YAML ni Markdown, conformément au commentaire du `Cargo.toml` du crate.
- [ ] Given une étape de CI qui ne contient aucune invocation `cargo` (installation de dépendances système, rapport d'échec), when l'extraction tourne, then l'étape est ignorée sans erreur.
- [ ] Given une recette portant un marqueur qui ne correspond à aucune étape du CI, when l'extraction tourne, then elle retourne une erreur nommant le marqueur orphelin.

#### US-086: Le test d'égalité et d'ordre
**Description:** As a agent de codage, I want qu'un test échoue quand le `justfile` et le CI divergent so that l'agrégat ne mente jamais sur ce que le CI exécute.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-085

**Acceptance Criteria:**
- [ ] Un test d'intégration de `agent-doc-gates` compare les deux listes extraites : même cardinalité, mêmes argv, même ordre.
- [ ] Given une porte ajoutée au CI et pas au `justfile`, when le test tourne, then il échoue en nommant l'étape et la commande à ajouter.
- [ ] Given un drapeau modifié d'un seul côté, par exemple `--no-deps` contre `--all-targets`, when le test tourne, then il échoue en affichant les deux argv.
- [ ] Given `fmt` et `lint` intervertis dans le `justfile`, when le test tourne, then il échoue en signalant l'ordre, ce qui est la preuve mécanique que `fmt` coupe avant les tests.
- [ ] Le test rapporte toutes les divergences d'un coup et ne s'arrête pas à la première.
- [ ] Le test ne lance aucun processus, ne lit aucune variable d'environnement, et ne saute jamais : il rend le même verdict sur un runner sans `just` installé.

---

### EP-029: Convergence documentaire

Supprimer les formulations divergentes, faire pointer les documents prescriptifs vers les recettes plutôt que vers des commandes copiées, et enregistrer la décision.

**Definition of Done:** `AGENTS.md` et `CONTRIBUTING.md` nomment des recettes et ne portent plus d'invocation brute de porte, une porte l'empêche de revenir, et une note de décision `process` consigne pourquoi le CI n'appelle pas `just`.

#### US-087: Les documents prescriptifs nomment les recettes
**Description:** As a contributeur externe, I want que `CONTRIBUTING.md` prescrive la même chose que le CI so that ma pull request ne rougisse pas sur une porte que j'ai lancée verte.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-082

**Acceptance Criteria:**
- [ ] `CONTRIBUTING.md:17` ne prescrit plus `cargo clippy --workspace --no-deps` : il nomme `just check`.
- [ ] La table « Build and verify » d'`AGENTS.md` nomme `just check`, `just check-local` et `just regen`, et renvoie à `just --list` pour l'inventaire.
- [ ] La liste des prérequis système d'`AGENTS.md` mentionne `just` à côté de `mold`, `libdbus-1-dev` et `pkg-config`, en précisant qu'il n'est pas requis par le CI.
- [ ] Given un contributeur sans `just` installé, when il suit `CONTRIBUTING.md`, then il trouve dans le même document que `cargo test --workspace` reste une voie complète et suffisante.
- [ ] `cargo test -p agent-doc-gates` reste vert : aucun lien Markdown interne n'est cassé par ces réécritures.

#### US-088: Une porte empêche la divergence de revenir
**Description:** As a mainteneur, I want qu'un test refuse une invocation brute de porte dans les documents prescriptifs so that la divergence corrigée ne se réintroduise pas.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-086, US-087

**Acceptance Criteria:**
- [ ] Un test vérifie qu'aucune des invocations `cargo` extraites du `justfile` n'apparaît littéralement dans `AGENTS.md` ni dans `CONTRIBUTING.md`.
- [ ] La portée est close et écrite : ces deux fichiers seulement. `docs/parity/offline-suite.md` publie une recette normative de trois commandes et n'est pas concerné ; `README.md` montre des transcriptions de session illustratives et n'est pas concerné.
- [ ] Given `cargo clippy --workspace --no-deps` réintroduit dans `CONTRIBUTING.md`, when le test tourne, then il échoue en nommant le fichier, la ligne et la recette à citer à la place.
- [ ] Le message d'échec dit quoi écrire, pas seulement ce qui est interdit.

#### US-089: La décision est enregistrée
**Description:** As a agent de codage futur, I want une note qui dise pourquoi le CI n'appelle pas `just` so that je ne re-litige pas ce choix ni ne le casse par simplification.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-086

**Acceptance Criteria:**
- [ ] Une note existe sous `docs/notes/implemented/process/`, datée, au format que `agent-doc-gates` impose : `# Note: <titre>`, ligne vide, `Statut: implemented`.
- [ ] Elle consigne les alternatives écartées : une étape `run: just check` unique, des étapes `run: just <recette>`, `Makefile`, `cargo-make`, alias `[alias]` avec crate `xtask`, et pour chacune la raison.
- [ ] Elle explique pourquoi la décision est une note et non un ADR : aucun changement dans `crates/` ne peut violer une liste de recettes, seul le test de non-dérive le peut, et il est lui-même la mise en œuvre de la note.
- [ ] Elle relie la note du lot 2 qui a rejeté les hooks, dont ce lot hérite sans la rouvrir.
- [ ] `cargo test -p agent-doc-gates` valide le format et les liens de la note.
- [ ] Given une note dont le statut déclaré contredit son répertoire ou dont une alternative manque, when `cargo test -p agent-doc-gates` tourne, then la porte existante échoue et la note est corrigée avant livraison.

---

### EP-030: Preuve du critère de succès

Mesurer et consigner que l'agrégat tient sa promesse : il reproduit le CI, il coupe au bon endroit, il reste assez rapide pour être lancé.

**Definition of Done:** le comportement d'arrêt anticipé est vérifié sur les deux versions de `just` observées, le temps mur est mesuré et consigné comme budget, et le CI est constaté inchangé.

#### US-090: L'arrêt sur `fmt` et le budget de temps sont mesurés
**Description:** As a mainteneur, I want une mesure consignée de l'arrêt anticipé et du temps mur so that le critère de succès du lot soit prouvé et pas seulement affirmé.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-082

**Acceptance Criteria:**
- [ ] Given un fichier délibérément mal formaté, when `just check` est lancé sur cache chaud, then la commande sort non nul en moins de 5 secondes et `cargo test` n'apparaît dans aucun processus lancé.
- [ ] Le temps mur de `just check` sur cache chaud est mesuré sur la machine de référence et consigné dans la note d'US-089, avec la mesure de départ (fmt 0,69 s, clippy 0,26 s, compilation des tests 0,20 s, suite 19 s).
- [ ] Le fichier est exécuté sans erreur de syntaxe par les deux versions de `just` observées, 1.21.0 et 1.57.0, ou l'écart constaté est consigné.
- [ ] Given un `Ctrl-C` pendant `just check`, when la commande est interrompue, then elle sort non nul et ne laisse aucun fichier de travail derrière elle.
- [ ] Given deux `just check` lancés en parallèle, when les deux tournent, then cargo sérialise par son verrou de fichier sans corrompre `target/`, et le comportement est consigné.

#### US-091: Le CI est constaté inchangé et les portes orphelines rattachées
**Description:** As a mainteneur, I want vérifier que le lot n'a rien retiré au CI et n'a laissé aucune porte hors agrégat so that le gain soit net.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-083, US-086, US-087

**Acceptance Criteria:**
- [ ] Le diff de `.github/workflows/ci.yml` ne touche que des commentaires et, le cas échéant, l'ajout du marqueur d'appariement : aucun `timeout`, aucun `tee`, aucun filtre, aucun bloc `GITHUB_STEP_SUMMARY` modifié.
- [ ] Aucune étape n'est ajoutée au workflow, en particulier aucune installation de `just`.
- [ ] `cargo run -p agent-parity -- check` et `-- drift` sont tous deux atteignables par une recette nommée, ce qui ramène à zéro le compte de portes documentées jamais exécutées.
- [ ] Given la table « Targeted verification signals » d'`AGENTS.md`, when on la relit ligne à ligne, then chaque commande qu'elle nomme est soit une recette, soit explicitement hors agrégat avec la raison écrite.
- [ ] Given une porte listée dans `docs/parity/offline-suite.md`, when le tableau est relu, then la recette correspondante existe ou l'écart est consigné dans la note.
- [ ] Given une étape du workflow modifiée au-delà d'un commentaire ou d'un marqueur, when le diff est relu, then le changement est annulé ou son motif est consigné dans la note d'US-089.

## Functional Requirements

- FR-01: Le dépôt doit porter un `justfile` racine exposant les portes de vérification comme recettes nommées.
- FR-02: `just check` doit exécuter, dans l'ordre, exactement les invocations `cargo` que `.github/workflows/ci.yml` exécute.
- FR-03: `just check` doit s'interrompre à la première porte en échec, sans lancer les suivantes.
- FR-04: `just check` doit réussir sur une machine dépourvue du clone Codex épinglé.
- FR-05: Le système doit exposer un niveau `check-local` ajoutant `agent-parity check` de façon bloquante et `agent-parity drift` de façon non bloquante.
- FR-06: Le système doit séparer les commandes qui mutent l'arbre de travail des commandes qui le constatent, et ne jamais lancer les premières depuis les secondes.
- FR-07: Un test de `cargo test --workspace` doit échouer quand le `justfile` et `ci.yml` divergent en contenu ou en ordre.
- FR-08: Le système ne doit PAS rendre le CI dépendant de `just`.
- FR-09: Le système ne doit PAS ajouter de dépendance au workspace ni de crate.
- FR-10: `AGENTS.md` et `CONTRIBUTING.md` ne doivent PAS porter d'invocation brute d'une porte présente dans le `justfile`.
- FR-11: Aucune recette ne doit écrire dans le clone résolu par `$PYXIS_CODEX_BASELINE` ni définir `PYXIS_LIVE_PARITY`.

## Non-Functional Requirements

- **Performance:** `just check` s'exécute en 60 s ou moins sur cache chaud sur la machine de référence, mesure de départ 20,2 s cumulées pour les quatre portes. Un échec de `fmt` sort en moins de 5 s.
- **Performance:** la porte de non-dérive ajoute 1 s ou moins à `cargo test --workspace` : elle lit deux fichiers texte de moins de 200 lignes chacun et ne lance aucun processus.
- **Reliability:** la porte de non-dérive rend le même verdict sur 100 % des machines, y compris sans `just` installé et sans clone Codex. Aucun chemin de saut conditionnel.
- **Reliability:** le CI conserve son plafond de 45 minutes et ses `timeout` par étape inchangés ; zéro étape ajoutée.
- **Maintainability:** le `justfile` compte 12 recettes ou moins, chacune documentée d'une ligne restituée par `just --list`.
- **Portability:** le `justfile` n'emploie que de la syntaxe disponible depuis `just` 1.0 ; il s'exécute sur les deux versions empaquetées observées, 1.21.0 (Ubuntu 24.04 universe) et 1.57.0 (Fedora 44).
- **Security:** zéro dépendance ajoutée, zéro action GitHub ajoutée, zéro écriture dans un chemin en lecture seule déclaré par les limites d'autorisation d'`AGENTS.md`.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Outil absent | `just` non installé sur la machine du contributeur | `CONTRIBUTING.md` documente `cargo test --workspace` comme voie complète ; le CI n'en dépend pas | "just is optional: the CI runs plain cargo commands" |
| 2 | Arrêt anticipé | `fmt` échoue dans `just check` | La recette avorte, les portes suivantes ne démarrent pas, sortie non nulle | `error: Recipe 'fmt' failed with exit code 1` |
| 3 | Porte non bloquante | `agent-parity drift` sort non nul, l'amont a bougé | Le sigil `-` laisse `check-local` poursuivre et rendre vert | La sortie de `drift` reste visible dans le journal |
| 4 | Artefact local absent | `$PYXIS_CODEX_BASELINE` non défini pendant `just check-local` | Sortie non nulle avec l'erreur typée d'`agent-parity`, aucune écriture dans le clone | `MissingClone` relayée telle quelle |
| 5 | Artefact local dérivé | Clone épinglé au mauvais commit ou avec des fichiers suivis modifiés | Sortie non nulle, `CommitMismatch` ou `DirtyTracked`, aucune correction automatique | L'erreur nomme le commit attendu |
| 6 | Dérive d'inventaire | Une porte ajoutée à `ci.yml` sans l'être au `justfile` | La porte de non-dérive échoue dans `cargo test --workspace` | Le message nomme l'étape et la commande à ajouter |
| 7 | Dérive d'ordre | `fmt` et `lint` intervertis d'un seul côté | Échec du même test, distinct du cas 6 | Le message dit que l'ordre diverge et lequel fait foi |
| 8 | Préfixe inconnu | Une étape du CI enveloppe `cargo` dans un wrapper autre que `timeout` | L'extraction échoue bruyamment plutôt que d'ignorer | Le message nomme le préfixe rencontré |
| 9 | Mutation involontaire | `just regen` lancée par erreur avant une revue | L'arbre est modifié et visible dans `git status` ; aucune recette de vérification ne peut la déclencher | Le commentaire de la recette annonce l'écriture |
| 10 | Interruption | `Ctrl-C` pendant `just check` | Sortie non nulle, aucun fichier résiduel, `target/` cohérent | Interruption standard, pas de message propre requis |
| 11 | Concurrence | Deux `just check` simultanés | Le verrou de fichier de cargo sérialise, aucune corruption | `Blocking waiting for file lock on build directory` |
| 12 | Réseau absent | `just check` sur cache froid sans réseau | `cargo` échoue au téléchargement du registre ; la recette relaie l'échec sans le masquer | L'erreur cargo est rendue telle quelle |
| 13 | Écart de version | `justfile` employant un attribut absent de `just` 1.21.0 | Interdit par revue et par contrainte de syntaxe ; découvert à l'exécution sur la machine la plus ancienne | `error: Expected ...` |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Le `justfile` et `ci.yml` divergent, et l'agrégat ment sur ce que le CI exécute | High sans porte | High | EP-028 : test d'égalité et d'ordre dans `cargo test --workspace`, sur le modèle déjà employé par `offline_suite.rs` |
| 2 | La divergence documentaire réapparaît après correction, comme elle est apparue une première fois | Medium | Medium | US-088 : porte close sur `AGENTS.md` et `CONTRIBUTING.md`, avec message disant quoi écrire |
| 3 | Écart de version de `just` entre machines et distributions | Medium | Medium | Syntaxe restreinte à `just` 1.0, aucun attribut, exécution vérifiée sur 1.21.0 et 1.57.0 |
| 4 | Le parseur du test casse sur une édition légitime de `ci.yml` | Medium | Low | Sous-ensemble documenté et étroit, échec bruyant sur préfixe inconnu, message nommant l'étape |
| 5 | `just check` grossit avec les lots 6 à 8 jusqu'à ne plus être lancé | Medium | High | Budget NFR de 60 s sur cache chaud ; toute porte ajoutée annonce son coût à chaud dans la note |
| 6 | Ajouter un prérequis système décourage un contributeur externe | Low | Medium | Le CI ne dépend pas de `just` ; `CONTRIBUTING.md` documente la voie `cargo` comme complète |
| 7 | Envelopper le CI dans `just` ferait perdre les diagnostics de `ci.yml` | Low après décision | High | Décision inverse actée en US-089 et contrainte par US-091, qui borne le diff du workflow |

## Non-Goals

- Aucun ordonnanceur, aucune concurrence, aucune validation de graphe. La suite complète mesure 19 s à chaud et compte quatre portes : `run-gates.ts` résout un problème que Pyxis n'a pas.
- Aucune sémantique `after`. Elle n'a de valeur qu'avec du parallélisme ; le seul besoin réel du dépôt est `allowFailure`, couvert par le sigil `-`.
- Aucun hook git, ni `pre-commit`, ni `pre-push`. La note du lot 2 a déjà rejeté cette voie comme contournable par `--no-verify`, et `dsh` a lui-même réduit ses hooks depuis.
- Aucune recette `check-fast`, malgré le libellé du plan de portage : elle n'a pas d'objet quand l'ensemble tient en 20 s à chaud.
- Aucune compilation `--release` ajoutée aux portes. Le trou est réel, le CI ne compile jamais en release et un test de benchmark reste inatteignable, mais c'est un lot distinct avec son propre budget de temps.
- Aucune installation de `just` sur le runner, aucune action tierce ajoutée.
- Aucune modification des corps de shell des étapes `Tests` et `Report failing tests` du CI.
- Aucune reprise des 14 modes nommés, des 142 scripts ni du buffering de sortie par porte de `dsh`.

## Files NOT to Modify

- `crates/agent-parity/src/lib.rs` et `crates/agent-parity/src/client_model.rs` - portent les deux `BASELINE_COMMIT` ; déplacer un pin est une décision explicite étrangère à ce lot.
- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` - générés et empreintés, jamais édités à la main.
- Le clone résolu par `$PYXIS_CODEX_BASELINE` - lecture seule par limite d'autorisation.
- `.github/workflows/ci.yml`, étapes `Tests` et `Report failing tests` - leur logique de shell est la mitigation écrite du cas « job annulé, log perdu ».
- `.cargo/config.toml` - porte les rustflags `mold` ; n'y ajouter aucune section `[alias]`, la voie alias étant écartée.
- `crates/agent-app-server/tests/schemas.rs` et le schéma généré - territoire du lot 6.
- `spikes/` - espace Phase 0 exclu.
- `/home/arthur/dev/deepseek-harness/**` - dépôt de référence, lecture seule. Aucun commit, aucun checkout, aucune écriture.
- `crates/agent-doc-gates/tests/note_tree.rs`, `note_format.rs`, `markdown_links.rs`, `adr_register.rs` - portes existantes ; le lot ajoute un fichier de test, il n'en modifie aucun.

## Technical Considerations

- **Emplacement de la porte:** recommandé, un module `gates` dans `crates/agent-doc-gates/src/` plus un test d'intégration. Cela élargit la responsabilité déclarée du crate, aujourd'hui « les enregistrements de décision du dépôt », vers « les descriptions que le dépôt fait de lui-même ». Alternative : un crate dédié, écarté comme disproportionné pour un test. L'ingénierie confirme-t-elle l'élargissement plutôt que le crate ?
- **Appariement recette/étape:** recommandé, un marqueur explicite en commentaire au-dessus de la recette, les noms d'étapes du CI contenant des espaces. Alternative : dériver du nom de recette, qui obligerait à renommer les étapes du CI en identifiants. Compromis : un commentaire de plus contre un YAML moins lisible.
- **Règle de normalisation côté CI:** recommandé, retirer un préfixe `timeout` avec ses options et comparer les argv. Tout autre préfixe fait échouer l'extraction. Faut-il accepter d'autres wrappers, ou forcer une décision explicite à chaque fois ?
- **Granularité de la comparaison:** recommandé, comparer les argv `cargo` et non les corps de shell, l'étape `Tests` portant une trentaine de lignes de journalisation qu'aucune recette ne reproduira. Conséquence assumée : le test prouve l'identité des commandes, pas celle des diagnostics.
- **Version plancher de `just`:** recommandé, ne déclarer aucune version et contraindre la syntaxe à ce qui existe depuis 1.0. Alternative : déclarer 1.21.0, plancher observé sur Ubuntu 24.04. La contrainte de syntaxe suffit-elle, ou faut-il un plancher écrit ?
- **Validité syntaxique du `justfile`:** aucune vérification automatique n'est prévue, `just --fmt --check` supposerait le binaire en CI. La revue et l'usage local suffisent-ils ?

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Portes du dépôt atteignables par une commande nommée | 0/6 | 6/6 | Month-1 | `just --list` confronté à la table « Build and verify » d'`AGENTS.md` |
| Portes documentées jamais exécutées | 2 (`agent-parity check`, `drift`) | 0 | Month-1 | Relecture d'`AGENTS.md` et de `docs/parity/offline-suite.md` |
| Formulations divergentes de la porte clippy dans les documents prescriptifs | 2 (`--no-deps`, `--all-targets`) | 1 | Month-1 | Test d'US-088 |
| Dérive `justfile` / `ci.yml` détectée mécaniquement | non | oui | Month-1 | `cargo test -p agent-doc-gates` |
| Étapes ajoutées au workflow CI | 0 | 0 | Month-1 | Diff de `.github/workflows/ci.yml` |
| Dépendances ajoutées | 0 | 0 | Month-1 | Diff de `Cargo.toml` du workspace et des crates |
| Temps mur de `just check` sur cache chaud | 20,2 s (0,69 + 0,26 + 0,20 + 19) | ≤ 60 s | Month-6 | Mesure consignée dans la note d'US-089, reprise après les lots 6 à 8 |
| Vérifications des lots 6 à 8 atteignables en local | sans objet | 100 % | Month-6 | Chaque lot ajoute sa recette ou consigne pourquoi non |

## Open Questions

- `just` devient-il un prérequis dur d'`AGENTS.md`, au même rang que `mold`, ou reste-t-il recommandé sans être bloquant ? Arthur, avant US-087 : la réponse change la formulation de `CONTRIBUTING.md` et le sort d'un contributeur qui ne l'installe pas.
- Quand les lots 6, 7 et 8 ajoutent une porte, entre-t-elle dans `check` ou dans un niveau nouveau ? Le précédent posé ici dit : tout ce que le CI peut exécuter entre dans `check`, tout ce qui exige un artefact local entre dans `check-local`. Arthur, à confirmer avant le lot 6.
- Le trou `--release`, constaté mais hors périmètre, mérite-t-il un lot dédié ou une porte non bloquante dans `check-local` ? Arthur, sans échéance : le seul symptôme connu est un test de benchmark inatteignable.
- L'estimation du plan, « 1 à 2 jours », est révisée à 23 points, soit 3 à 4 jours, la porte de non-dérive n'ayant pas été anticipée. Le périmètre est-il maintenu, ou EP-028 devient-il un lot séparé ? Arthur, avant de démarrer EP-027.
[/PRD]
