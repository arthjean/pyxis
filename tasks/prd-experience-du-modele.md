[PRD]
# PRD: Expérience du modèle documentée

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-21 | Arthur Jean | Rédaction initiale, lot #7 du plan de portage DeepSeek Harness |
| 1.1 | 2026-08-21 | Arthur Jean | Ajout de la table de correspondance dsh vers Pyxis et des ancres `Source dsh` par story, pour que la reprise des décisions soit vérifiable ligne à ligne contre le dépôt source |

## Problem Statement

Pyxis envoie à chaque requête un préfixe dont il maîtrise l'ordre, la taille et la stabilité, et n'écrit nulle part ce que ce préfixe contient. Cinq défaillances mesurées sur l'état du dépôt au 2026-08-21 :

1. **La contrainte de préfixe cacheable vit en commentaires de code et dans aucun document.** Six sites la portent : `crates/agent-runtime/src/context.rs:220-221` (« Bumped only when the injected bytes moved. Two steps sharing a generation produced the same prefix »), `crates/agent-cli/src/context.rs:42,55,57,471` (« that is what keeps the cacheable prefix stable », « or a refresh would break the cached prefix »), et `crates/agent-provider/src/chatgpt.rs:72-73` (UUID v4 stable envoyé en `prompt_cache_key` à chaque requête). `docs/ARCHITECTURE.md` ne contient ni « KV », ni « prompt cache », ni « préfixe » : sa seule occurrence de « modèle voit » est un commentaire de pseudo-code ligne 292. La conséquence est asymétrique : le contrat du fournisseur pose que l'ordre du préfixe est `tools`, `system`, `messages` et que modifier une définition d'outil invalide le cache entier, or `docs/tool-catalog.md` rend 26 117 octets de descriptions et de schémas qui occupent précisément ce premier niveau. Une pull request qui reformule une `description()` d'outil jette le cache complet de toutes les sessions, et rien dans le dépôt ne permet à son relecteur de le savoir.

2. **Le texte que le modèle lit verbatim n'est inventorié nulle part.** 3 449 octets d'instructions embarquées (`crates/agent-cli/prompts/gpt5_generic.md`, 2 609 octets ; `crates/agent-cli/prompts/codex_finetuned.md`, 840 octets), chargées en dur par `crates/agent-provider/src/models/embedded.rs` quand le catalogue distant est injoignable. S'y ajoutent inconditionnellement `HARNESS` (`crates/agent-cli/src/prompt.rs:20-38`), qui déclare au modèle que la section gagne contre tout ce qui la précède, et conditionnellement `CODE_MODE_ONLY` (`prompt.rs:43-52`) quand `runtime.tool_mode.hides_nested_tools()`. Aucun document ne dit lequel de ces quatre textes s'ajoute quand, ni dans quel ordre, ni que le dernier réécrit la contradiction du premier.

3. **Les littéraux que le modèle lit dans les résultats d'outils sont invisibles, et ils viennent de quatre crates différents.** `PRUNED_PLACEHOLDER = "[tool result pruned to save context]"` (`crates/agent-core/src/compaction.rs:24`), `NOT_PUBLISHED` (`crates/agent-tools/src/context_window.rs:28`), le `continuation_hint` de `ToolResultTruncation` (`crates/agent-core/src/tools.rs:49`), et le corps HTTP `blocked by pyxis network allow-list: {host} (allowed: {allowed})` que le proxy renvoie (`crates/agent-sandbox/src/proxy.rs:400`) et qui remonte dans la sortie d'un `bash`. Quatre crates écrivent dans le transcript et aucun ne le déclare ; celui du sandbox n'est même pas soupçonnable depuis son nom.

4. **Un crate n'a aucun endroit où déclarer qu'il ne touche pas le modèle.** Il n'existe zéro `README.md` sous `crates/` : les seuls fichiers `.md` de l'arbre sont les deux prompts. Les seize crates portent 2 213 lignes de `//!` dont la fonction est d'orienter un lecteur dans le code, pas de décrire ce que le modèle voit. L'absence de documentation et l'absence d'effet sont donc indistinguables, et c'est le mode d'échec le plus coûteux : un relecteur qui ne trouve rien sur `agent-session` ne peut pas conclure.

5. **Aucune porte ne rattrape le trou, et aucune ne le pourrait dans son périmètre actuel.** Les sept portes d'`agent-doc-gates` (122 tests dans `crates/agent-doc-gates/tests/`) couvrent les notes, les liens, le registre ADR, l'inventaire de recettes, la prose prescriptive et le graphe de crates. Aucune ne lit `crates/` autrement que par les manifestes, et `markdown_documents` (`crates/agent-doc-gates/src/links.rs:25-38`) ne parcourt que les `.md` de la racine plus tout `docs/`.

**Why now:** le lot #6 vient de livrer `docs/tool-catalog.md` avec 29 sections `### \`nom\`` ancrables, et ce document n'a aujourd'hui aucun lien entrant vers l'une de ses ancres : la cible existe depuis quatre commits et rien ne la vise. Le même lot a livré `crate_directories()` et `collect_manifests()` (`crates/agent-doc-gates/src/crate_graph.rs`), c'est-à-dire l'énumération des seize crates que cette porte doit parcourir, elle aussi sans second usage. Les deux briques dont ce lot dépend sont neuves et inemployées. L'ordre du plan compte pour la même raison qu'au lot précédent : le lot #8 ajoute encore une porte documentaire, donc écrire celle-ci maintenant lui laisse un précédent au lieu de deux migrations. Enfin la dette de la défaillance 1 ne se stabilise pas : chaque outil ajouté grossit le premier niveau du préfixe, et le seul document qui a jamais écrit les seuils de compaction (`docs/parity/audits/parity-audit-2026-07-24.md:729,833,1488`) est un audit qu'`AGENTS.md:117-119` déclare non normatif.

## Overview

Le lot fait entrer dans le dépôt un contrat d'écriture et la porte qui le tient. Le contrat est un document normatif français, `docs/model-experience.md`, qui dit ce qu'un crate doit écrire quand il touche ce que le modèle lit : une section `## Model Experience` dans son `README.md`, un titre H3 par surface, et sous chacun trois champs H4 dans un ordre fixe, `#### What the model sees`, `#### Token effect`, `#### KV Cache effect`, chacun suivi d'exactement un paragraphe non vide. La porte est un huitième fichier de test d'`agent-doc-gates`, atteint par `just test` donc par `just check`, qui lit les seize crates et refuse un crate model-facing sans section.

De DeepSeek Harness, quatre décisions se reprennent et aucune ligne ne se copie, le vérificateur source (`/home/arthur/dev/deepseek-harness/scripts/verify-package-readme-model-experience.ts`, 557 lignes) étant du TypeScript. La première est le triplet ordonné et sa densité : trois champs, jamais deux, jamais quatre, et un paragraphe de prose sous chacun plutôt qu'un tableau, parce qu'un tableau invite la case vide et que le champ le plus utile est celui qui explique. La deuxième, et c'est celle qui rend le contrat tenable, est qu'il y a trois classifications et pas une exigence : la forme structurée, la forme courte auditée (`None, as ...` ou `Indirectly, through ...`, suivie quand même du champ de cache), et l'omission nominative. Chaque entrée d'omission porte sa justification écrite, ce qui distingue mécaniquement l'absence d'effet de l'oubli. La troisième est le croisement bidirectionnel de la liste : une entrée qui nomme un paquet inexistant échoue, et un paquet présent dans deux listes échoue. La quatrième est l'ancrage littéral : une entrée qui décrit le schéma d'un outil doit lier une section ancrée du catalogue généré, et le vérificateur valide le fragment contre les titres réels, ce qui interdit à la documentation de décrire un outil que le catalogue ne connaît pas.

