# Note: ce que chaque crate envoie au modèle s'écrit à côté de son code

Statut: implemented

## Problème

Le dépôt envoyait à chaque requête un préfixe dont il maîtrisait l'ordre, la taille et la
stabilité, et n'écrivait nulle part ce que ce préfixe contenait. Six mesures sur l'état du
2026-08-21, avant ce lot :

1. **Six sites portaient la contrainte de préfixe cacheable, tous en commentaires de code, zéro
   dans `docs/`.** `crates/agent-runtime/src/context.rs` pour la génération d'injection,
   `crates/agent-cli/src/context.rs` en quatre points pour la stabilité du préfixe et le
   rafraîchissement qui la casserait, et `crates/agent-provider/src/chatgpt.rs` pour l'UUID v4
   stable envoyé en `prompt_cache_key`.
   [`docs/ARCHITECTURE.md`](../../../ARCHITECTURE.md) ne contenait ni « KV », ni « prompt cache »,
   ni « préfixe ». La conséquence était asymétrique : le contrat du fournisseur pose que l'ordre
   du préfixe est `tools`, `system`, `messages`, donc une pull request qui reformulait une
   `description()` d'outil jetait le cache de toutes les sessions ouvertes, et rien ne permettait
   à son relecteur de le savoir.
2. **3 449 octets d'instructions embarquées n'étaient nommés dans aucun document.**
   `crates/agent-cli/prompts/gpt5_generic.md` pour 2 609 octets et
   `crates/agent-cli/prompts/codex_finetuned.md` pour 840, chargés en dur quand le catalogue
   distant de modèles est injoignable. S'y ajoutaient `HARNESS` inconditionnellement et
   `CODE_MODE_ONLY` sous condition, deux constantes de `crates/agent-cli/src/prompt.rs`. Aucun
   document ne disait lequel de ces quatre textes s'ajoute quand ni dans quel ordre.
3. **26 117 octets de descriptions et de schémas d'outils occupaient le premier niveau du
   préfixe**, rendus par [`docs/tool-catalog.md`](../../../tool-catalog.md), un fichier de
   31 628 octets qui donne les 29 outils sans dire qu'ils sont envoyés à chaque requête.
4. **Quatre littéraux atteignaient le transcript sans être déclarés**, depuis quatre crates
   différents : `PRUNED_PLACEHOLDER` de la compaction, `NOT_PUBLISHED` de l'outil de fenêtre de
   contexte, le `continuation_hint` de la troncature de résultat d'outil, et le corps 403 du
   proxy réseau, qui remonte dans la sortie du `bash` qui l'a reçu. Le dernier n'est même pas
   soupçonnable depuis le nom de son crate.
5. **Zéro `README.md` existait sous `crates/`.** Les seuls fichiers `.md` de l'arbre étaient les
   deux prompts. Un crate n'avait donc aucun endroit où déclarer qu'il ne touche pas le modèle,
   et l'absence de documentation était indistinguable de l'absence d'effet : un relecteur qui ne
   trouvait rien sur `agent-session` ne pouvait rien conclure. C'est le mode d'échec le plus
   coûteux des six, parce qu'il rend le silence ambigu.
6. **Aucune des sept portes d'`agent-doc-gates` ne pouvait rattraper le trou** : aucune ne lisait
   `crates/` autrement que par les manifestes, et le balayage de la porte de liens s'arrêtait aux
   `.md` de la racine et de `docs/`.

## Décision

Un contrat écrit une fois, [`docs/model-experience.md`](../../../model-experience.md), et une
huitième porte qui le tient,
[`crates/agent-doc-gates/src/model_experience.rs`](../../../../crates/agent-doc-gates/src/model_experience.rs),
éprouvée par 47 tests. La réponse n'est pas un document central de plus : c'est une section par
crate, dans son `README.md`, à côté du code qu'elle décrit, et une porte qui refuse l'absence.

