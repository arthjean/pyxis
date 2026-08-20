[PRD]
# PRD: Arbre de notes de décision

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-20 | Arthur Jean | Rédaction initiale, lot #2 du plan de portage DeepSeek Harness |

## Problem Statement

Pyxis enregistre ses décisions dans un fichier unique, `docs/DECISIONS.md`, et rien ne vérifie ce fichier. Quatre défaillances mesurées sur l'état actuel du dépôt :

1. **L'index ment déjà.** `docs/DECISIONS.md` déclare 13 ADR (`## ADR-1` à `## ADR-13`) mais son tableau récapitulatif n'en liste que 12 : ADR-13 (`docs/DECISIONS.md:426`) est absent de son propre sommaire. Un lecteur qui se fie à l'index ignore une décision structurante, le NO-GO sur les sous-agents mutateurs.
2. **Le format dérive sans signal.** Le fichier annonce en tête un format par décision (Contexte / Décision / Justification / Alternatives écartées / Conséquences & risques). Quatre ADR sur treize n'ont pas de section « Alternatives écartées » : ADR-7, ADR-9, ADR-11 et ADR-13, ce dernier cumulant les deux défauts. Or l'absence d'alternatives consignées est le reproche numéro un adressé au format ADR original de Nygard, et la raison même pour laquelle une décision se re-litige.
3. **Les décisions récentes n'entrent plus dans le registre.** Quatre documents datés vivent hors de lui : `docs/parity-audit-2026-07-24.md` (2 551 lignes, 348 Ko), `-25`, `-27` et `docs/parity-strategy-2026-07-27.md`. Ce dernier porte une décision nette et argumentée, ne pas partir de la base Codex CLI, qui n'apparaît dans aucun ADR. Le besoin se résout aujourd'hui par accumulation de fichiers ad hoc à la racine de `docs/`.
4. **Aucun enregistrement n'est vérifié mécaniquement.** Le dépôt possède une porte documentaire (`crates/agent-parity/tests/offline_suite.rs`, qui parse un tableau Markdown et vérifie chaque ligne contre le dépôt) et une porte de comparaison octet à octet (`crates/agent-app-server/tests/schemas.rs`). Aucune des deux ne couvre les décisions. Sur 167 références `ADR-N` réparties dans 35 fichiers, y compris du code Rust, zéro est vérifiée.

**Why now:** Pyxis est développé principalement par des agents de codage, dont le contexte est effacé entre les sessions. Le lot #1 du plan de portage vient de produire un `AGENTS.md` de 146 lignes de règles impératives, aujourd'hui non commité et sans une seule règle liée à sa justification. La pratique établie en 2025-2026 pour ce type de dépôt est de projeter les décisions vers les fichiers de contexte des agents, ce qui suppose que chaque règle puisse pointer vers l'enregistrement qui la justifie. Sans arbre de notes, `AGENTS.md` reste un ensemble d'assertions non traçables, et chaque décision de lot suivant (3 à 11) s'enregistre à nouveau comme un fichier daté à la racine de `docs/`.

## Overview

Le lot introduit `docs/notes/`, un arbre où le chemin d'un fichier encode deux axes : `{cycle de vie}/{classe}/aaaa-mm-jj-sujet.md`. Le cycle de vie est le répertoire (`proposed`, `implemented`, `rejected`), donc une note change de statut en changeant de répertoire, et la classe est le type de décision (`feature`, `bug-fix`, `simplification`, `architecture`, `process`, `testing`). Le format du fichier est fixe sur ses trois premières lignes, son corps ouvre sur `## Problème`, et une section d'alternatives est obligatoire.

Ce format n'est pas une convention écrite dans un document : c'est une porte. Un nouveau crate `agent-doc-gates` porte un walker de l'arbre et deux vérifications, la structure du chemin et le format du fichier, exécutées par `cargo test --workspace` que la CI lance déjà. Une décision mal enregistrée fait échouer la suite, comme un test qui casse. Le crate n'importe aucun crate Pyxis et n'entre pas dans le graphe du binaire.

`docs/DECISIONS.md` est conservé. Les 167 références `ADR-N` disséminées dans le code et la documentation font de cet identifiant un point d'ancrage stable qu'une dissolution en fichiers casserait sans contrepartie. L'arbre accueille ce que le registre ADR n'accueille pas : les décisions de processus, les mesures datées, les propositions non encore tranchées et les refus. Une règle de frontière écrite dans `AGENTS.md` dit laquelle des deux formes reçoit une décision donnée, et la porte vérifie en plus que l'index de `DECISIONS.md` liste bien tous ses ADR, ce qu'il ne fait pas aujourd'hui.

Deux divergences par rapport aux formats établis sont assumées et compensées. Encoder le cycle de vie par le répertoire n'a pas de précédent hors de DeepSeek Harness : MADR 4.0.0 et adr-tools mettent le statut dans le fichier. Le bénéfice est qu'un statut divergent du répertoire devient mécaniquement impossible ; le coût est le lien relatif cassé à chaque transition. Ce coût est payé dans le même lot par une vérification des liens Markdown internes de `docs/`, qui n'existe pas aujourd'hui et qui protège aussi la migration des quatre documents datés.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Enregistrements de décision vérifiés mécaniquement | 100 % des notes de `docs/notes/` | 100 %, plus l'index de `DECISIONS.md` |
| Décisions consignées hors registre à la racine de `docs/` | 0 | 0 |
| Décisions du dépôt portant leurs alternatives | 13/13 ADR + 100 % des notes | idem, sans régression |
| Notes natives produites après l'adoption du format | 1 (celle qui enregistre ce lot) | au moins 1 par lot du plan de portage livré |
| Liens Markdown internes cassés dans `docs/` | 0, prouvé par la porte | 0 |

## Target Users

