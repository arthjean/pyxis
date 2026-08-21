# Note: trois documents de structure deviennent dérivés et prouvés frais

Statut: implemented

## Problème

Le dépôt publiait quatre artefacts générés et prouvés frais, les deux schémas d'app-server et
les deux matrices de parité. Tout le reste de `docs/` était rédigé à la main, y compris les
documents dont le contenu est intégralement dérivable du code. Quatre écarts étaient mesurables
sur l'état du 2026-08-21, avant ce lot :

1. **Des crates absents de tableaux qui se donnaient pour exhaustifs.** Seize crates existaient
   sous `crates/`. Le [`README.md`](../../../../README.md) en listait dix, le tableau de
   [`ARCHITECTURE.md`](../../../ARCHITECTURE.md) onze, son graphe ASCII onze aussi, et ce graphe
   omettait l'arête `agent-tools -> agent-code-mode`. Chaque crate ajouté depuis leur écriture
   les avait rendus un peu plus faux sans déclencher aucun signal.
2. **Des clés de configuration sans documentation normative.** `KNOWN_KEYS` en comptait quinze.
   Cinq n'apparaissaient dans aucun document du dépôt (`cost_budget_micro_usd`,
   `input_cost_micro_per_ktok`, `output_cost_micro_per_ktok`, `overload_fallback_model`,
   `safe_commands`) et deux, `token_budget` et `web_search`, n'apparaissaient que dans des audits
   de parité qu'[`AGENTS.md`](../../../../AGENTS.md) déclare non normatifs : sept clés sur quinze
   sans document opposable.
3. **Un écart de deux entre les clés de sécurité annoncées et celles que le code refuse.** Le
   `README.md` en annonçait cinq, `SECURITY_KEYS` en refusait sept : `web_search` et
   `safe_commands` s'étaient ajoutées au code sans que la phrase suive. Un `-c web_search=true`
   échouait donc sans que le document ait prévenu.
4. **Dix désarmements de `returns_untrusted` listés nulle part.** Le défaut du trait `Tool` est
   fermé, et dix implémentations d'`agent-tools` le redescendaient à `false`. Chacune avait été
   revue seule, sur sa propre ligne, contre rien. Savoir qu'un diff de trois lignes portait le
   compte de dix à onze demandait un `grep` sur tout l'espace de travail, et savoir si le
   onzième était cohérent avec les dix autres demandait de les ouvrir.

Ce qui rend ces quatre écarts coûteux ici est le lecteur visé. Un agent de codage lit `AGENTS.md`
puis `ARCHITECTURE.md` en entrée de session avec un contexte vierge et prend le premier tableau
trouvé pour la vérité : celui des crates ne mentionnait ni `agent-app-server` ni les deux crates
Code Mode, donc l'agent cherchant où poser une méthode de protocole inventait un emplacement.
Et rien n'empêchait la prochaine occurrence : les quatre artefacts générés du dépôt l'étaient
parce que quelqu'un avait écrit un générateur pour chacun, aucune règle ne disant qu'un document
décrivant une structure doit être dérivé. Le taux de dérive observé était de trois documents
sur trois.

## Décision

Trois documents deviennent dérivés, chacun rendu par une fonction pure de son état collecté vers
une `String`, chacun comparé octet pour octet à son fichier par un test que `cargo test
--workspace` exécute : [`crate-graph.md`](../../../crate-graph.md) depuis les seize
`crates/*/Cargo.toml`, [`tool-catalog.md`](../../../tool-catalog.md) depuis les `DynTool` que le
binaire enregistre réellement, [`config-catalog.md`](../../../config-catalog.md) depuis la table
déclarative confrontée à `KNOWN_KEYS` et `SECURITY_KEYS`. La mécanique n'est pas inventée :
`crates/agent-app-server/tests/schemas.rs` la portait déjà, avec une variable d'environnement qui
bascule le même code de la comparaison vers l'écriture et un message d'échec qui nomme la commande
de régénération. Le lot en produit trois instances de plus, sous une seule variable,
`PYXIS_UPDATE_CATALOGS`, et ajoute leurs trois lignes à `regen` du
[`justfile`](../../../../justfile), jamais à une recette de vérification. Une porte le tient
désormais mécaniquement : un commutateur d'écriture hors de `regen`, ou une recette qui atteint
`regen`, échoue `cargo test -p agent-doc-gates`.

Une comparaison de fraîcheur seule est aveugle à un générateur incomplet : elle accepte un
document rendu depuis zéro élément. Chaque catalogue porte donc un garde de complétude qui le
confronte à sa source de vérité, dans les deux sens : `crates/*/Cargo.toml` pour le graphe, les
sites `.register(` de `main.rs` pour les outils, `KNOWN_KEYS`, `SECURITY_KEYS` et les variables
`PYXIS_*` lues sous `crates/*/src` pour la configuration. Une entrée qui n'instancie aucun outil
échoue, une clé que le loader accepte et que la table ignore échoue, et l'inverse aussi.