Trois divergences volontaires viennent de la taille et des invariants de Pyxis. La classification n'est pas deux listes d'exception sur un défaut implicite, c'est une table exhaustive des seize crates : à cette échelle, un crate non classé peut échouer en nommant les trois formes disponibles, ce que dsh ne peut pas se permettre sur 227 paquets. Le précédent le plus proche est `#[expect]`, stabilisé en Rust 1.81, dont l'apport n'est pas la suppression mais le fait qu'une suppression devenue inutile redevienne une erreur. Ensuite, la vérification d'ancrage vit dans la nouvelle porte et pas dans la porte de liens : `crates/agent-doc-gates/src/links.rs:12-13` refuse les ancres par décision écrite et testée, refus déjà réaffirmé au changelog v1.1 du PRD des catalogues générés. La nouvelle porte, elle, doit de toute façon récolter les 29 titres H3 du catalogue pour son propre garde de littéral concret, donc le fragment se valide là où la donnée est déjà chargée, et la porte de liens gagne seulement les douze README dans son balayage pour que leurs liens relatifs cessent d'être invérifiés. Enfin le README de crate n'entre pas dans le rustdoc : `#![doc = include_str!("../README.md")]` créerait un registre unique séduisant, mais les seize crates portent `publish = false` (`Cargo.toml:12`), donc aucun rendu crates.io ne consomme ce fichier, et le coût réel de l'inclusion est un travail de liens intra-doc que le lot n'a aucune raison de payer.

La difficulté propre à Pyxis n'est ni la porte ni le format, c'est que la table du plan de portage désigne les mauvais fichiers. Elle annonce les données dans `crates/agent-core/src/budget.rs` et `crates/agent-core/src/prompt.rs` ; or `prompt.rs` d'`agent-core` ne porte aucun texte, il porte le plafond `MAX_NON_HISTORY_CONTEXT_BYTES = 64 * 1024` et la priorité de sacrifice, tandis que le texte verbatim vit dans `crates/agent-cli/prompts/`, dans `crates/agent-provider/src/models/embedded.rs` et dans les deux constantes de `crates/agent-cli/src/prompt.rs`. Le périmètre réel est donc plus large que deux crates : huit crates portent une surface structurée, dont `agent-app-server`, qui laisse un client externe injecter la `description` et le schéma d'un outil dans le même registre que les natifs (`crates/agent-app-server/src/bridge.rs:107-140`), et `agent-sandbox`, dont le seul littéral est un corps 403.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Crates model-facing portant leur section `## Model Experience` | 8/8 | tous |
| Crates du workspace sans classification écrite | 0/16 | 0, quel que soit le nombre de crates |
| Sites de préfixe cacheable documentés hors du code | 6/6 | tous |
| Littéraux lus par le modèle et non déclarés | 0/4 | 0 |
| Liens entrants vers une ancre de `docs/tool-catalog.md` | ≥ 1 | ≥ 1 |
| Textes système verbatim cités dans un document | 4/4 | tous |
| Dépendances ajoutées au workspace | 0 | 0 |
| Temps mur ajouté à `just test` sur cache chaud | ≤ 1 s | ≤ 1 s |
| Recettes ajoutées au `justfile` | 0 | 0 |

## Target Users

### Relecteur d'un changement de surface model-facing
- **Role:** Arthur Jean relisant une pull request qui reformule une description d'outil, ajoute une phrase au prompt système ou change un placeholder de troncature.
- **Behaviors:** lit le diff, juge la formulation, vérifie que le comportement décrit existe.
- **Pain points:** le diff ne dit pas que la ligne touchée est dans le premier niveau du préfixe cacheable, donc que sa modification invalide le cache de `tools`, de `system` et de `messages` d'un coup pour toutes les sessions. Trois octets changés dans `bash.rs` coûtent la même chose que trente lignes.
- **Current workaround:** aucun. La connaissance existe dans six commentaires répartis sur trois crates, qu'aucun chemin de lecture ne rassemble.
- **Success looks like:** le README du crate touché porte, sous le champ de cache de la surface concernée, la phrase qui dit ce que ce changement invalide ; le diff de la pull request contient cette phrase quand la surface change.

### Agent de codage ajoutant un outil ou un provider
- **Role:** Claude Code ou Codex recevant une tâche sur Pyxis avec un contexte vierge.
- **Behaviors:** lit `AGENTS.md`, suit la table « Where new behavior goes », écrit le module, l'enregistre, lance `just check`.
- **Pain points:** rien ne lui dit que sa `description()` est du texte que le modèle lit à chaque requête, ni qu'une description bavarde se paye sur chaque tour, ni que son résultat d'outil entre dans une fenêtre dont le plafond est de 64 Kio et dont la compaction se déclenche à 70 % et 80 % de `max_context - output_reserve` (`crates/agent-core/src/budget.rs`). Il écrit une description en optimisant la clarté seule.
- **Current workaround:** lire `budget.rs`, `compaction.rs`, `spill_policy.rs` et `context.rs` avant d'écrire trois lignes, ce que rien ne prescrit et que seul un agent qui doute déjà fait.
- **Success looks like:** un document normatif nommé dans `AGENTS.md`, une section à remplir dans le README du crate qu'il touche, et une porte qui refuse son changement tant que la section ment ou manque.

### Contributeur externe évaluant l'empreinte de Pyxis
- **Role:** développeur Rust arrivant par le `README.md` et cherchant ce que l'agent envoie réellement au fournisseur.
- **Behaviors:** cherche le prompt système, le catalogue d'outils, la politique de troncature.
- **Pain points:** `docs/tool-catalog.md` donne les 29 descriptions mais ne dit pas qu'elles sont envoyées à chaque requête ni dans quel ordre ; les 3 449 octets d'instructions embarquées ne sont nommés dans aucun document ; `HARNESS` n'est lisible qu'en ouvrant `crates/agent-cli/src/prompt.rs`.
- **Current workaround:** lire quatre fichiers source répartis sur trois crates.
- **Success looks like:** `crates/agent-cli/README.md` cite les textes verbatim dans des blocs `markdown`, dit lequel s'ajoute quand, et renvoie au catalogue pour les schémas.

## Research Findings

### Contexte concurrentiel

- **DeepSeek Harness**, la source du lot, tient ce contrat sur 227 paquets. La distribution est le renseignement principal : `SENTENCE_MODEL_EXPERIENCE` compte 125 entrées de forme courte (67 en `kind: 'none'`, 58 en `kind: 'indirect'`), `NO_MODEL_EXPERIENCE_SECTION` en compte 4, et une centaine de paquets seulement portent la forme structurée. Autrement dit, plus de la moitié des paquets déclarent explicitement ne pas toucher le modèle. Un contrat qui n'aurait prévu que la forme structurée aurait produit 125 sections creuses, et la note de décision de dsh (`/home/arthur/dev/deepseek-harness/.agents/notes/implemented/process/2026-07-12-package-model-experience-contract.md`) enregistre précisément ce refus parmi ses alternatives écartées, avec celui du catalogue central généré et celui des comptes de tokens numériques.
- **`markdownlint` MD043 est le seul équivalent sur étagère**, et il montre où s'arrête l'outillage générique : il impose un tableau de titres attendus à un jeu de fichiers, avec `*` pour « zéro titre non spécifié » et `+` pour « un ou plus », mais il ne lit rien sous les titres. Il ne peut donc pas exiger un paragraphe non vide, ni valider une forme courte, ni croiser une liste d'exceptions avec le disque. Il coûte de surcroît une dépendance Node que ce dépôt s'interdit. Le constat vaut aussi pour Vale, qui juge le style d'une phrase et non la structure d'un contrat.
- **Le précédent le plus proche du croisement d'allowlist est `#[expect]`, stabilisé en Rust 1.81.** Sa valeur n'est pas de supprimer un lint, `#[allow]` le faisait déjà, mais de faire échouer la suppression quand la raison a disparu, par `unfulfilled_lint_expectations`. Les deux lints Clippy introduits avec lui, `allow_attributes` et `allow_attributes_without_reason`, complètent la même idée : une exception sans motif écrit est un défaut. C'est exactement la forme du croisement bidirectionnel repris ici. Le versant ESLint de la même pratique, la justification obligatoire après `--` dans un `eslint-disable`, confirme la convergence sans rien ajouter.
- **La synchronisation README/rustdoc a trois outils et aucun ne convient.** `cargo-sync-rdme` exige nightly car il dépend de fonctions instables de rustdoc ; `cargo-doc2readme` et `readme-sync` ajoutent une dépendance de développement pour un bénéfice qui suppose un rendu crates.io. Or les seize crates sont `publish = false`. La forme native, `#![doc = include_str!("../README.md")]`, est gratuite mais transporte un coût documenté de liens intra-doc, que la note de Linebender détaille : les liens du README doivent devenir résolvables depuis le contexte du crate. Aucune de ces options ne sert le but du lot, qui est qu'une porte lise un fichier.
- **Le contrat de cache du fournisseur est la source normative du troisième champ.** Il pose que « les préfixes de cache sont créés dans l'ordre `tools`, `system`, puis `messages`, cet ordre formant une hiérarchie où chaque niveau bâtit sur les précédents », que modifier une définition d'outil invalide les trois niveaux, et que la fenêtre de rétrolecture d'un point de césure est de 20 blocs. Il documente aussi l'erreur exacte que la conception de `crates/agent-cli/src/context.rs` évite déjà : placer le contenu volatil, un horodatage typiquement, devant le contenu stable détruit la réutilisation du préfixe entier. Le champ `KV Cache effect` n'a donc pas à être inventé pour Pyxis, il a à être extrait d'un code qui l'applique déjà.
- **La littérature d'ingénierie de contexte de 2025-2026 converge sur la même règle** et la chiffre : préfixe stable d'abord, contenu dynamique en dernier, avec des mesures publiées de 953 ms contre 2 727 ms de temps au premier token entre un préfixe stable et un préfixe perturbé. Le chiffre ne se reprend pas dans le PRD, l'environnement n'étant pas comparable, mais il fixe l'ordre de grandeur du bénéfice que la documentation protège.
- **Écarts et limites relevés.** Le mode d'échec « frais mais inutile », relevé au lot précédent, prend ici une forme particulière : une section `## Model Experience` qui paraphrase la `description()` de l'outil ne dit rien. C'est la raison pour laquelle le champ `What the model sees` exige un littéral concret, code inline, bloc `markdown` imbriqué ou lien ancré vers le catalogue, plutôt qu'une phrase libre. La limite honnête du contrat est qu'il ne prouve pas la véracité de la prose : il prouve la présence, l'ordre, la densité et l'ancrage. Il déplace la vérification humaine d'un espace ouvert vers un formulaire, il ne la supprime pas.