### Agent de codage éditant le dépôt
- **Role:** Claude Code, Codex ou tout agent qui reçoit une tâche sur Pyxis avec un contexte vierge.
- **Behaviors:** lit `AGENTS.md` en entrée de session, ne lit presque jamais `docs/ARCHITECTURE.md` (44 Ko) ni `docs/DECISIONS.md` (50 Ko) intégralement, cherche par `grep` sur un identifiant quand une règle le surprend.
- **Pain points:** une règle de `AGENTS.md` sans justification accessible se lit comme arbitraire, donc se contourne ou se re-litige ; un fichier de 50 Ko ne se charge pas pour vérifier un point ; rien n'indique si une décision trouvée est encore vraie.
- **Current workaround:** relire le code pour reconstruire l'intention, ou proposer à nouveau une option déjà rejetée.
- **Success looks like:** un chemin lisible dit le statut et la classe avant l'ouverture du fichier, et les trois premières lignes disent le verdict.

### Mainteneur unique
- **Role:** Arthur Jean, seul contributeur visible, travaillant par sessions séparées sur des lots dépendants.
- **Behaviors:** décide vite, consigne quand le format ne coûte rien, produit des documents datés ad hoc quand il coûte quelque chose (quatre en trois jours en juillet).
- **Pain points:** `DECISIONS.md` sature à 13 ADR dans un fichier unique ; l'index a déjà divergé sans que personne ne le voie ; une décision de processus n'a pas de place et finit à la racine de `docs/`.
- **Current workaround:** un nouveau fichier daté à la racine de `docs/`, non lié depuis nulle part, qui sort du champ de vision au commit suivant.
- **Success looks like:** consigner coûte un fichier de trente lignes dans un chemin évident, et oublier une section fait échouer `cargo test`.

## Research Findings

### Competitive Context
- **Nygard (2011) + adr-tools :** Title / Status / Context / Decision / Consequences, statut dans le corps, fichiers séquentiels `NNNN-titre.md`. N'exige pas les alternatives rejetées, qui restent implicites. C'est le reproche documenté le plus fréquent. adr-tools génère son sommaire mécaniquement.
- **MADR 4.0.0 (sept. 2024) :** Context and Problem Statement, Decision Drivers, Considered Options, Decision Outcome, Consequences. Statut optionnel en front matter YAML, répertoire `docs/decisions/`, sous-répertoires par structure architecturale et non par cycle de vie.
- **arc42 :** les ADR vivent dans la section 9 du template, avec des critères de qualité plutôt qu'un format rigide.
- **Market gap:** aucun format établi n'encode le cycle de vie par le répertoire, et aucun ne croise mécaniquement le statut déclaré avec l'emplacement du fichier. C'est la propriété que ce lot retient de DeepSeek Harness, contre le courant dominant et en connaissance de cause.

### Best Practices Applied
- Un fichier par décision, car chaque décision a son propre cycle de vie et sera un jour remplacée individuellement (Fowler, Thunderbird, ctaverna).
- Alternatives consignées obligatoires : une décision enregistrée sans ce qu'elle a battu invite la re-litigation. C'est le manque principal du format Nygard, déjà corrigé par le format de `DECISIONS.md`, conservé ici.
- Contrainte mécanique plutôt que discipline : `adr-toolkit` (2025, explicitement « agent friendly ») fait échouer la CI sur titre, statut et statuts autorisés. Le projet `adrkit` résume la position : « an unenforceable ADR is a wish ».
- Index généré ou vérifié plutôt que tenu à la main : la dérive d'index est un mode d'échec documenté, et adr-tools y répond par la génération.
- Décisions projetées vers les fichiers de contexte des agents (`AGENTS.md`, `CLAUDE.md`), tendance 2025-2026 : écrire la décision une fois et la lier depuis la règle qu'elle justifie.

*Sources complètes dans la synthèse de recherche de la session ; références principales : adr.github.io/madr, martinfowler.com/bliki/ArchitectureDecisionRecord.html, github.com/lordcraymen/adr-toolkit, docs.arc42.org/tips/9-9.*

### Implémentation de référence : DeepSeek Harness

Racine du dépôt de référence, en lecture seule : `/home/arthur/dev/deepseek-harness`, commit `141eb6f` du 2026-08-19, licence MIT. Ces fichiers sont la source des décisions de conception portées ici. **Aucune ligne de TypeScript n'est transcrite** : Pyxis est en Rust, ce qui se reprend est la décision, jamais le code, et cette contrainte suffit à écarter toute question de licence ou d'inventaire de portage.

| Source dsh | Ce qui se reprend | Ce qui ne se reprend pas |
|---|---|---|
| `.agents/notes/README.md` (125 lignes) | La spécification entière : chemin à deux axes (§ *Layout and naming*, l. 7), classification (l. 21), quand écrire (l. 44), en-tête de trois lignes (l. 58), squelettes par cycle de vie (l. 76), alternatives obligatoires (l. 109), déplacement entre cycles (l. 119) | § *Archiving and deletion* (l. 36) et § *Chinese counterparts* (l. 123), tous deux hors périmètre |
| `scripts/agent-note-tree.ts` (83 lignes) | La décision structurante du lot : un walker **partagé** rendant `(notes, erreurs)`, consommé par plusieurs portes plutôt que dupliqué. Ensembles fermés (l. 12, 19), liste blanche de racine (l. 25), refus de `INDEX.md` (l. 47), messages nommant fichier et règle (l. 48, 54, 72) | Le saut des contreparties `.zh.md` (l. 64), sans objet ici |
| `scripts/verify-agent-note-format.ts` (94 lignes) | La date d'adoption comme constante unique (l. 13), le marqueur de dispense littéral (l. 16), la table statut par cycle de vie (l. 22), la table des sections requises (l. 29), les titres bannis en `implemented` (l. 36), le filtrage des blocs délimités avant analyse structurelle (l. 45), la dispense invalide au-delà de la date (l. 82) | Rien |
| `scripts/verify-agent-note-classification.ts` (27 lignes) | **Troisième porte que le plan de portage ne mentionne pas.** Sa vraie contribution est le garde-fou de chemins hérités (l. 14 à 16) : un ancien emplacement interdit explicitement plutôt que laissé vacant. C'est ce que fait ici l'interdiction des documents datés à la racine de `docs/` | Le découpage en trois scripts distincts : Pyxis les regroupe dans un seul crate |
| `scripts/verify-archived-agent-notes.ts` (115 lignes) | Rien | Hors périmètre par décision, voir *Non-Goals* |
| `scripts/run-gates.ts` l. 670 à 672 | Le fait, contre-intuitif, que les portes de note tournent dans l'agrégat `doc-sync` et **non** en pre-commit. C'est ce qui justifie de remplacer le signal « pre-commit » du plan de portage par `cargo test` | Le graphe de portes à `needs` et `after`, qui est le lot #5 |
| `lefthook.yml` (55 lignes) | Le constat par la négative : le pre-commit y exécute l'appariement de traduction, les notes archivées, oxlint, les notices et le manifeste, jamais la porte de format | Tout le reste |
| `AGENTS.md` (151 lignes) | Le patron d'ancrage : chaque règle porte un lien relatif vers la note qui la justifie, forme reprise en US-054 | La règle « toute PR non triviale contient une note », voir *Non-Goals* |