**Trois formes ferment l'ensemble.** La forme structurée porte un H3 par surface et sous chacun
trois champs H4 dans un ordre fixe, `#### What the model sees`, `#### Token effect`,
`#### KV Cache effect`, chacun suivi d'exactement un paragraphe non vide. La forme courte, pour
un crate qui n'écrit aucun texte mais change ce que le modèle verra, tient en une phrase ouvrant
sur `None, as ` ou `Indirectly, through `, suivie du seul champ de cache : elle coûte quand même
un raisonnement. L'omission nominative n'a pas de fichier du tout, sa justification vivant dans
la table, seul endroit où la lire.

**La classification est une table exhaustive des seize crates**, la constante `CLASSIFICATION`,
confrontée au disque dans les deux sens : un crate non classé échoue en nommant les trois formes,
une entrée orpheline échoue aussi. Huit crates sont structurés, quatre en forme courte, quatre en
omission. Le seuil de taille est ce qui rend ce choix possible : à seize crates, énumérer coûte
seize lignes et un crate ajouté au dépôt échoue à la porte avant qu'un lecteur se fie à une
liste muette. La source dsh, elle, ne pouvait pas se le permettre sur 227 paquets, et devait
donc partir d'un défaut implicite corrigé par deux listes d'exception.

**Le troisième champ est le seul contenu que le code ne dit pas.** Ce qu'un crate écrit se lit
dans le crate ; ce que cette écriture coûte en budget et ce qu'elle fait au préfixe réutilisable
ne se lit nulle part. C'est pourquoi la forme courte garde ce champ après avoir perdu les deux
autres.

**Ce que la porte prouve s'arrête à la forme.** Présence, ordre, densité, ancrage par un littéral
concret, et validité du fragment d'un lien vers le catalogue d'outils. Elle ne prouve jamais la
véracité de la prose : une section formellement conforme et matériellement fausse passe, et le
contrat l'énonce plutôt que de le taire. La seule chose qui casse mécaniquement quand le code
bouge est l'ancre de catalogue, qui meurt au renommage d'un outil ; c'est la raison d'être de la
règle d'ancrage, un littéral faux se voyant à la lecture du code voisin là où une paraphrase
fausse ne se voit pas.

**La porte n'ajoute ni recette ni étape de CI.** Elle est un huitième fichier de test atteint par
`just test`, donc par `just check`, et l'égalité entre les dépendances de `check` et les recettes
marquées interdisait de toute façon une cinquième recette marquée. Sa commande ciblée est
`cargo test -p agent-doc-gates`, celle des sept portes qui la précèdent, et
[`AGENTS.md`](../../../../AGENTS.md) la nomme dans sa table des signaux ciblés.

**La vérification d'ancrage vit dans la nouvelle porte, pas dans la porte de liens.** Celle-ci
refuse les ancres par décision écrite et testée, et ce lot ne rouvre pas ce refus : la nouvelle
porte doit de toute façon récolter les 29 titres du catalogue pour son garde de littéral concret,
donc le fragment se valide là où la donnée est déjà chargée. La porte de liens gagne seulement
les `README.md` de crate dans son balayage, pour que leurs liens relatifs cessent d'être
invérifiés.

## Alternatives écartées

**Générer les sections depuis le code.** Écartée en premier, et c'est l'alternative que la note
homologue de dsh avait écartée en premier aussi. Une section dérivée dirait ce que le code dit
déjà et perdrait le seul contenu utile, l'effet sur le budget et sur le cache, qui n'est
dérivable d'aucun jeton. Le mode d'échec visé a un nom : « frais mais inutile ». Les trois
catalogues générés du dépôt sont le contre-exemple utile, parce qu'ils rendent des faits que le
code porte littéralement ; l'effet de cache n'en est pas un.

