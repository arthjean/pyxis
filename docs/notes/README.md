# Arbre des notes de décision

Une note enregistre une décision qui touche ce dépôt : le verdict, ce qu'il a
écarté, ce qu'il coûte, la part que ni le code ni les documents d'architecture ne
portent. Ce format n'est pas une convention écrite : le crate `agent-doc-gates` le
vérifie et `cargo test --workspace` échoue sur une note mal placée ou mal formée.
Toute règle tenue par la machine est énoncée ici ; les conventions humaines qui
restent sont listées à la fin.

## Le chemin porte deux axes

Toute note vit à `{cycle de vie}/{classe}/aaaa-mm-jj-sujet.md`, relativement à
`docs/notes/` : le chemin dit le statut et le type de la décision avant qu'on
ouvre le fichier. Changer de statut, c'est changer de répertoire, et un statut
divergent du répertoire devient impossible, la porte croisant les deux.

Le cycle de vie est le répertoire de premier niveau, et l'ensemble est fermé :

- `proposed/` : la décision est proposée, ni tranchée ni construite.
- `implemented/` : la décision est prise et livrée. La note décrit ce qui est, au
  présent, et suit le code quand un chemin ou un nom bouge.
- `rejected/` : la proposition a été pesée et refusée. La note reste tant que sa
  raison empêche une erreur tentante.

La classe est le répertoire imbriqué, et l'ensemble est fermé lui aussi :

| Classe | Ce qu'elle couvre |
|---|---|
| `feature` | Une capacité nouvelle offerte à l'utilisateur ou au modèle. |
| `bug-fix` | La correction d'un défaut observé ou d'un trou qu'un post-mortem a mis au jour. |
| `simplification` | Un retrait de code, de comportement ou de surface, sans capacité nouvelle. |
| `architecture` | Une décision de structure sur la source livrée : graphe de crates, invariants, vocabulaire du runtime. |
| `process` | L'outillage et la politique autour du code : portes, dépendances, conventions de dépôt. Jamais le comportement livré. |
| `testing` | L'infrastructure et la stratégie de test. |

`architecture` porte sur ce qu'on livre, `process` sur ce qui entoure la
livraison ; `simplification` retire de la surface, `bug-fix` répare un comportement.
Ajouter une classe étend l'ensemble du crate et ce tableau ensemble. Le nom du
fichier est `aaaa-mm-jj-sujet.md`, où la date est celle de l'événement décrit,
jamais celle du commit qui l'ajoute au dépôt. L'arbre ne contient que des fichiers
`.md`, et seul ce `README.md` est autorisé à sa racine.

## Les trois premières lignes

Elles sont exactement `# Note: <titre>`, une ligne vide, puis la ligne de statut,
suivie d'une ligne vide : quatre lignes suffisent à connaître le titre et le
verdict. La valeur du statut reprend littéralement le nom du répertoire, ce qui
rend la comparaison exacte et évite une table de correspondance :
`Statut: proposed`, `Statut: implemented` ou `Statut: rejected - <raison>`. Le
rejet est le seul statut qui porte du contenu, sa raison étant ce que le lecteur
vient chercher. Le statut ne porte ni date ni parenthèse, la date vivant dans le
nom du fichier et le reste dans git, et une note n'a qu'une ligne de statut.

## Le squelette du corps

Le corps ouvre toujours sur `## Problème`, écrit pour tenir sans la solution. Les
sections récurrentes portent ces noms et pas d'autres ; les sections techniques
propres à une note restent libres entre elles.

```markdown
# Note: <titre>

Statut: proposed

## Problème
## Proposition
## Alternatives écartées
## Critères d'acceptation
## Risques
```

```markdown
# Note: <titre>

Statut: implemented

## Problème
## Décision
## Alternatives écartées
## Conséquences
```

```markdown
# Note: <titre>

Statut: rejected - la raison en une ligne

## Problème
## Proposition
## Alternatives écartées
```

Dans une note `implemented/`, les titres propres à une proposition sont refusés :
`## Proposition`, `## Plan`, `## Plan de migration` et `## Critères d'acceptation`.
Une note livrée dit ce qui est, pas ce qui était prévu. Une note `rejected/` est à
l'inverse la proposition gelée : elle garde ses sections d'origine et le verdict
vit sur la ligne de statut. Les lignes d'un bloc de code délimité ne sont pas de
la structure : une note cite un en-tête en exemple sans que la porte s'en saisisse.

## Alternatives écartées, obligatoires

Toute note porte `## Alternatives écartées` : chaque option réellement pesée et
pourquoi elle a perdu. Une décision consignée sans ce qu'elle a battu se
re-litige, et c'est la défaillance que ces notes existent pour empêcher.

Les alternatives se consignent, elles ne s'inventent pas. Une note antérieure au
2026-08-20, date d'adoption du format, dont les alternatives ne sont pas
reconstructibles porte à leur place ce commentaire, comparé littéralement :

```text
<!-- note-format: alternatives-non-consignees (note anterieure au format) -->
```

La porte le refuse sur une note datée du 2026-08-20 ou après, et le refuse aussi
quand la note porte en plus une vraie section d'alternatives. Cette date est la
constante `FORMAT_ADOPTED` de `crates/agent-doc-gates/src/lib.rs`, écrite une fois.

## Liens et absence d'index

Un renvoi d'une note vers une autre est un lien Markdown relatif, jamais une
référence en prose ni un numéro : un lien se vérifie, une phrase ne se vérifie pas.

Aucun fichier d'index centralisé n'est autorisé dans l'arbre, `INDEX.md` en tête.
Un index tenu à la main diverge en silence, et celui de `docs/DECISIONS.md` a déjà
divergé de ses propres sections sans que personne ne le voie. L'arbre est son
propre inventaire : on parcourt ses répertoires ou on cherche par `grep`.

## Déplacer une note entre deux cycles de vie

Déplacer le fichier ne suffit pas : la ligne de statut et le squelette du
répertoire d'arrivée sont repris dans le même changement, sinon la porte échoue. De
`proposed/` vers `implemented/`, `## Proposition` devient `## Décision` au présent
et `## Critères d'acceptation` comme `## Risques` se replient dans
`## Conséquences` ; vers `rejected/`, seule la raison s'ajoute au statut.

## Ce que la machine ne vérifie pas

Ces règles sont des conventions humaines ; aucune porte ne les tient.

- Quand écrire une note : rien n'impose d'en produire une par changement, la
  contrainte utile étant « toute décision qu'on pourrait vouloir rejouer ».
- Que la date du nom soit celle de l'événement plutôt que celle du commit.
- Que la classe retenue soit la bonne parmi les six.
- Que les alternatives consignées aient réellement été pesées à l'époque.
- Que les liens relatifs résolvent vers un fichier existant.

## La porte

`cargo test -p agent-doc-gates`, incluse dans le `cargo test --workspace` que la
CI lance déjà. Elle ne lit rien hors du dépôt et n'accède pas au réseau.