## Assumptions & Constraints

### Assumptions (to validate)
- Un mainteneur unique consignera une décision sans règle « toute PR non triviale en contient une », à condition que le coût d'entrée soit un fichier court dans un chemin évident. DeepSeek Harness impose cette règle et compte 726 notes anglaises ; Pyxis l'écarte et vise l'ordre de la dizaine la première année. Non validé, c'est le risque d'adoption principal.
- Six classes ne produiront pas de répertoires morts, parce que git ne suit pas les répertoires vides : une classe sans note n'existe simplement pas sur le disque.
- Les trois audits de parité contiennent assez de matière décisionnelle pour justifier une note, ou assez peu pour qu'une note unique les référence. Cette bascule est tranchée par US-052, pas avant.
- La date portée par le nom de fichier reste la date de l'événement décrit et non la date du commit. Les quatre documents à migrer sont datés de juillet dans leur nom et ont tous été ajoutés à git le 2026-08-12 : la règle « date de première proposition selon l'historique git » de DeepSeek Harness est inapplicable telle quelle ici.

### Hard Constraints
- Aucun toolchain non-Rust n'existe dans le dépôt : ni `package.json`, ni `scripts/`, ni `.git/hooks` peuplé, ni `core.hooksPath`. La porte est du Rust exécuté par `cargo test`, et le signal « pre-commit » du plan de portage est remplacé en connaissance de cause.
- Le lot ne doit ajouter aucune dépendance au workspace. Le parsing se fait à la main, comme `offline_suite.rs` qui parse un tableau Markdown sans crate dédiée.
- `crates/agent-doc-gates` ne doit importer aucun crate Pyxis et ne doit apparaître dans le graphe de dépendances d'aucun binaire livré.
- La convention de langue de `AGENTS.md` s'applique : les documents d'architecture sous `docs/` sont en français, le code et ses commentaires en anglais. Les noms de tests sont des phrases complètes en snake_case.
- Un test d'intégration qui a besoin de `panic!` comme mécanisme de rapport porte un `#![allow(...)]` au niveau du fichier avec un commentaire disant pourquoi (`clippy.toml` ne couvre pas les tests d'intégration).
- Ni `docs/parity/offline-suite.md`, ni les matrices de parité générées, ni les schémas de l'app-server ne sont touchés par ce lot.

## Quality Gates

Ces commandes doivent passer pour chaque user story :
- `cargo fmt --all -- --check` - formatage, tel que la CI l'exécute
- `cargo clippy --workspace --all-targets` - lints, sans `-D warnings` par décision documentée
- `cargo test --workspace --no-fail-fast` - suite complète, qui inclut désormais la porte documentaire

## Epics & User Stories

### EP-014: Format exécutable et porte de structure

Établit la spécification de l'arbre et la rend exécutable. À la fin de l'epic, un fichier mal placé ou mal formé fait échouer la suite de tests, et la spécification qui le décrit est elle-même vérifiée par la porte qu'elle décrit.

**Definition of Done:** `docs/notes/README.md` existe et décrit le format ; `crates/agent-doc-gates` expose un walker et deux vérifications ; `cargo test --workspace` échoue sur une note invalide et passe sur l'arbre réel.

#### US-047: Spécification exécutable de l'arbre
**Description:** En tant qu'agent de codage, je veux une spécification de moins de 150 lignes qui dise où va une note et à quoi elle ressemble, afin d'en écrire une correctement sans lire le code de la porte.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Référence dsh:** `.agents/notes/README.md` en entier, en particulier l. 7, 21, 44, 58, 76, 109, 119

**Acceptance Criteria:**
- [ ] `docs/notes/README.md` existe, fait au plus 150 lignes, et est rédigé en français conformément à la convention de `docs/`
- [ ] Il définit l'ensemble fermé des trois cycles de vie et l'ensemble fermé des six classes, chaque classe avec la phrase qui la distingue de sa voisine la plus proche
- [ ] Il donne le gabarit exact des trois premières lignes d'une note et le squelette de sections attendu pour chacun des trois cycles de vie
- [ ] Il énonce la règle de nommage `aaaa-mm-jj-sujet.md` et précise que la date est celle de l'événement décrit, pas celle du commit
- [ ] Il énonce que les liens entre notes sont des liens Markdown relatifs, jamais une référence en prose
- [ ] Il énonce qu'aucun fichier d'index centralisé n'est autorisé dans l'arbre, avec la raison
- [ ] Given une note refusée par la porte, when on cherche la règle violée dans ce document, then elle y est énoncée, aucune règle mécanique n'existant sans contrepartie écrite
- [ ] Given un lecteur qui suit uniquement ce document, when il écrit une note, then la porte de US-048 et US-049 l'accepte sans modification
- [ ] Given une règle décrite dans ce document, when elle n'est vérifiée par aucune porte, then le document le signale explicitement comme convention non tenue par la machine

