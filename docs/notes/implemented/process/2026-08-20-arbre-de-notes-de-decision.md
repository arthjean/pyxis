# Note: adopter un arbre de notes de décision vérifié par cargo test

Statut: implemented

## Problème

Le dépôt enregistrait ses décisions dans un fichier unique, [`docs/DECISIONS.md`](../../../DECISIONS.md),
et rien ne le lisait. Quatre défaillances étaient mesurables sur l'état du 2026-08-20, avant ce lot :

1. Le fichier déclarait treize ADR en sections et son tableau récapitulatif n'en listait que douze.
   ADR-13, le NO-GO sur les sous-agents mutateurs, était absent de son propre sommaire.
2. Le fichier annonçait en tête un format par décision, et quatre ADR sur treize n'avaient pas de
   section d'alternatives : ADR-7, ADR-9, ADR-11 et ADR-13.
3. Les décisions récentes n'entraient plus dans le registre. Quatre documents datés vivaient à la
   racine de `docs/`, dont un qui portait une décision nette,
   [ne pas partir de la base Codex CLI](../architecture/2026-07-27-ne-pas-partir-de-la-base-codex.md),
   sans qu'aucun ADR ne la mentionne.
4. Aucun enregistrement n'était vérifié mécaniquement, alors que le dépôt savait déjà faire ça
   ailleurs : `crates/agent-parity/tests/offline_suite.rs` parse un tableau Markdown et le confronte
   au dépôt.

Ce qui rend ces quatre défauts coûteux ici plutôt qu'ennuyeux ailleurs, c'est que Pyxis est édité
principalement par des agents dont le contexte est effacé entre les sessions. Une règle sans
justification accessible se lit comme arbitraire, donc se contourne ou se re-litige, et un fichier
de 50 Ko ne s'ouvre pas pour vérifier un point.

## Décision

Les décisions que le registre ADR n'accueille pas vivent dans `docs/notes/`, un arbre dont le
chemin encode deux axes, `{cycle de vie}/{classe}/aaaa-mm-jj-sujet.md`. Le cycle de vie est le
répertoire : changer de statut, c'est déplacer le fichier, et un statut divergent de son
emplacement devient impossible puisque la porte croise les deux. Le format complet est dans
[`docs/notes/README.md`](../../README.md), et il n'est pas une convention : c'est une porte.
[`crates/agent-doc-gates`](../../../../crates/agent-doc-gates/src/lib.rs) porte un walker partagé
qui rend `(notes, erreurs)`, la vérification du format, celle des liens Markdown relatifs et celle
de la cohérence du registre ADR. Une décision mal enregistrée fait échouer
`cargo test --workspace`, comme un test qui casse.

**`docs/DECISIONS.md` est conservé, et l'arbre le borde au lieu de le remplacer.** La mesure qui
tranche est le nombre de renvois : `git grep -oE 'ADR-[0-9]+'` rend aujourd'hui 211 occurrences
dans 39 fichiers, dont 18 fichiers Rust, contre 167 dans 35 fichiers quand la question a été posée.
Un identifiant cité depuis le code compilé est un point d'ancrage stable, et le dissoudre en
fichiers casserait 211 renvois sans rien acheter. La frontière est un test et non un goût : une
décision est un ADR quand un changement futur des crates livrées peut la violer, une note quand
rien dans `crates/` ne le peut. Elle est écrite des deux côtés,
dans [`AGENTS.md`](../../../../AGENTS.md) et dans le README de l'arbre, et l'ADR l'emporte en cas
de désaccord.

**La porte s'exécute par `cargo test --workspace`, jamais par un hook.** Aucune infrastructure de
hook n'existe dans ce dépôt : ni `package.json`, ni `scripts/`, ni `.git/hooks` peuplé, ni
`core.hooksPath`. Et la référence dont ce format vient ne l'exécute pas non plus en pre-commit :
dans DeepSeek Harness, `scripts/run-gates.ts` l. 670 à 672 range les portes de note dans l'agrégat
`doc-sync`, tandis que le pre-commit de `lefthook.yml` n'exécute que l'appariement des traductions,
les notes archivées, oxlint, les notices tierces, le contrôle d'espaces et le manifeste de vendor :
aucune porte de format de note. Le signal « pre-commit » du plan de portage est donc
remplacé en connaissance de cause par la commande de test que la CI lance déjà, et
`cargo test -p agent-doc-gates` est la commande ciblée, mesurée à moins de deux secondes sur un
cache chaud.

## Alternatives écartées

**Maintenir le fichier unique et la discipline.** C'est l'état de départ, et son verdict est
mesuré, pas supposé : le fichier a perdu ADR-13 de son propre index et laissé quatre décisions sans
alternatives, sans que personne ne le voie pendant des mois. Rien dans cette option ne change ce qui
a produit la dérive, à savoir qu'aucune machine ne lit le document. Ajouter des ADR de processus
aurait en plus dilué un registre dont la valeur tient à ce qu'il ne contient que des invariants
opposables au code.

