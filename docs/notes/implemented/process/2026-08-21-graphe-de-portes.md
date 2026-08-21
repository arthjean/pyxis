# Note: le CI n'appelle pas `just`, une porte prouve qu'il exécute la même chose

Statut: implemented

## Problème

Le dépôt portait six vérifications mécaniques et aucune commande qui les nommait. L'inventaire
vivait dans [`.github/workflows/ci.yml`](../../../../.github/workflows/ci.yml), en YAML, et rien
ne le reproduisait ailleurs. Quatre défaillances étaient mesurables sur l'état du 2026-08-21,
avant ce lot :

1. Deux portes documentées n'étaient exécutées par rien. `cargo run -p agent-parity -- check` et
   `-- drift` étaient prescrites par [`AGENTS.md`](../../../../AGENTS.md) et publiées comme recette
   normative dans [`docs/parity/offline-suite.md`](../../../parity/offline-suite.md) ; ni le CI ni
   `cargo test --workspace` ne les lançait. La première exige le clone Codex épinglé, donc est
   structurellement inexécutable sur un runner GitHub ; la seconde sort non nul par conception
   quand l'amont a bougé, donc ne peut jamais être une étape bloquante. Les deux étaient restées
   orphelines faute d'un endroit où les déclarer.
2. La même porte s'écrivait de trois façons. `AGENTS.md` et le workflow disaient
   `cargo clippy --workspace --all-targets`, [`CONTRIBUTING.md`](../../../../CONTRIBUTING.md)
   disait `cargo clippy --workspace --no-deps`. Un contributeur qui suivait le second ne lançait
   pas la porte du premier : `--no-deps` ne compile pas les cibles de test, donc un lint dans un
   `#[cfg(test)]` passait en local et cassait en CI, sur une commande qu'il venait de voir verte.
3. Rien ne reproduisait le CI en local : ni `justfile`, ni `Makefile`, ni `scripts/`, ni hook. Un
   contributeur enchaînait quatre commandes à la main, dans le bon ordre, ou n'en lançait aucune.
4. Trois régénérations qui mutent l'arbre de travail vivaient dans deux documents différents, sans
   distinction typographique d'avec les commandes qui ne font que constater, alors que la
   différence est catégorique.

Ce qui rend ces défauts coûteux ici plutôt qu'ennuyeux ailleurs est la même chose qui a justifié
l'arbre de notes : le dépôt est édité principalement par des agents dont le contexte est effacé
entre les sessions. Un agent lit `AGENTS.md`, exécute la plus étroite vérification qu'il croit
suffisante et ne rouvre pas le YAML avant de livrer.

## Décision

Les portes du dépôt sont des recettes d'un [`justfile`](../../../../justfile) racine. Quatre
recettes feuilles portent les quatre commandes `cargo` du CI, dans le même ordre ; `check` les
compose et s'arrête à la première qui échoue, ce que `just` fait nativement puisqu'une ligne
sortant non nul avorte la recette. `check-local` ajoute les deux portes de parité, `drift`
préfixée du sigil `-` pour qu'un amont qui a bougé s'affiche sans rendre le verdict rouge.
`regen`, qui écrit dans le dépôt, n'est la dépendance d'aucune recette de vérification. La recette
par défaut est `just --list` : la commande sans argument catalogue les portes au lieu d'en
exécuter une.

**Le CI, lui, n'appelle pas `just`.** C'est la décision structurante du lot et elle va contre la
lecture naïve du plan de portage dont il vient. Le workflow conserve ses étapes verbatim, avec
leurs `timeout` par étape, le `tee` vers `cargo-test.log`, le filtre de flux et le résumé
`GITHUB_STEP_SUMMARY`. Ces propriétés ont un motif écrit dans le fichier lui-même : un job annulé
par `timeout-minutes` n'archive aucun log, donc les étapes doivent échouer d'elles-mêmes et
lisiblement. Envelopper cela dans une recette détruirait ces diagnostics, et les remplacer étape
par étape ajouterait une installation de `just` sur le runner et un écart de version pour zéro
gain, la recette n'ajoutant rien à une invocation `cargo` d'une ligne.