#### US-048: Crate `agent-doc-gates` et walker de l'arbre
**Description:** En tant que mainteneur, je veux un walker qui parcoure `docs/notes/` et rende les notes valides accompagnées d'une erreur par violation de structure, afin que les deux portes partagent une seule source de vérité sur la forme de l'arbre.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-047

**Acceptance Criteria:**
- [ ] `crates/agent-doc-gates` existe avec `publish = false`
- [ ] Le walker suit la décision de `scripts/agent-note-tree.ts` l. 41 à 72 : ensembles fermés, une erreur par violation, message nommant fichier et règle, `[lints] workspace = true`, et n'ajoute aucune dépendance au `Cargo.toml` racine
- [ ] `cargo tree -p agent-doc-gates` ne fait apparaître aucun crate `agent-*` autre que lui-même
- [ ] Le walker rend une paire (notes valides, erreurs) plutôt que d'interrompre au premier problème, et chaque erreur nomme le chemin fautif et la règle violée
- [ ] Given un répertoire de premier niveau qui n'est pas un cycle de vie connu, when le walker parcourt l'arbre, then il produit une erreur nommant les valeurs autorisées
- [ ] Given un fichier à une profondeur autre que `{cycle}/{classe}/fichier.md`, when le walker le rencontre, then il produit une erreur nommant la profondeur observée
- [ ] Given un nom de fichier qui ne commence pas par `aaaa-mm-jj-`, when le walker le rencontre, then il produit une erreur, et le fichier n'apparaît pas dans les notes valides
- [ ] Given un `INDEX.md` à la racine de l'arbre, when le walker parcourt, then il produit une erreur dédiée citant l'interdiction
- [ ] Given un arbre `docs/notes/` vide ou absent, when le walker s'exécute, then il rend zéro note et zéro erreur, sans panique

#### US-049: Porte de format des notes
**Description:** En tant que mainteneur, je veux qu'une note dont l'en-tête, le statut ou le squelette de sections ne correspond pas à son cycle de vie fasse échouer `cargo test`, afin que le format ne dérive pas comme celui de `DECISIONS.md` a dérivé.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-048
**Référence dsh:** `scripts/verify-agent-note-format.ts` l. 22, 29, 36 pour les tables, l. 45 pour le filtrage des blocs délimités

**Acceptance Criteria:**
- [ ] Le test échoue si la ligne 1 n'est pas un titre de note conforme au gabarit, si la ligne 2 n'est pas vide, si la ligne 3 n'est pas une ligne de statut, ou si la ligne 4 n'est pas vide
- [ ] Given une note dans `implemented/` dont la ligne de statut annonce un autre cycle de vie, when la porte s'exécute, then elle échoue en nommant le désaccord entre le répertoire et le statut
- [ ] Given une note dans `rejected/`, when son statut ne porte pas de raison de rejet en une ligne, then la porte échoue
- [ ] Given une note dont le premier titre de section n'est pas `## Problème`, when la porte s'exécute, then elle échoue en citant le titre trouvé
- [ ] Given une note dans `implemented/` portant un titre de section propre à une proposition, when la porte s'exécute, then elle échoue en nommant le titre fautif
- [ ] Given une note sans section d'alternatives et sans le marqueur de dispense daté, when la porte s'exécute, then elle échoue
- [ ] Given une note contenant un bloc de code délimité qui inclut des lignes ressemblant à des titres de section ou à une ligne de statut, when la porte s'exécute, then ces lignes sont ignorées et la note valide passe
- [ ] Given une seconde ligne de statut ailleurs dans le corps, when la porte s'exécute, then elle échoue
- [ ] Les noms des tests sont des phrases complètes en snake_case décrivant le comportement prouvé
- [ ] Le fichier de test porte un `#![allow(...)]` au niveau fichier avec un commentaire justifiant pourquoi la panique est ici le mécanisme de rapport

#### US-050: Dispense datée pour les enregistrements antérieurs
**Description:** En tant que mainteneur, je veux qu'une note antérieure à l'adoption du format puisse déclarer que ses alternatives ne sont pas reconstructibles, afin de ne pas fabriquer après coup un raisonnement qui n'a pas eu lieu.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-049
**Référence dsh:** `scripts/verify-agent-note-format.ts` l. 13, 16, 79, 82

**Acceptance Criteria:**
- [ ] La date d'adoption du format est une constante unique du crate, citée dans `docs/notes/README.md`
- [ ] Given une note datée avant l'adoption et portant le marqueur de dispense exact, when la porte s'exécute, then elle passe sans section d'alternatives
- [ ] Given une note datée à partir du jour d'adoption et portant le marqueur, when la porte s'exécute, then elle échoue en citant la date d'adoption
- [ ] Given une note portant à la fois le marqueur et une vraie section d'alternatives, when la porte s'exécute, then elle échoue en demandant de retirer le marqueur
- [ ] Le marqueur est une chaîne exacte, comparée littéralement, et sa forme est donnée dans le README

---

### EP-015: Migration du corpus daté existant

Absorbe les quatre documents datés qui vivent à la racine de `docs/` et paie le coût que la migration crée : les liens relatifs qui pointaient vers eux.

**Definition of Done:** plus aucun document de décision daté à la racine de `docs/` ; tous les liens internes de `docs/` résolvent vers un fichier existant, prouvé par une porte.

#### US-051: Vérification des liens Markdown internes
**Description:** En tant que mainteneur, je veux qu'un lien relatif cassé entre documents de `docs/` fasse échouer la suite, afin que déplacer une note entre deux cycles de vie soit une opération sûre plutôt qu'une source de rot silencieux.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-048

