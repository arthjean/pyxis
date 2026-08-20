[PRD]
# PRD: Débordement de sortie d'outil

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-20 | Arthur Jean | Rédaction initiale, lot #4 du plan de portage DeepSeek Harness |

## Problem Statement

Une sortie d'outil trop grosse n'est pas déplacée hors du contexte : elle est **détruite**. Six défauts mesurés sur l'état actuel du dépôt.

1. **Bash jette les octets à l'acquisition.** Le lecteur de `crates/agent-tools/src/bash.rs` draine à mesure (`out.bytes.drain(0..overflow); out.omitted += overflow`), donc la sortie complète n'existe jamais en mémoire. Une compilation de 10 Mio laisse 30 000 octets de queue et un compteur d'omission exact. Le début, où se trouve la première erreur, est perdu définitivement, et aucune relance ne le rend si la commande n'est pas déterministe.
2. **Le champ prévu pour le localisateur porte une constante.** `ToolResultTruncation.continuation_hint` (`crates/agent-core/src/tools.rs`) est rempli dans `bound_feedback` par la chaîne fixe `"Re-run the tool with a narrower query or explicit range."`. Relancer est le seul recours proposé au modèle, et il est indisponible exactement dans les cas qui comptent : build non reproductible, réponse réseau, session d'exécution déjà consommée.
3. **L'endroit qui tronque ne peut pas écrire.** `bound_feedback` vit dans `agent-core`, dont le `Cargo.toml` documente que la seule dépendance hors cœur autorisée est `agent-tokenizer`, headless, et qui n'a aucune I/O. La décision de tronquer est structurellement séparée de toute capacité de persistance : il n'existe aujourd'hui aucun endroit du dépôt où une sortie d'outil puisse être sauvegardée.
4. **`read` ment sur sa propre promesse.** Son message de dépassement annonce `read by ranges with offset/limit`, alors que `read.rs:87` lit `file.take((MAX_BYTES + 1) as u64)` **depuis l'octet 0** et applique `offset` comme un numéro de ligne à l'intérieur de ce préfixe de 2 000 000 octets. Aucune valeur d'`offset` n'atteint l'octet 2 000 001. Un artefact de 10 Mio serait relisable à 20 %, et le message d'invitation à paginer ne peut pas être honoré.
5. **`grep` saute en silence.** `MAX_FILE_BYTES = 5_000_000` (`crates/agent-tools/src/grep.rs`) : au-delà, le fichier n'est ni lu ni signalé. Un modèle qui cherche dans un fichier de 10 Mio reçoit « aucun résultat », faux négatif indiscernable d'une absence réelle. La seconde voie de relecture est donc muette au moment précis où elle servirait.
6. **La troncature est hors contrat.** `ToolResultView` porte déjà `truncation: Option<ToolResultTruncation>` (`crates/agent-core/src/event.rs:328`), mais `docs/EVENT_SCHEMA.md:67` documente `tool_result` comme `{id, content, is_error, error_kind?, untrusted}` et aucun code de `crates/agent-tui/` ne la rend. Un champ traverse le fil JSONL sans être documenté, et y écrire un chemin le publierait sans que le contrat le dise.

**Why now :** le lot #3 vient de livrer ADR-14 et la règle « borner le rappel, jamais la clé de détection », qui a fixé la zone de bornage de `crates/agent-core/src/tools.rs`. Le lot #4 traverse la même zone et doit s'y brancher pendant que la décision est fraîche, plutôt que de réécrire une seconde discipline de bornage ailleurs. Par ailleurs le chiffre de 10 Mio circule déjà dans le dépôt sans que rien ne sache le stocker : `MAX_CELL_OUTPUT_BYTES = 10 * 1024 * 1024` (`crates/agent-code-mode/src/protocol.rs:20`) autorise une cellule à produire cette quantité, et `crates/agent-tools/src/exec_session.rs` prouve sur 12 Mio que `omitted + kept == produced`, c'est-à-dire prouve exactement que les octets manquants sont perdus.

## Overview

Le lot ajoute un seul mécanisme : avant qu'une sortie d'outil trop grosse ne soit réduite, elle est écrite en entier dans un fichier, et le résultat rendu au modèle devient un aperçu borné suivi du chemin de ce fichier. Rien d'autre ne change dans la boucle. Le champ qui portera ce chemin existe déjà, `continuation_hint`, et le plafond qui déclenche le déversement existe déjà, `MAX_TOOL_OUTPUT_BYTES`. Le lot ne crée ni clé de configuration, ni outil visible du modèle, ni événement.

Le découpage en trois responsabilités reste celui de la référence, parce qu'il est le seul point non évident de sa conception : le **stockage** ne sait que persister un texte et rendre un localisateur, la **politique** ne sait que décider quand déverser et composer le remplacement, et le **vocabulaire** garde le localisateur opaque pour son consommateur. Ce qui diverge est l'emplacement de la racine. La référence choisit un répertoire privé sous le tmp du système, créé par `mkdtemp` pour que le suffixe soit imprévisible ; Pyxis ne le peut pas, parce que `confine` (`crates/agent-tools/src/path.rs:70`) refuse à `read` et `grep` tout chemin hors du workspace, ce qui rendrait l'artefact illisible et le déversement inutile. La racine est donc `.pyxis/spill/` sous le workspace, et l'imprévisibilité du chemin est remplacée par une propriété plus forte que Pyxis possède déjà : `.pyxis` figure dans `PROTECTED_SUBPATHS`, donc aucun outil, bash compris, ne peut y écrire, tandis que `confine` seul autorise la lecture. Le runtime écrit, les outils lisent, et le modèle ne peut pas planter un lien symbolique dans la racine. Les permissions restrictives et l'ouverture exclusive sont conservées telles quelles contre les autres utilisateurs locaux de la machine.

Deux conséquences ne sont pas optionnelles, sous peine de livrer un mécanisme qui promet un fichier illisible. La relecture doit devenir vraie : `read` doit adresser un octet au-delà de son plafond au lieu de relire toujours le même préfixe, et `grep` doit dire qu'il a sauté un fichier au lieu de rendre un résultat vide. Et la racine, placée sous le workspace, entre dans le parcours de `grep` et de `glob`, qui n'ont aujourd'hui aucun filtre : sans exclusion, chaque recherche récursive relirait le contenu déversé, ce qui remettrait dans le contexte exactement ce que le lot en retire.

La décision est enregistrée en ADR-15. Le test de frontière d'`AGENTS.md` tranche seul : une pull request sur `crates/` peut déplacer la racine hors du workspace, la rendre inscriptible par un outil, ou transformer une panne de stockage en `is_error`. Rien dans l'arbre de notes n'a compétence sur une décision qu'un diff peut violer.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Octets d'une sortie d'outil perdus définitivement au-delà du plafond | 0 quand le stockage est disponible | 0, sans régression |
| Plafond du résultat rendu au modèle, aperçu et notice compris | `MAX_TOOL_OUTPUT_BYTES`, prouvé par test sur 10 Mio | idem |
| Octet adressable le plus haut par `read` dans un fichier de 10 Mio | le dernier | le dernier |
| Panne de stockage transformant un appel réussi en `is_error` | 0 | 0 |
| Clés de configuration ajoutées | 0 | 0 |
| Outils visibles du modèle ajoutés | 0 | 0 |

## Target Users

### Modèle en cours de tour
- **Role:** le consommateur du `tool_result`, seul destinataire de l'aperçu et du localisateur.
- **Behaviors:** relit par plages avec `offset` et `limit` quand un message le lui dit, cherche avec `grep` avant de lire, relance une commande quand il n'a pas d'autre recours.
- **Pain points:** une sortie de compilation dont il ne voit que la queue, une invitation à paginer que l'outil n'honore pas au-delà de 2 Mo, un `grep` muet sur les gros fichiers.
- **Current workaround:** relancer la commande en espérant qu'elle produise moins, ou la rediriger vers un fichier avec un shell puis lire ce fichier, ce qui consomme deux appels d'outil et un tour supplémentaire.
- **Success looks like:** un aperçu qui montre les deux extrémités, un chemin, et une relecture par fenêtres qui atteint réellement n'importe quel octet.