**Imposer la forme structurée partout.** Seize sections à trois champs par surface auraient forcé
un paragraphe fourre-tout sur les crates qui n'écrivent rien, et un paragraphe fourre-tout est
exactement la prose que personne ne relit. La forme courte accepte de perdre deux champs pour
garder le troisième renseigné.

**N'exiger la section que des crates enregistrant un outil.** La règle aurait été mécanique et
gratuite, et elle aurait manqué `agent-sandbox`, dont le seul littéral model-facing est un corps
403 renvoyé par un proxy, et `agent-cli`, qui porte les quatre textes système sans enregistrer
d'outil. Le critère « enregistre un outil » n'est pas le critère « atteint le modèle ».

**`#![doc = include_str!("../README.md")]`.** Un registre unique séduisant, écarté sur son coût :
les seize crates portent `publish = false`, donc aucun rendu externe ne consomme le fichier, et
l'inclusion aurait demandé un travail de liens intra-doc, tous les liens relatifs du README étant
alors résolus par rustdoc et non par la porte de liens. Le prix est payé pour rien.

**Étendre la porte de liens aux ancres.** Elle refuse les ancres par décision écrite et testée,
prouver `#un-titre` demandant un parseur de titres complet pour un gain marginal. Ce lot vérifie
le seul fragment dont il a besoin, contre un catalogue généré dont les titres sont énumérables
sans parseur, et là où il charge déjà la donnée. Les deux portes ne se recouvrent pas : le chemin
est l'affaire de l'une, le fragment de l'autre.

**Un tableau à la place des paragraphes.** Trois colonnes auraient été plus compactes et auraient
invité la case vide, `n/a` en tête. Un paragraphe de prose sous chaque champ est plus long à
écrire et c'est le but : le champ le plus utile est celui qui explique, et il ne tient pas dans
une cellule.

**Une table de classification en markdown dans le contrat, parsée par la porte.** Le précédent
des classes de notes plaidait pour, le manifeste vide d'`agent-doc-gates` plaidait contre : un
parseur de plus est du code écrit à la main sur une entrée que le dépôt produit lui-même, alors
qu'une constante Rust mal formée est une erreur de compilation.

## Conséquences

Douze `README.md` existent maintenant sous `crates/`, huit structurés et quatre en forme courte,
et quatre crates sont classés en omission sans fichier. La mesure de sortie répond à celle
d'entrée : les six sites de préfixe cacheable sont écrits hors du code, les quatre textes système
sont cités verbatim, les quatre littéraux sont déclarés, et le catalogue d'outils reçoit ses
premiers liens entrants vers ses ancres.

Le coût est un second registre à côté des 2 213 lignes de `//!` du workspace, et le risque est
qu'ils divergent. Le contrat sépare les questions plutôt que les fusionner : le `//!` dit comment
le crate est fait, la section dit ce que le modèle en reçoit. Une section qui recopie son `//!`
est un signal de relecture, pas un état acceptable.

Le risque résiduel assumé est la section fraîche mais fausse. La porte tiendra la forme
indéfiniment sur une prose recopiée d'un état ancien du code, et seule l'ancre de catalogue meurt
d'elle-même. Le fermer demanderait le catalogue généré que la première alternative écarte, donc
il reste ouvert et écrit.

Un crate ajouté au dépôt échoue désormais à la porte tant qu'il n'est pas classé, et un crate
classé sans effet qui gagne une section échoue en citant ses deux déclarations contradictoires.
C'est ce qui borne la durée de vie d'un classement complaisant.

Cette décision est une note et non un ADR parce que rien dans `crates/` ne peut la violer :
aucune pull request sur la source livrée ne contredit une règle d'écriture de README, seule une
porte documentaire le peut. C'est exactement la frontière que
[`docs/notes/README.md`](../../README.md) et [`AGENTS.md`](../../../../AGENTS.md) posent, et le
symétrique d'ADR-12, qui est un ADR parce qu'un changement d'`agent-runtime` peut le casser.