**Acceptance Criteria:**
- [ ] La porte parcourt les fichiers Markdown de `docs/` et de la racine du dépôt, et résout chaque lien relatif contre le disque
- [ ] Given un lien relatif pointant vers un fichier inexistant, when la porte s'exécute, then elle échoue en nommant le fichier source, la cible et la ligne
- [ ] Given un lien absolu vers un site externe, when la porte s'exécute, then il est ignoré sans accès réseau
- [ ] Given un lien vers une ancre d'un fichier existant, when la porte s'exécute, then la partie ancre est ignorée et le fichier seul est vérifié
- [ ] Given l'état du dépôt avant toute migration, when la porte s'exécute, then elle passe, ce qui établit la ligne de base avant US-052 et US-053
- [ ] Aucun accès réseau n'a lieu pendant l'exécution de la porte

#### US-052: Migration de la décision de stratégie de parité
**Description:** En tant qu'agent de codage, je veux que la décision « ne pas partir de la base Codex CLI » soit une note de décision trouvable par son chemin, afin de ne pas re-proposer un fork déjà écarté avec argumentaire.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-050, US-051

**Acceptance Criteria:**
- [ ] `docs/parity-strategy-2026-07-27.md` devient une note sous `docs/notes/implemented/`, dans la classe justifiée par la règle du README
- [ ] La note porte le verdict en tête et conserve les alternatives réellement pesées, dont le fork de la base Codex et l'absorption crate par crate
- [ ] Given la porte de format, when elle s'exécute sur la note migrée, then elle passe, avec ou sans marqueur de dispense selon ce que le contenu d'origine permet
- [ ] Given les documents qui liaient l'ancien chemin, when la porte de liens s'exécute après la migration, then elle passe
- [ ] Given un déplacement fait sans réécriture de l'en-tête, when la porte de format s'exécute, then elle échoue, ce qui interdit une migration purement mécanique
- [ ] Le déplacement est fait de façon à ce que `git log --follow` retrouve l'historique du fichier d'origine
- [ ] Given un lecteur qui n'ouvre que les quatre premières lignes, when il lit la note, then il connaît le verdict sans lire le corps

#### US-053: Traitement des trois audits de parité
**Description:** En tant que mainteneur, je veux que les trois audits datés cessent d'être des fichiers orphelins à la racine de `docs/`, afin qu'un lecteur sache s'il regarde une mesure périmée ou une décision en vigueur.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-052

**Acceptance Criteria:**
- [ ] Le PRD tranche par écrit, dans la note produite, si les trois audits deviennent trois notes ou une note unique qui les référence, avec la raison retenue
- [ ] Given l'audit du 24 juillet qui pèse 348 Ko, when la solution retenue est appliquée, then aucune note de l'arbre ne dépasse un ordre de grandeur qui la rende illisible, ou bien le README documente explicitement l'exception
- [ ] Given la chaîne de liens relatifs entre les audits du 24, du 25 et du 27, when la migration est faite, then la porte de liens passe
- [ ] Given le lien depuis `docs/parity/README.md`, when la migration est faite, then il résout
- [ ] Given un audit qui est une mesure et non une décision, when il entre dans l'arbre, then son statut et sa classe reflètent ce qu'il est, sans lui inventer une décision
- [ ] Given un audit déplacé sans reprise de ses liens internes, when la porte de liens s'exécute, then elle échoue, et la migration n'est pas considérée comme faite
- [ ] Aucun fichier de décision ou d'audit daté ne subsiste à la racine de `docs/`

---

### EP-016: Frontière avec le registre ADR

Empêche l'arbre de devenir un second système concurrent de `docs/DECISIONS.md` et répare les deux défaillances mesurées du registre existant.

**Definition of Done:** une règle écrite dit quelle forme reçoit quelle décision ; l'index de `DECISIONS.md` est vérifié mécaniquement ; les treize ADR portent leurs alternatives.

#### US-054: Règle de frontière et ancrage dans le guide d'agent
**Description:** En tant qu'agent de codage, je veux savoir si la décision que je viens de prendre est un ADR ou une note, afin de ne pas créer deux enregistrements concurrents de la même chose.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-047
**Référence dsh:** `AGENTS.md` de dsh, dont chaque règle porte un lien relatif vers sa note

**Acceptance Criteria:**
- [ ] `AGENTS.md` gagne une entrée pointant vers `docs/notes/README.md` dans sa table de lecture, et une règle de frontière dans sa section sur l'ordre d'autorité
- [ ] La règle donne un critère discriminant vérifiable, pas une préférence : un lecteur appliquant le critère à ADR-12 et à la décision migrée en US-052 obtient la même réponse que celle retenue ici
- [ ] `docs/notes/README.md` porte la règle réciproque et pointe vers `docs/DECISIONS.md`
- [ ] Given une décision qui relève de l'ADR, when elle est enregistrée, then elle n'a pas de note miroir dans l'arbre
- [ ] Given un agent lisant uniquement `AGENTS.md`, when il doit consigner une décision, then il trouve le chemin de l'arbre sans ouvrir `docs/ARCHITECTURE.md`
- [ ] L'ordre d'autorité en cas de désaccord entre une note et un ADR est explicite

#### US-055: Porte de cohérence de l'index des ADR
**Description:** En tant que mainteneur, je veux qu'un ADR absent du tableau récapitulatif de `docs/DECISIONS.md` fasse échouer la suite, afin que la défaillance observée sur ADR-13 ne puisse pas se reproduire.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-048

**Acceptance Criteria:**
- [ ] La porte extrait les titres de section `## ADR-N` et les lignes du tableau récapitulatif, puis compare les deux ensembles
- [ ] Given un ADR présent en section mais absent du tableau, when la porte s'exécute, then elle échoue en nommant l'identifiant manquant
- [ ] Given une ligne de tableau pour un ADR sans section correspondante, when la porte s'exécute, then elle échoue en nommant l'identifiant orphelin
- [ ] Given l'état actuel du dépôt, when la porte s'exécute avant correction, then elle échoue sur ADR-13, ce qui prouve que la porte détecte la défaillance réelle
- [ ] Le tableau récapitulatif est corrigé et la porte passe ensuite
- [ ] Given une numérotation d'ADR non contiguë, when la porte s'exécute, then elle ne fabrique pas d'erreur sur le trou