**MADR 4.0.0, avec le statut en front matter YAML.** C'est le format le plus établi et le mieux
outillé, et c'est le seul concurrent sérieux. Écarté sur un point précis : le statut y est une clé
du fichier, donc rien n'empêche un document classé `accepted` de contredire l'endroit où il vit, et
c'est exactement la classe de dérive que ce lot existe pour supprimer. Ses sous-répertoires
organisent par structure architecturale, pas par cycle de vie, ce qui laisse la question du statut
entièrement à la discipline. Son front matter YAML aurait par ailleurs demandé un analyseur ou une
dépendance, que ce lot s'interdit.

**adr-tools et le format Nygard, fichiers séquentiels `NNNN-titre.md` avec index généré.** L'index
généré est la bonne réponse à la dérive d'index, et elle est reprise ici sous une autre forme :
aucun index du tout. Écarté parce que le format n'exige pas les alternatives rejetées, ce qui est
le reproche documenté numéro un qui lui est adressé, et parce que la numérotation séquentielle
force une coordination sur un compteur global pour un gain nul quand le nom porte déjà une date.

**Le schéma retenu, cycle de vie encodé par le répertoire.** Il n'a pas de précédent hors de
DeepSeek Harness, et il est retenu en connaissance de cause pour la seule propriété qu'aucun autre
format n'offre : un statut divergent de son emplacement est mécaniquement impossible, pas
seulement découragé. Ce qu'il coûte est réel et il est payé dans le même lot. Chaque transition
casse les liens relatifs qui pointaient vers l'ancien chemin, donc la porte de liens a été livrée
avant la première migration ; elle a immédiatement signalé un renvoi cassé que la migration des
audits venait de produire, ce qu'aucun relecteur n'aurait vu. `git log --follow` devient par
ailleurs heuristique sur ces fichiers, et le déplacement se fait en conséquence de façon à
préserver le suivi de renommage.

**Une porte en hook pre-commit, comme le proposait le plan de portage.** Écartée pour la raison
donnée plus haut : le dépôt n'a aucune infrastructure de hook, et la référence elle-même range ses
portes de note dans un agrégat plutôt que dans son pre-commit. Un hook aurait ajouté une
infrastructure entière pour un signal que `cargo test --workspace` porte déjà, et qui serait resté
contournable par `--no-verify`.

**Un simple fichier de test dans `agent-parity` plutôt qu'un crate dédié.** Moins coûteux d'un
`Cargo.toml`, mais en désaccord avec le rôle déclaré de ce crate, qui est la vérification de la
baseline Codex. Un crate séparé sans aucune dépendance rend en plus vérifiable la propriété qui
compte, à savoir que ces portes n'entrent dans le graphe d'aucun binaire livré.

**Une règle « toute PR non triviale contient une note ».** DeepSeek Harness l'impose et compte 726
notes anglaises. Écartée : Pyxis a un mainteneur unique, et la contrainte utile est « toute décision
qu'on pourrait vouloir rejouer ». La porte ne vérifie que ce qui est écrit, elle n'exige pas
d'écrire.

## Conséquences

Consigner une décision coûte un fichier court dans un chemin évident, et oublier une section fait
échouer `cargo test`. C'est le pari de ce lot, et son risque : si la pratique s'effondre après un
mois, aucune porte ne le signalera, puisque aucune n'exige d'écrire. Le succès se mesure au nombre
de notes que produiront les lots suivants du plan de portage, pas à un quota.

Le dispositif se garde lui-même. Cette note déplacée un jour vers `rejected/` sans que sa ligne de
statut suive fait échouer la suite, et la porte nomme le désaccord entre le répertoire et le
statut. C'est la même mécanique qui interdit une migration purement mécanique, et c'est ce qui
distingue cet arbre d'une convention écrite.

L'arbre est son propre inventaire : aucun index n'est produit ni autorisé, on parcourt les
répertoires ou on cherche par `grep`. Un lecteur qui veut savoir pourquoi l'arbre existe trouve
cette note par son nom sous `docs/notes/implemented/process/`, à côté de celle qui explique
[pourquoi les audits de parité n'y sont pas](2026-08-20-audits-de-parite-hors-de-l-arbre.md).

Le registre ADR sort réparé du même lot : son tableau récapitulatif liste désormais ses treize
sections et une porte le vérifie, et les quatre ADR qui ne disaient pas ce qu'ils avaient battu le
disent ou déclarent explicitement que leurs alternatives ne sont pas reconstructibles. Aucun
document de décision daté ne subsiste à la racine de `docs/`.