### Utilisateur qui lit le transcript
- **Role:** Arthur, ou tout utilisateur du TUI ou du mode `-p`.
- **Behaviors:** relit un transcript après coup, rejoue un fichier JSONL, cherche la sortie complète d'une commande qui a échoué.
- **Pain points:** la sortie complète n'existe nulle part, ni dans le transcript ni sur le disque ; le compteur d'omission dit combien a été perdu, ce qui documente la perte sans la réparer.
- **Current workaround:** relancer la commande à la main dans un autre terminal.
- **Success looks like:** un fichier sous `.pyxis/spill/` qu'il peut ouvrir avec ses propres outils, et un transcript qui ne grossit pas de 10 Mio pour autant.

### Agent de codage modifiant le déversement
- **Role:** un agent au contexte vierge chargé d'une tâche touchant le stockage, le plafond ou la relecture.
- **Behaviors:** lit `AGENTS.md`, cherche l'enregistrement qui justifie une constante ou un emplacement avant de le changer.
- **Pain points:** rien n'explique pourquoi la racine est sous le workspace plutôt que dans le tmp, ni pourquoi une panne d'écriture ne doit pas remonter au modèle.
- **Current workaround:** aucun, le mécanisme n'existe pas.
- **Success looks like:** ADR-15 répond aux deux questions, et une modification qui les viole casse un test nommé.

## Research Findings

### Competitive Context
- **Claude Code** persiste la sortie dépassant son plafond dans `~/.claude/projects/<project>/<session-id>/tool-results/<tool-use-id>.txt` et injecte un bloc `<persisted-output>` portant la taille, le chemin et un aperçu des deux premiers kilo-octets ; la relecture passe par l'outil `Read` ordinaire, sans outil dédié. Le plafond de capture de Bash vaut environ 30 000 caractères, avec troncature au milieu. Sources : [anthropics/claude-code#23948](https://github.com/anthropics/claude-code/issues/23948), [#19901](https://github.com/anthropics/claude-code/issues/19901).
- **Codex CLI** a longtemps tronqué en dur à 10 Kio ou 256 lignes, puis a exposé une limite en tokens, et déverse aujourd'hui les sorties de hooks au-delà d'environ 2 500 tokens vers le tmp du système, avec aperçu tête et queue plus chemin, la rétention étant déléguée au nettoyage du système d'exploitation. Sources : [openai/codex#6426](https://github.com/openai/codex/issues/6426), [PR #21069](https://github.com/openai/codex/pull/21069).
- **Cursor, Aider, Cline, OpenHands, Goose :** point mince, aucune documentation primaire d'un déversement vers fichier trouvée. Tous tronquent ou compactent. Aucun chiffre ne leur est attribué ici.
- **Market gap :** aucun des harnais relevés ne documente de politique de rétention, et aucun ne garantit que le fichier déversé soit intégralement relisable par ses propres outils. Le second point est précisément là où Pyxis échouerait aujourd'hui, `read` plafonnant à 2 Mo depuis l'octet 0.