#### US-056: Alternatives manquantes des ADR existants
**Description:** En tant qu'agent de codage, je veux que chaque ADR dise ce qu'il a écarté, afin de ne pas reproposer une option déjà pesée par le mainteneur.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-055

**Acceptance Criteria:**
- [ ] ADR-7, ADR-9, ADR-11 et ADR-13 portent une section d'alternatives, ou déclarent explicitement que leurs alternatives ne sont pas reconstructibles
- [ ] Given une alternative consignée, when elle n'a pas réellement été pesée à l'époque, then elle n'est pas inventée et la dispense est utilisée à la place
- [ ] La porte de US-055 est étendue pour vérifier que chaque section `## ADR-N` porte une section d'alternatives ou la dispense
- [ ] Given un futur ADR ajouté sans alternatives, when la porte s'exécute, then elle échoue
- [ ] Given l'état du dépôt après correction, when la porte s'exécute, then les treize ADR passent

---

### EP-017: Adoption prouvée

Vérifie que le dispositif fonctionne sur un usage réel plutôt que sur ses seuls tests, et qu'il est exécutable là où la CI l'exécute déjà.

**Definition of Done:** la porte tourne dans la commande de test du workspace, sa commande est documentée, et la première note native du dépôt est celle qui enregistre ce lot.

#### US-057: Première note native
**Description:** En tant que mainteneur, je veux que la décision d'adopter cet arbre soit elle-même enregistrée comme une note conforme, afin que le dispositif soit prouvé par son premier usage réel et non par ses seuls tests.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-050, US-054

**Acceptance Criteria:**
- [ ] Une note sous `docs/notes/implemented/process/` datée du jour de l'adoption enregistre la décision d'adopter l'arbre
- [ ] Elle porte une vraie section d'alternatives, sans dispense, citant au minimum le maintien du fichier unique, MADR avec statut en front matter, et le schéma retenu
- [ ] Elle enregistre pourquoi `docs/DECISIONS.md` est conservé, en citant le nombre de références `ADR-N` mesuré dans le dépôt
- [ ] Elle enregistre la divergence assumée par rapport aux formats établis sur l'encodage du cycle de vie par le répertoire, et la contrepartie payée en US-051
- [ ] Given la porte de format, when elle s'exécute sur cette note, then elle passe
- [ ] Given cette note déplacée un jour vers `rejected/` sans que sa ligne de statut suive, when la porte s'exécute, then la suite échoue, ce qui prouve que le dispositif se garde lui-même
- [ ] Given un lecteur cherchant pourquoi l'arbre existe, when il parcourt `docs/notes/implemented/process/`, then il trouve la note par son nom sans index

#### US-058: Exécution et documentation de la porte
**Description:** En tant que mainteneur, je veux que la porte s'exécute par la commande de test que la CI lance déjà et que sa commande ciblée soit documentée, afin qu'elle ne dépende d'aucune discipline ni d'aucune infrastructure absente.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-049, US-051, US-055

**Acceptance Criteria:**
- [ ] `cargo test --workspace --no-fail-fast` exécute la porte sans configuration ni variable d'environnement
- [ ] La commande ciblée du crate est ajoutée à la table des signaux de vérification de `AGENTS.md`
- [ ] Given une note invalide introduite volontairement, when la commande du workspace s'exécute, then elle échoue et le message identifie le fichier et la règle en une ligne
- [ ] Given l'arbre valide, when la porte s'exécute, then elle termine en moins de deux secondes sur la machine de développement
- [ ] Aucune étape n'est ajoutée à `.github/workflows/ci.yml`
- [ ] Le choix de ne pas passer par un hook est tracé, `scripts/run-gates.ts` l. 670 à 672 et `lefthook.yml` de dsh montrant que la référence ne l'exécute pas non plus en pre-commit, la porte étant couverte par la commande de test existante
- [ ] Given un contributeur sans accès réseau, when il lance la suite, then la porte passe

## Functional Requirements

- FR-01: Le système doit rendre le cycle de vie et la classe d'une décision lisibles depuis le chemin du fichier, sans ouvrir celui-ci.
- FR-02: Le système doit refuser un fichier dont le statut déclaré contredit le répertoire qui le contient.
- FR-03: Le système doit refuser une note dépourvue de section d'alternatives, sauf dispense datée antérieure à l'adoption du format.
- FR-04: Le système doit rendre une erreur par violation, chacune nommant le fichier et la règle, plutôt que de s'arrêter à la première.
- FR-05: Le système doit ignorer le contenu des blocs de code délimités lorsqu'il analyse la structure d'un document.
- FR-06: Le système doit refuser un ADR présent dans `docs/DECISIONS.md` mais absent de son tableau récapitulatif, et réciproquement.
- FR-07: Le système doit refuser un lien Markdown relatif de `docs/` qui ne résout pas vers un fichier existant.
- FR-08: Le système ne doit émettre aucune requête réseau et ne doit lire aucun fichier hors du dépôt.
- FR-09: Le système ne doit pas exiger de fichier d'index de l'arbre, et doit refuser qu'un tel fichier soit ajouté.
- FR-10: Le système ne doit pas imposer d'enregistrer une décision pour toute modification, la règle d'écriture restant une convention humaine énoncée dans le README.

## Non-Functional Requirements