**Le risque devient la dérive entre les deux fichiers, et c'est ce risque qui est traité
mécaniquement.** [`crates/agent-doc-gates/src/gates.rs`](../../../../crates/agent-doc-gates/src/gates.rs)
extrait la liste ordonnée des invocations `cargo` de chaque côté, apparie une recette et une étape
par un marqueur `# ci-step:` porté en commentaire, retire un préfixe `timeout` et ses options, et
échoue bruyamment sur tout autre préfixe. Un test d'intégration compare cardinalité, argv et
ordre, et rapporte toutes les divergences d'un coup. Le même module tient `AGENTS.md` et
`CONTRIBUTING.md` aux noms de recettes : une invocation qui partage la tête d'une porte et en
diverge y est refusée, avec un message qui dit quelle recette écrire. La portée de cette
seconde porte est close à ces deux fichiers ; `docs/parity/offline-suite.md` publie une recette
normative destinée à être recopiée et `README.md` montre des transcriptions de session, deux
endroits où écrire une invocation est précisément le propos.

Le critère de succès « `just check` reproduit le CI » cesse ainsi d'être une intention et devient
une assertion qui échoue rouge dans `cargo test --workspace`, sans lancer aucun processus, sans
lire aucune variable d'environnement et sans `just` installé.

**Pourquoi une note et non un ADR.** La frontière écrite dans [`docs/notes/README.md`](../../README.md)
est un test : une décision entre dans le registre quand un changement futur des crates livrées
peut la violer. Aucun changement dans `crates/` ne peut violer une liste de recettes : le
`justfile` n'entre dans le graphe d'aucun binaire, et la seule chose qui puisse contredire cette
décision est la porte de non-dérive elle-même, qui est sa mise en œuvre et non son arbitre. Le
choix de ne pas faire dépendre le CI de `just` se re-litigerait pourtant volontiers, un
relecteur pressé y voyant une simplification évidente : c'est exactement ce qu'une note existe
pour empêcher.

## Alternatives écartées

**Une étape unique `run: just check` dans le workflow.** C'est la convention de DeepSeek Harness,
dont ce lot vient : son YAML appelle des agrégats nommés et ne contient aucun nom de porte. Elle
supprimerait la duplication d'inventaire, donc rendrait la porte de non-dérive sans objet.
Écartée sur ce que le workflow perdrait : un seul verdict pour quatre portes, plus de `timeout`
par étape, plus de `tee` vers un log archivé, plus de résumé listant les tests en échec. La
mitigation écrite dans `ci.yml` contre le cas « job annulé, log perdu » disparaîtrait avec elle.

**Des étapes `run: just <recette>`, une par porte.** Elle garde la granularité par étape mais
ajoute une installation de `just` sur le runner, donc soit `apt-get install just` (1.21.0 sur
Ubuntu 24.04), soit un snap, soit une action tierce, alors que le workflow prend une position
explicite contre les actions tierces à tag mutable. Le gain serait nul : la recette n'ajoute rien
à une invocation `cargo` d'une ligne, et les corps de shell des deux étapes coûteuses ne se
transposent pas en recettes sans se réécrire.

**Un `Makefile`.** Disponible partout, aucun prérequis à installer. Écarté sur ses pièges propres,
tous connus et tous coûteux pour un fichier lu par des agents : cibles fantômes à déclarer en
`.PHONY`, une ligne par processus shell, tabulations significatives, et un `--list` qui n'existe
pas. `just` a été conçu en retirant ces pièges à `make`, et le catalogue des portes est
précisément ce que ce lot livre.

**`cargo-make`.** Rust-natif, dépendances de tâches déclarées en TOML, connaissance de Cargo.
Écarté : il s'installe par `cargo install cargo-make`, donc se compile, ce qui est plus cher à
installer que la totalité de ce qu'il lance ici, et `Makefile.toml` est verbeux pour quatre
commandes.