### Best Practices Applied
- Remplacer l'observation par un pointeur plutôt que par un résumé : la fidélité reste exacte et restaurable, contrairement à une compression avec perte. Le système de fichiers sert de mémoire externe, relue sélectivement par `read` et `grep`. Sources : [LangChain, filesystems for context engineering](https://www.langchain.com/blog/how-agents-can-use-filesystems-for-context-engineering), [context offloading](https://aipatternbook.com/context-offloading).
- Ne jamais tronquer en silence : le message doit dire qu'une omission a eu lieu, de combien, et comment récupérer. Une troncature non signalée conduit le modèle à traiter l'aperçu comme complet. Source : [tool result truncation, the silent bug](https://dev.to/gabrielanhaia/tool-result-truncation-the-silent-bug-that-makes-agents-lie-3epe).
- Une seule copie : Claude Code a re-sérialisé la sortie complète dans son journal de session malgré le déversement, rendant la reprise inutilisable à 12 Mo. Source : [anthropics/claude-code#23948](https://github.com/anthropics/claude-code/issues/23948). Vérifié sur Pyxis : `bound_feedback` est appliqué avant l'émission (`crates/agent-core/src/agent.rs:1554`) et `ToolResultView::from_model` clone le contenu déjà réduit (`crates/agent-core/src/event.rs:318`), donc le déversement en amont produit mécaniquement une copie unique. La propriété est acquise, elle doit être préservée.
- Création sécurisée : un chemin prévisible laisse pré-créer un fichier ou un lien symbolique ([CWE-377](https://cwe.mitre.org/data/definitions/377.html)) ; l'ouverture exclusive ferme la fenêtre entre le contrôle et l'usage ([CWE-59](https://cwe.mitre.org/data/definitions/59.html), [CWE-367](https://cwe.mitre.org/data/definitions/367.html)) ; un nom dérivé d'une entrée non fiable doit être assaini de façon injective ([CWE-22](https://cwe.mitre.org/data/definitions/22.html)).

### Implémentation de référence : DeepSeek Harness

Racine du dépôt de référence, en lecture seule : **`/home/arthur/dev/deepseek-harness`**, commit `141eb6fef8` du 2026-08-19, licence MIT. Les deux tableaux de cette section abrègent les chemins pour rester lisibles ; ils se résolvent tous sous cette racine, et chaque story répète le chemin absolu complet pour être exploitable sans revenir ici. Les numéros de ligne valent pour ce commit. **Aucune ligne de TypeScript n'est transcrite** : dsh est en TypeScript, Pyxis en Rust, ce qui se reprend est la décision de conception, jamais le code, et cette contrainte suffit à écarter toute question de licence ou d'inventaire de portage.

Cinq fichiers à lire, dans cet ordre :

| Fichier dsh | À quoi il sert ici |
|---|---|
| `/home/arthur/dev/deepseek-harness/.agents/notes/implemented/architecture/2026-07-08-tool-output-spill-files.md` | La note de décision : pourquoi la création appartient au runtime et non à l'outil `write`, ce qui est hors périmètre, ce qui est différé. La lecture d'entrée |
| `/home/arthur/dev/deepseek-harness/packages/spill/spill/src/types.ts` (73 lignes) | Le vocabulaire : localisateur opaque, propriétaire, source descriptive, référence rendue |
| `/home/arthur/dev/deepseek-harness/packages/spill/spill-local/src/store.ts` (120 lignes, à lire intégralement) | La mécanique de stockage : racine privée, encodage de segment, ouverture exclusive |
| `/home/arthur/dev/deepseek-harness/packages/spill/spill-policy/src/index.ts` (232 lignes) | La politique : quand déverser, et la réserve d'octets de la notice |
| `/home/arthur/dev/deepseek-harness/packages/spill/spill-policy/README.md` | Le contrat en prose, la répartition des responsabilités entre politique générique et déversement possédé par l'outil |

Ancres exactes, décision par décision.

| Décision | Ancre dsh | Ce qui se reprend | Ce qui ne se reprend pas | Story |
|---|---|---|---|---|
| Localisateur opaque | `spill/src/types.ts:13-18` | Le consommateur affiche le localisateur, ne le parse jamais | Le type marqué (`Branded`), sans objet en Rust où un type nouveau suffit | US-070 |
| Propriétaire, espace de nommage au moment de la sauvegarde | `spill/src/types.ts:30-39` | Le regroupement par session, pour qu'un futur nettoyage puisse tomber par session | Le passage du `sessionId` jusqu'au point d'écriture : Pyxis résout la racine par thread dans le binaire et ne fait descendre qu'un chemin déjà namespacé | US-070, US-072 |
| Source purement descriptive | `spill/src/types.ts:41-53` | Le nom d'outil et l'identifiant d'appel servent le nom de fichier et l'inspection, jamais un contrôle d'accès | Le champ `label`, qui distingue chez dsh le résultat de la copie de journal de dispatch, deuxième bras hors périmètre ici | US-070 |
| Le stockage ne fait qu'une chose | `spill/src/index.ts:8-12` et `:41-43` | Sauvegarder du texte, rendre un localisateur, échouer franchement sur une vraie panne, laisser l'appelant décider de la dégradation | Le service abstrait et son enregistrement dans un conteneur : Pyxis a une implémentation et aucune seconde en vue | US-070 |
| Racine privée et imprévisible | `spill-local/src/store.ts:19-30` | Les permissions restrictives sur la racine, contre les autres utilisateurs locaux | `mkdtemp` sous le tmp du système : `confine` (`path.rs:70`) refuse la lecture hors workspace, donc l'artefact serait illisible. Divergence assumée numéro 1 | US-070, US-072 |
| Encodage de segment injectif | `spill-local/src/store.ts:35-63` | La propriété : injectif sur toutes les chaînes, neutralise `../`, les chemins absolus, l'octet NUL et les séparateurs, échappe `.` et `..` en entier, la chaîne vide ne produit jamais un segment vide | L'encodage par unité de code UTF-16 : Rust encode par point de code | US-071 |
| Répertoire par session | `spill-local/src/store.ts:66-76` | Le hachage court et stable de l'identifiant, pour grouper sans exposer l'identifiant dans le chemin | La construction à chaque sauvegarde : Pyxis résout la racine une fois | US-070 |
| Écriture exclusive | `spill-local/src/store.ts:96-119` | Le préfixe aléatoire devant le nom lisible, l'ouverture qui échoue sur tout chemin existant, lien symbolique compris, et les permissions propriétaire seul | Rien | US-070 |
| Absence de configuration vaut désactivation | `spill-policy/src/index.ts:110-119` | La règle : pas de stockage disponible signifie pas de déversement, pas un défaut implicite | La clé `maxInlineBytes` et sa validation au chargement : l'invariant 15 interdit la clé, l'absence est portée par `Option` | US-073 |
| Texte simple uniquement | `spill-policy/src/index.ts:79-87`, `:200-201` | Un résultat portant un bloc non textuel n'est pas touché | Rien | US-073 |
| `read` exclu | `spill-policy/src/index.ts:195-197`, `spill-policy/README.md:18` | L'exclusion, pour éviter la boucle relire puis déverser puis relire | Le second bras qui déverse quand même la copie de journal : hors périmètre | US-073 |
| Meilleur effort strict | `spill-policy/src/index.ts:138-161`, `spill-policy/README.md:31` | Pas de propriétaire, pas de stockage, ou échec d'écriture : journaliser et rendre le résultat original. Un échec ne transforme jamais un appel réussi en erreur | Rien | US-073 |
| Réserve d'octets de la notice | `spill-policy/src/index.ts:163-187` | Le coût de la notice réservé à l'intérieur du plafond, prix calculé au pire cas, et l'abandon du remplacement si même la notice seule dépasse | Rien | US-074 |
| Aperçu tête et queue | `spill-policy/src/index.ts:94-102` | Le partage du budget entre les deux extrémités, la moitié haute allant à la tête | Le composant de rétention dédié : Pyxis a déjà `largest_fitting` et `truncate_tail` | US-074 |
| Texte de la notice | `spill-policy/src/index.ts:104-108` | La forme : omission chiffrée, localisateur, indication de récupération, le tout sur une ligne parenthésée | Le texte littéral et le chemin absolu. Divergence assumée numéro 2 | US-074 |
| Le déversement à l'acquisition appartient à l'outil | `spill-policy/README.md:37`, `2026-07-08-tool-output-spill-files.md:132-146` | La règle : la politique générique ne voit que le résultat final, donc un outil dont le flux est déjà réduit avant ce point doit déverser lui-même | Le report en travail différé : Pyxis ne peut pas le reporter, `bash` est le producteur des 10 Mio du signal de vérification. Divergence assumée numéro 3 | US-076 |
| Dépendance à la relecture | `2026-07-08-tool-output-spill-files.md:180-182` | L'aveu explicite : la valeur du stockage local dépend de la capacité de `read` et `grep` à inspecter le chemin, et une politique de confinement future doit soit autoriser ce chemin, soit changer de stockage | Rien. Pyxis est exactement le cas que la note anticipe, et il est traité dans EP-025 plutôt que différé | US-077, US-078 |
| Rétention absente | `spill-local/README.md:55-58`, `2026-07-08-tool-output-spill-files.md:157-163` | L'aveu : les fichiers persistent, parce qu'une session reprise ou dérivée peut encore référencer un chemin | L'absence totale de borne : Pyxis ajoute un plafond de répertoire, priorisé bas | US-081 |

**Divergences assumées.** Quatre, chacune forcée par une contrainte de Pyxis absente chez dsh.

1. **La racine est dans le workspace, pas dans le tmp du système.** `confine` refuse à `read` et `grep` tout chemin hors workspace, et le sandbox Landlock (`crates/agent-sandbox/src/fs.rs:329-373`) n'ouvre en écriture que le workspace et les répertoires d'état déclarés. Une racine hors workspace produirait un artefact que le modèle ne peut ni lire ni chercher, c'est-à-dire un déversement inutile. La perte d'imprévisibilité est compensée par une propriété que dsh n'a pas : `.pyxis` est dans `PROTECTED_SUBPATHS` (`crates/agent-tools/src/path.rs:33`), donc aucun outil ne peut y écrire, et le modèle ne peut pas planter de lien symbolique dans la racine.
2. **Le localisateur est relatif au workspace.** dsh rend un chemin absolu. Pyxis rend `.pyxis/spill/...`, pour trois raisons : `read` résout déjà le relatif contre le workspace, aucun chemin absolu ne fuit dans le fil JSONL ni vers l'app-server, et la notice plus courte laisse plus de budget à l'aperçu.
3. **`bash` déverse au fil de l'acquisition.** dsh classe ce cas en travail différé. Pyxis ne le peut pas : le signal de vérification du lot porte sur 10 Mio, et le seul producteur courant de cette quantité est `bash`, dont le lecteur détruit les octets avant que quiconque puisse les voir.
4. **Il n'y a pas de couture abstraite de stockage.** dsh sépare un service abstrait de son implémentation locale pour permettre un stockage distant. Pyxis est mono-machine, l'app-server tourne à côté du workspace, et le budget de complexité interdit une abstraction sans exigence actuelle. Une struct concrète, un module, aucun trait.

## Assumptions & Constraints

**Assumptions**
- Un artefact déversé restant sous le workspace est lisible par `read` et `grep` sans changement de politique. **Vérifié** : `confine` (`path.rs:70`) n'accepte que la relation d'ancêtre avec le workspace et ne consulte pas `PROTECTED_SUBPATHS`, qui n'est lu que par `guard_write_target` (`path.rs:98`).
- Aucun outil ne peut écrire dans la racine de déversement. **Vérifié** : `PROTECTED_SUBPATHS` contient `.pyxis` et `guard_protected_path` est appelé avant la décision de permission, donc ni `DontAsk` ni `BypassPermissions` ne le lèvent.
- Le déversement en amont de `bound_feedback` produit une copie unique. **Vérifié** : `agent.rs:1554` puis `event.rs:318`. Cette propriété est ce qui protège Pyxis du défaut de journal observé chez Claude Code, et elle doit être préservée par la position du point d'insertion.
- Un modèle à qui l'on donne un chemin relatif l'utilise tel quel avec `read`. **Non validable hors ligne**, versé en Open Questions. Le coût du pari est un appel d'outil raté et un message d'erreur explicite.
- Les noms d'outils MCP, qui viennent d'un serveur tiers, sont la seule source non fiable du nom de fichier suggéré. **Risque faible**, converti en test par US-071 : l'encodage est prouvé injectif indépendamment de la provenance.

**Constraints**
- **Invariant 1** (`docs/ARCHITECTURE.md`) : `agent-core` ne dépend que d'`agent-tokenizer` et n'a aucune I/O. Le déversement ne peut donc pas vivre là où `bound_feedback` décide de tronquer.
- **Invariant 3** : toute sortie d'outil est non fiable par défaut et le taint se propage. Un artefact déversé est de la sortie d'outil écrite sur disque, et sa relecture par `read` reste non fiable, `returns_untrusted` valant `true` par défaut (`crates/agent-tools/src/tool.rs:353`).
- **Invariant 15** : aucune clé de configuration publique pour l'orchestration. Le plafond et la racine sont des constantes de crate ; rien n'entre dans `crates/agent-cli/src/settings.rs`.
- Un `tool_result` par `tool_use` : le remplacement change le contenu d'un résultat, jamais son nombre ni son identifiant.
- Le sandbox est appliqué une fois, au démarrage, sur des répertoires existants : la racine doit être créée avant `enforce_sandbox` (`crates/agent-cli/src/main.rs:777`) et passée dans `agent_state_dirs`, exactement comme `sessions_dir` (`main.rs:973`).
- Langue : `docs/` en français, code, commentaires et messages destinés au modèle en anglais.
- Le clone du baseline Codex résolu par `$PYXIS_CODEX_BASELINE` est en lecture seule. Aucune matrice de parité ne référence la troncature de sortie d'outil, donc aucune régénération n'est due.

## Quality Gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace --no-fail-fast
```

Signaux ciblés : `cargo test -p agent-tools` (stockage, politique, `bash`, `read`, `grep`), `cargo test -p agent-core` (bornage et vue d'événement), `cargo test -p agent-doc-gates` (registre ADR et liens internes).

## Epics & User Stories

### EP-022: Le stockage

Le module qui persiste un texte et rend un localisateur, et rien d'autre : ni politique de déclenchement, ni rétention, ni API de relecture.

**Priority:** P0
**Definition of done:** un texte de 10 Mio est écrit dans un fichier sous `.pyxis/spill/`, avec des permissions propriétaire seul, un nom dérivé d'une entrée non fiable sans possibilité de traversée, une ouverture qui échoue sur tout chemin préexistant, et un localisateur relatif au workspace ; la racine est accessible en écriture sous Landlock.

#### US-070: Module de stockage du déversement
**Priority:** P0 | **Size:** M (3 pts) | **Dependencies:** None

**Description:** En tant que développeur du runtime, je veux un point unique capable d'écrire une sortie d'outil complète sur disque, afin que la décision de tronquer cesse d'être une décision de détruire.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill/src/index.ts:8-12` et `:41-43`, `/home/arthur/dev/deepseek-harness/packages/spill/spill-local/src/store.ts:96-119`. Commit `141eb6fef8`, lecture seule.

**Acceptance Criteria:**
- [ ] Un module `crates/agent-tools/src/spill.rs` expose une struct concrète portant sa racine résolue, et une méthode qui prend le nom d'outil, l'identifiant d'appel et le texte complet, puis rend un localisateur et le nombre d'octets écrits.
- [ ] Le fichier est créé sous `<racine>/<préfixe hexadécimal aléatoire>-<nom assaini>`, avec une ouverture exclusive qui échoue si le chemin existe déjà, lien symbolique compris, et des permissions propriétaire seul ; le répertoire est créé avec des permissions propriétaire seul également.
- [ ] Le localisateur rendu est un chemin **relatif au workspace**, et un test prouve qu'il ne contient aucun préfixe absolu.
- [ ] Une vraie panne d'écriture, permissions ou disque plein, rend une erreur typée : le module ne dégrade pas lui-même, il laisse l'appelant décider.
- [ ] Aucun trait n'est introduit : une seule implémentation existe, et le commentaire de module dit que l'abstraction est refusée tant qu'un second stockage n'est pas requis.
- [ ] **Unhappy path :** deux sauvegardes du même nom d'outil dans la même milliseconde produisent deux fichiers distincts, et un test le prouve en appelant deux fois avec des arguments identiques.

#### US-071: Encodage injectif du nom de fichier
**Priority:** P0 | **Size:** S (2 pts) | **Dependencies:** Blocked by US-070

**Description:** En tant que développeur du runtime, je veux qu'un nom d'outil arbitraire ne puisse jamais devenir un chemin, afin qu'un serveur MCP tiers ne dirige pas une écriture hors de la racine.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill-local/src/store.ts:35-63`. Commit `141eb6fef8`, lecture seule.

**Acceptance Criteria:**
- [ ] Une fonction transforme une chaîne quelconque en un segment de chemin unique, en gardant littéraux les caractères alphanumériques, le point, le tiret et le tiret bas, et en échappant tout le reste, l'échappement lui-même étant échappé.
- [ ] Les jetons `.` et `..` pris en entier sont échappés, et la chaîne vide produit un segment non vide.
- [ ] Un test prouve l'injectivité sur un jeu d'entrées comprenant `../../etc/passwd`, `/etc/passwd`, une chaîne contenant un octet nul, une chaîne contenant un séparateur, `.`, `..` et la chaîne vide : aucune paire distincte ne produit le même segment.
- [ ] Un test prouve qu'aucune de ces entrées ne produit un chemin résolu hors de la racine.
- [ ] **Unhappy path :** un nom d'outil de plus de 255 octets une fois encodé est refusé ou tronqué de façon déterministe avant l'appel système, et le test dit lequel des deux, la limite de nom de fichier du système étant sinon atteinte au moment de l'écriture.

#### US-072: Racine créée avant l'enfermement et déclarée au sandbox
**Priority:** P0 | **Size:** S (2 pts) | **Dependencies:** Blocked by US-070

**Description:** En tant qu'utilisateur du binaire, je veux que la racine de déversement soit inscriptible sous Landlock, afin que le mécanisme fonctionne dans le mode où le sandbox est réellement appliqué.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill/src/types.ts:30-39` pour le regroupement par session. Commit `141eb6fef8`, lecture seule. Le câblage suit le précédent interne de `sessions_dir`, pas dsh.

**Acceptance Criteria:**
- [ ] `crates/agent-cli/src/main.rs` crée `<workspace>/.pyxis/spill/<hachage court du thread>` avant l'appel à `enforce_sandbox`, au même endroit et de la même façon que `sessions_dir` (`main.rs:973`).
- [ ] Ce chemin est passé dans `agent_state_dirs`, donc ajouté à l'ensemble inscriptible par `writable_dirs` (`crates/agent-sandbox/src/fs.rs:361-373`).
- [ ] `ToolCtx` reçoit un champ optionnel portant le stockage ; sa valeur par défaut est l'absence, qui vaut désactivation complète du déversement, ce qui préserve le comportement des tests et des appels hors binaire.
- [ ] Le nom du répertoire est un hachage court de l'identifiant de thread, jamais l'identifiant en clair : le chemin apparaît dans le contexte du modèle et dans le fil JSONL.
- [ ] Aucune clé n'est ajoutée à `crates/agent-cli/src/settings.rs`, et un test ou une lecture de diff le confirme.
- [ ] **Unhappy path :** si la création de la racine échoue au démarrage, le binaire journalise et démarre sans déversement, exactement comme il démarre aujourd'hui, plutôt que de refuser de démarrer.

### EP-023: La politique de déversement

Quand déverser, et quel résultat le modèle voit à la place. Ce que la référence appelle la politique, et rien d'autre.

**Priority:** P0
**Definition of done:** une sortie dépassant `MAX_TOOL_OUTPUT_BYTES` est écrite en entier, le résultat rendu au modèle est un aperçu des deux extrémités suivi d'une notice portant le localisateur, l'ensemble tenant sous le plafond, et toute défaillance du stockage laisse le résultat original visible sans `is_error`.

#### US-073: Point de déversement et règles d'exclusion
**Priority:** P0 | **Size:** M (3 pts) | **Dependencies:** Blocked by US-070, US-072

**Description:** En tant que modèle, je veux que seules les sorties qui gagnent à être déversées le soient, afin de ne pas payer un aller-retour pour relire ce que je viens de demander.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill-policy/src/index.ts:110-119` et `:190-209`, `/home/arthur/dev/deepseek-harness/packages/spill/spill-policy/README.md:18` et `:31`. Commit `141eb6fef8`, lecture seule.

**Acceptance Criteria:**
- [ ] Le déversement est décidé dans `run_one_inner` de `crates/agent-tools/src/registry.rs`, après la production de la sortie et avant la construction de l'issue, seul point où le texte complet existe encore et où le contexte de l'appel est connu.
- [ ] Le déclencheur est la taille en octets du contenu comparée à `MAX_TOOL_OUTPUT_BYTES` ; aucune constante nouvelle n'est introduite pour le seuil.
- [ ] Un résultat portant des images ou un contenu structuré n'est pas touché.
- [ ] L'outil `read` est exclu, avec un commentaire nommant la boucle qu'il évite : relire, déverser, relire à nouveau.
- [ ] Absence de stockage, ou échec d'écriture, journalise à l'échelon avertissement et rend le résultat original inchangé ; un test prouve qu'un échec d'écriture ne met pas `is_error` à vrai et ne change pas le contenu.
- [ ] **Unhappy path :** un appel dont la sortie dépasse le plafond et dont le stockage échoue produit exactement le même `tool_result` qu'aujourd'hui, troncature comprise, et un test compare les deux.

#### US-074: Remplacement borné, notice réservée
**Priority:** P0 | **Size:** M (3 pts) | **Dependencies:** Blocked by US-073

**Description:** En tant que modèle, je veux voir les deux extrémités de la sortie et savoir combien manque, afin de décider si la relecture vaut un appel d'outil.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill-policy/src/index.ts:94-108` et `:163-187`. Commit `141eb6fef8`, lecture seule.

**Acceptance Criteria:**
- [ ] Le remplacement est composé d'un aperçu de tête et d'un aperçu de queue, le budget étant partagé entre les deux, la moitié supérieure allant à la tête, chaque coupe tombant sur une frontière de caractère.
- [ ] Le coût en octets de la notice est réservé à l'intérieur du plafond avant le calcul de l'aperçu, et le prix est calculé au pire cas du compte d'omission, de sorte que la notice finale ne soit jamais plus longue que ce qui a été réservé.
- [ ] Un test prouve sur une sortie de 10 Mio que le résultat rendu au modèle, aperçu et notice compris, ne dépasse pas `MAX_TOOL_OUTPUT_BYTES`.
- [ ] Si la notice seule ne tient pas sous le plafond, le remplacement est abandonné et le résultat original est conservé ; un commentaire dit que le fichier déjà écrit est un orphelin inoffensif.
- [ ] La notice dit le nombre d'octets omis, le localisateur, et la manière de relire ; elle est en anglais, comme tout contenu destiné au modèle.
- [ ] **Unhappy path :** un caractère multi-octets chevauchant exactement la limite de tête produit un remplacement UTF-8 valide, et le test le construit explicitement.

#### US-075: Localisateur porté par le contrat existant
**Priority:** P0 | **Size:** S (2 pts) | **Dependencies:** Blocked by US-074

**Description:** En tant que consommateur du fil d'événements, je veux que le chemin déversé arrive dans un champ documenté, afin de ne pas découvrir un chemin dans un champ que le contrat ne mentionne pas.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill/src/types.ts:13-18` pour l'opacité du localisateur. Commit `141eb6fef8`, lecture seule.

**Acceptance Criteria:**
- [ ] Le localisateur voyage dans `ToolResultTruncation.continuation_hint`, et `original_bytes` porte la taille réelle de la sortie complète, non celle de l'aperçu.
- [ ] `docs/EVENT_SCHEMA.md` documente le champ `truncation` de `tool_result`, ses sous-champs, et dit explicitement que `continuation_hint` peut contenir un chemin relatif au workspace.
- [ ] Un test prouve qu'un résultat déversé sérialisé en JSONL contient le chemin relatif et aucun chemin absolu.
- [ ] Le consommateur ne parse pas le localisateur : aucun code de `crates/agent-tui/` ni de `crates/agent-app-server/` ne le découpe ni ne le reconstruit.
- [ ] **Unhappy path :** un résultat non déversé continue de porter `truncation` à l'absent quand il tient sous le plafond, et le champ reste omis de la sérialisation.

### EP-024: La sortie que la politique ne peut pas atteindre

Le cas que la référence classe en travail différé et que le signal de vérification du lot rend obligatoire : un flux réduit avant que le résultat final n'existe.

**Priority:** P0
**Definition of done:** une commande produisant 10 Mio sur sa sortie standard laisse un fichier de 10 Mio sur le disque, et le résultat rendu au modèle reste borné.

#### US-076: `bash` déverse au fil de l'acquisition
**Priority:** P0 | **Size:** L (5 pts) | **Dependencies:** Blocked by US-070, US-072

**Description:** En tant qu'utilisateur lançant une compilation bavarde, je veux que le début de la sortie survive, afin de lire la première erreur plutôt que la dernière.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill-policy/README.md:37` et `/home/arthur/dev/deepseek-harness/.agents/notes/implemented/architecture/2026-07-08-tool-output-spill-files.md:132-146`, qui posent la règle sans la livrer. Commit `141eb6fef8`, lecture seule. Le comportement est propre à Pyxis et doit être argumenté comme tel dans ADR-15.

**Acceptance Criteria:**
- [ ] Le lecteur de `crates/agent-tools/src/bash.rs` ouvre un fichier de déversement au premier dépassement du plafond en mémoire, et y écrit les octets qu'il draine au lieu de les jeter ; en deçà du plafond, aucun fichier n'est créé et le comportement est inchangé.
- [ ] Le fichier contient la totalité des octets produits, dans l'ordre, et un test sur 10 Mio compare la taille du fichier au nombre d'octets produits.
- [ ] Le résultat rendu au modèle reste borné par le chemin existant et porte le localisateur ainsi que le compte d'omission exact, la propriété `omitted + kept == produced` déjà prouvée restant vraie.
- [ ] Le flux d'erreur standard suit la même règle que la sortie standard, ou le commentaire dit pourquoi il en diffère.
- [ ] Une erreur d'écriture en cours de drain désactive le déversement pour l'appel, journalise, et laisse la commande se terminer normalement : la sortie n'est pas perdue davantage qu'aujourd'hui et l'appel n'échoue pas.
- [ ] **Unhappy path :** une commande annulée en cours de production laisse un fichier partiel dont le contenu correspond à ce qui a été effectivement lu, et le test prouve que l'annulation ne laisse pas de descripteur ouvert.

### EP-025: La relecture

Rendre vraie la promesse portée par la notice. La référence signale explicitement cette dépendance, et Pyxis est le cas qu'elle anticipe.

**Priority:** P0
**Definition of done:** n'importe quel octet d'un fichier de 10 Mio est atteignable par `read`, `grep` ne saute plus un fichier en silence, et une recherche récursive ne relit pas le contenu déversé.

#### US-077: `read` adresse réellement un fichier au-delà de son plafond
**Priority:** P0 | **Size:** M (3 pts) | **Dependencies:** None

**Description:** En tant que modèle relisant un artefact déversé, je veux que `offset` atteigne la fin d'un fichier de 10 Mio, afin que l'invitation à paginer soit honorée.

**Source de conception :** `/home/arthur/dev/deepseek-harness/.agents/notes/implemented/architecture/2026-07-08-tool-output-spill-files.md:180-182`, qui pose que la valeur du stockage local dépend de la capacité des outils de lecture à inspecter le chemin. Commit `141eb6fef8`, lecture seule. La mécanique est propre à Pyxis.

**Acceptance Criteria:**
- [ ] `read` cesse de lire un préfixe de `MAX_BYTES` depuis l'octet 0 : les lignes précédant `offset` sont sautées en flux, et le plafond s'applique à la fenêtre émise, non au fichier.
- [ ] Un test lit la dernière ligne d'un fichier de 10 Mio en une seule invocation, avec un `offset` la désignant, et prouve que le contenu rendu est celui de la fin du fichier.
- [ ] Le message de continuation existant continue de dire la plage rendue et le prochain `offset`, et devient exact : suivre l'indication de proche en proche parcourt tout le fichier.
- [ ] Le message de dépassement partiel est réécrit ou retiré selon qu'il reste vrai, aucun message ne devant survivre à la propriété qu'il décrivait.
- [ ] Aucun paramètre n'est ajouté au schéma de l'outil : `offset` et `limit` suffisent, et le contrat visible du modèle ne change pas.
- [ ] **Unhappy path :** un `offset` supérieur au nombre de lignes du fichier rend un message explicite disant le nombre de lignes réel, et non un contenu vide silencieux.

#### US-078: `grep` dit qu'il a sauté un fichier
**Priority:** P1 | **Size:** S (2 pts) | **Dependencies:** None

**Description:** En tant que modèle cherchant dans un artefact déversé, je veux savoir qu'un fichier a été ignoré, afin de ne pas conclure à une absence de correspondance.

**Source de conception :** aucune. dsh ne traite pas ce cas ; c'est une conséquence du choix de racine de Pyxis, à argumenter dans ADR-15.

**Acceptance Criteria:**
- [ ] Un fichier dépassant `MAX_FILE_BYTES` produit une ligne de signalement nommant le chemin et la raison, au lieu d'être ignoré sans trace.
- [ ] Le signalement est borné : un répertoire contenant de nombreux fichiers trop gros produit un compte agrégé plutôt qu'une ligne par fichier au-delà d'un petit nombre.
- [ ] Le signalement suggère `read` avec `offset` et `limit`, seule voie que le lot rend réellement complète.
- [ ] **Unhappy path :** une recherche ne rencontrant aucun fichier trop gros produit exactement la sortie d'aujourd'hui, sans ligne supplémentaire, et un test le prouve.

#### US-079: La racine de déversement sort du parcours de recherche
**Priority:** P0 | **Size:** S (2 pts) | **Dependencies:** Blocked by US-072

**Description:** En tant qu'utilisateur payant les tours, je veux qu'une recherche récursive ne relise pas ce qui vient d'être déversé, afin que le mécanisme ne remette pas dans le contexte ce qu'il en a retiré.

**Source de conception :** aucune. dsh place sa racine hors de l'arbre parcouru et n'a pas ce problème ; c'est le prix de la divergence numéro 1.

**Acceptance Criteria:**
- [ ] Le parcours de `crates/agent-tools/src/grep.rs` et celui de `crates/agent-tools/src/glob.rs` n'entrent pas dans `.pyxis`, l'exclusion étant dérivée de `PROTECTED_SUBPATHS` plutôt que réécrite en dur.
- [ ] Un chemin explicitement demandé sous `.pyxis` reste lisible : l'exclusion porte sur le parcours, pas sur le confinement, faute de quoi la notice deviendrait inutilisable.
- [ ] Un test prouve qu'une recherche à la racine du workspace ne rend aucune correspondance provenant d'un fichier déversé, alors que la même recherche visant directement le chemin déversé la rend.
- [ ] Le commentaire dit que l'exclusion couvre aussi les fichiers de session, aujourd'hui parcourus par la même absence de filtre.
- [ ] **Unhappy path :** un workspace sans répertoire `.pyxis` produit exactement les mêmes résultats qu'avant le changement.

### EP-026: Enregistrement et bornes

Ce qui empêche la décision d'être re-litigée, et ce qui empêche le disque de croître sans borne.

**Priority:** P0
**Definition of done:** ADR-15 existe, est indexé, la porte documentaire est verte, et la taille de la racine de déversement est bornée par une constante.

#### US-080: ADR-15, déversement de sortie d'outil
**Priority:** P0 | **Size:** M (3 pts) | **Dependencies:** Blocked by US-073, US-076, US-079

**Description:** En tant qu'agent de codage modifiant le déversement, je veux un enregistrement qui dise pourquoi la racine est dans le workspace et pourquoi une panne ne remonte pas au modèle, afin de ne pas re-litiger une décision déjà pesée.

**Acceptance Criteria:**
- [ ] `docs/DECISIONS.md` reçoit `## ADR-15` au format du registre : Contexte, Décision, Justification, Alternatives écartées, Conséquences et risques, à la suite d'ADR-14.
- [ ] La justification nomme les trois contraintes qui forcent l'emplacement : le refus de `confine` hors workspace, l'écriture Landlock limitée aux répertoires déclarés, et l'interdiction d'écriture des outils sur `.pyxis`.
- [ ] Les alternatives écartées nomment au moins la racine sous le tmp du système, l'outil de relecture dédié, la couture de stockage abstraite et la clé de configuration du seuil, chacune avec la raison du refus.
- [ ] La ligne d'index du tableau récapitulatif est ajoutée et `cargo test -p agent-doc-gates` est vert.
- [ ] Aucune note miroir n'est écrite dans `docs/notes/` : la règle de frontière d'`AGENTS.md` l'interdit pour une décision qu'une pull request sur `crates/` peut violer.
- [ ] `docs/ARCHITECTURE.md` décrit le déversement dans la partie consacrée aux outils et lie ADR-15, sans ajouter d'invariant numéroté redondant avec lui.
- [ ] **Unhappy path :** retirer temporairement la ligne d'index fait échouer `cargo test -p agent-doc-gates`, ce qui est vérifié une fois puis annulé.

#### US-081: Plafond de la racine de déversement
**Priority:** P2 | **Size:** S (2 pts) | **Dependencies:** Blocked by US-072

**Description:** En tant qu'utilisateur d'un workspace de longue vie, je veux que les artefacts déversés cessent de s'accumuler sans borne, afin qu'une session bavarde ne remplisse pas le disque.

**Source de conception :** `/home/arthur/dev/deepseek-harness/packages/spill/spill-local/README.md:55-58` et `/home/arthur/dev/deepseek-harness/.agents/notes/implemented/architecture/2026-07-08-tool-output-spill-files.md:157-163`, qui documentent l'absence de rétention comme une limite connue. Commit `141eb6fef8`, lecture seule. Pyxis ajoute la borne que dsh n'a pas.

**Acceptance Criteria:**
- [ ] Une constante de crate fixe la taille maximale cumulée de `.pyxis/spill/`, et le commentaire justifie l'ordre de grandeur.
- [ ] Au démarrage d'un thread, si la racine dépasse ce plafond, les répertoires de threads les plus anciens sont supprimés jusqu'à repasser dessous ; le répertoire du thread courant n'est jamais candidat.
- [ ] La suppression ne touche que des chemins sous `.pyxis/spill/`, vérifié avant l'appel système, et un test prouve qu'un chemin hors de cette racine est refusé.
- [ ] Le nettoyage journalise ce qu'il supprime, à l'échelon information : une suppression silencieuse d'un fichier qu'un transcript référence serait indiagnosticable.
- [ ] **Unhappy path :** une racine sous le plafond ne supprime rien, et une erreur de suppression journalise sans empêcher le thread de démarrer.

## Functional Requirements

| ID | Requirement | Story |
|----|-------------|-------|
| FR-01 | Une sortie d'outil dépassant `MAX_TOOL_OUTPUT_BYTES` est écrite intégralement dans un fichier avant toute réduction | US-070, US-073 |
| FR-02 | Le fichier est créé sous `.pyxis/spill/`, avec des permissions propriétaire seul et une ouverture qui échoue sur tout chemin préexistant | US-070 |
| FR-03 | Le nom du fichier dérive du nom d'outil par un encodage injectif qui ne peut produire ni traversée ni chemin absolu | US-071 |
| FR-04 | Le localisateur rendu au modèle est relatif au workspace | US-070, US-075 |
| FR-05 | Le résultat rendu au modèle, aperçu et notice compris, ne dépasse jamais `MAX_TOOL_OUTPUT_BYTES` | US-074 |
| FR-06 | La notice dit le nombre d'octets omis, le localisateur et la manière de relire | US-074 |
| FR-07 | L'absence de stockage, ou son échec, laisse le résultat original inchangé et ne met jamais `is_error` à vrai | US-073 |
| FR-08 | L'outil `read` n'est jamais déversé | US-073 |
| FR-09 | Un résultat portant un contenu non textuel n'est pas déversé | US-073 |
| FR-10 | `bash` écrit sa sortie complète dans le fichier de déversement au fil de l'acquisition | US-076 |
| FR-11 | Tout octet d'un fichier de 10 Mio est atteignable par `read` au moyen d'`offset` et `limit` | US-077 |
| FR-12 | `grep` signale tout fichier qu'il n'a pas lu à cause de sa taille | US-078 |
| FR-13 | Un parcours récursif de `grep` ou `glob` n'entre pas dans `.pyxis` | US-079 |
| FR-14 | La racine de déversement est créée avant l'application du sandbox et déclarée dans les répertoires d'état inscriptibles | US-072 |
| FR-15 | Le système n'ajoute aucune clé de configuration ni aucun outil visible du modèle | US-072, US-077 |
| FR-16 | La taille cumulée de la racine de déversement est bornée par une constante | US-081 |

## Non-Functional Requirements

| ID | Category | Requirement | Measurement |
|----|----------|-------------|-------------|
| NFR-01 | Contexte | Le résultat rendu au modèle pour une sortie de 10 Mio reste sous 30 000 octets | Test sur 10 Mio comparant la longueur du contenu |
| NFR-02 | Fidélité | Le fichier déversé contient exactement les octets produits, sans réencodage ni normalisation de fin de ligne | Comparaison octet à octet sur 10 Mio |
| NFR-03 | Mémoire | Le déversement de `bash` n'augmente pas la mémoire résidente au-delà du plafond en mémoire déjà en vigueur, quelle que soit la taille produite | Test sur 10 Mio, le tampon en mémoire restant borné par `MAX_OUTPUT` |
| NFR-04 | Latence | Le déversement ajoute au plus une écriture séquentielle de la taille de la sortie ; aucune relecture du fichier n'a lieu pendant l'appel | Revue du chemin de code, absence d'appel de lecture dans le module |
| NFR-05 | Sécurité | Le répertoire est en permissions propriétaire seul, les fichiers aussi, et l'ouverture est exclusive : CWE-377, CWE-59, CWE-367 fermées | Test des bits de permission et test d'ouverture sur un chemin préexistant |
| NFR-06 | Sécurité | Aucune entrée non fiable ne peut produire un chemin hors de la racine : CWE-22 fermée | Test d'injectivité et de confinement sur le jeu d'entrées hostiles |
| NFR-07 | Contexte | Une sortie déversée n'apparaît qu'une fois dans le fil : le contenu complet n'est jamais sérialisé dans le fichier de session | Test comparant la taille du JSONL avant et après un appel de 10 Mio |
| NFR-08 | Surface publique | Zéro clé ajoutée à `crates/agent-cli/src/settings.rs`, zéro variante ajoutée à `AgentEvent`, zéro outil ajouté au registre | Lecture du diff |
| NFR-09 | Disque | La racine de déversement reste sous une constante de crate documentée | Test créant des répertoires de threads factices au-delà du plafond |

## Edge Cases & Error States

| # | Scénario | Déclencheur | Comportement attendu | Message |
|---|---|---|---|---|
| 1 | Sortie juste au-dessus du plafond | Sortie de 30 001 octets | Déversement, remplacement plus court que l'original | Notice standard |
| 2 | Notice plus longue que le plafond | Plafond réduit ou localisateur très long | Aucun remplacement, contenu original conservé | Avertissement journalisé, rien au modèle |
| 3 | Stockage absent | Appel hors binaire, ou création de racine échouée au démarrage | Comportement d'aujourd'hui, troncature sans localisateur | Aucun |
| 4 | Écriture refusée | Permissions, disque plein | Résultat original conservé, appel réussi | Avertissement journalisé |
| 5 | Écriture refusée en cours de drain `bash` | Disque plein pendant une commande longue | Déversement désactivé pour l'appel, commande menée à son terme | Avertissement journalisé |
| 6 | Chemin préexistant | Collision, ou lien symbolique planté | L'ouverture échoue, aucun écrasement | Traité comme un échec d'écriture |
| 7 | Nom d'outil hostile | Outil MCP nommé `../../etc/passwd` | Encodé en un segment unique sous la racine | Aucun |
| 8 | Caractère multi-octets à la coupe | Aperçu tombant au milieu d'un caractère | Coupe sur la frontière, UTF-8 valide | Notice standard |
| 9 | Relecture au-delà de 2 Mo | `read` avec un `offset` élevé sur un fichier de 10 Mio | Fenêtre correcte rendue | Indication de continuation exacte |
| 10 | `offset` au-delà de la fin | Modèle qui suit une indication périmée | Message explicite disant le nombre de lignes réel | Message d'erreur nommant la limite |
| 11 | Recherche récursive après déversement | `grep` à la racine du workspace | Aucune correspondance issue de `.pyxis` | Aucun |
| 12 | Fichier trop gros pour `grep` | Artefact de 10 Mio visé directement | Signalement explicite du saut | Ligne nommant le chemin et la raison |
| 13 | Commande annulée en cours | Annulation pendant une sortie volumineuse | Fichier partiel cohérent, aucun descripteur fuité | Aucun |
| 14 | Thread repris | Reprise référençant un localisateur ancien | Le fichier existe encore si le plafond ne l'a pas évincé, sinon `read` échoue normalement | Erreur de fichier absent |
| 15 | Racine au-dessus du plafond | Workspace de longue vie | Éviction des répertoires de threads les plus anciens | Information journalisée |

## Risks & Mitigations

| # | Risque | Probabilité | Impact | Mitigation |
|---|---|---|---|---|
| 1 | Le déversement écrit sous le workspace, donc dans un dépôt Git de l'utilisateur | Certaine | Moyen | La racine est sous `.pyxis`, déjà exclue par convention et déjà utilisée pour les sessions ; US-079 la retire aussi du parcours des outils de recherche. Si `.pyxis` n'est pas ignoré par Git chez l'utilisateur, le problème préexiste au lot |
| 2 | Un artefact référencé par un transcript est évincé par le plafond | Moyenne | Faible | L'éviction porte sur les répertoires de threads les plus anciens, jamais sur le thread courant, et journalise. La relecture d'un artefact évincé produit une erreur de fichier absent, lisible |
| 3 | La boucle relire puis déverser puis relire réapparaît par une autre porte | Faible | Moyen | `read` est exclu du déversement, et US-077 rend la relecture paginée, donc chaque fenêtre reste très en dessous du plafond |
| 4 | Le taint traverse le disque : une sortie non fiable devient un fichier du workspace | Certaine | Moyen | La relecture reste non fiable, `returns_untrusted` valant `true` par défaut. ADR-15 énonce que le déversement ne blanchit rien, et l'invariant 3 continue de s'appliquer à la relecture |
| 5 | Le déversement au fil de l'acquisition ralentit `bash` sur une sortie volumineuse | Moyenne | Faible | Écriture séquentielle sans relecture, fichier ouvert seulement au premier dépassement ; en deçà du plafond, aucun coût |
| 6 | Deux mécanismes de bornage se superposent, `bound_feedback` et le remplacement | Certaine | Faible | Le remplacement tient sous `MAX_TOOL_OUTPUT_BYTES`, très en dessous des 64 Kio de `MAX_MODEL_TOOL_RESULT_BYTES`, donc `bound_feedback` n'a plus rien à tronquer. Un test prouve que `truncation` porte le localisateur et non le message générique |
| 7 | La modification de `read` change le comportement d'un outil très utilisé | Certaine | Élevé | Le contrat visible du modèle ne change pas, aucun paramètre n'est ajouté ; les tests existants de `read` restent verts, et le message de dépassement est réécrit dans le même changement que la propriété qu'il décrit |
| 8 | Le chemin apparaît dans le fil JSONL et l'app-server | Certaine | Faible | Chemin relatif au workspace, jamais absolu, et `docs/EVENT_SCHEMA.md` le documente dans le même lot |

## Non-Goals

- **Aucun outil de relecture visible du modèle.** La référence l'exclut également. `read` et `grep` suffisent une fois EP-025 livré, et un outil supplémentaire coûterait une place permanente dans le prompt système.
- **Aucun second bras pour la copie durable des sous-appels de Code Mode.** dsh borne aussi le journal de dispatch ; Pyxis rend au programme sa valeur complète, qui a déjà traversé la frontière, et le lot ne touche pas ce chemin.
- **Aucune couture de stockage abstraite.** Une seule implémentation existe et aucun stockage distant n'est requis. Le trait sera introduit le jour où un second stockage existe, pas avant.
- **Aucune politique de rétention par âge ni suppression à la fin de session.** Une session persistée, reprise ou dérivée peut encore référencer un chemin. Seul un plafond de taille est retenu, priorisé bas.
- **Aucun déversement des sessions d'exécution persistantes.** `exec_session` a son propre plafond et son propre compteur d'omission ; l'étendre est un lot distinct.
- **Aucune remontée du contenu déversé dans la compaction.** La compaction travaille sur le transcript, qui ne contient que l'aperçu.
- **Aucune clé de configuration pour le seuil ni pour la racine.** Interdit par l'invariant 15.
- **Aucun rendu de la troncature dans le TUI.** Le champ est documenté dans le contrat d'événement, son affichage est une décision de client, hors périmètre.

## Files NOT to Modify

- `/home/arthur/dev/deepseek-harness` : dépôt de référence en **lecture seule**. Il se lit au commit `141eb6fef8`, et rien n'y est écrit, commité, ni récupéré depuis l'amont. Aucune ligne de son TypeScript n'entre dans Pyxis, donc ni `docs/codex-port-inventory.md` ni `NOTICE-CODEX.md` ne sont concernés.
- Le clone du baseline Codex résolu par `$PYXIS_CODEX_BASELINE` : lecture seule, aucun commit, aucun checkout, aucune écriture.
- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés et empreintés, jamais édités à la main. Aucune matrice ne référence la troncature de sortie d'outil, donc aucune régénération n'est due.
- `crates/agent-cli/src/settings.rs` : aucune clé de configuration, invariant 15.
- `crates/agent-core/src/agent.rs` autour de `bound_feedback` : le bornage du cœur reste tel quel, le déversement se branche en amont dans `agent-tools`.
- `spikes/` : espace Phase 0 jetable et exclu.
- `docs/notes/` : aucune note pour cette décision, portée par ADR-15.

## Technical Considerations

- **Emplacement du point d'insertion.** Recommandé : `run_one_inner` de `crates/agent-tools/src/registry.rs`, seul endroit où le texte complet existe encore, où l'identifiant d'appel est connu, et où le résultat n'a pas encore été transformé en issue. Le hook `PostToolUse` qui suit observe sans réécrire, par décision explicite (US-019 AC4), donc il n'est pas candidat. L'ingénierie confirme que rien entre ce point et `bound_feedback` ne recopie le contenu.
- **Identité du propriétaire.** Recommandé : ne pas ajouter d'identifiant de session à `ToolCtx`, mais lui donner un stockage déjà namespacé, construit par le binaire au même endroit que `sessions_dir`. L'alternative, faire descendre un `ThreadId` d'`agent-runtime` jusqu'au contexte d'outil, introduit un concept de conversation dans un crate qui n'en a pas et n'apporte rien de plus au moment de l'écriture. À confirmer si un besoin futur exige de connaître le thread dans un outil.
- **Forme de l'aperçu.** Recommandé : tête et queue, contre la queue seule d'aujourd'hui. Compromis : la queue seule est ce que `truncate_tail` fait pour `bash`, et elle est bonne quand seul le verdict final compte ; la tête et la queue sont meilleures quand le fichier complet est récupérable, parce que l'aperçu ne sert plus à contenir l'information mais à décider s'il faut relire. `largest_fitting` fournit déjà la coupe sur frontière de caractère dans les deux sens.
- **Relecture de `read`.** Recommandé : sauter les lignes en flux avant `offset`, plafonner la fenêtre émise. Alternative : ajouter un décalage en octets, rejetée parce qu'elle ajoute un concept visible du modèle pour un gain nul, `offset` en lignes suffisant à atteindre n'importe quel octet par pas successifs. À confirmer : le coût du saut en flux sur un fichier de 10 Mio, mesurable par un test chronométré si l'ingénierie le juge utile.
- **Ordre des changements.** Le stockage et son câblage précèdent tout le reste ; `bash` et la relecture sont indépendants l'un de l'autre et peuvent avancer en parallèle une fois EP-022 livré ; ADR-15 se rédige en dernier, quand les trois décisions qu'il enregistre sont éprouvées.

## Success Metrics

| Métrique | Baseline (2026-08-20) | Cible | Horizon | Mesure |
|---|---|---|---|---|
| Octets récupérables d'une sortie de 10 Mio | 30 000 | 10 485 760 | à la livraison de EP-024 | Test comparant la taille du fichier aux octets produits |
| Octet le plus haut adressable par `read` | 2 000 000 | dernier octet du fichier | à la livraison de US-077 | Test lisant la dernière ligne d'un fichier de 10 Mio |
| Fichiers ignorés par `grep` sans trace | tous ceux au-dessus de 5 Mo | 0 | à la livraison de US-078 | Test sur un fichier de 10 Mio |
| Taille du résultat rendu au modèle pour 10 Mio | 30 000 | ≤ 30 000, localisateur compris | à la livraison de EP-023 | Test de longueur |
| Pannes de stockage visibles du modèle | sans objet | 0 | à la livraison de US-073 | Test d'échec d'écriture vérifiant `is_error` |
| Champs du fil JSONL non documentés dans `docs/EVENT_SCHEMA.md` | 1 (`truncation`) | 0 | à la livraison de US-075 | Lecture du contrat |
| Enregistrements gouvernant le déversement | 0 | 1 ADR indexé, porte verte | à la livraison de US-080 | `cargo test -p agent-doc-gates` |
| Correspondances de recherche provenant d'artefacts déversés | sans objet | 0 | à la livraison de US-079 | Test de parcours |

## Open Questions

| Question | Impact | Comment trancher |
|---|---|---|
| Un modèle utilise-t-il un chemin relatif tel quel, ou tente-t-il de le rendre absolu ? | Détermine si le localisateur doit finalement être absolu, au prix d'une fuite dans le fil | Observable dès les premiers tours réels : un appel `read` raté sur un chemin reconstruit suffit à trancher |
| L'aperçu tête et queue vaut-il mieux que la queue seule, une fois le fichier récupérable ? | Détermine la forme du remplacement pour `bash`, où la queue porte le verdict | Non mesurable hors ligne. À observer sur des sessions réelles, le compte de relectures effectives étant l'indicateur |
| Le plafond de la racine doit-il évincer par thread ou par fichier ? | Par thread est plus grossier mais rend la reprise prévisible : un thread est entier ou absent | Laissé ouvert jusqu'à US-081, priorisée P2 |
| Faut-il déverser aussi la sortie des sessions d'exécution persistantes ? | Étend le mécanisme à un second producteur de gros volumes | À rouvrir si un usage réel de `exec_command` produit des sorties au-delà du plafond de façon répétée |
[/PRD]
