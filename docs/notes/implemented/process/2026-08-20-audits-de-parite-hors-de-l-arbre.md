# Note: les audits de parité restent des mesures, hors de l'arbre des notes

Statut: implemented

## Problème

Trois audits de parité datés vivaient à la racine de `docs/` :
[`parity-audit-2026-07-24.md`](../../../parity/audits/parity-audit-2026-07-24.md), 2 551 lignes et
348 Ko, puis [`-25`](../../../parity/audits/parity-audit-2026-07-25.md) et
[`-27`](../../../parity/audits/parity-audit-2026-07-27.md), plus courts. Aucun n'était lié depuis un
sommaire, aucun ne disait s'il décrivait l'état en vigueur ou un instantané périmé, et un lecteur
tombant dessus ne pouvait pas savoir lequel des deux il tenait.

L'arbre des notes ouvrait une place évidente, et le plan de portage proposait de les y verser tels
quels. Mais un audit n'est pas une décision : c'est une mesure prise un jour donné. L'y verser
obligeait à lui inventer une section `## Décision` que son auteur n'avait pas écrite, ce que
[le format](../../README.md) interdit explicitement, et le fichier de 348 Ko y serait devenu une
anomalie permanente au milieu de notes de trente lignes.

## Décision

Les trois audits deviennent une note unique qui les référence, celle-ci, et les fichiers eux-mêmes
descendent sous `docs/parity/audits/`, à côté de la baseline de parité qu'ils ont précédée. Le
répertoire `docs/parity/` est déjà le domicile du sujet, et son
[`README.md`](../../../parity/README.md) les nommait déjà comme du contexte historique : le
déplacement ne fait que rendre l'emplacement conforme à ce que le document disait.

Ce qui entre dans l'arbre, c'est la décision de chemin que ces audits ont portée, et elle y est
déjà : [ne pas partir de la base Codex CLI](../architecture/2026-07-27-ne-pas-partir-de-la-base-codex.md).
Les mesures restent consultables, sans statut de décision, sans cycle de vie, et sans qu'un lecteur
de l'arbre ait à les traverser.

Plus aucun document de décision ou d'audit daté ne subsiste à la racine de `docs/`.

## Alternatives écartées

**Trois notes, une par audit.** Écartée pour la raison qui décide : il aurait fallu écrire une
`## Décision` là où l'auteur n'avait consigné qu'une mesure. Le format le refuse et la dispense
datée ne couvre pas ce cas, puisqu'elle vaut pour des alternatives non reconstructibles, pas pour
une décision inexistante. Fabriquer un verdict après coup est précisément la dérive que ces notes
existent pour empêcher.

**Une note unique, les audits versés dans l'arbre.** Écartée à cause du volume : 348 Ko dans un
arbre dont la promesse est qu'on lit une note en entier. La seule échappatoire aurait été de
documenter l'exception dans le README, c'est-à-dire d'affaiblir le format pour un fichier.

**Les laisser à la racine de `docs/`.** Écartée : c'est l'état de départ, et son défaut mesuré est
qu'un fichier orphelin sort du champ de vision au commit suivant. Le déplacement sous
`docs/parity/` le rattache à un document qui le présente.

**Les supprimer.** Écartée : les mesures qu'ils portent (vélocité amont de Codex, dépendances
internes de ses crates, couplage Responses de son cœur) sont ce qui soutient la décision de chemin.
Une décision dont la preuve a disparu se re-litige.

## Conséquences

`docs/parity/audits/` devient l'endroit où atterrit une mesure datée qui n'est pas une décision, ce
qui donne une réponse au prochain audit sans rouvrir la question. La frontière est nette : l'arbre
reçoit des décisions, `docs/parity/` reçoit des mesures.

Les liens relatifs entre les trois audits ont survécu au déplacement parce qu'ils ont bougé
ensemble, mais le renvoi de la note d'architecture vers l'audit du 27 a cassé, et c'est
[la porte de liens](../../../../crates/agent-doc-gates/src/links.rs) qui l'a signalé avant que
quiconque l'ouvre. C'est le premier usage réel de cette porte, et la démonstration de ce qu'elle
achète : sans elle, la migration se serait déclarée faite avec un lien mort.