- **Performance:** la porte complète s'exécute en moins de 2 secondes sur un arbre de 200 notes, et n'ajoute pas plus de 5 secondes au temps de `cargo test --workspace` sur un cache chaud, contre un plafond de job CI de 45 minutes.
- **Isolation:** `agent-doc-gates` déclare 0 dépendance sur un crate `agent-*` et 0 nouvelle dépendance dans le `Cargo.toml` racine ; il n'apparaît dans le graphe d'aucun binaire livré.
- **Diagnosticabilité:** 100 % des violations produisent un message d'au plus une ligne contenant le chemin relatif du fichier fautif et le nom de la règle ; une exécution rapporte toutes les violations en une passe.
- **Lisibilité:** `docs/notes/README.md` fait au plus 150 lignes ; les trois premières lignes d'une note suffisent à en connaître le titre et le verdict.
- **Réseau:** 0 accès réseau, y compris pour la vérification des liens, qui ne résout que le relatif.
- **Reproductibilité:** la porte rend le même verdict quel que soit le répertoire de travail depuis lequel `cargo test` est lancé.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Arbre absent | `docs/notes/` n'existe pas encore | Zéro note, zéro erreur, pas de panique | aucun |
| 2 | Répertoire de cycle de vie inconnu | `docs/notes/draft/` créé à la main | Échec, valeurs autorisées listées | `structure: draft/ n'est pas un cycle de vie connu (autorisés : ...)` |
| 3 | Classe inconnue | note rangée sous une classe hors des six | Échec nommant le chemin et les classes valides | `structure: <chemin> : classe "<x>" inconnue` |
| 4 | Profondeur incorrecte | note posée directement sous un cycle de vie | Échec nommant la profondeur observée | `structure: <chemin> : attendu {cycle}/{classe}/fichier.md` |
| 5 | Nom non daté | `note-sur-le-cache.md` | Échec, fichier exclu des notes valides | `structure: <chemin> : le nom doit être aaaa-mm-jj-sujet.md` |
| 6 | Statut divergent du répertoire | note dans `implemented/` annonçant un autre statut | Échec nommant le désaccord | `format: <chemin> : statut incompatible avec le répertoire` |
| 7 | Jetons de format dans un bloc de code | README ou note contenant un exemple délimité | Lignes ignorées, document valide | aucun |
| 8 | Double ligne de statut | statut répété plus bas dans le corps | Échec | `format: <chemin> : la ligne de statut doit être unique` |
| 9 | Dispense sur une note récente | marqueur porté par une note postérieure à l'adoption | Échec citant la date d'adoption | `format: <chemin> : la dispense ne vaut que pour les notes antérieures au <date>` |
| 10 | Dispense et section d'alternatives ensemble | les deux présents | Échec demandant de retirer le marqueur | `format: <chemin> : retirer la dispense` |
| 11 | Lien relatif cassé après déplacement | note migrée entre deux cycles de vie | Échec nommant source, cible et ligne | `lien: <source>:<ligne> : <cible> est introuvable` |
| 12 | Index centralisé réintroduit | `docs/notes/INDEX.md` ajouté | Échec dédié citant l'interdiction | `structure: INDEX.md : index centralisé interdit` |
| 13 | ADR hors index | nouveau `## ADR-14` sans ligne de tableau | Échec nommant l'identifiant | `decisions: ADR-14 absent du tableau récapitulatif` |
| 14 | Encodage inattendu | fichier Markdown non UTF-8 | Échec propre nommant le fichier, sans panique non gérée | `format: <chemin> : illisible` |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Le schéma à deux axes n'a pas de précédent hors de DeepSeek Harness : MADR et adr-tools mettent le statut dans le fichier. Les déplacements cassent les liens et rendent `git log --follow` heuristique | High | Med | La porte de liens (US-051) est livrée avant la première migration ; le déplacement se fait de façon à préserver le suivi de renommage (US-052) ; la divergence est enregistrée comme telle dans la note native (US-057) |
| 2 | Effondrement de la pratique après le premier mois, mode d'échec documenté des registres d'ADR | Med | High | Aucune règle « toute PR » n'est imposée ; la porte ne vérifie que ce qui est écrit, elle n'exige pas d'écrire ; le succès se mesure au nombre de notes produites par les lots suivants, pas à un quota |
| 3 | Deux systèmes concurrents, l'arbre et le registre ADR, sans frontière nette | Med | High | Règle de frontière écrite des deux côtés avec critère discriminant vérifiable (US-054) ; ordre d'autorité explicite |
| 4 | Notes rétroactives écrites pour la forme, au contexte reconstruit après coup | Med | Med | Dispense datée (US-050) plutôt que fabrication d'alternatives ; interdiction explicite d'inventer une alternative (US-053, US-056) |
| 5 | L'audit de 348 Ko entre dans l'arbre et y devient une anomalie permanente | Med | Low | US-053 tranche explicitement entre éclatement, note de renvoi et exception documentée, avant de déplacer quoi que ce soit |
| 6 | La porte devient une nuisance : messages illisibles, échecs en cascade sur une erreur unique | Low | Med | Une erreur par violation, jamais d'arrêt à la première (FR-04) ; message d'une ligne nommant fichier et règle (NFR) |
| 7 | Le crate `agent-doc-gates` dérive vers un outil général et entre dans le graphe du binaire | Low | High | `cargo tree` vérifié en critère d'acceptation (US-048) ; responsabilité unique déclarée dans le doc-comment du crate |

## Non-Goals

- **Pas de traduction.** DeepSeek Harness maintient une contrepartie chinoise par note et une porte d'appariement. Pyxis n'adresse pas ce public et doublerait le coût de chaque modification.
- **Pas d'arbre archivé gelé.** Le tri par valeur future, le manifeste de hachages et le gel permanent supposent un corpus de plusieurs centaines de notes. À reconsidérer au-delà de 100 notes actives.
- **Pas de règle « toute PR non triviale contient une note ».** DeepSeek Harness l'impose et compte 726 notes ; Pyxis a un mainteneur unique, et la contrainte utile est « toute décision qu'on pourrait vouloir rejouer ».
- **Pas de dissolution de `docs/DECISIONS.md`.** 167 références `ADR-N` dans 35 fichiers en font un point d'ancrage stable. Le registre reste, la frontière est écrite.
- **Pas de hook git.** Aucune infrastructure de hook n'existe dans le dépôt, et DeepSeek Harness lui-même n'exécute pas sa porte de format en pre-commit mais dans un agrégat de portes.
- **Pas de `justfile` ni d'agrégat de commandes.** C'est le lot #5 du plan de portage, dont ce lot ne dépend pas.
- **Pas de catalogue généré depuis l'arbre.** Aucun index centralisé n'est produit, par décision. C'est le lot #6 qui traite des artefacts générés.
- **Pas de vérification des ancres de liens.** Seule l'existence du fichier cible est vérifiée ; valider les fragments demanderait un analyseur de titres complet pour un gain marginal.