### Correspondance dsh vers Pyxis

Racine du dépôt source : `/home/arthur/dev/deepseek-harness`, **lecture seule**, aucune ligne copiée. Les chemins de la colonne « Source dsh » sont relatifs à cette racine, et les deux fichiers nommés par le plan de portage sont `scripts/verify-package-readme-model-experience.ts` (557 lignes, le vérificateur) et `docs/cookbook/adding-a-package.md` (la prescription rédigée). Les numéros de ligne sont ceux lus au 2026-08-21 : ils servent la navigation, l'autorité restant le contenu.

| # | Décision reprise | Source dsh | Ce que Pyxis en fait | Story |
|---|---|---|---|---|
| 1 | Section canonique `## Model Experience`, triplet H4 fermé et ordonné | `scripts/verify-package-readme-model-experience.ts:13-18` (`HEADING`, `FIELD_HEADINGS`) | Mêmes intitulés anglais, repris verbatim | US-107, US-109 |
| 2 | La section est bornée au H2 suivant, pas à la fin du fichier | idem `:330-351` | Même bornage, sans l'ancre `## Known Limitations` (`:14`, `:331`) que Pyxis n'a pas | US-109 |
| 3 | Densité : un paragraphe non vide par champ, une ligne blanche entre chaque élément | idem `:363-382` | Même règle, mêmes deux échecs distincts, densité et espacement | US-109, US-110 |
| 4 | Trois classifications et non une exigence | idem `:32` (`NO_MODEL_EXPERIENCE_SECTION`), `:44` (`SENTENCE_MODEL_EXPERIENCE`) ; `docs/cookbook/adding-a-package.md:107` | Table exhaustive des seize crates au lieu de deux allowlists sur défaut implicite | US-108 |
| 5 | Amorces exactes de la forme courte | idem `:355` : `/^None, as .+\.$/` et `/^Indirectly, through .+\.$/` | Mêmes amorces, même exigence de point final | US-110 |
| 6 | Forme courte fermée à exactement trois lignes de contenu | idem `:360` (`content.length !== 3 || rawContent.length !== 3`) | Même fermeture : phrase, `#### KV Cache effect`, un paragraphe | US-110 |
| 7 | Croisement bidirectionnel de l'allowlist avec le disque | idem `:254` (`scannedPackages`), `:266` et `:278` | Même croisement, plus l'échec du crate non classé que la table exhaustive rend possible | US-108 |
| 8 | Le champ de vue exige un littéral concret, pas une paraphrase | idem `:195` (`validateNestedVerbatim`), `:515` (`hasConcreteLiteral`) | Trois ancrages admis : code inline, bloc imbriqué, lien ancré | US-109 |
| 9 | Le prompt système se cite et ne se décrit pas | idem `:236` (`isDirectSystemPromptEntry`), `:509` (`promptWithoutVerbatim`) ; cookbook `:105` (H5 titré plus clôture ```markdown) | Même règle, appliquée à `HARNESS`, `CODE_MODE_ONLY` et aux deux prompts embarqués | US-109, US-113 |
| 10 | Une entrée de schéma d'outil lie une section ancrée du catalogue généré | idem `:231` (`headingFragment`), `:241` (`toolCatalogLinkFragments`), `:246-251` (récolte) | Même ancrage, sur les H3 ``### `nom` `` de Pyxis là où dsh récolte des H2 (`:248`) | US-111 |
| 11 | Le champ de cache distingue quatre formes, et « n'invalide pas » est une clause bornée | `docs/cookbook/adding-a-package.md:105` | Vocabulaire repris tel quel : croissance en ajout seul, préfixe stable répété, remplacement de tokens antérieurs, requête indépendante | US-107 |
| 12 | Le rationale vit dans une note de processus, pas dans le vérificateur | `.agents/notes/implemented/process/2026-07-12-package-model-experience-contract.md` | Note homologue sous `docs/notes/implemented/process/` | US-119 |

Trois divergences assumées, chacune adossée à une contrainte de Pyxis et non à une préférence : la table exhaustive plutôt que les allowlists (ligne 4), justifiée par seize crates contre 227 paquets ; la récolte de H3 plutôt que de H2 (ligne 10), imposée par la forme de `docs/tool-catalog.md` ; et l'absence de la section `## Known Limitations and Deferred Work` (ligne 2), qui borne la section chez dsh et n'existe pas ici, où le H2 suivant quel qu'il soit joue ce rôle.

Ce que Pyxis ne reprend pas, faute de contrepartie : le standard de prose que le cookbook invoque (`.agents/skills/dsh-prose-standard/SKILL.md`) reste hors périmètre, Pyxis n'ayant pas d'équivalent, et la seconde allowlist du vérificateur de limitations (`scripts/verify-package-readme-limitations.ts`) est explicitement indépendante chez dsh comme ici.


## Assumptions & Constraints

### Assumptions (to validate)
- La classification en trois formes tient sur les seize crates sans cas ambigu. Le cas le plus fragile est `agent-tokenizer` : il n'écrit aucun texte mais décide quand la compaction se déclenche quand le fournisseur omet l'usage, ce qui change ce que le modèle voit au tour suivant. Classé en forme courte `Indirectly, through ...` ; à valider par US-117, et si la phrase courte ne suffit pas, il passe en forme structurée plutôt qu'en omission.
- Douze README nouveaux ne créent pas de registre concurrent avec les 2 213 lignes de `//!`. L'hypothèse tient parce que les deux répondent à des questions différentes : `//!` dit comment le crate est fait, la section dit ce que le modèle en reçoit. À falsifier au premier README qui recopie son `//!`.
- Les 29 titres `### \`nom\`` de `docs/tool-catalog.md` se transforment en fragments GitHub par une règle simple : minuscules, retrait des accents graves, espaces en tirets. Aucun nom d'outil ne contient d'espace ni de majuscule aujourd'hui, donc le fragment est le nom nu. À valider par US-111 ; si un outil futur porte un caractère hors `[a-z0-9_]`, la règle de slug se précise là plutôt que dans chaque README.
- Le coût dépasse l'estimation « 1 jour » du plan de portage, qui n'a anticipé ni les douze README, ni le fait que huit crates portent une surface structurée au lieu des deux annoncés, ni le refus d'ancres de la porte de liens. Estimation révisée : 38 points, soit 4 à 5 jours.

### Hard Constraints
- Zéro dépendance ajoutée. Le manifeste d'`agent-doc-gates` interdit toute entrée `[dependencies]` et impose un parseur écrit à la main, comme pour l'arbre de notes et le `justfile`.
- `agent-doc-gates` n'importe aucun crate Pyxis et n'entre dans le graphe d'aucun binaire.
- Aucune recette n'entre au `justfile` et aucune étape n'entre à `.github/workflows/ci.yml`. `aggregate_violations` (`crates/agent-doc-gates/src/gates.rs:231-253`) exige que la liste de dépendances de `check` soit exactement égale à la liste des recettes marquées `# ci-step:` ; la porte est donc un test, couvert par l'étape `Tests` existante.
- La porte n'écrit rien. Aucun commutateur d'écriture n'est ajouté à `WRITE_SWITCHES`, et les README sont rédigés à la main : ils ne sont pas dérivables, une section générée depuis le code étant exactement le mode d'échec « frais mais inutile ».
- La porte ne lance aucun processus, n'ouvre aucun socket et ne lit aucune variable d'environnement. C'est la condition pour vivre dans `cargo test --workspace`.
- Les README de crate sont en **anglais**, comme le reste de ce qui vit sous `crates/` selon `AGENTS.md:126-129`, et parce qu'ils citent verbatim du texte anglais destiné au modèle. `docs/model-experience.md` est en **français**, comme tout document de `docs/`, et les messages de la porte sont en français comme ceux des sept portes existantes.
- Les textes cités du code (prompts système, descriptions d'outils, placeholders) sont reproduits verbatim, sans traduction ni reformulation : une citation divergente serait pire qu'aucune citation.
- Le clone résolu par `$PYXIS_CODEX_BASELINE` reste en lecture seule ; aucun test du lot ne le touche.
- Aucun test du lot ne met `PYXIS_LIVE_PARITY`.
- `spikes/` reste hors périmètre : la porte ne parcourt que `crates/*`.
- Le dépôt `/home/arthur/dev/deepseek-harness` se lit, ne s'écrit pas et ne se copie pas.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatage du workspace
- `cargo clippy --workspace --all-targets` - lints, sans `-D warnings` par décision documentée
- `cargo test --workspace --no-fail-fast` - suite complète, nomme tous les tests en échec
- `just check` - l'agrégat des quatre portes du CI est vert
- `git status --porcelain` - vide après `just check` : cette porte ne peut rien écrire

## Epics & User Stories

### EP-035: Le contrat écrit et la porte qui le tient

La mécanique complète, éprouvée sur les crates au fur et à mesure qu'ils se documentent. Elle ferme la défaillance 5 et rend les quatre autres constatables.

**Definition of Done:** `docs/model-experience.md` énonce le contrat, un huitième fichier de test d'`agent-doc-gates` le vérifie sur les seize crates, et `just check` échoue sur un crate model-facing sans section.

#### US-107: Le contrat est écrit une fois, dans le document que la porte cite
**Description:** As a agent de codage, I want un document normatif qui dit quoi écrire et sous quelle forme so that la section ne soit pas reconstituée à partir des README existants à chaque fois.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Source dsh:** `scripts/verify-package-readme-model-experience.ts`:13-18 pour le triplet, `docs/cookbook/adding-a-package.md`:105 pour le vocabulaire de cache et la clause « does not invalidate », `docs/cookbook/adding-a-package.md`:107 pour les trois formes

**Acceptance Criteria:**
- [ ] `docs/model-experience.md` existe, en français, et énonce la section `## Model Experience`, le H3 par surface, et les trois champs H4 dans leur ordre fixe, chacun suivi d'exactement un paragraphe non vide séparé par une ligne blanche.
- [ ] Le document énonce les trois formes, structurée, courte auditée et omission nominative, et dit laquelle s'applique à quoi.
- [ ] Le document nomme les quatre formes que peut prendre un effet de cache, croissance en ajout seul, préfixe stable répété, remplacement de tokens antérieurs, requête indépendante, et dit que « n'invalide pas » signifie que le crate préserve un préfixe déjà réutilisable, jamais une promesse du fournisseur.
- [ ] Le document dit que la prose sous `#### What the model sees` doit être ancrée par un littéral concret, code inline, bloc `markdown` imbriqué ou lien ancré vers `docs/tool-catalog.md`, et pourquoi une paraphrase ne suffit pas.
- [ ] Le document énonce la frontière : la porte prouve la présence, l'ordre, la densité et l'ancrage, jamais la véracité de la prose.
- [ ] Le document porte un exemple complet de chacune des trois formes, recopiable.
- [ ] Given une règle énoncée dans le document et non tenue par la porte, when un lecteur la suit, then le document l'a déjà signalée comme telle : les règles mécaniques et les règles de jugement sont séparées, comme dans `docs/notes/README.md`.
- [ ] Given la porte en échec, when elle rend son message, then celui-ci cite `docs/model-experience.md`.

#### US-108: Les seize crates sont classés, et la classification est confrontée au disque
**Description:** As a mainteneur, I want une table exhaustive qui donne à chaque crate sa forme et sa justification so that l'absence de section cesse d'être indistinguable de l'oubli.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-107
**Source dsh:** `scripts/verify-package-readme-model-experience.ts`:32 et :44 pour les deux allowlists, :254, :266 et :278 pour le croisement avec le disque

**Acceptance Criteria:**
- [ ] `agent-doc-gates` porte une constante de classification donnant, pour chacun des seize crates, l'une des trois formes, et pour les deux formes non structurées une justification écrite en clair.
- [ ] Given un répertoire `crates/<nom>/` portant un `Cargo.toml` et absent de la table, when la porte s'exécute, then elle échoue en nommant le crate et les trois formes disponibles. La table est confrontée à `crate_directories()`, déjà écrit pour le graphe de crates.
- [ ] Given une entrée de la table nommant un crate qui n'existe plus, when la porte s'exécute, then elle échoue en nommant l'entrée périmée. C'est le versant `unfulfilled_lint_expectations` du croisement : une exception dont la raison a disparu est un défaut.
- [ ] Given une entrée classée en omission dont le crate porte pourtant une section `## Model Experience`, when la porte s'exécute, then elle échoue : les deux déclarations se contredisent et la table doit trancher.
- [ ] Given une justification vide ou réduite à un mot, when la porte s'exécute, then elle échoue : une omission sans motif lisible est un oubli déguisé.
- [ ] Given `crates/` vide ou illisible, when la porte s'exécute, then elle échoue plutôt que de valider une classification vide.
- [ ] La table est ordonnée par nom de crate et un test prouve cet ordre, pour qu'une insertion se lise dans le diff à sa place.

#### US-109: La forme structurée est vérifiée dans son ordre et sa densité
**Description:** As a relecteur, I want que la porte refuse une section aux champs manquants, désordonnés ou vides so that une section présente vaille information et pas décor.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-108
**Source dsh:** `scripts/verify-package-readme-model-experience.ts`:330-351 pour le bornage, :363-382 pour la densité, :195 et :515 pour le littéral concret, :236 et :509 pour le prompt cité

**Acceptance Criteria:**
- [ ] La porte lit `crates/<nom>/README.md` de chaque crate classé structuré et exige une section `## Model Experience` unique.
- [ ] Chaque H3 de la section porte les trois H4 `#### What the model sees`, `#### Token effect`, `#### KV Cache effect`, dans cet ordre exact.
- [ ] Given un champ suivi de zéro paragraphe, ou de deux paragraphes, when la porte s'exécute, then elle échoue en nommant le crate, la surface et le champ.
- [ ] Given un H4 inconnu sous un H3 de la section, when la porte s'exécute, then elle échoue : l'ensemble des champs est fermé.
- [ ] Given une section sans aucun H3, when la porte s'exécute, then elle échoue : une section sans surface ne dit rien.
- [ ] Given un H3 dont le champ `What the model sees` ne contient ni code inline, ni bloc imbriqué, ni lien ancré vers le catalogue, when la porte s'exécute, then elle échoue en nommant les trois formes d'ancrage acceptées.
- [ ] Given un H3 dont le titre contient « system prompt », when la porte s'exécute, then elle exige sous `What the model sees` un H5 titré suivi d'un bloc de code ```markdown, le texte envoyé au modèle devant être cité et non décrit.
- [ ] Given plusieurs violations dans un même README, when la porte s'exécute, then elle les rend toutes en une fois, une ligne chacune, comme le fait déjà la porte de prose.
- [ ] Given un `README.md` absent pour un crate classé structuré, when la porte s'exécute, then elle échoue en nommant le fichier attendu et le document de contrat.

#### US-110: La forme courte est bornée et l'omission reste sans fichier
**Description:** As a mainteneur, I want que la déclaration d'absence d'effet soit aussi contrainte que la déclaration d'effet so that une phrase courte ne devienne pas un moyen d'échapper au contrat.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-108
**Source dsh:** `scripts/verify-package-readme-model-experience.ts`:355 pour les amorces, :360 pour la fermeture à trois lignes, :375-380 pour l'espacement

**Acceptance Criteria:**
- [ ] Un crate classé en forme courte porte un `README.md` avec une section `## Model Experience` dont le contenu est exactement une phrase de classification, une ligne blanche, le H4 `#### KV Cache effect`, une ligne blanche, un paragraphe.
- [ ] La phrase de classification commence par `None, as ` ou par `Indirectly, through ` selon la forme déclarée dans la table, et se termine par un point. Given une phrase d'une autre forme, when la porte s'exécute, then elle échoue en citant les deux amorces admises.
- [ ] Given une forme courte portant un H3 ou l'un des deux autres champs, when la porte s'exécute, then elle échoue : la forme courte est fermée, sans quoi elle devient une forme structurée dégradée.
- [ ] Given un crate classé en omission portant un `README.md` avec une section `## Model Experience`, when la porte s'exécute, then elle échoue, conformément au croisement d'US-108.
- [ ] Un crate classé en omission n'a pas besoin de `README.md` et la porte n'en exige aucun : sa justification vit dans la table, qui est le seul endroit où la lire.
- [ ] Given un crate classé en forme courte dont la justification de table et la phrase du README se contredisent sur la forme, `None` d'un côté et `Indirectly` de l'autre, when la porte s'exécute, then elle échoue en citant les deux.

#### US-111: L'ancrage vers le catalogue d'outils est prouvé par la porte qui en dépend
**Description:** As a agent de codage, I want qu'un lien vers une section du catalogue d'outils soit vérifié jusqu'au fragment so that la documentation ne puisse pas décrire un outil que le catalogue ne connaît pas.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-109
**Source dsh:** `scripts/verify-package-readme-model-experience.ts`:231, :241 et :246-251 pour la récolte de fragments et la validation des ancres

**Acceptance Criteria:**
- [ ] La porte récolte les titres `### \`nom\`` de `docs/tool-catalog.md` et en dérive les fragments attendus, minuscules, accents graves retirés.
- [ ] Given un lien de README pointant `docs/tool-catalog.md#<fragment>` dont le fragment n'est produit par aucun titre, when la porte s'exécute, then elle échoue en nommant le crate, la ligne, le fragment et le nombre de fragments connus.
- [ ] Given un catalogue rendant zéro titre H3, when la porte s'exécute, then elle échoue plutôt que de valider tous les liens : un garde de récolte, sur le modèle de celui du catalogue d'outils.
- [ ] Given un outil renommé et le catalogue régénéré, when `just test` s'exécute, then la porte échoue sur chaque README qui pointait l'ancien fragment.
- [ ] `markdown_documents` (`crates/agent-doc-gates/src/links.rs:25-38`) parcourt aussi les `crates/*/README.md`, pour que leurs liens relatifs morts fassent échouer la porte de liens existante.
- [ ] Le refus d'ancres de la porte de liens (`crates/agent-doc-gates/src/links.rs:12-13`) reste intact et son test le prouve toujours : la vérification de fragment vit dans cette porte-ci, qui charge le catalogue pour son propre garde de littéral.
- [ ] Given un lien vers le catalogue sans fragment du tout, when la porte s'exécute, then il est accepté par cette porte et vérifié par la porte de liens : les deux responsabilités ne se recouvrent pas.

---

### EP-036: Les huit surfaces structurées

Le contenu, et la seule partie du lot qui produit de la connaissance nouvelle plutôt qu'une contrainte. C'est ici que les six commentaires de préfixe cacheable et les 3 449 octets d'instructions embarquées sortent du code.

**Definition of Done:** les huit crates model-facing portent leur section, chaque surface citée est ancrée par un littéral, et les défaillances 1 à 3 du Problem Statement sont constatables dans un document.

#### US-112: `agent-tools` déclare ses vingt-neuf descriptions et ses littéraux de troncature
**Description:** As a relecteur, I want savoir que les descriptions d'outils occupent le premier niveau du préfixe so that une reformulation cesse d'être jugée comme une simple question de style.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-111

**Acceptance Criteria:**
- [ ] `crates/agent-tools/README.md` porte la section avec au minimum les surfaces : catalogue d'outils exposé au modèle, sorties d'outils et leur troncature, dépassement de fenêtre, et messages d'erreur d'outil.
- [ ] La surface du catalogue lie une section ancrée de `docs/tool-catalog.md` plutôt que de recopier une description, et le champ de cache dit que le catalogue est le premier niveau du préfixe et qu'en changer une description invalide les trois niveaux.
- [ ] Le champ `Token effect` de cette surface cite les 26 117 octets rendus par la section `## Outils` du catalogue comme ordre de grandeur, en disant de quoi ce chiffre est la mesure.
- [ ] Les littéraux `NOT_PUBLISHED` (`crates/agent-tools/src/context_window.rs:28`) et le `continuation_hint` de `ToolResultTruncation` sont cités verbatim en code inline ou en bloc.
- [ ] `NEVER_SPILLED` (`crates/agent-tools/src/spill_policy.rs`) apparaît sous la surface de troncature, avec ce que le déversement change pour le modèle.
- [ ] Given un lecteur cherchant si une sortie d'outil peut remplacer des tokens déjà envoyés, when il lit le champ de cache de la surface de troncature, then la réponse y est explicite.
- [ ] La section ne paraphrase aucune `description()` : elle renvoie au catalogue, qui est généré.

#### US-113: `agent-cli` cite verbatim les quatre textes système et dit lequel s'ajoute quand
**Description:** As a contributeur externe, I want lire le texte système exact et sa règle de composition so that je cesse d'ouvrir trois fichiers source pour savoir ce que Pyxis envoie.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-111
**Source dsh:** `docs/cookbook/adding-a-package.md`:105 pour le H5 titré suivi d'une clôture ```markdown, `scripts/verify-package-readme-model-experience.ts`:236 pour la détection d'une entrée de prompt direct

**Acceptance Criteria:**
- [ ] `crates/agent-cli/README.md` porte une surface de prompt système dont le titre contient « system prompt », donc soumise à la règle du H5 et du bloc ```markdown d'US-109.
- [ ] `HARNESS` (`crates/agent-cli/src/prompt.rs:20-38`) est cité verbatim dans un bloc, et la prose dit qu'il s'ajoute à toute sélection et qu'il déclare gagner contre ce qui le précède.
- [ ] `CODE_MODE_ONLY` (`prompt.rs:43-52`) est cité verbatim, avec sa condition exacte, `runtime.tool_mode.hides_nested_tools()`.
- [ ] Les deux instructions embarquées de `crates/agent-cli/prompts/` sont nommées avec leur taille et leur condition de chargement, qui est l'indisponibilité du catalogue distant (`crates/agent-provider/src/models/embedded.rs`).
- [ ] Une surface distincte couvre le bloc `<environment>` et le contexte projet de `crates/agent-cli/src/context.rs`, avec le budget de 32 000 octets, les candidats `AGENTS.md` et `CLAUDE.md`, et la profondeur de remontée de 24.
- [ ] Le champ de cache de cette surface énonce l'ordre stable-puis-volatil et dit ce qu'il protège, en reprenant les quatre commentaires de `context.rs:42,55,57,471` plutôt qu'en les paraphrasant de mémoire.
- [ ] Given un lecteur qui veut savoir si un rafraîchissement d'`AGENTS.md` en cours de tour casse le préfixe, when il lit ce champ, then la réponse y est explicite.

#### US-114: `agent-core` déclare ses littéraux, ses seuils et ses opérations auxiliaires
**Description:** As a agent de codage, I want savoir quels textes le cœur injecte dans le transcript et à quels seuils il compacte so that j'écrive un outil en connaissant la fenêtre.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-111

**Acceptance Criteria:**
- [ ] `crates/agent-core/README.md` cite verbatim `PRUNED_PLACEHOLDER` et `SUMMARY_SYSTEM` (`crates/agent-core/src/compaction.rs:24` et suivantes).
- [ ] Une surface couvre la fenêtre : `MAX_NON_HISTORY_CONTEXT_BYTES = 64 * 1024` (`crates/agent-core/src/prompt.rs`), les seuils micro à 70 % et auto à 80 % de `max_context - output_reserve` (`crates/agent-core/src/budget.rs`), et la priorité de sacrifice.
- [ ] Une surface couvre les opérations auxiliaires de `crates/agent-core/src/auxiliary/`, en nommant les huit variantes d'`AuxiliaryOperation`, et son champ de cache dit qu'une requête auxiliaire est indépendante et ne partage pas le préfixe du tour.
- [ ] Le champ de cache de la surface de compaction dit que la compaction remplace des tokens antérieurs, donc invalide le préfixe au-delà du point de coupe, ce qui la distingue de la croissance en ajout seul.
- [ ] Given un lecteur cherchant pourquoi les seuils mesurent la croissance et non l'absolu après une compaction, when il lit la surface de fenêtre, then la réponse y est et renvoie à `mark_compacted`.
- [ ] Les valeurs citées sont celles du code au jour de la rédaction, et la section dit que le code arbitre : aucun chiffre n'est reformulé en approximation.

#### US-115: `agent-runtime` et `agent-provider` déclarent l'assemblage du préfixe et son rendu
**Description:** As a relecteur, I want que la propriété de stabilité du préfixe soit écrite là où elle est tenue so that elle cesse de vivre dans deux commentaires que personne ne rassemble.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-111

**Acceptance Criteria:**
- [ ] `crates/agent-runtime/README.md` porte une surface d'assemblage de contexte citant `StepContext.generation` et sa garantie, « two steps sharing a generation produced the same prefix » (`crates/agent-runtime/src/context.rs:220-221`).
- [ ] La séparation stable / volatile de `build()`, la borne `MAX_SECTION_BYTES` par section et le mémo de dernière valeur valide sont décrits avec ce que chacun protège, la réutilisation d'une section illisible étant un non-événement pour le préfixe et non un trou.
- [ ] `crates/agent-provider/README.md` porte une surface de rendu de requête et une surface de `prompt_cache_key`, citant l'UUID v4 stable par session de `crates/agent-provider/src/chatgpt.rs:72-73`.
- [ ] La section d'`agent-provider` déclare les instructions embarquées comme surface, avec leur condition de repli, et renvoie à `agent-cli` pour leur composition plutôt que de la dupliquer.
- [ ] Given un lecteur cherchant qui décide de l'ordre `tools`, `system`, `messages`, when il lit l'une des deux sections, then elle nomme le fournisseur comme source de la règle et le crate comme lieu où elle est respectée.
- [ ] Given une section qui affirmerait que Pyxis garantit un succès de cache, when la relecture s'exécute, then elle est corrigée : la formulation admise est que le crate préserve un préfixe réutilisable.

#### US-116: `agent-mcp`, `agent-app-server` et `agent-code-mode` déclarent les catalogues alimentés du dehors
**Description:** As a relecteur de la surface de sécurité, I want savoir quel texte d'origine externe entre dans le préfixe so that la défense fail-closed soit lisible à côté de ce qu'elle protège.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-111

**Acceptance Criteria:**
- [ ] `crates/agent-mcp/README.md` distingue deux surfaces : les descriptions rédigées de ses trois outils de ressources (`crates/agent-mcp/src/resource_tools.rs:160,262,355`), citées verbatim, et les descriptions d'origine serveur, qui traversent sans être réécrites.
- [ ] La surface d'origine serveur dit que son texte n'est pas contrôlé par ce dépôt et que sa politique reste fail-closed, ce que le crate applique déjà.
- [ ] `crates/agent-app-server/README.md` déclare `ClientTool` (`crates/agent-app-server/src/bridge.rs:107-140`) : un client externe injecte une `description` et un schéma dans le même registre que les natifs, avec les mêmes permissions, la même défense de souillure et les mêmes hooks.
- [ ] Le champ de cache de `ClientTool` dit qu'un outil client apparaissant ou disparaissant modifie le premier niveau du préfixe, donc invalide le cache complet de la session.
- [ ] `crates/agent-code-mode/README.md` déclare le catalogue imbriqué rendu en TypeScript et dit ce que le modèle voit à la place des outils natifs quand `hides_nested_tools()` est vrai.
- [ ] Given un lecteur cherchant pourquoi un outil MCP déclaré `read_only` reste traité comme non fiable, when il lit la section d'`agent-mcp`, then le raisonnement y est écrit, ce que déclare déjà `bridge.rs`.

---

### EP-037: Les surfaces auditées et l'intégration

Ce qui transforme huit README en contrat de dépôt : les huit crates restants se prononcent, et les documents d'entrée envoient le lecteur au bon endroit.

**Definition of Done:** les seize crates sont classés et conformes, `just check` est vert, `AGENTS.md` nomme le contrat, et une note de décision enregistre la mesure.

#### US-117: Les huit crates restants se prononcent
**Description:** As a agent de codage, I want qu'un crate sans effet le déclare so that je cesse de confondre absence d'effet et absence de documentation.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-110

**Acceptance Criteria:**
- [ ] `agent-session`, `agent-tokenizer`, `agent-code-mode-v8` et `agent-sandbox` portent un `README.md` en forme courte, avec l'amorce déclarée dans la table et un champ de cache renseigné.
- [ ] La forme courte d'`agent-sandbox` nomme le corps 403 du proxy (`crates/agent-sandbox/src/proxy.rs:400`) comme le chemin par lequel un littéral du crate atteint le transcript, à travers la sortie d'un outil d'exécution.
- [ ] La forme courte d'`agent-session` dit qu'il rejoue ce qui a déjà été envoyé, et son champ de cache dit ce qu'une reprise fait au préfixe.
- [ ] La forme courte d'`agent-tokenizer` dit qu'il décide quand la compaction se déclenche quand le fournisseur omet l'usage, donc qu'il agit sur ce que le modèle verra au tour suivant sans écrire aucun texte.
- [ ] `agent-auth`, `agent-tui`, `agent-doc-gates` et `agent-parity` sont classés en omission, chacun avec sa justification dans la table, et n'ont pas de `README.md` de ce fait.
- [ ] Given un relecteur cherchant si `agent-tui` peut écrire dans le transcript, when il lit la justification d'omission, then elle dit que le crate rend vers l'humain et que le sens de la flèche est l'argument.
- [ ] Given l'un de ces huit crates gagnant plus tard une surface directe, when la porte s'exécute, then le croisement d'US-108 le fait échouer avant qu'un lecteur ne se fie à la déclaration périmée.

#### US-118: Les documents d'entrée nomment le contrat et sa commande
**Description:** As a agent de codage, I want trouver le contrat depuis `AGENTS.md` so that je ne découvre pas la porte par son échec.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-117

**Acceptance Criteria:**
- [ ] `AGENTS.md` gagne une ligne dans sa table « Where to read more » renvoyant à `docs/model-experience.md`.
- [ ] La table « Targeted verification signals » d'`AGENTS.md` gagne une ligne pour cette porte, avec `cargo test -p agent-doc-gates` comme commande ciblée et `just test` comme agrégat qui la porte, sur le modèle de la ligne existante des enregistrements de décision.
- [ ] La table « Where new behavior goes » dit qu'ajouter une surface visible du modèle demande la section dans le README du crate concerné.
- [ ] `CONTRIBUTING.md` mentionne le contrat dans sa section Development, en nommant `just check` et non une invocation `cargo` qui entrerait en collision avec une porte.
- [ ] Given ces ajouts, when la porte de prose s'exécute, then elle reste verte : aucune commande écrite ne partage la tête d'une recette marquée.
- [ ] Given un lecteur suivant le lien depuis `AGENTS.md`, when la porte de liens s'exécute, then elle résout la cible.

#### US-119: La note de décision consigne la mesure et la frontière
**Description:** As a mainteneur futur, I want la trace de ce que ce lot a mesuré et écarté so that la prochaine porte documentaire ne rejoue pas les mêmes arbitrages.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-118
**Source dsh:** `.agents/notes/implemented/process/2026-07-12-package-model-experience-contract.md`, la note homologue et ses alternatives écartées

**Acceptance Criteria:**
- [ ] Une note vit à `docs/notes/implemented/process/2026-08-21-experience-du-modele.md` et respecte le format que la porte de notes vérifie.
- [ ] La note enregistre la mesure d'entrée : six sites de préfixe cacheable en commentaires, zéro dans `docs/`, 3 449 octets d'instructions embarquées, 26 117 octets de catalogue d'outils, quatre littéraux non déclarés, zéro `README.md` sous `crates/`.
- [ ] Sa section « Alternatives écartées » couvre au minimum : générer les sections depuis le code, imposer la forme structurée partout, n'exiger la section que des crates enregistrant un outil, `#![doc = include_str!]` avec son coût de liens intra-doc, l'extension de la porte de liens aux ancres, et le tableau à la place des paragraphes.
- [ ] La note dit pourquoi la classification est une table exhaustive plutôt que deux allowlists, en nommant le seuil de taille qui rend ce choix possible.
- [ ] La note explique pourquoi cette décision est une note et non un ADR : rien dans `crates/` ne peut la violer, seule une porte documentaire le peut, conformément à la frontière d'`AGENTS.md`.
- [ ] Given la note écrite, when `cargo test -p agent-doc-gates` s'exécute, then les portes de format, d'arbre et de liens la valident.

## Functional Requirements

- FR-01: Le dépôt doit porter un document normatif unique énonçant le contrat d'expérience du modèle.
- FR-02: Chaque crate de `crates/` doit être classé dans exactement une des trois formes, et la classification doit être confrontée au disque dans les deux sens.
- FR-03: Un crate classé structuré doit porter un `README.md` avec une section `## Model Experience`, un H3 par surface, et sous chaque H3 les trois champs H4 dans leur ordre fixe.
- FR-04: Chaque champ doit être suivi d'exactement un paragraphe non vide.
- FR-05: Le champ `What the model sees` doit être ancré par un littéral concret.
- FR-06: Une surface dont le titre nomme le prompt système doit citer le texte verbatim dans un bloc de code.
- FR-07: Un lien vers `docs/tool-catalog.md` portant un fragment doit viser un titre réel du catalogue.
- FR-08: Un crate classé en omission ne doit pas porter de section `## Model Experience`.
- FR-09: Le système ne doit pas générer les sections depuis le code.
- FR-10: La porte ne doit ajouter ni recette au `justfile`, ni étape au CI, ni commutateur d'écriture.
- FR-11: Un échec doit nommer le crate, la surface, le champ, et le document de contrat.
- FR-12: Toutes les violations d'un même README doivent être rendues en une seule exécution.

## Non-Functional Requirements

- **Performance:** la porte ajoute au plus 1 s de temps mur à `just test` sur cache chaud, et lit au plus 17 fichiers, seize README candidats plus `docs/tool-catalog.md`.
- **Hermétisme:** zéro processus lancé, zéro socket ouvert, zéro variable d'environnement lue, zéro accès réseau, zéro lecture hors du dépôt.
- **Dépendances:** zéro entrée ajoutée à `[dependencies]`, du workspace comme d'`agent-doc-gates`, dont le manifeste l'interdit.
- **Déterminisme:** deux exécutions consécutives sur le même arbre rendent la même liste de violations dans le même ordre ; aucun `HashMap` n'atteint le rendu des messages.
- **Lisibilité des échecs:** une violation tient sur une ligne, préfixée du nom de la porte, comme les sept portes existantes.
- **Empreinte documentaire:** aucune section `## Model Experience` ne dépasse 8 Kio, seuil au-delà duquel le contenu appartient à un document de `docs/` que la section lie.
- **Sécurité:** aucun secret, aucun jeton et aucun chemin absolu de machine ne figure dans les textes cités verbatim ; les blocs de prompt sont recopiés depuis la source du dépôt et non depuis une session.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Crate non classé | Un crate est ajouté à `crates/` | La porte échoue avant même de chercher un README | « expérience du modèle: `<crate>` n'est pas classé : structuré, forme courte ou omission justifiée, voir `docs/model-experience.md` » |
| 2 | Entrée de table périmée | Un crate est supprimé ou renommé | La porte échoue sur l'entrée orpheline | « expérience du modèle: l'entrée `<crate>` ne correspond à aucun crate » |
| 3 | Déclarations contradictoires | Un crate classé en omission gagne une section | La porte échoue en citant les deux déclarations | « expérience du modèle: `<crate>` est classé en omission et porte pourtant une section » |
| 4 | README absent | Un crate structuré n'a pas de `README.md` | La porte échoue en nommant le fichier attendu | « expérience du modèle: `crates/<crate>/README.md` est attendu et absent » |
| 5 | Champ vide | Un H4 est suivi d'une ligne blanche puis d'un autre titre | La porte échoue en nommant crate, surface et champ | « expérience du modèle: `<crate>` / `<surface>` / `#### Token effect` : aucun paragraphe » |
| 6 | Champs désordonnés | `KV Cache effect` précède `Token effect` | La porte échoue en donnant l'ordre attendu | « expérience du modèle: `<crate>` / `<surface>` : ordre attendu `What the model sees`, `Token effect`, `KV Cache effect` » |
| 7 | Ancre morte | Un outil est renommé et le catalogue régénéré | La porte échoue sur chaque README pointant l'ancien fragment | « expérience du modèle: `<crate>:<ligne>` : `#<fragment>` n'existe pas parmi les 29 sections du catalogue » |
| 8 | Catalogue sans titres | `docs/tool-catalog.md` rend zéro H3 | La porte échoue plutôt que de valider tous les liens | « expérience du modèle: le catalogue d'outils ne rend aucune section, récolte impossible » |
| 9 | Forme courte débordante | Une forme courte gagne un H3 | La porte échoue : la forme est fermée | « expérience du modèle: `<crate>` est en forme courte, un H3 y est interdit » |
| 10 | Paraphrase non ancrée | `What the model sees` ne contient aucun littéral | La porte échoue en citant les trois ancrages admis | « expérience du modèle: `<crate>` / `<surface>` : ni code inline, ni bloc, ni lien ancré » |
| 11 | Prompt système décrit et non cité | Une surface nomme le prompt sans bloc ```markdown | La porte échoue | « expérience du modèle: `<crate>` / `<surface>` : le texte système se cite, il ne se décrit pas » |
| 12 | `crates/` illisible | Répertoire absent ou droits refusés | La porte échoue plutôt que de valider une classification vide | « expérience du modèle: `crates/` est vide ou illisible » |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Les sections deviennent fraîches mais fausses : la porte prouve la forme, pas la véracité, et une prose recopiée d'un état ancien du code survit à la porte indéfiniment | High | High | Le champ `What the model sees` exige un littéral concret, code inline, bloc cité ou ancre de catalogue. Un littéral faux se voit à la lecture du code voisin, une paraphrase fausse non. L'ancre, elle, est mécaniquement vérifiée et casse au renommage. Le contrat écrit énonce cette limite plutôt que de la taire |
| 2 | Douze README créent un second registre concurrent des 2 213 lignes de `//!`, et les deux divergent | Med | Med | Le contrat sépare les questions : `//!` dit comment le crate est fait, la section dit ce que le modèle en reçoit. Le refus de `#![doc = include_str!]` est délibéré et argumenté dans la note d'US-119. Une section qui recopie son `//!` est un signal de relecture, pas un état acceptable |
| 3 | La forme courte devient l'échappatoire par défaut : classer un crate en `None, as ...` coûte une phrase et ferme le sujet | Med | High | La table est exhaustive et sa justification est obligatoire et non vide. La forme courte exige quand même le champ de cache, donc un raisonnement. Et le croisement d'US-108 fait échouer un crate classé sans effet qui gagne une surface, ce qui borne la durée de vie d'un classement complaisant |
| 4 | Le périmètre réel est quatre fois celui annoncé par le plan, qui dit 1 jour et deux crates | High | Med | Le PRD révise à 38 points et huit crates structurés, et sépare la mécanique (EP-035) du contenu (EP-036) pour que la porte livre même si la rédaction s'étale. EP-037 est P1 et peut suivre |
| 5 | Une valeur citée verbatim, seuil ou taille, dérive du code sans que rien ne le voie | Med | Med | Les chiffres cités sont peu nombreux et adossés à un chemin de fichier lisible. La ligne d'`AGENTS.md` ajoutée par US-118 nomme la commande ciblée. Le lot ne prétend pas fermer ce risque : le fermer demanderait un catalogue généré, écarté au titre du mode d'échec « frais mais inutile » |
| 6 | Le fragment d'ancre se dérive mal si un futur outil porte un caractère hors `[a-z0-9_]` | Low | Low | La règle de slug vit dans la porte, à un seul endroit, et son test la fixe sur les 29 noms actuels. Un nom exotique fait échouer la porte plutôt que de produire un lien silencieusement faux |

## Non-Goals

- **Générer les sections depuis le code.** Une section dérivée dirait ce que le code dit déjà et perdrait le seul contenu utile, l'effet sur le budget et sur le cache, qui n'est pas dérivable. C'est l'alternative que la note de dsh a écartée en premier et le mode d'échec que le lot précédent a nommé.
- **Prouver la véracité de la prose.** La porte prouve présence, ordre, densité et ancrage. Une section formellement conforme et matériellement fausse passe, et le contrat le dit.
- **Compter les tokens.** Aucun chiffre de tokens n'est exigé ni vérifié : il dépend du tokenizer, du modèle et du contenu, et un chiffre faux serait pire qu'une description qualitative. Les tailles citées sont des octets de fichiers, mesurables.
- **Faire entrer les README dans le rustdoc.** `#![doc = include_str!]` est écarté : les seize crates sont `publish = false`, donc aucun rendu externe ne consomme le fichier, et l'inclusion coûterait un travail de liens intra-doc sans contrepartie.
- **Étendre la porte de liens aux ancres.** `crates/agent-doc-gates/src/links.rs:12-13` refuse les ancres par décision écrite et testée. Ce lot ne rouvre pas ce refus ; il vérifie le seul fragment dont il a besoin, là où il charge déjà la donnée.
- **Documenter `spikes/`.** L'arbre est un espace jetable exclu du workspace.
- **Ajouter une recette ou une étape CI.** L'égalité de `check` et de l'inventaire marqué l'interdit, et un test suffit.
- **Couvrir les prompts des sous-agents et les catalogues distants de modèles.** Ils entrent au périmètre quand une surface les rend model-facing de façon stable ; aujourd'hui la table les classe avec le crate qui les compose.

## Files NOT to Modify

- `docs/crate-graph.md`, `docs/tool-catalog.md`, `docs/config-catalog.md` : générés et comparés octet à octet ; une édition manuelle est perdue à la régénération suivante. Ce lot les lit, il ne les écrit pas.
- `docs/parity/codex-baseline-matrix.json`, `docs/parity/codex-client-model-matrix.json` : générés et empreintés, hors périmètre.
- `justfile`, `.github/workflows/ci.yml` : l'égalité entre les dépendances de `check` et les recettes marquées est prouvée par `agent-doc-gates` ; ce lot n'ajoute ni recette ni étape.
- `crates/agent-doc-gates/Cargo.toml` : sa section `[dependencies]` reste vide, son commentaire dit pourquoi.
- Le clone résolu par `$PYXIS_CODEX_BASELINE` : lecture seule absolue.
- `crates/agent-cli/prompts/*.md` : ce lot cite ces textes, il ne les réécrit pas. Modifier un prompt est un changement de comportement, pas de documentation.
- `spikes/` : hors workspace.

## Technical Considerations

- **Emplacement de la porte:** un module `model_experience` dans `agent-doc-gates` et un huitième fichier de test, sur le modèle des sept existants. Recommandé, parce que `aggregate_violations` interdit une cinquième recette marquée. Ingénierie à confirmer : le module lit-il `docs/tool-catalog.md` lui-même, ou reçoit-il les fragments d'un helper partagé avec une future porte ?
- **Forme de la classification:** une constante de tuples `(crate, forme, justification)` plutôt qu'un tableau markdown parsé. Recommandé : le manifeste interdit un parseur de plus, et une constante mal formée est une erreur de compilation. Alternative : une table dans `docs/model-experience.md`, sur le modèle des classes de notes, au prix d'un parseur à la main.
- **Découpage des surfaces:** combien de H3 par crate ? Le lot en propose quatre pour `agent-tools`, deux à trois ailleurs. Trop de surfaces diluent, trop peu forcent un paragraphe fourre-tout. À trancher crate par crate pendant EP-036, sans règle numérique.
- **Slugification des ancres:** minuscules, retrait des accents graves, espaces en tirets, suffisant pour les 29 noms actuels qui sont tous `[a-z0-9_]`. À confirmer : faut-il implémenter la règle GitHub complète maintenant, ou échouer explicitement sur un nom hors du jeu attendu ?
- **Portée du balayage de la porte de liens:** ajouter `crates/*/README.md` à `markdown_documents` élargit une fonction que trois portes utilisent. À confirmer : effet sur `check_links` pour les crates sans README, et sur le temps d'exécution.
- **Migration:** aucune. Rien n'existe à migrer, aucun format n'est remplacé, et la porte est verte dès qu'elle est écrite si EP-036 la précède. Ordre recommandé : US-107 et US-108 d'abord, puis alterner porte et contenu pour que chaque règle soit éprouvée sur un crate réel avant la suivante.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Crates model-facing avec section | 0/8 | 8/8 | Month-1 | `cargo test -p agent-doc-gates` |
| Crates du workspace classés | 0/16 | 16/16 | Month-1 | croisement de la table et de `crate_directories()` |
| Sites de préfixe cacheable documentés hors du code | 0/6 | 6/6 | Month-1 | inspection des sections d'US-113 et US-115 |
| Littéraux lus par le modèle et déclarés | 0/4 | 4/4 | Month-1 | inspection des sections d'US-112, US-114 et US-117 |
| Textes système verbatim cités | 0/4 | 4/4 | Month-1 | blocs ```markdown de `crates/agent-cli/README.md` |
| Liens entrants vers une ancre du catalogue d'outils | 0 | ≥ 1 | Month-1 | comptage des fragments par la porte |
| Dépendances ajoutées | 0 | 0 | Month-6 | `git diff` sur les `Cargo.toml` |
| Recettes ajoutées au `justfile` | 0 | 0 | Month-6 | `just --list` et la porte d'inventaire |
| Crates ajoutés au dépôt sans classification | N/A (nouveau) | 0 | Month-6 | échec de la porte au premier crate non classé |
| Temps mur ajouté à `just test` | 0 s | ≤ 1 s | Month-1 | mesure avant et après sur cache chaud |

## Open Questions

- La règle de slug d'ancre s'implémente-t-elle complètement maintenant ou échoue-t-elle explicitement hors de `[a-z0-9_]` ? Arthur Jean, avant US-111 ; la réponse change la taille du module et le message d'échec du cas 7.
- `agent-tokenizer` tient-il en forme courte ? Arthur Jean, pendant US-117 ; s'il ne tient pas, il passe en structuré et EP-037 gagne 2 points.
- Le seuil de 8 Kio par section est-il le bon plafond, ou faut-il le laisser au jugement ? Arthur Jean, avant US-112, qui est la section la plus fournie ; un plafond mécanique demanderait une règle de porte de plus.
- La table de classification vit-elle en constante Rust ou en tableau parsé dans `docs/model-experience.md` ? Arthur Jean, avant US-108 ; le PRD recommande la constante, le précédent des classes de notes plaide l'autre.
- Le lot #8 ajoutera-t-il une porte lisant elle aussi `crates/*/README.md` ? Si oui, la récolte de fragments et le balayage des README se factorisent dès maintenant plutôt qu'après.
[/PRD]
