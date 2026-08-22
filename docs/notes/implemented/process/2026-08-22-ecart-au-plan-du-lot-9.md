# Note: Écart au plan sur le lot 9, et ordre de lecture de ses sources

Statut: implemented

## Problème

La ligne 9 de [`docs/deepseek-harness-porting-plan.md`](../../../deepseek-harness-porting-plan.md)
décrit le registre de travaux de fond par un signal de vérification en trois clauses : « un `bash`
long survit à la fin du tour, son résultat revient, un redémarrage le retrouve ». Les deux
premières se lisent directement dans les sources citées. La troisième, non : elle n'a **aucune
source**. Le fournisseur que la ligne nomme comme référence, `packages/jobs/jobs-local/src/index.ts:1-10`,
ouvre sur un doc-comment disant qu'il garde tout en mémoire, donc DeepSeek Harness ne fait
survivre aucun travail à un redémarrage. Un lecteur qui prendrait la ligne du plan pour un
inventaire de ce qui est porté conclurait que la survie de processus a été livrée ailleurs, ou
qu'elle reste due.

La colonne source de cette même ligne a un second défaut, indépendant : elle nomme les trois
fichiers du **registre** (`types.ts`, `jobs-local/src/index.ts`, `tool-jobs/src/index.ts`) et
aucun des deux fichiers qui **produisent** un travail de fond, `packages/shell/tool-bash/src/index.ts`
et `packages/shell/tool-bash/src/background.ts`. Lus dans l'ordre du plan, les trois fichiers cités
donnent la comptabilité sans le producteur, c'est-à-dire la moitié qui ne dit pas pourquoi le
registre existe.

## Décision

L'écart est consigné ici et tranché là-bas. La frontière retenue, « les enregistrements sont
durables, les processus ne le sont pas ; une reprise rapporte et n'exécute rien », est
[ADR-16](../../../DECISIONS.md), parce qu'une pull request sur `crates/` peut la violer en
ajoutant une ré-attache par pid. Cette note ne la répète pas et ne l'arbitre pas : elle enregistre
que la clause 3 a été **renégociée sur les invariants de Pyxis** faute de source à porter, et non
implémentée d'après un modèle amont.

L'ordre de lecture des sources du lot est celui-ci, et il n'est pas celui de la colonne du plan :

1. `packages/shell/tool-bash/src/index.ts` et `packages/shell/tool-bash/src/background.ts`, les
   producteurs, qui montrent ce qu'un travail de fond est chez dsh : un enfant ordinaire du
   processus courant.
2. `packages/jobs/jobs/src/types.ts`, le vocabulaire fermé dont FR-01 reprend les cinq états.
3. `packages/jobs/jobs-local/src/index.ts`, la comptabilité, dont la ligne 422 fixe le moment
   exact où un résultat est marqué livré.
4. `packages/jobs/tool-jobs/src/index.ts`, la surface offerte au modèle.

Le plan lui-même n'est pas corrigé : c'est un document daté d'intention, et le réécrire après coup
effacerait la trace de ce qui a été décidé quand.

## Alternatives écartées

| Option | Pourquoi écartée |
|---|---|
| **Corriger la colonne source et le signal de vérification dans le plan** | Le plan est un artefact daté d'intention, pas un inventaire tenu à jour. L'éditer effacerait précisément ce que cette note existe pour garder : que la clause 3 a été écrite sans source et renégociée ensuite. |
| **Ne rien consigner, ADR-16 suffisant** | ADR-16 dit ce qui est décidé, pas que le plan promettait autre chose. Le risque 3 du PRD est nommément « la clause 3 du plan est lue comme livrée alors qu'elle est renégociée » : c'est un défaut de lecture, et un ADR ne s'attrape pas depuis la ligne 9 du plan. |
| **Faire de cet écart un ADR plutôt qu'une note** | La règle de frontière d'`AGENTS.md` tranche seule : rien dans `crates/` ne peut contredire « la ligne 9 du plan omet deux fichiers ». Un chemin de lecture déjà pris est exactement le cas que l'arbre de notes couvre. |
| **Lier cette note depuis ADR-16** | Le `README.md` de l'arbre pose la règle réciproque : un ADR n'a pas de note miroir, une note qui le touche le lie. Le lien va donc d'ici vers ADR-16 et pas l'inverse. |

## Conséquences

- La ligne 9 du plan reste telle qu'elle a été écrite, avec sa colonne source incomplète et sa
  troisième clause sans source. Qui la lit sans passer par cette note gardera l'impression que
  la survie de processus était portée.
- Une future ligne de plan citant `jobs-local` comme preuve de durabilité doit être vérifiée
  avant d'être reprise : ce fournisseur est en mémoire, et l'a toujours été.
- Le lot 10, la planification de travaux, est ordonné après celui-ci et héritera de la même
  frontière : un travail planifié qu'un redémarrage traverse sera rapporté, pas ressuscité, tant
  qu'ADR-16 n'est pas rouvert.
