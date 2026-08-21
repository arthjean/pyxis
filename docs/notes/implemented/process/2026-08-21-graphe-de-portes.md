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
sortant non nul avorte la recette. Les deux portes de parité sont elles aussi des recettes, `parity`
et `drift`, que `check-local` compose : `drift` y est préfixée du sigil `-` pour qu'un amont qui a
bougé s'affiche sans rendre le verdict rouge. Les nommer séparément est ce qui les rend lançables
seules, sans payer les vingt secondes de `check`, et ce qui permet à la table des signaux ciblés
d'`AGENTS.md` de citer une recette plutôt qu'une commande.
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
sous 60 secondes sur cache chaud pour continuer d'être lancé. La section « Mesures » ci-dessous
consigne le relevé de livraison et la mesure de départ qu'il confirme. Toute porte ajoutée par un
lot ultérieur annonce son coût à chaud ici, à la suite de ce même tableau.

`just` devient un prérequis local, jamais un prérequis du CI. `CONTRIBUTING.md` documente en toutes
lettres que la voie `cargo` reste complète et suffisante pour un contributeur qui ne veut pas
l'installer, et le workflow n'a gagné aucune étape, aucune action tierce et aucune dépendance.

Le précédent que ce lot pose pour les suivants est explicite : tout ce que le CI peut exécuter
entre dans `check`, tout ce qui exige un artefact local, comme le clone Codex épinglé, entre dans
`check-local`, et tout ce qui écrit dans l'arbre entre dans `regen`.

## Mesures

Relevé du 2026-08-21 à la livraison du lot, sur la machine de référence : AMD Ryzen 7 7800X3D,
16 fils, 30 Gio de mémoire, Fedora 44, noyau 7.1.8, `cargo` 1.97.1, `rustc` 1.97.1, `mold` forcé
par `.cargo/config.toml`. Les mesures ci-dessous sont des relevés `/usr/bin/time` sur cache chaud,
c'est-à-dire après un `just check` complet immédiatement précédent.

| Porte | Départ, à la rédaction du PRD | Livraison, cache chaud |
|---|---|---|
| `just fmt` | 0,69 s | 0,69 s |
| `just lint` | 0,26 s | 0,21 s |
| `just build-tests` | 0,20 s | 0,19 s |
| `just test` | 19 s | 18,54 s |
| `just check` | 20,2 s cumulées | 19,66 s, 19,69 s, 19,62 s sur trois lancements |

Le budget de 60 secondes est donc tenu avec une marge d'un facteur trois, et la mesure de départ
est confirmée plutôt que corrigée. La suite pèse 94 % du total : c'est le seul poste qu'un lot
ultérieur puisse faire dériver.

**L'arrêt anticipé est mesuré, pas supposé.** Avec une fonction délibérément mal formatée ajoutée
à `crates/agent-doc-gates/src/lib.rs`, `just check` sort en **0,69 s** avec le code 1, exactement
le coût de `fmt` seul. La preuve que rien d'autre n'a démarré ne repose pas sur cette durée : un
`cargo` factice placé en tête de `PATH`, qui journalise son argv avant de relayer au vrai binaire,
n'a enregistré qu'une seule ligne, `cargo fmt --all -- --check`. Aucun `cargo clippy`, aucun
`cargo test` n'a été lancé. Le relevé est identique sous les deux versions : même 0,69 s, même
code 1 et même unique ligne journalisée sous 1.21.0 que sous 1.58.0, l'arrêt anticipé étant donc
prouvé sur le plancher autant que sur la borne haute. Seul le libellé diffère,
``error: recipe `fmt` failed on line 26 with exit code 1`` sous 1.58.0 contre
``error: Recipe `fmt` failed on line 26 with exit code 1`` sous 1.21.0 : `just` a changé la casse
d'un mot, pas la teneur de son diagnostic. Le PRD, lui, anticipait
`error: Recipe 'fmt' failed with exit code 1`, sans le numéro de ligne que les deux versions
donnent déjà.