La mesure après le lot, sur les quatre écarts ci-dessus :

| Métrique | Avant | Après |
|---|---|---|
| Crates absents d'un tableau publié comme exhaustif | 6 pour le `README.md`, 5 pour `ARCHITECTURE.md` | 0 sur 16 : les deux documents renvoient au graphe rendu et ne recopient plus |
| Clés de `KNOWN_KEYS` sans documentation normative | 7 sur 15 | 0 sur 15, avec type, défaut, couche, drapeau, variable et caractère de sécurité |
| Écart entre les clés de sécurité annoncées et refusées | 2, cinq annoncées contre sept refusées | 0 : le compte est rendu depuis `SECURITY_KEYS` et le `README.md` renvoie au catalogue |
| Sites désarmant `returns_untrusted` lisibles dans un document | 0 sur 10 | 10 sur 10, dans une colonne dont la synthèse donne le compte |

Le coût en temps a été mesuré sur cache chaud, trois exécutions de chaque côté sur la même
machine. La suite privée des trois portes, par un triple `--skip` sur `crate_graph`,
`tool_catalog` et `config_catalog`, prend 18,60 s, 18,56 s et 18,83 s. La suite entière prend
18,55 s, 18,60 s et 18,71 s. L'écart des médianes est nul et tient sous le
bruit de mesure, de l'ordre de trois dixièmes de seconde : les trois portes coûtent moins que la
dispersion de la suite, très en deçà du plafond de 3 s. C'était attendu, les trois binaires de
test rapportant 0,00 s, 0,01 s et 0,05 s de temps d'exécution propre ; ce que le lot ajoute est
de la lecture de fichiers, pas du calcul.

Trois décisions de conception que le plan de portage n'avait pas anticipées ont été prises en
cours de route.

**La répartition des générateurs suit l'accès à la donnée, pas le thème.** Le graphe de crates se
lit dans seize fichiers TOML, donc il tient entièrement dans `agent-doc-gates`, dont le manifeste
interdit toute dépendance et impose un parseur écrit à la main, comme pour l'arbre de notes et
pour le `justfile`. Les deux autres ont besoin de symboles privés à `agent-cli` : `KNOWN_KEYS`,
`SECURITY_KEYS` et `ConfigLayer` pour la configuration, l'instanciation de `DynTool` pour les
outils. Or `agent-cli` n'a qu'une cible `[[bin]]` et aucun `[lib]`, donc `crates/agent-cli/tests/`
ne peut rien en importer : les deux générateurs vivent sous `#[cfg(test)]` dans la cible binaire,
où les tests unitaires s'exécutent bien. Ouvrir une cible de bibliothèque pour exposer les
internes d'un binaire à un besoin documentaire aurait été un changement plus large que le lot.

**Le catalogue d'outils garde un manifeste rédigé plutôt que `default_registry`.** Le jeu réel est
câblé dans vingt-neuf sites d'enregistrement de `main.rs`, dont plusieurs exigent un handle de
runtime, tandis que `agent_tools::default_registry` n'en expose que onze et n'est référencé que
par des tests. Rendre le catalogue depuis `default_registry` aurait documenté un jeu d'outils que
personne n'utilise. Le manifeste est donc écrit à la main, ce qui n'est tenable que parce qu'un
garde le croise avec les sites `.register(` dans les deux sens : un outil enregistré et absent du
manifeste échoue, une entrée de manifeste que plus rien n'enregistre aussi.

**Le rôle d'un crate descend dans son propre manifeste.** Sans une ligne `description` dans chacun
des seize `Cargo.toml`, le graphe généré n'aurait pu porter que des noms et des arêtes, et les deux
tableaux rédigés seraient restés en place avec leur dérive. Avec elle, le rôle vit à côté du code,
le générateur le lit, et les tableaux du `README.md` et d'`ARCHITECTURE.md` deviennent un lien. La
colonne « dépendances interdites » d'`ARCHITECTURE.md`, elle, reste rédigée : c'est un invariant,
pas un fait, et l'absence actuelle d'une arête ne dit pas qu'elle est proscrite.

**Pourquoi une note et non un ADR.** La frontière écrite dans [`README.md`](../../README.md) de
l'arbre et dans `AGENTS.md` est un test : une décision entre au registre ADR quand un changement
futur des crates livrées peut la violer. « Un document de structure est dérivé » est une pratique
d'outillage, non un invariant de code : rien dans `crates/` ne peut la contredire, seul un futur
document rédigé à la main le pourrait, et c'est une décision de dépôt. La note est donc le
registre juste, comme pour l'[arbre de notes](2026-08-20-arbre-de-notes-de-decision.md) et le
[graphe de portes](2026-08-21-graphe-de-portes.md), dont ce lot est le second usage.