**Un alias `[alias]` dans `.cargo/config.toml`, avec ou sans crate `xtask`.** La voie idiomatique
Rust, et la seule sans prérequis. Écartée deux fois : un alias Cargo n'enchaîne pas plusieurs
commandes, donc la séquence retomberait dans un crate `xtask`, soit du code Rust à compiler et à
maintenir là où quatre lignes de recettes suffisent, et l'inventaire cesserait d'être lisible sans
ouvrir une fonction `main`. `.cargo/config.toml` porte par ailleurs les rustflags `mold` et n'a
aucune raison de porter en plus l'inventaire des portes.

**Une porte en hook `pre-commit` ou `pre-push`.** Écartée sans être rouverte : la note
[adopter un arbre de notes de décision vérifié par cargo test](2026-08-20-arbre-de-notes-de-decision.md)
a déjà tranché ce point au lot précédent, un hook restant contournable par `--no-verify` pour un
signal que `cargo test --workspace` porte déjà. La référence dont ce lot vient a elle-même
dégonflé ses hooks depuis, son `pre-push` ne lançant plus qu'un typecheck et aucune porte du
graphe.

**Transposer l'ordonnanceur de `run-gates.ts`.** La source du lot est un ordonnanceur de 967
lignes : graphe validé avant tout démarrage (graphe vide, identifiant dupliqué, dépendance
inconnue, cycle), concurrence bornée, sortie attribuable par porte. Écarté en bloc : la suite
complète mesure une vingtaine de secondes sur cache chaud, il n'y a rien à paralléliser, et une
liste de quatre commandes n'a ni identifiant dupliqué ni cycle possible. Deux idées seulement en
sont reprises, et aucune ne demande de code : une porte qui doit réussir avant la suivante, une
porte qui doit seulement avoir retombé. `just` porte les deux nativement.

**Comparer les corps de shell plutôt que les argv.** Ce serait la comparaison la plus forte, et
elle est hors d'atteinte : l'étape `Tests` porte une trentaine de lignes de journalisation
qu'aucune recette ne reproduira jamais, puisque c'est précisément pour les garder que le CI
n'appelle pas `just`. La conséquence est assumée et écrite : la porte prouve l'identité des
commandes, pas celle des diagnostics.

**Aucune porte, la discipline.** C'est l'état de départ, et son verdict est mesuré et non supposé :
la divergence `--no-deps` contre `--all-targets` a vécu dans deux documents prescriptifs sans que
personne ne la voie, et les deux portes de parité sont restées orphelines assez longtemps pour
être documentées comme telles. Rien dans cette option ne change ce qui a produit la dérive, à
savoir qu'aucune machine ne lit ces fichiers.

## Conséquences

Ajouter une porte au CI coûte désormais une recette et un marqueur, sinon `cargo test --workspace`
échoue en nommant l'étape et la commande à ajouter. C'est le prix assumé de la duplication
d'inventaire, et il est payé à chaque édition de l'un des deux fichiers plutôt qu'une fois, en
rouge, sur la pull request d'un tiers.

Le budget de temps est une contrainte tenue à la main, pas par une porte : `just check` doit rester
sous 60 secondes sur cache chaud pour continuer d'être lancé. La mesure de départ, relevée à la
rédaction du PRD, était de 20,2 secondes cumulées (fmt 0,69 s, clippy 0,26 s, compilation des
tests 0,20 s, suite 19 s). Toute porte ajoutée par un lot ultérieur annonce son coût à chaud ici.

`just` devient un prérequis local, jamais un prérequis du CI. `CONTRIBUTING.md` documente en toutes
lettres que la voie `cargo` reste complète et suffisante pour un contributeur qui ne veut pas
l'installer, et le workflow n'a gagné aucune étape, aucune action tierce et aucune dépendance.

Le précédent que ce lot pose pour les suivants est explicite : tout ce que le CI peut exécuter
entre dans `check`, tout ce qui exige un artefact local, comme le clone Codex épinglé, entre dans
`check-local`, et tout ce qui écrit dans l'arbre entre dans `regen`.
