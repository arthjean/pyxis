# Expérience du modèle

Ce document est le contrat que la porte `model_experience` d'`agent-doc-gates`
fait respecter. Il dit ce qu'un crate écrit de ce que le modèle reçoit de lui,
sous quelle forme, et où s'arrête la preuve mécanique.

Il existe parce que la contrainte la plus coûteuse du dépôt ne vivait qu'en
commentaires : le préfixe envoyé à chaque requête est ordonné `tools`, `system`,
`messages`, et reformuler la `description()` d'un outil jette le cache de toutes
les sessions ouvertes. Rien ne permettait au relecteur d'une telle pull request
de le savoir. La réponse n'est pas un document central de plus : c'est une
section par crate, à côté du code qu'elle décrit, et une porte qui refuse
l'absence.

## Ce que la porte prouve, et ce qu'elle ne prouve pas

La frontière est nette, et ce document l'énonce plutôt que de la taire.

La porte prouve la **présence** d'une section pour chaque crate classé, l'**ordre**
des champs, la **densité** de chaque champ, l'**ancrage** de la prose par un
littéral concret, et la validité du **fragment** d'un lien vers le catalogue
d'outils.

La porte ne prouve **jamais la véracité** de la prose. Une section formellement
conforme et matériellement fausse passe. C'est la limite acceptée du dispositif :
la seule chose qui casse mécaniquement quand le code bouge est l'ancre de
catalogue, qui meurt au renommage d'un outil. Un littéral recopié faux se voit à
la lecture du code voisin ; une paraphrase fausse, non. C'est la raison d'être de
la règle d'ancrage.

Les deux registres sont donc séparés, comme dans
[`docs/notes/README.md`](notes/README.md) :

**Règles mécaniques**, tenues par la porte : les sections « Les trois formes »,
« La forme structurée », « L'ancrage », « Le prompt système se cite », « La forme
courte » et « L'omission ».

**Règles de jugement**, tenues par la relecture seule : le découpage en surfaces,
le choix de ce qui mérite d'être cité, l'exactitude des chiffres, le choix de la
bonne forme parmi les quatre de la section « Les quatre formes d'un effet de
cache », et la formulation de « préserve un préfixe réutilisable » plutôt que
« garantit un succès de cache ».

## Les trois formes

Chacun des seize crates de `crates/` porte exactement une forme, déclarée dans la
constante `CLASSIFICATION` de
[`crates/agent-doc-gates/src/model_experience.rs`](../crates/agent-doc-gates/src/model_experience.rs).
La table est exhaustive et confrontée au disque dans les deux sens : un crate
absent de la table fait échouer la porte, une entrée sans crate aussi.

| Forme | S'applique à | Fichier attendu |
|---|---|---|
| Structurée | Un crate dont un texte, un schéma ou un littéral atteint le modèle | `crates/<nom>/README.md` avec une section à H3 |
| Courte auditée | Un crate sans texte direct mais dont le comportement change ce que le modèle verra | `crates/<nom>/README.md` avec une section fermée d'une phrase et d'un champ |
| Omission nominative | Un crate dont rien n'atteint le modèle | Aucun. La justification vit dans la table, seul endroit où la lire |

Les deux formes non structurées portent une justification écrite en clair dans la
table. Une justification vide ou réduite à un mot fait échouer la porte : une
omission sans motif lisible est un oubli déguisé.

## La forme structurée

La section s'ouvre par un titre `## Model Experience`, unique dans le fichier.
Elle porte au moins un H3, un par **surface** : un chemin distinct par lequel le
crate atteint le modèle. Une section sans H3 ne dit rien et fait échouer la porte.

Sous chaque H3 viennent exactement trois H4, dans cet ordre fixe :

1. `#### What the model sees`
2. `#### Token effect`
3. `#### KV Cache effect`

L'ensemble des champs est fermé : un H4 inconnu sous un H3 de la section fait
échouer la porte. Chaque champ est suivi d'**exactement un paragraphe non vide**,
séparé du titre par une ligne blanche. Zéro paragraphe est un champ décoratif,
deux est une section qui aurait dû se scinder en deux surfaces.

Les blocs de code et les H5 ne comptent pas comme des paragraphes : ils sont de la
matière citée, pas de la prose.

### L'ancrage

La prose de `#### What the model sees` doit être ancrée par un **littéral
concret**, sous l'une de ces trois formes exactement :

- du code inline, entre accents graves, citant le nom d'une constante, d'un
  fichier ou le texte lui-même ;
- un bloc de code imbriqué citant le texte verbatim ;
- un lien ancré vers [`docs/tool-catalog.md`](tool-catalog.md), fragment compris.

Une paraphrase ne suffit pas, et la raison est le mode d'échec que ce lot combat :
une section fraîche mais fausse survit indéfiniment. Un littéral faux se lit à
côté du code qui le porte, et une ancre morte casse la porte au renommage.

### Le prompt système se cite

Une surface dont le titre H3 contient « system prompt » est soumise à une règle de
plus : sous `#### What the model sees`, un H5 titré suivi d'un bloc ```` ```markdown ````.
Le texte envoyé au modèle se cite, il ne se décrit pas. Une description de prompt
est exactement le genre de prose qui dérive sans que rien ne le voie.

