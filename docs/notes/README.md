# Arbre des notes de décision

Une note enregistre une décision qui touche ce dépôt : le verdict, ce qu'il a écarté, ce qu'il
coûte, la part que ni le code ni les documents d'architecture ne portent. Ce format n'est pas
une convention mais une porte : `agent-doc-gates` le vérifie et `cargo test --workspace` échoue
sur une note mal placée ou mal formée. Sa commande ciblée est `cargo test -p agent-doc-gates` ;
elle ne lit rien hors du dépôt et n'accède pas au réseau. Toute règle tenue par la machine est
énoncée ici, les autres sont listées à la fin.

## Frontière avec le registre ADR

Une décision entre dans [`docs/DECISIONS.md`](../DECISIONS.md) quand un changement futur
des crates livrées peut la violer, et dans cet arbre quand rien dans `crates/` ne le peut.
ADR-12 fixe la façon dont `agent-runtime` atteint `run_agent`, une pull request peut la
casser : c'est un ADR. [Ne pas partir de la base Codex](implemented/architecture/2026-07-27-ne-pas-partir-de-la-base-codex.md)
tranche un chemin déjà pris qu'aucun crate ne contredit : c'est une note. Un ADR n'a pas
de note miroir, une note qui le touche le lie. En cas de désaccord, l'ADR l'emporte et la
note est périmée : elle se corrige ou part en `rejected/`.

## Le chemin porte deux axes

Toute note vit à `{cycle de vie}/{classe}/aaaa-mm-jj-sujet.md`, relativement à `docs/notes/` :
le chemin dit le statut et le type de la décision avant qu'on ouvre le fichier. Changer de
statut, c'est changer de répertoire, et un statut divergent du répertoire devient impossible,
la porte croisant les deux.

Le cycle de vie est le répertoire de premier niveau, et l'ensemble est fermé :

- `proposed/` : la décision est proposée, ni tranchée ni construite.
- `implemented/` : la décision est prise et livrée. La note décrit ce qui est, au présent, et suit le code quand un chemin ou un nom bouge.
- `rejected/` : la proposition a été pesée et refusée. La note reste tant que sa raison empêche une erreur tentante.

La classe est le répertoire imbriqué, et l'ensemble est fermé lui aussi :

| Classe | Ce qu'elle couvre |
|---|---|
| `feature` | Une capacité nouvelle offerte à l'utilisateur ou au modèle. |
| `bug-fix` | La correction d'un défaut observé ou d'un trou qu'un post-mortem a mis au jour. |
| `simplification` | Un retrait de code, de comportement ou de surface, sans capacité nouvelle. |
| `architecture` | Une décision de structure sur la source livrée : graphe de crates, invariants, vocabulaire du runtime. |
| `process` | L'outillage et la politique autour du code : portes, dépendances, conventions de dépôt. Jamais le comportement livré. |
| `testing` | L'infrastructure et la stratégie de test. |

`architecture` porte sur ce qu'on livre, `process` sur ce qui entoure la livraison ;
`simplification` retire de la surface, `bug-fix` répare un comportement. Ajouter une classe
étend l'ensemble du crate et ce tableau ensemble. Le nom du fichier est `aaaa-mm-jj-sujet.md`,
où la date est celle de l'événement décrit et non celle du commit. L'arbre ne contient que des
fichiers `.md`, et seul ce `README.md` est autorisé à sa racine.

## Les trois premières lignes

Elles sont exactement `# Note: <titre>`, une ligne vide, puis la ligne de statut, suivie d'une
ligne vide : quatre lignes suffisent à connaître le titre et le verdict. Le statut reprend
littéralement le nom du répertoire, ce qui rend la comparaison exacte et évite une table :
`Statut: proposed`, `Statut: implemented` ou `Statut: rejected - <raison>`. Le rejet est le
seul qui porte du contenu, sa raison étant ce que le lecteur vient chercher, et le statut est
unique dans la note.

## Le squelette du corps

Le corps ouvre toujours sur `## Problème`, écrit pour tenir sans la solution. Les
sections récurrentes portent ces noms et pas d'autres ; les autres restent libres.

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
`## Proposition`, `## Plan`, `## Plan de migration`, `## Critères d'acceptation`. Une note livrée
dit ce qui est, pas ce qui était prévu ; une note `rejected/` est la proposition gelée, sections
d'origine gardées et verdict sur la ligne de statut. Les lignes d'un bloc délimité ne sont pas de
la structure : un en-tête cité en exemple échappe à la porte.

## Alternatives écartées, obligatoires

Toute note porte `## Alternatives écartées` : chaque option réellement pesée et pourquoi elle a
perdu. Une décision consignée sans ce qu'elle a battu se re-litige, et c'est la défaillance que
ces notes existent pour empêcher. Les alternatives se consignent, elles ne s'inventent pas :
une note antérieure au 2026-08-20, date d'adoption du format, dont les alternatives ne sont pas
reconstructibles porte à leur place ce commentaire, comparé littéralement :

```text
<!-- note-format: alternatives-non-consignees (note anterieure au format) -->
```

La porte le refuse sur une note datée du 2026-08-20 ou après, et le refuse aussi quand la note
porte en plus une vraie section d'alternatives. Cette date est la constante `FORMAT_ADOPTED` de
`crates/agent-doc-gates/src/lib.rs`, écrite une fois.

## Liens et absence d'index

Un renvoi vers une autre note est un lien Markdown relatif, jamais une référence en prose : un
lien se vérifie, une phrase ne se vérifie pas. Tout lien relatif des fichiers Markdown de
`docs/` et de la racine résout vers un fichier existant, sinon la suite échoue en nommant
source, ligne et cible. Seule l'existence du fichier compte : l'ancre `#un-titre` est retirée
avant résolution, une adresse `http://` ou `mailto:` est ignorée sans accès réseau, un lien
remontant hors du dépôt est refusé, et un lien cité dans un bloc délimité est un exemple.

Aucun fichier d'index centralisé n'est autorisé dans l'arbre, `INDEX.md` en tête. Un index tenu
à la main diverge en silence, et celui de `docs/DECISIONS.md` a déjà divergé de ses sections
sans que personne ne le voie. L'arbre est son propre inventaire : on parcourt ses répertoires
ou on cherche par `grep`.

## Déplacer une note entre deux cycles de vie

Déplacer le fichier ne suffit pas : la ligne de statut et le squelette du répertoire d'arrivée sont
repris dans le même changement, sinon la porte échoue. De `proposed/` vers `implemented/`,
`## Proposition` devient `## Décision` au présent, `## Critères d'acceptation` et `## Risques` se
replient dans `## Conséquences` ; vers `rejected/`, la raison s'ajoute au statut, et la porte de
liens dit ensuite quels renvois le déplacement a cassés.

## Ce que la machine ne vérifie pas

Ces règles sont des conventions humaines ; aucune porte ne les tient.

- Quand écrire une note : rien n'impose d'en produire une par changement, la contrainte utile
  étant « toute décision qu'on pourrait vouloir rejouer ».
- Que la date du nom soit celle de l'événement plutôt que celle du commit.
- Que la classe retenue soit la bonne parmi les six.
- Que les alternatives consignées aient réellement été pesées à l'époque.
- Que l'ancre d'un lien désigne un titre qui existe dans le fichier cible.