**Les deux versions de `just` rendent le même arbre.** `just --list` et `just --dry-run check-local`
produisent une sortie identique octet pour octet sous 1.21.0, la version empaquetée par Ubuntu
24.04 et le plancher que ce fichier vise, et sous la version installée localement. Le sigil `-` s'y
comporte pareillement : sous les deux, une recette appelée par `-just` échoue bruyamment sans
changer le verdict de celle qui l'appelle, ce qui est ce dont `check-local` dépend pour `drift`. Un écart au PRD
est à consigner : celui-ci annonçait 1.57.0 pour Fedora 44, la machine de référence porte
**1.58.0**, installée par `cargo install`. Le plancher testé reste 1.21.0, donc l'écart ne déplace
que la borne haute, sur laquelle aucune contrainte ne pèse.

**L'interruption ne laisse rien.** Un `SIGINT` envoyé au groupe de processus pendant la porte
`test` fait sortir `just` en **130** avec
``error: recipe `test` was terminated on line 41 by signal 2``. `git status --porcelain` est vide
juste après, aucun `*.rs.bk`, `*.snap.new` ni `*.orig` n'apparaît dans l'arbre, et le `just check`
suivant repasse vert sans reconstruire : `target/` survit à l'interruption.

**La concurrence est sérialisée par cargo, pas par une recette.** Deux `just check` lancés
simultanément sortent tous les deux en 0. Le second affiche
`Blocking waiting for file lock on build directory` puis
`Blocking waiting for file lock on package cache`, attend, et reprend. `git status` reste vide et
le `just check` suivant repasse vert. Aucune exclusion mutuelle n'est donc à écrire dans le
`justfile` : cargo la porte déjà, et l'ajouter masquerait le message qui explique l'attente.

**Ce qui reste hors agrégat, et pourquoi.** Les trois commandes de la recette normative de
[`docs/parity/offline-suite.md`](../../../parity/offline-suite.md) sont désormais toutes
atteignables par une recette : `cargo test --workspace --no-fail-fast` est `just test`,
`cargo run -p agent-parity -- check` est `just parity`, `-- drift` est `just drift`, et
`check-local` compose les trois. Le compte de portes documentées que rien n'exécute tombe donc à
zéro. Une seule commande de ce document reste volontairement sans recette, la recette live
`PYXIS_LIVE_PARITY=1 cargo test -p agent-cli --test live_parity_sol` : elle dépense l'abonnement du
mainteneur contre un point de terminaison OpenAI réel, et les limites d'autorisation d'`AGENTS.md`
lui réservent une décision de session explicite. Une recette la rendrait lançable par
autocomplétion, ce qui est précisément ce que cette limite interdit.

Le workflow, lui, n'a pas bougé : le diff de
[`.github/workflows/ci.yml`](../../../../.github/workflows/ci.yml) sur toute la durée du lot est
vide. Aucune étape ajoutée, aucun `timeout` déplacé, aucun `tee`, aucun filtre, aucun bloc
`GITHUB_STEP_SUMMARY` touché, et aucune installation de `just` sur le runner. L'appariement passe
par le marqueur `# ci-step:` porté côté `justfile`, donc il n'a rien demandé au workflow.

**L'état local du clone épinglé n'est pas celui du dépôt.** Au moment du relevé, `just parity` sort
non nul sur un `CommitMismatch`, le clone résolu par `$PYXIS_CODEX_BASELINE` étant à un autre
commit que `BASELINE_COMMIT`, et `just drift` rapporte 22 différences de contrat. Les deux sorties
sont conformes aux cas 3 et 5 du tableau d'erreurs du PRD : la porte nomme le commit attendu et ne
corrige rien. Elles constatent l'état d'une machine, pas un défaut du lot, et c'est bien pour cela
que `check-local` ne tourne jamais en CI.