## La forme courte

Le contenu de la section est fermé, dans cet ordre exact et sans rien d'autre :

1. une phrase de classification, sur un paragraphe ;
2. une ligne blanche ;
3. le H4 `#### KV Cache effect` ;
4. une ligne blanche ;
5. un paragraphe.

La phrase de classification commence par `None, as ` ou par `Indirectly, through `
selon la forme déclarée dans la table, et se termine par un point. Une phrase
d'une autre amorce fait échouer la porte, qui cite les deux admises. Une amorce
qui contredit la table fait échouer la porte, qui cite les deux déclarations : la
table et le README ne peuvent pas dire deux choses.

Un H3, un `#### What the model sees` ou un `#### Token effect` sous une forme
courte font échouer la porte. La forme est fermée, sans quoi elle deviendrait une
forme structurée dégradée : le champ de cache reste obligatoire précisément parce
qu'il demande un raisonnement, et que sans lui la forme courte serait l'échappatoire
par défaut.

## L'omission

Un crate classé en omission n'a pas besoin de `README.md`, et la porte n'en exige
aucun. Sa justification vit dans la table.

Un crate classé en omission qui porte pourtant une section `## Model Experience`
fait échouer la porte : les deux déclarations se contredisent et la table doit
trancher. C'est ce croisement qui borne la durée de vie d'un classement
complaisant, le jour où le crate gagne une surface.

## Les quatre formes d'un effet de cache

Le champ `#### KV Cache effect` décrit ce que le crate fait au préfixe déjà
envoyé. Il en existe quatre formes, et nommer la bonne est le travail du champ :

**Croissance en ajout seul.** Le crate ajoute des tokens après ceux qui existent,
sans toucher à ce qui précède. Le préfixe antérieur reste réutilisable.

**Préfixe stable répété.** Le crate rend les mêmes octets à la même place d'une
requête à l'autre. C'est ce que protègent l'ordre stable-puis-volatil et la
génération de `StepContext`.

**Remplacement de tokens antérieurs.** Le crate réécrit ce qui a déjà été envoyé :
une compaction, une troncature rétroactive, un catalogue d'outils qui change. Tout
ce qui suit le point de coupe cesse d'être réutilisable.

**Requête indépendante.** Le crate émet une requête qui ne partage pas le préfixe
du tour, donc ne le touche ni ne le réutilise.

« N'invalide pas » signifie que **le crate préserve un préfixe déjà réutilisable**.
Ce n'est jamais une promesse du fournisseur : aucun crate de ce dépôt ne garantit
un succès de cache, et une section qui l'affirmerait est à corriger.

## Exemple : forme structurée

````markdown
## Model Experience

### Tool catalog

#### What the model sees

Les vingt-neuf outils enregistrés, chacun avec sa `description()` et son schéma
d'entrée. Le texte exact est rendu par
[`docs/tool-catalog.md`](../../docs/tool-catalog.md#read), qui est généré depuis
les outils eux-mêmes.

#### Token effect

La section `## Outils` du catalogue rend 26 117 octets de descriptions et de
schémas, mesure du fichier rendu et non un compte de tokens.

#### KV Cache effect

Préfixe stable répété tant que l'ensemble enregistré ne bouge pas. Le catalogue
occupe le premier niveau du préfixe, avant `system` et `messages` : changer une
description remplace des tokens antérieurs et invalide les trois niveaux.
````

## Exemple : forme structurée avec prompt système

````markdown
## Model Experience

### System prompt

#### What the model sees

Les instructions sélectionnées, closes par la section `HARNESS` que
`select_system_prompt` ajoute à toute sélection.

##### `HARNESS`

```markdown
# Pyxis harness contract

This section describes the harness you are ACTUALLY running in.
```

#### Token effect

Le contrat de harnais pèse 1 429 octets, ajoutés aux instructions du catalogue.

#### KV Cache effect

Préfixe stable répété : le texte est une constante, identique à chaque requête
d'une session.
````

## Exemple : forme courte

````markdown
## Model Experience

Indirectly, through the tool output that carries its refusal body.

#### KV Cache effect

Croissance en ajout seul : le corps de refus arrive dans un `tool_result`, après
tout ce qui a déjà été envoyé, et ne remplace aucun token antérieur.
````

## Exemple : omission

L'omission n'a pas de fichier : son exemple recopiable est l'entrée de la table,
dans `CLASSIFICATION`.

```rust
Classified {
    name: "agent-parity",
    shape: Shape::Omitted,
    justification: "Le crate lit le clone Codex épinglé et rend des matrices de contrat : il ne s'exécute qu'en vérification, hors de toute session, et rien de ce qu'il produit n'est injecté dans un tour.",
},
```

## Le message d'échec

Une violation tient sur une ligne, préfixée de `expérience du modèle: `, et nomme
le crate, la surface, le champ quand ils sont connus, et ce document. Toutes les
violations d'un même README sont rendues en une seule exécution : s'arrêter à la
première transforme un README à corriger en plusieurs allers-retours.

La commande ciblée est `cargo test -p agent-doc-gates`, portée par `just test` et
donc par `just check`. La porte n'ajoute ni recette au `justfile`, ni étape au CI,
ni commutateur d'écriture : elle ne rend qu'un verdict.