## Alternatives écartées

**`gen-doc-graphs.ts` comme source du lot.** Le plan de portage désignait ce script de DeepSeek
Harness comme l'origine des trois catalogues. La lecture du dépôt source a montré que c'était une
erreur d'ancre : ses 1 478 lignes rendent les coutures de capacité, le cycle de vie et un index,
et Pyxis n'a pas d'équivalent des soixante-dix coutures de capacité. Les trois catalogues cités
dans la même cellule viennent en réalité de trois autres scripts. Reprendre le premier aurait
produit un document sans lecteur ; les trois retenus répondent chacun à un écart mesuré.

**L'empreinte SHA-256, la forme qu'`agent-parity` emploie pour ses matrices.** Elle prouve qu'un
fichier n'a pas bougé, pas qu'il dit la vérité sur le code d'aujourd'hui, et son message d'échec
donne deux chaînes hexadécimales là où une comparaison de `String` donne un diff. Le lot compare
donc les octets rendus, ce que `schemas.rs` faisait déjà.

**Le diff de la première ligne divergente**, imprimé par le générateur d'outils de dsh. `assert_eq!`
sur deux `String` rend déjà un diff en Rust, et `schemas.rs` en est le précédent : écrire un
comparateur de lignes aurait ajouté du code pour retirer de l'information.

**La régénération suivie d'un `git diff` sale**, la forme de `tfplugindocs`, `terraform-docs` et
des scripts `verify-*.sh` de Kubernetes. Elle suppose `git` disponible et un arbre propre, là où
la comparaison dans le test rend le même verdict sur n'importe quel runner, sans processus lancé
ni socket ouvert.

**Les outils MCP dynamiques dans le catalogue d'outils.** Leur nombre dépend des serveurs connectés
au démarrage : aucun document comparé octet pour octet ne peut les contenir sans échouer selon
l'environnement de celui qui lance la suite. Ils entrent par `.register_dyn(` et le catalogue dit
explicitement qu'ils sont hors périmètre, plutôt que de les omettre en silence.

**`cargo_metadata` et les registres à l'édition des liens, `inventory` et `linkme`.** Le premier
donnerait le graphe résolu mais suppose de lancer `cargo`, ce que l'hermétisme du lot interdit et
que le manifeste sans dépendance d'`agent-doc-gates` rend impossible. Les seconds servent à
collecter des éléments dispersés entre crates, là où Pyxis a un `Registry::register` explicite :
l'énumération directe suffit et n'ajoute aucune dépendance. Le lot ferme sur zéro dépendance
ajoutée à l'espace de travail.

**Le marqueur `linguist-generated=true`**, convention Go du fichier généré. GitHub replie le diff
d'un fichier ainsi marqué. Or ici le diff EST l'artefact de revue, puisque le bénéfice du catalogue
d'outils est qu'un désarmement de `returns_untrusted` se voie dans la pull request. Les trois
catalogues portent donc leur en-tête de fichier généré et ne sont pas marqués.

## Conséquences

Les trois catalogues sont réécrits par `just regen` et par rien d'autre. Un contributeur qui
corrige une cellule à la main perd sa correction à la régénération suivante : le remède est dans
la source que le catalogue lit, le `Cargo.toml` du crate, l'outil qui déclare sa propriété, la
table déclarative de `config_catalog.rs`. `AGENTS.md` et `CONTRIBUTING.md` le disent maintenant,
et une ligne par catalogue nomme sa commande de régénération dans la table des signaux ciblés.

Un catalogue périmé fait échouer `cargo test --workspace` en nommant sa commande de régénération,
donc le CI le refuse comme il refuse un test rouge. Un fichier absent est traité comme périmé : le
remède est le même. Les trois portes vivent dans l'étape `Tests` existante, le CI ne gagne aucune
étape, et la porte de non-dérive entre le `justfile` et le workflow reste verte parce que `regen`
ne porte aucun marqueur `# ci-step:`.

Le coût est un couplage nouveau entre le code et trois documents : ajouter un crate, enregistrer
un outil ou accepter une clé impose une régénération dans le même changement. C'est le couplage
recherché, et il est visible plutôt que tacite. Le coût secondaire est la taille des diffs de
`main.rs` et des manifestes : une modification du câblage des outils change le catalogue, donc la
pull request porte les deux, ce qui est précisément ce qui rend le désarmement relisible.

L'écart entre ce qu'un document affirme et ce que le code fait n'est plus rattrapable par une
relecture attentive : il est rattrapé par la suite ou il n'existe pas. Le lot ne crée toutefois
aucune règle générale imposant qu'un futur document de structure soit dérivé ; il traite les trois
qui avaient dérivé et laisse le précédent faire le reste.