## Files NOT to Modify

- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés et empreintés, jamais édités à la main
- `crates/agent-parity/src/lib.rs` et `crates/agent-parity/src/client_model.rs` : portent les deux pins `BASELINE_COMMIT`, dont le déplacement est une décision explicite hors de ce lot
- `docs/app-server/protocol.schema.json` et `docs/app-server/protocol.d.ts` : régénérés uniquement par la commande dédiée
- `docs/parity/offline-suite.md` et `crates/agent-parity/tests/offline_suite.rs` : le second parse le premier ; renommer ou déplacer un test qui y est listé casse la porte existante
- `.github/workflows/ci.yml` : la porte est couverte par la commande de test existante, aucune étape n'est ajoutée
- `/home/arthur/dev/deepseek-harness` : dépôt de référence externe, lecture seule ; aucun commit, aucune écriture, et aucune transcription de son TypeScript vers Pyxis
- `spikes/` : espace de travail jetable exclu de la compilation
- Le clone Codex résolu par `$PYXIS_CODEX_BASELINE` : lecture seule absolue

## Technical Considerations

- **Emplacement de la porte :** recommandé, un crate `crates/agent-doc-gates` ramassé automatiquement par `members = ["crates/*"]`. L'alternative est un fichier de test dans `agent-parity`, moins coûteux mais en désaccord avec le rôle déclaré de ce crate, qui est la vérification de la baseline Codex. Ingénierie à confirmer.
- **Nom du crate :** `agent-doc-gates` plutôt que `agent-docs`, qui entrerait en collision avec le nom d'un sous-agent utilisé quotidiennement sur ce dépôt. Le nom dit ce que le crate est, un ensemble de portes documentaires, et non un dépôt de documentation. À rejeter si un nom plus court est préféré.
- **Structure interne :** recommandé, un walker partagé qui rend `(notes, erreurs)` et deux tests d'intégration qui le consomment, sur le modèle exact du couplage `agent-note-tree.ts` / portes de DeepSeek Harness. Faut-il une lib publique ou tout dans `tests/` ? Une lib permet le partage entre les deux tests ; c'est le seul argument.
- **Parsing :** recommandé, un analyseur artisanal ligne par ligne, comme `offline_suite.rs`. Une crate Markdown apporterait la robustesse sur les cas tordus au prix d'une dépendance que le lot s'interdit. Le seul cas non trivial est le suivi des blocs de code délimités.
- **Vérification des liens :** la porte doit-elle couvrir uniquement `docs/`, ou aussi `AGENTS.md`, `README.md` et `CONTRIBUTING.md` à la racine ? Le lot recommande d'inclure la racine, puisque c'est de là que partiront les liens vers l'arbre.
- **Migration :** `git mv` préserve le suivi de renommage mieux qu'une suppression suivie d'une création, mais le contenu change aussi (en-tête réécrit). Faut-il deux commits, le déplacement puis la réécriture, pour garder l'historique lisible ? Recommandé : oui.
- **Format de la ligne de statut :** recommandé, une valeur qui reprend littéralement le nom du répertoire, ce qui rend la comparaison exacte et sans table de correspondance. L'alternative, un statut en français accordé au répertoire anglais, ajoute une table pour un gain esthétique.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Enregistrements de décision vérifiés mécaniquement | 0 sur 17 | 100 % des notes de l'arbre | Fin du lot | La porte passe dans `cargo test --workspace` |
| ADR absents du tableau récapitulatif de `DECISIONS.md` | 1 sur 13 (ADR-13) | 0 | Fin du lot | Porte US-055 |
| ADR sans section d'alternatives | 4 sur 13 (ADR-7, 9, 11, 13) | 0 | Fin du lot | Porte US-056 |
| Documents de décision datés à la racine de `docs/` | 4 | 0 | Fin du lot | `ls docs/*.md` |
| Liens Markdown internes cassés | non mesuré | 0 | Fin du lot | Porte US-051 |
| Notes produites par les lots suivants du plan de portage | 0 | au moins 1 par lot livré | Month-6 | Comptage dans `docs/notes/` |
| Règles de `AGENTS.md` liées à leur justification | 0 | au moins les règles issues des lots 3 à 11 | Month-6 | Liens relatifs depuis `AGENTS.md` |

## Open Questions

- Les trois audits de parité deviennent-ils trois notes, une note de renvoi, ou restent-ils hors de l'arbre comme mesures ? Tranché par le mainteneur dans US-053, avant tout déplacement ; l'audit de 348 Ko est ce qui rend la question réelle.
- La classe `simplification` est-elle retenue ? Le tableau du plan de portage n'en cite que cinq et l'omet, alors que DeepSeek Harness la ferme à six et qu'elle y est peuplée dans les quatre cycles de vie. Ce PRD retient six classes ; à confirmer en US-047, l'ensemble étant fermé et son extension délibérée.
- La porte de liens couvre-t-elle les fichiers Markdown de la racine en plus de `docs/` ? À trancher en US-051 ; le lot recommande oui.
- Quel critère discriminant sépare un ADR d'une note ? À formuler en US-054 de façon vérifiable, faute de quoi les deux systèmes se recouvriront en six mois.
- Faut-il enregistrer les lots 1 et 5 du plan de portage comme notes une fois livrés, rétroactivement ou non ? Dépend de la réponse à la question précédente.
[/PRD]
