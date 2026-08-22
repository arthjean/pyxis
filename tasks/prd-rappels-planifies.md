[PRD]
# PRD: Rappels planifiés

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-22 | Arthur Jean | Rédaction initiale, lot #10 du plan de portage DeepSeek Harness |

## Problem Statement

Pyxis sait attendre et ne sait pas revenir. Cinq défaillances mesurées sur l'état de l'arbre au 2026-08-22 :

1. **La seule attente que le dépôt offre au modèle bloque le tour qui l'a demandée.** L'outil `Sleep` (`crates/agent-tools/src/time.rs:36`) borne sa pause à `MAX_SLEEP = 12h`, et le commentaire qui fixe cette borne dit ce qu'elle protège : une pause est du temps de tour dépensé. Un modèle qui doit revenir dans six heures n'a donc que deux options, tenir un tour ouvert six heures ou ne pas revenir. Aucun mécanisme du dépôt ne représente « plus tard, dans un autre tour ».

2. **Rien n'est planifiable, donc rien de planifié n'est durable.** `grep -rn "schedule" crates/` ne rend aucun module, aucun type, aucune variante d'événement. `ThreadEventPayload` (`crates/agent-runtime/src/event.rs:95`) porte douze variantes couvrant les tours, les agents et les travaux de fond, et aucune qui exprime une échéance. Un rappel n'est pas seulement absent de l'interface : il est absent du format durable, donc rien ne peut le rendre durable après coup sans toucher au journal.

3. **L'acteur de fil n'a aucun réveil sur horloge murale.** Le `select!` de `ThreadActor` (`crates/agent-runtime/src/thread.rs:968-995`) a quatre bras : la fin d'un tour, l'annulation, l'échéance de traînard et la boîte aux lettres. Le troisième est le seul temporel, et il est monotone : `straggler_deadline: Option<Instant>` (`:827`) mesure une patience de 2 s après une annulation, pas une date. Le `Clock` injecté (`crates/agent-core/src/clock.rs:7-12`) porte `now_ms` et `sleep(Duration)`, et aucune forme d'échéance. Un fil oisif n'a donc littéralement aucune raison de se réveiller : il attend une commande, et rien d'autre.

4. **Le lot 9 a livré la moitié aval du mécanisme et personne ne peut s'en servir.** `WakeBudget` (`crates/agent-runtime/src/jobs.rs:96-160`), `CompletionDelivery`, `InputOrigin { Human, Runtime }` et la clé d'idempotence `job-completion:{job_id}` existent et fonctionnent, mais leur unique producteur est la complétion d'un travail de fond. Le chemin « quelque chose du runtime ouvre un tour, sous budget, sans qu'un humain l'ait demandé à cet instant » est construit, testé et mono-client. Le coût marginal d'un second client est faible, et il ne baissera pas en attendant.

5. **La cible que le plan désigne est décrite de travers, et la corriger change où le travail se fait.** La ligne 26 du plan dit que « `crates/agent-runtime/src/inputs.rs` porte déjà une file d'entrées en attente bornée à 16 ». Le fichier fait 135 lignes et ne contient aucune constante : `TurnInputs` est une `Mutex<VecDeque<String>>` non bornée. La borne existe, elle vaut bien 16 (`MAX_PENDING_INPUTS`, `crates/agent-runtime/src/thread.rs:50`), et elle est appliquée par l'appelant, deux fois, à `thread.rs:1142` pour les tours en file et `:1194` pour les entrées de pilotage du tour courant. La conclusion du plan tient donc, « un rappel arrivé est une entrée comme une autre », mais le fichier à ouvrir n'est pas celui qu'il nomme. Un implémenteur qui suivrait la colonne irait chercher une borne dans un fichier qui n'en a pas.

**Why now:** le lot 9 est certifié et il a payé le prix d'entrée. Le budget de réveils, l'origine d'entrée portée en donnée, la livraison sous fil oisif contre pilotage d'un tour en cours, la clé d'idempotence et la réconciliation de reprise sont livrés et couverts. Ce lot ajoute un producteur à un chemin existant plutôt qu'un second sous-système. Attendre a un coût précis : `ThreadEventPayload` est additif, donc chaque lot qui passe ajoute des journaux écrits sans variante de planification, et l'état de planification se reconstruit par pli sur l'historique complet du fil. Plus le format vieillit sans ces variantes, plus la première session qui en gagne une est ancienne, et plus longtemps il faudra lire des journaux qui n'en portent pas.

## Overview

**Racines et convention de chemins.** Tout chemin en `crates/`, `docs/` ou `tasks/` est relatif à `/home/arthur/dev/pyxis`. Tout chemin en `packages/` est relatif à `/home/arthur/dev/deepseek-harness`, cité en **lecture seule** : dsh est en TypeScript sur Cordis, Pyxis est en Rust, et rien ne s'y copie. Ce qui se reprend sont les décisions de conception, chacune ancrée sur la ligne qui la porte. Le champ `**Source dsh:**` de chaque story nomme la ligne à ouvrir avant de l'implémenter, et dit « aucune » quand la story n'a pas de source, ce qui est en soi une information.

Le lot fait entrer dans `agent-runtime` un domaine de planification pur et un bras d'horloge dans l'acteur de fil. Un rappel est un enregistrement durable, reconstruit par pli sur le journal du fil, jamais un état stocké. Quand son échéance passe, l'acteur soumet le texte du rappel comme une entrée ordinaire d'origine `Runtime`, sous le budget de réveils que le lot 9 a posé. Il n'existe ni file de rattrapage, ni processus, ni minuterie durable : la seule chose qui survit à un redémarrage est l'enregistrement, et l'acteur qui rouvre recalcule l'échéance depuis la date.

Sept décisions de conception se reprennent de DeepSeek Harness. La première est la séparation d'un domaine pur et d'un runtime effectif : `packages/schedule/schedule/src/domain.ts` fait 807 lignes sans jamais lire l'heure, `runtime.ts` en fait 324 et porte tous les effets. La deuxième est le pli événementiel : l'état de planification n'est jamais persisté, il est **recalculé** depuis les événements `schedule/change` du journal (`domain.ts`, `foldScheduleEvents`), ce qui rend une reprise identique à un démarrage. La troisième est la récurrence en dernière occurrence seulement : `resolveEveryOccurrence` avance de `Math.floor((acceptedAt - target) / interval)` pas d'un coup au lieu d'énumérer un arriéré, et c'est ce qui empêche un rappel de cinq minutes de délivrer une journée de retard en rafale. La quatrième est le vocabulaire d'erreurs fermé, rendu comme **valeur de retour** et non comme exception (`tools.ts`, schémas `oneOf`), avec un `internal_error` qui ne divulgue jamais l'exception. La cinquième est le cadre anti-injection : un en-tête `[SCHEDULE REMINDER]`, les champs dynamiques échappés par `JSON.stringify`, et une ligne disant explicitement que le contenu est un rappel non fiable et non une nouvelle instruction de l'utilisateur. La sixième est la borne de fréquence, `MIN_EVERY_INTERVAL_SECONDS = 300`. La septième est la frontière de livraison fixée à une valeur unique et documentée comme telle : `deliveryMode: 'session-local'`, « the original session must be live ».

Quatre divergences volontaires viennent des invariants de Pyxis, et chacune est un renversement argumenté, pas un oubli. L'ordre est inversé : dsh appelle `agent.followup(message)` à `runtime.ts:275` **avant** `session.append` à `:284`, donc au moins une fois ; l'invariant 12 impose l'inverse, et `Submission.client_message_id` donne l'idempotence qui transforme cela en exactement une fois, ce que dsh ne peut pas faire. La validation avant écriture de dsh (`invariant.ts`, qui valide le flux candidat entier sur `internal/dispatch`) n'entre pas : Pyxis n'a aucun crochet avant écriture, en ajouter un serait un mécanisme général pour un cas unique, et le pli refuse en lecture ce que dsh refuse en écriture. Le budget de réveils, qui n'existe pas chez dsh, gouverne le dispatch, et un refus **conserve** le rappel en `overdue` au lieu de le perdre. Enfin la constante de segment de minuterie change de valeur et de justification : les 2 147 483 647 ms de dsh sont le plafond `i32` de `setTimeout`, un artefact du langage hôte, là où Pyxis choisit une minute pour ce que cela achète réellement, borner le retard après une veille de machine que `CLOCK_MONOTONIC` ne compte pas.

Le périmètre de production est étroit et nommé. Un module `crates/agent-runtime/src/schedule.rs` portant le vocabulaire fermé, le pli, la décision d'échéance et le calcul de récurrence, tous purs ; trois variantes additives de `ThreadEventPayload` ; un cinquième bras dans le `select!` de l'acteur ; trois outils en lecture et écriture d'enregistrements ; un ADR. Le reste est du test, du document et une régénération de catalogues.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Rappels créés avant un arrêt et délivrés après un redémarrage | 100 % | 100 % |
| Rappels délivrés deux fois | 0 | 0 |
| Rappels perdus par un budget de réveils épuisé | 0 | 0 |
| Occurrences délivrées en rafale pour un rappel récurrent en retard | ≤ 1 | ≤ 1 |
| Tours ouverts sans entrée humaine intercalée | ≤ 3 | ≤ 3 |
| Fils rendus inouvrables par un enregistrement de planification illisible | 0 | 0 |
| Clés de configuration utilisateur ajoutées | 0 | 0 |
| Bumps de `SESSION_SCHEMA_VERSION` | 0 | 0 |
| Dépendances ajoutées au workspace, hors US-162 | 0 | 0 |
| Outils enregistrés ajoutés | 3 | 3 |
| Tours ouverts par `resume.rs` lui-même | 0 | 0 |

## Target Users

### L'humain qui délègue une échéance

- **Role:** Arthur, en session interactive, qui veut être ramené sur un sujet à une date plutôt que s'en souvenir.
- **Behaviors:** demande « relance-moi dans deux heures sur la revue du lot 11 », ferme le terminal, revient plus tard, parfois après un redémarrage du binaire.
- **Pain points:** rien ne le ramène. `Sleep` tiendrait un tour ouvert deux heures, ce que la borne de 12 h autorise techniquement et qu'aucun usage ne justifie. La seule alternative hors du dépôt est un `at` ou un `systemd-run` qui n'a aucun accès au fil et ne peut donc que lui envoyer une notification, pas rouvrir la conversation avec son contexte.
- **Current workaround:** un rappel dans un autre outil, et le coût de reconstruire le contexte à la main.
- **Success looks like:** le rappel arrive dans le fil où il a été créé, comme un tour ordinaire, avec le contexte déjà en place, que le processus ait été arrêté entre-temps ou non.

### Le modèle qui doit revenir sur son propre travail

- **Role:** l'agent qui lance une migration longue, poste une revue, ou attend un effet dont il sait la date.
- **Behaviors:** appellerait `schedule_create` avec `after: 3600` puis finirait son tour au lieu de dormir dedans.
- **Pain points:** il n'a que `Sleep`, qui consomme le tour, et `exec_command`, qui lui donne un processus dont il doit se souvenir de l'identifiant. Aucun des deux ne représente une échéance.
- **Current workaround:** dormir, ou abandonner. Le lot 9 a déjà consigné ce second comportement, l'abandon, pour les travaux de fond.
- **Success looks like:** trois appels, `schedule_create`, `schedule_list`, `schedule_delete`, un vocabulaire d'erreurs nommé qui lui dit exactement ce qu'il a mal fait, et un rappel qui revient sans qu'il ait à tenir quoi que ce soit en mémoire.

### L'agent de codage qui ajoute un producteur de réveils

- **Role:** Claude Code ou Codex recevant une tâche qui doit ouvrir un tour non demandé.
- **Behaviors:** lit `AGENTS.md`, cherche où le budget de réveils se dépense, veut savoir si son cas le partage ou en ouvre un second.
- **Pain points:** après le lot 9 il n'y avait qu'un producteur, donc la question ne se posait pas et rien ne l'arbitrait. Un second producteur qui prendrait son propre budget rendrait la garantie « au plus trois tours sans entrée humaine » fausse sans qu'aucun test ne casse.
- **Current workaround:** aucun ; il devine.
- **Success looks like:** une règle écrite disant que le budget est unique et partagé, et un test qui échoue si un producteur l'ignore.

## Research Findings

### Contexte concurrentiel

Aucun agent de terminal grand public n'expose de rappel planifié durable au modèle. Claude Code a des tâches de fond, des hooks et une commande `/rewind`, et rien de temporel : la seule primitive proche est le lancement d'une commande shell de fond, sans échéance. Codex CLI route par `unified_exec` et expose `/ps` et `/stop` ; sa ligne de base ne porte aucun type de planification. Gemini CLI n'a ni registre de fond ni ordonnanceur. La comparaison utile est donc négative : ce lot n'a pas de concurrent à égaler, il a une conception unique à porter.

Le voisinage réel est ailleurs. Les ordonnanceurs système, `at`, `systemd-run --on-calendar`, `cron`, résolvent la partie horloge et ratent la partie qui compte : ils n'ont aucun accès au fil, donc ils peuvent lancer un processus et pas rouvrir une conversation dans son contexte. C'est exactement la raison pour laquelle dsh fixe `deliveryMode: 'session-local'` : la valeur du rappel n'est pas la notification, c'est le contexte qui l'entoure.

La borne de fréquence a un précédent net dans ce voisinage : `cron` ne descend pas sous la minute et `systemd` impose `AccuracySec`. dsh choisit cinq minutes, ce qui est plus strict que les deux et se justifie autrement : chez lui comme ici, un dispatch coûte une requête modèle complète, historique compris, et non un `fork`.

### Bonnes pratiques reprises

Un domaine pur sans horloge, testable sans tokio et sans temps réel. Un état recalculé par pli plutôt que stocké, ce qui rend une reprise identique à un démarrage et supprime toute classe de bug de désynchronisation. Une récurrence qui saute au dernier créneau au lieu d'énumérer un arriéré. Des erreurs nommées rendues comme valeurs de retour, pour que le modèle puisse corriger son appel au lieu de recevoir une exception opaque. Un `internal_error` qui ne divulgue jamais l'exception sous-jacente. Un cadre de rappel qui déclare son contenu non fiable, avec les champs dynamiques échappés. Une borne minimale de fréquence. Une frontière de livraison unique, documentée comme telle plutôt que laissée implicite.

### Correspondance dsh vers Pyxis

Racine du dépôt source : `/home/arthur/dev/deepseek-harness`. Chemins relatifs à `packages/schedule/schedule/src/`.

Le paquet source tient en huit fichiers et 2 003 lignes. L'ordre de lecture ci-dessous n'est pas
alphabétique : il va de la décision vers l'effet, et un implémenteur qui le suit rencontre le
vocabulaire avant le mécanisme qui s'en sert.

| # | Fichier | Lignes | Ce qu'il décide | Repris par |
|---|---------|--------|-----------------|------------|
| 1 | `types.ts` | 221 | le vocabulaire fermé : trois enregistrements, deux états, un mode de livraison, dix codes d'erreur | US-148, US-152, US-160, US-162 |
| 2 | `domain.ts` | 807 | tout le calcul, sans jamais lire l'heure : pli, décodage, récurrence, cadre de rappel | US-149, US-151, US-161, US-162 |
| 3 | `runtime.ts` | 324 | tous les effets : décision d'échéance, segment de minuterie, ordre livraison puis persistance, latch de défaillance | US-150, US-155, US-156 |
| 4 | `tools.ts` | 467 | la surface offerte au modèle : trois outils, schémas de sortie en `oneOf`, erreurs comme valeurs | US-158, US-159, US-160 |
| 5 | `index.ts` | 77 | le câblage : quand le runtime démarre, et à quelle condition il se réarme sur l'oisiveté | US-153 |
| 6 | `invariant.ts` | 53 | la validation avant écriture, approche **écartée** par ce lot | US-149, en contrepoint |
| 7 | `transaction.ts` | 23 | la sérialisation des opérations, **sans objet** ici : `ThreadActor` est déjà écrivain unique | aucune |
| 8 | `persistence.ts` | 31 | le vidage explicite du magasin, **sans objet** ici : `JsonlThreadStore` fait `sync_data` par ligne | aucune |

Les lignes 6 à 8 sont citées pour que leur absence de reprise soit une décision lisible et non un
oubli : les trois écarts qu'elles portent sont argumentés aux lignes 16 à 19 de la table suivante.

| # | Décision | Source dsh | Reprise dans Pyxis | Écart |
|---|----------|-----------|--------------------|-------|
| 1 | Trois formes d'enregistrement, `after`, `at`, `every` | `types.ts:13-51` | trois variantes de `ScheduleRule` | `at` zoné arrive en P1 (US-162) ; `after` et `every` sont de l'arithmétique epoch |
| 2 | État fermé à deux valeurs, `scheduled` et `overdue` | `types.ts:108` | `ScheduleState` en enum Rust, exhaustif au `match` | aucun ; le compilateur rend la fermeture vérifiable |
| 3 | Domaine pur contre runtime effectif | `domain.ts` (807 l.) contre `runtime.ts` (324 l.) | `schedule.rs` pur, câblage dans `thread.rs` | aucun ; c'est le patron de `jobs.rs` contre `thread.rs` du lot 9 |
| 4 | État recalculé par pli, jamais stocké | `domain.ts:575`, `foldScheduleEvents` | pli sur `ThreadEvent` à l'ouverture du fil | aucun ; `resume.rs` plie déjà les douze variantes existantes |
| 5 | Version du format de changement | `SCHEDULE_CHANGE_VERSION = 1` (`domain.ts:21`) | `THREAD_RUNTIME_VERSION = 1` déjà en place | dsh versionne son sous-format ; Pyxis a déjà une version de runtime pour l'ensemble |
| 6 | Dernière occurrence seulement | `domain.ts:519`, `resolveEveryOccurrence` | même arithmétique en `u64` millisecondes epoch | aucun ; c'est la décision la plus importante du lot |
| 7 | Le dispatch d'un récurrent porte `acceptedAt`, un ponctuel non | `types.ts:93`, `EveryScheduleDispatchChange` | même asymétrie dans la variante durable | aucun ; sans ce champ le pli ne peut pas recalculer le créneau |
| 8 | Erreurs nommées rendues comme valeurs | `tools.ts:117`, schémas `oneOf` | enum d'erreurs rendue en texte stable | Pyxis n'a pas de schéma de sortie d'outil ; `list_jobs` fixe le précédent du texte |
| 9 | `internal_error` ne divulgue pas l'exception | `tools.ts:177`, `internalError()` | même message fixe, la cause part en `tracing` | aucun |
| 10 | Suppression d'un inconnu : un succès, pas une erreur | `types.ts:206-208`, `ScheduleDeleteResult` | `deleted: false` avec `schedule_not_found` | aucun ; c'est ce qui rend un réessai sûr |
| 11 | Cadre anti-injection avec échappement | `domain.ts:779`, `renderReminderFraming` | même structure d'en-tête, échappement JSON | aucun ; entre dans `## Model Experience` |
| 12 | Borne minimale de fréquence | `MIN_EVERY_INTERVAL_SECONDS = 300` (`domain.ts:24`) | constante de crate, même valeur | dsh en fait une constante de module, Pyxis une constante de crate ; l'invariant 15 interdit d'en faire une clé |
| 13 | Segment de minuterie borné, horloge relue à chaque réveil | `MAX_TIMER_DELAY_MS = 2_147_483_647` (`runtime.ts:22`) | `MAX_TIMER_SEGMENT = 60 s` | **écart assumé** : la valeur de dsh est le plafond `i32` de `setTimeout` ; Pyxis choisit ce qui borne le retard après une veille système |
| 14 | Frontière de livraison unique et documentée | `types.ts:111`, `deliveryMode: 'session-local'` | livraison au fil propriétaire seulement | aucun ; le fil est déjà la frontière de propriété du lot 9 |
| 15 | Livrer puis persister | `runtime.ts:275` puis `:284` | **écart assumé** : persister puis livrer | l'invariant 12 l'impose ; `client_message_id` rend cela exactement une fois là où dsh est au moins une fois |
| 16 | Validation du flux candidat avant écriture | `invariant.ts:38` | **écart assumé** : pli total et faillible en douceur, en lecture | Pyxis n'a pas de crochet avant écriture ; un fil ne doit jamais devenir inouvrable |
| 17 | Verrou `faulted` sur journal corrompu | `runtime.ts:84`, le latch `faulted` | **écart assumé** : le pli compte les enregistrements illisibles et continue | même raison que 16 |
| 18 | Chaîne de transactions par agent | `transaction.ts:13` | **écart assumé** : aucun ; `ThreadActor` est déjà écrivain unique | la sérialisation que dsh doit construire est structurelle chez Pyxis |
| 19 | Flush de persistance en préflux et postflux | `persistence.ts:24`, appelé à `runtime.ts:235` et `:315` | **écart assumé** : `JsonlThreadStore` fait `sync_data` par ligne | c'est pourquoi `persistence_uncertain` disparaît : l'état « peut-être écrit » n'est pas représentable |
| 20 | Ne se réarme sur l'oisiveté que si le journal porte déjà un changement | `index.ts:51` | même court-circuit : pas d'enregistrement, pas de bras armé | aucun ; un fil sans rappel ne se réveille jamais |

## Assumptions & Constraints

### Assumptions (to validate)

- **HAUTE** : un cinquième bras dans le `select!` de `ThreadActor` ne change aucun comportement des quatre existants, en particulier l'ordre `biased;` qui donne la priorité à la fin de tour puis à l'annulation. Validée par US-155, qui place le bras **après** les quatre et le prouve par un test d'ordonnancement.
- **HAUTE** : le budget de réveils du lot 9 peut être partagé par un second producteur sans que la garantie « au plus trois tours sans entrée humaine intercalée » ne devienne fausse. Validée par US-157, qui alterne complétions de travaux et rappels dans la même chaîne et compte les tours.
- **MOYENNE** : un rappel `overdue` conservé par un refus de budget et délivré à la prochaine entrée humaine est le comportement attendu, plutôt qu'une perte silencieuse ou un contournement du budget. Non réfutable par un test ; US-157 la borne et la notice de US-163 la rend visible.
- **MOYENNE** : trois variantes additives de plus n'alourdissent pas le pli de manière perceptible. Un rappel écrit une entrée à la création, une par dispatch et une à la suppression, contre une entrée par tour aujourd'hui. Validée par US-152.
- **BASSE** : aucune surface app-server n'a besoin de changer. Un rappel produit une entrée ordinaire, donc un tour ordinaire, donc les événements que le protocole publie déjà.

### Hard Constraints

- **Invariant 11** : un tour produit exactement un état terminal, persisté avant publication. Un dispatch suit la même règle : écrit avant d'être soumis.
- **Invariant 12** : une opération acceptée est durable avant d'être acquittée ; un `client_message_id` déjà accepté rend les identifiants d'origine et ne ré-exécute rien. C'est ce qui rend le dispatch exactement une fois.
- **Invariant 13** : un seul arbre d'annulation. Le bras de minuterie est un bras de `select!` de l'acteur, donc annulé avec lui ; aucune tâche détachée, aucun `JoinHandle::abort`.
- **Invariant 15 et ADR-12** : toute limite d'orchestration est une constante de crate. Zéro clé de configuration, zéro drapeau, zéro variable `PYXIS_*` de comportement.
- **ADR-16** : les enregistrements sont durables, les processus ne le sont pas ; une reprise rapporte et n'exécute rien. Un rappel n'a pas de processus, et ADR-17 écrit pourquoi cela ne rouvre pas ADR-16.
- **Format durable** : les entrées sont additives, `SESSION_SCHEMA_VERSION` reste à 1, `THREAD_RUNTIME_VERSION` reste à 1, et un lecteur antérieur mappe les nouvelles entrées sur `SessionEntry::Unknown`.
- **`agent-core`** n'émet que des `AgentEvent` structurés, jamais d'ANSI ni de couleur.
- **Graphe de crates** : `agent-tools` dépend d'`agent-runtime`, jamais l'inverse (`crates/agent-tools/Cargo.toml:22-26`). Le domaine de planification ne peut donc pas vivre dans `agent-tools`.
- **Le clone Codex** résolu par `$PYXIS_CODEX_BASELINE` est en lecture seule, sans exception.
- **Défauts fermés** : les défauts du trait `Tool` restent fermés. Le texte d'un rappel est fourni par l'humain ou par le modèle, il réentre plus tard hors de la conversation qui l'a écrit, et son cadre le déclare non fiable.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatage du workspace
- `cargo clippy --workspace --all-targets` - lints, sans `-D warnings` par décision documentée
- `cargo test --workspace --no-fail-fast` - suite complète, nomme tous les tests en échec
- `just check` - l'agrégat des quatre portes du CI est vert
- `just parity` - les matrices gelées correspondent toujours au clone épinglé
- `git status --porcelain` - vide après `just check` : aucune porte de vérification n'écrit

## Epics & User Stories

Rappel : `**Source dsh:**` est relatif à `/home/arthur/dev/deepseek-harness`, en lecture seule ; les chemins `crates/` sont relatifs à `/home/arthur/dev/pyxis`.

### EP-046: Le domaine pur

Le vocabulaire fermé, le pli qui reconstruit l'état depuis le journal, la décision d'échéance et la récurrence en dernière occurrence seulement. Aucune horloge, aucun tokio, aucun effet. Ferme la défaillance 2.

**Definition of Done:** un test de domaine construit une suite d'enregistrements, la plie, demande la décision d'échéance à un instant donné en paramètre, et obtient le même résultat à chaque exécution sans qu'aucune horloge réelle ne soit lue.

#### US-148: Le vocabulaire d'un rappel est fermé et vérifié par le compilateur
**Description:** As a agent de codage, I want une règle, un état et un jeu d'erreurs fermés dans un seul module so that un `match` non exhaustif refuse de compiler au lieu de laisser passer une forme oubliée.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Source dsh:** `packages/schedule/schedule/src/types.ts:13-51` pour les trois interfaces d'enregistrement, `:105` pour l'union `ScheduleChange`, `:108` pour `ScheduleState`, `:111` pour `ScheduleDeliveryMode`, `:125-198` pour les dix codes d'erreur et leur union

**Acceptance Criteria:**
- [ ] Un module `crates/agent-runtime/src/schedule.rs` porte `ScheduleId`, `ScheduleRule`, `ScheduleState`, `ScheduleRecord` et `ScheduleError` ; aucun de ces types n'est `#[non_exhaustive]`.
- [ ] `ScheduleRule` porte exactement `After { seconds }`, `At { at_ms }` et `Every { first_at_ms, interval_seconds }` ; le doc-comment de `At` renvoie à US-162 pour la question du temps civil zoné.
- [ ] `ScheduleState` porte exactement `Scheduled` et `Overdue` ; son doc-comment énonce que `Overdue` est ce qui rend un rappel récupérable après un refus de budget ou un arrêt du processus.
- [ ] `ScheduleError` porte exactement neuf variantes : `InvalidPrompt`, `InvalidSelector`, `InvalidRule`, `InvalidTimeZone`, `NotFuture`, `TimeOutOfRange`, `FrequencyTooHigh`, `CorruptScheduleLog`, `Internal` ; un doc-comment nomme le dixième code de dsh, `persistence_uncertain`, et dit pourquoi il n'entre pas (`JsonlThreadStore` fait `sync_data` par ligne et empoisonne son écrivain, donc l'état intermédiaire n'existe pas).
- [ ] `MIN_EVERY_INTERVAL_SECONDS = 300`, `MAX_ACTIVE_SCHEDULES = 16` et `MAX_SCHEDULE_PROMPT_CHARS = 1024` sont des constantes de crate, chacune avec un doc-comment disant ce qu'elle protège.
- [ ] Le mode de livraison est une valeur unique documentée comme frontière v1, sur le patron de `ScheduleDeliveryMode` de dsh ; ce n'est pas un `bool`, pour que l'élargir plus tard soit l'ajout d'une variante.
- [ ] Given un `ScheduleRecord` sérialisé puis désérialisé, when il est comparé à l'original, then il est égal ; un test de tour complet le prouve.
- [ ] Given une règle avec un intervalle de 299 s, when elle est validée, then `FrequencyTooHigh` est rendue et nomme la borne.
- [ ] Aucune clé de configuration, aucun drapeau, aucune variable `PYXIS_*` n'est ajouté.

#### US-149: Le pli reconstruit l'état et ne rend jamais un fil inouvrable
**Description:** As a humain reprenant une session, I want que l'état de planification soit recalculé depuis le journal so that une reprise soit identique à un démarrage, et qu'un enregistrement illisible soit compté plutôt que fatal.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-148
**Source dsh:** `packages/schedule/schedule/src/domain.ts:575` pour `foldScheduleEvents`, et `:465` pour `decodeScheduleChange`, qui est l'endroit exact où dsh refuse un enregistrement illisible ; `packages/schedule/schedule/src/invariant.ts:38` pour l'approche opposée, le crochet `internal/dispatch` qui valide avant écriture, explicitement écartée ici

**Acceptance Criteria:**
- [ ] Une fonction pure prend une suite d'entrées durables de planification et rend l'état plié : les enregistrements actifs, leur échéance courante et leur état.
- [ ] La fonction ne lit aucune horloge ; l'instant courant est un paramètre.
- [ ] Given une création puis une suppression du même identifiant, when la suite est pliée, then l'enregistrement est absent, et non présent et marqué.
- [ ] Given un dispatch d'un ponctuel, when la suite est pliée, then l'enregistrement est absent : un ponctuel délivré n'existe plus.
- [ ] Given un dispatch d'un récurrent portant son instant d'acceptation, when la suite est pliée, then l'échéance suivante est recalculée depuis cet instant et non depuis l'échéance d'origine.
- [ ] Given un enregistrement illisible au milieu de la suite, when elle est pliée, then il est ignoré, un compteur d'illisibles est incrémenté, une trace `tracing` de niveau `warn` le nomme, et le pli continue ; le fil s'ouvre.
- [ ] Given un dispatch sans création correspondante, when la suite est pliée, then il est ignoré comme illisible ; aucune panique, aucun `unwrap`.
- [ ] Given une suite pliée deux fois, when les deux résultats sont comparés, then ils sont égaux ; le pli est une fonction, pas une machine à état mutable.
- [ ] Un test plie une suite de mille entrées et mesure ; le pli est linéaire et la story consigne le temps mesuré.

#### US-150: La décision d'échéance choisit un seul dû à la fois
**Description:** As a mainteneur, I want une fonction qui, pour un état plié et un instant, rend soit un dû unique, soit un lot de récurrents, soit une attente so that l'acteur n'ait aucune logique de sélection à porter.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-149
**Source dsh:** `packages/schedule/schedule/src/runtime.ts:35`, `dueDecision(folded, now)` : le ponctuel le plus ancien par échéance puis par ordre de création, ou le lot complet des récurrents, ou une attente portant la prochaine échéance

**Acceptance Criteria:**
- [ ] La fonction est pure : état plié et instant en entrée, décision en sortie, aucune horloge lue.
- [ ] La décision est un enum fermé à trois formes : un ponctuel dû, un lot de récurrents dus, ou une attente portant optionnellement la prochaine échéance.
- [ ] Given deux ponctuels dus à la même échéance, when la décision est prise, then celui créé en premier gagne ; l'ordre est total et déterministe.
- [ ] Given un ponctuel et un récurrent tous deux dus, when la décision est prise, then l'ordre entre les deux formes est fixé par un doc-comment et un test, jamais laissé au hasard du parcours.
- [ ] Given aucun enregistrement, when la décision est prise, then une attente sans échéance est rendue, et l'acteur n'arme rien.
- [ ] Given un enregistrement dont l'échéance est dans le futur, when la décision est prise, then une attente portant cette échéance est rendue.
- [ ] Given un instant antérieur à toute échéance et un état non vide, when la décision est prise, then aucun dû n'est rendu ; un test le vérifie avec un instant reculé, ce qui couvre un recul d'horloge murale.

#### US-151: La récurrence saute au dernier créneau et ne rattrape jamais un arriéré
**Description:** As a humain ayant laissé un fil fermé une journée, I want qu'un rappel de cinq minutes délivre une occurrence et non deux cent quatre-vingt-huit so that le retour dans le fil ne soit pas une rafale.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-148
**Source dsh:** `packages/schedule/schedule/src/domain.ts:519`, `resolveEveryOccurrence` : `Math.floor((acceptedAt - target) / interval)`, le refus explicite d'un instant d'acceptation antérieur à l'échéance active, et le plafond `MAX_FOUR_DIGIT_YEAR_MS` défini à `:27`

**Acceptance Criteria:**
- [ ] Une fonction pure prend un enregistrement récurrent et un instant d'acceptation, et rend l'occurrence retenue plus l'échéance suivante.
- [ ] Le calcul avance d'un nombre entier de pas en une opération ; aucune boucle n'énumère les créneaux manqués.
- [ ] Given un instant d'acceptation antérieur à l'échéance active, when la fonction est appelée, then elle rend une erreur nommée et ne calcule rien ; c'est un invariant du pli, pas une entrée utilisateur.
- [ ] Given un rappel de 300 s en retard de 86 400 s, when la fonction est appelée, then une seule occurrence est rendue et l'échéance suivante est dans le futur par rapport à l'instant d'acceptation.
- [ ] Given une échéance suivante qui déborde la borne de représentation, when la fonction est appelée, then l'occurrence est rendue **sans** échéance suivante, ce qui termine le récurrent au lieu de le corrompre.
- [ ] Le calcul est en arithmétique saturante ou vérifiée ; un test vise `u64::MAX` et prouve qu'aucun débordement ne panique.
- [ ] Un test nomme la propriété prouvée dans son nom, `a_recurring_reminder_a_day_late_delivers_one_occurrence_not_a_backlog`.

### EP-047: L'écriture durable et la frontière de reprise

Les trois variantes additives, le pli à l'ouverture du fil, et l'ADR qui dit pourquoi délivrer un rappel en retard ne contredit pas ADR-16. Ferme les défaillances 2 et 5.

**Definition of Done:** un test crée un rappel, ferme le fil, rouvre le journal, et retrouve l'enregistrement avec son échéance recalculée, sans que `resume.rs` n'ait ouvert un seul tour.

#### US-152: Trois variantes additives, et aucune version ne bouge
**Description:** As a mainteneur, I want que la création, le dispatch et la suppression d'un rappel soient des entrées du journal de fil so that un redémarrage retrouve exactement ce qui existait, sans qu'aucun format existant ne change.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-148
**Source dsh:** `packages/schedule/schedule/src/types.ts:105`, l'union `ScheduleChange` à trois membres, et l'asymétrie de l'instant d'acceptation entre `OneShotScheduleDispatchChange` (`:86`) qui ne le porte pas et `EveryScheduleDispatchChange` (`:93`) qui le porte

**Acceptance Criteria:**
- [ ] `ThreadEventPayload` (`crates/agent-runtime/src/event.rs:95`) gagne exactement trois variantes : `ScheduleCreated`, `ScheduleDispatched`, `ScheduleDeleted`, calquées sur `JobRegistered`, `JobStateChanged` et `JobReported`.
- [ ] `ScheduleDispatched` porte l'instant d'acceptation pour un récurrent et ne le porte pas pour un ponctuel ; le doc-comment dit que sans ce champ le pli ne peut pas recalculer le créneau.
- [ ] `ThreadEvent::turn_id()` (`crates/agent-runtime/src/event.rs:72`) traite les trois nouvelles variantes explicitement ; le `match` reste exhaustif sans bras générique.
- [ ] Le `match &event.payload` de `crates/agent-runtime/src/resume.rs:101` traite les trois nouvelles variantes explicitement ; il reste exhaustif sans bras générique.
- [ ] `SESSION_SCHEMA_VERSION` reste à 1 et `THREAD_RUNTIME_VERSION` (`crates/agent-runtime/src/event.rs:31`) reste à 1.
- [ ] Given un lecteur antérieur à ce lot rencontrant l'une des trois entrées, when il la lit, then elle est mappée sur `SessionEntry::Unknown` et ignorée ; le test suit la convention de `a_v1_reader_maps_the_three_job_entries_to_unknown_and_resumes_past_them` (`crates/agent-session/src/lib.rs`).
- [ ] Given un rappel créé, when l'outil rend son identifiant, then l'entrée `ScheduleCreated` est déjà commise ; un test coupe l'écriture entre les deux et prouve que l'acquittement n'arrive pas.
- [ ] Given un échec d'écriture à la création, when il survient, then la création est refusée par une erreur nommée et aucun bras de minuterie n'est armé.
- [ ] L'écriture passe par `commit` (`crates/agent-runtime/src/thread.rs:890`) ; aucune variante de `StoreOperation` n'est ajoutée, ou la story explique laquelle et pourquoi.

#### US-153: La reprise plie, rapporte, et n'exécute rien
**Description:** As a humain reprenant une session, I want que la reprise elle-même reste un chemin de lecture so that la frontière posée par ADR-16 tienne, et que ce qui délivre un rappel en retard soit l'acteur vivant et non le code de reprise.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-149, US-152
**Source dsh:** `packages/schedule/schedule/src/index.ts:55`, où le plugin appelle `runtime.start()` à la création de l'agent, ce qui est ce qui rejoue après un redémarrage ; et `:51`, où il ne se réarme sur l'oisiveté que si le journal porte déjà un événement `schedule/change`

**Acceptance Criteria:**
- [ ] `resume.rs` plie les entrées de planification et rend l'état plié dans son rapport ; il n'appelle aucune soumission, n'ouvre aucun tour et ne dépense aucun budget.
- [ ] Given un fil rouvert dont un rappel est en retard, when la reprise se termine, then aucun tour n'a été ouvert et le rapport nomme le rappel comme dû.
- [ ] Given ce même fil, when l'acteur démarre et arme son bras, then le rappel en retard est dispatché par le chemin ordinaire, et un test le prouve de bout en bout : création, arrêt de l'acteur, réouverture sur le même magasin, arrivée de l'entrée.
- [ ] Given un journal antérieur à ce lot, when il est rouvert, then aucun rappel n'est trouvé, aucune erreur n'est levée et aucune migration n'est écrite.
- [ ] Given un journal portant un enregistrement de planification illisible, when il est rouvert, then le fil s'ouvre, le compteur d'illisibles est rapporté, et les autres rappels fonctionnent.
- [ ] Given une double reprise consécutive, when la seconde a lieu, then aucun dispatch n'est réécrit ; l'idempotence vient de la clé de message, pas d'un drapeau ad hoc.
- [ ] Un test nomme la propriété prouvée : `a_resume_folds_and_reports_and_opens_no_turn_of_its_own`.

#### US-154: ADR-17 distingue un rappel d'un processus ressuscité
**Description:** As a mainteneur, I want que la frontière soit écrite so that une pull request ultérieure ne puisse ni faire exécuter la reprise, ni invoquer ADR-16 pour refuser un rappel en retard.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-153
**Source dsh:** aucune. dsh n'a pas d'ADR, et sa frontière de livraison est implicite dans le type à valeur unique `ScheduleDeliveryMode` (`packages/schedule/schedule/src/types.ts:111`)

**Acceptance Criteria:**
- [ ] `docs/DECISIONS.md` gagne **ADR-17**, en français, au format des seize précédents, nommant les surfaces qu'il contraint : `crates/agent-runtime/src/schedule.rs`, `crates/agent-runtime/src/thread.rs`, `crates/agent-runtime/src/resume.rs`.
- [ ] L'ADR énonce la distinction : ADR-16 oppose enregistrements et **processus**, or un rappel n'a pas de processus ; ce qu'une reprise ne doit jamais faire est ressusciter une exécution dont elle ne peut pas prouver l'identité, et non soumettre une entrée que l'humain a écrite d'avance.
- [ ] L'ADR fixe le partage du seuil : le budget de réveils est **unique** pour tous les producteurs de tours non demandés ; un second budget est explicitement refusé, avec la raison.
- [ ] L'ADR fixe la conservation : un dispatch refusé par le budget laisse l'enregistrement en `Overdue` et ne le perd pas.
- [ ] L'ADR nomme les alternatives écartées : la reprise qui délivre elle-même, l'attente d'une entrée humaine avant toute délivrance, et un budget séparé pour les rappels.
- [ ] `cargo test -p agent-doc-gates` passe, la porte des décisions acceptant le nouvel enregistrement.
- [ ] ADR-16 n'est pas modifié ; si sa lecture doit être précisée, ADR-17 le dit et ADR-16 reste tel quel.

### EP-048: L'horloge dans l'acteur

Le cinquième bras du `select!`, le dispatch persisté avant livraison, et le budget de réveils partagé. Ferme les défaillances 1, 3 et 4.

**Definition of Done:** un test pilote une horloge factice, avance le temps, et observe une entrée d'origine `Runtime` arriver dans le fil, sans qu'aucune durée réelle n'ait été attendue.

#### US-155: Le bras de minuterie est borné et relit l'horloge murale à chaque réveil
**Description:** As a mainteneur, I want un cinquième bras qui dort par segments et revérifie l'heure so that une veille de machine ou un saut d'horloge ne fasse ni tirer trop tôt, ni attendre indéfiniment.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-150, US-152
**Source dsh:** `packages/schedule/schedule/src/runtime.ts:22` pour `MAX_TIMER_DELAY_MS`, et `:179` pour le `Math.min(target - now, MAX_TIMER_DELAY_MS)` qui borne le segment ; le corps de `driveOnce` montre que chaque réveil recalcule la décision au lieu de faire confiance à la minuterie

**Acceptance Criteria:**
- [ ] Le `select!` de `crates/agent-runtime/src/thread.rs:968-995` gagne un cinquième bras, placé **après** les quatre existants, l'ordre `biased;` restant tel quel ; un test d'ordonnancement prouve qu'une fin de tour et une annulation gagnent toujours sur une échéance.
- [ ] Le bras ne tient aucun emprunt de `self` : l'échéance est copiée hors de `self` avant le `select!`, et l'`Arc<dyn Clock>` est cloné avant la boucle, sur le patron du commentaire existant « the `select!` arms may not hold a borrow of `self` ».
- [ ] `MAX_TIMER_SEGMENT` est une constante de crate valant 60 s, avec un doc-comment disant pourquoi Pyxis diverge de la valeur de dsh : celle-ci est le plafond `i32` de `setTimeout`, tandis que `CLOCK_MONOTONIC` n'avance pas pendant une veille système, donc le segment borne le retard après un réveil de machine.
- [ ] Le bras dort le minimum entre le temps restant et `MAX_TIMER_SEGMENT`, et relit `clock.now_ms()` après chaque segment avant de décider.
- [ ] Given aucun enregistrement actif, when la boucle tourne, then le bras est un futur qui ne se résout jamais, sur le patron de `straggler(deadline)` (`crates/agent-runtime/src/thread.rs:880-886`) ; un fil sans rappel ne se réveille jamais.
- [ ] Given une échéance à trois secondes, when le bras est armé, then il dort trois secondes et non un segment entier.
- [ ] Given une échéance à trois heures, when le bras est armé, then il se réveille par segments et redécide à chaque fois ; un test avec horloge factice compte les réveils.
- [ ] Given une horloge murale reculée après l'armement, when le segment expire, then rien n'est dispatché et le bras se réarme ; le réveil ne déclenche jamais par lui-même.
- [ ] Le temps est celui du `Clock` injecté, jamais `Instant::now()` ni `SystemTime::now()` ; aucun test de cette story n'attend une durée réelle supérieure à 50 ms.

#### US-156: Un dispatch est durable avant d'être livré, et exactement une fois
**Description:** As a humain, I want que le rappel soit écrit avant d'être soumis so that un arrêt entre les deux le rejoue sans le dupliquer, là où dsh ne peut garantir qu'au moins une fois.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-155
**Source dsh:** `packages/schedule/schedule/src/runtime.ts:275`, `agent.followup(message)`, puis `:284`, `session.append('schedule/change', ...)` : l'ordre exact dont ce lot prend le contrepied, et qui est ce qui limite dsh à une livraison au moins une fois

**Acceptance Criteria:**
- [ ] L'entrée `ScheduleDispatched` est commise **avant** toute soumission ; l'ordre est asserté par un test qui observe la séquence d'écritures.
- [ ] La soumission porte `client_message_id = "schedule-dispatch:{schedule_id}:{occurrence_at_ms}"` et `InputOrigin::Runtime`.
- [ ] Given une soumission dont la clé a déjà été acceptée, when elle est resoumise, then les identifiants d'origine sont rendus et aucun second tour n'est ouvert ; c'est le chemin `already_accepted` existant (`crates/agent-runtime/src/thread.rs:1138`).
- [ ] Given un arrêt entre l'écriture et la soumission, when le fil est rouvert, then le rappel est délivré une fois, jamais zéro, jamais deux ; le test coupe précisément entre les deux.
- [ ] Given un rappel dispatché pendant qu'un tour court, when le dispatch a lieu, then il entre par le chemin de pilotage, jamais entre un appel d'outil et son résultat ; le comportement suit `on_steer` (`crates/agent-runtime/src/thread.rs:1161`).
- [ ] Given un échec d'écriture de l'entrée de dispatch, when il survient, then rien n'est soumis, l'enregistrement reste dû, et une trace `warn` le nomme ; le fil ne s'arrête pas.
- [ ] Given un fil dont la file d'entrées est pleine à `MAX_PENDING_INPUTS`, when un dispatch survient, then il est refusé sans perdre l'enregistrement, qui reste `Overdue`.
- [ ] Aucun `JoinHandle::abort` n'apparaît sur ce chemin ; le bras de minuterie est annulé avec l'acteur.

#### US-157: Le budget de réveils est unique, partagé, et un refus conserve le rappel
**Description:** As a humain, I want que rappels et complétions de travaux se partagent le même seuil de trois so that la garantie du lot 9 reste vraie, et qu'un refus retarde le rappel au lieu de le perdre.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-156
**Source dsh:** aucune. dsh n'a pas de budget de réveils : sa seule borne est `deliveryMode: 'session-local'` (`packages/schedule/schedule/src/types.ts:111`). C'est une divergence pure, imposée par le lot 9

**Acceptance Criteria:**
- [ ] Le dispatch consulte le `WakeBudget` existant (`crates/agent-runtime/src/jobs.rs:96-160`) ; aucun second budget, aucune seconde constante n'est introduite.
- [ ] Given un budget épuisé, when une échéance passe, then aucun tour n'est ouvert, aucune entrée `ScheduleDispatched` n'est écrite, et l'enregistrement passe en `Overdue`.
- [ ] Given un rappel en `Overdue` et une entrée d'origine `Human`, when celle-ci est soumise, then le budget se réarme et le rappel est délivré ; un test le prouve dans cet ordre.
- [ ] Given une alternance de deux complétions de travaux et de deux rappels sur un fil oisif, when la chaîne se déroule, then au plus trois tours sont ouverts sans entrée humaine intercalée ; le test compte les tours et couvre l'hypothèse HAUTE de partage du budget.
- [ ] Given un rappel récurrent en retard d'une journée et un budget épuisé, when le budget se réarme, then **une** occurrence est délivrée ; la conservation et la dernière occurrence seulement se composent, et le test le nomme.
- [ ] Given une livraison sous `CompletionDelivery::Quiet`, when une échéance passe, then aucun tour n'est ouvert ; sous `-p` un rappel ne réveille jamais rien, et l'enregistrement reste dû.
- [ ] Un doc-comment du module de planification renvoie à ADR-17 pour la règle du budget unique.

### EP-049: Ce que le modèle voit

Les trois outils, le vocabulaire d'erreurs rendu comme valeur, le cadre anti-injection, et le sélecteur de date civile. Ferme la défaillance 1 côté modèle.

**Definition of Done:** `docs/tool-catalog.md` régénéré compte trente-trois outils, et le README du crate qui compose le cadre de rappel porte la section `## Model Experience` correspondante.

#### US-158: schedule_create accepte exactement un sélecteur et nomme ce qu'il refuse
**Description:** As a modèle, I want créer un rappel avec un sélecteur unique et recevoir une erreur nommée quand je me trompe so that je puisse corriger mon appel au lieu de recevoir une exception opaque.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-152, US-155
**Source dsh:** `packages/schedule/schedule/src/tools.ts:253`, `validateCreateArgs` et son exigence d'exactement un sélecteur ; `:117`, `CREATE_OUTPUT_SCHEMA` en `oneOf` qui fait des erreurs des valeurs de retour ; `:177`, `internalError()` qui ne divulgue pas l'exception

**Acceptance Criteria:**
- [ ] Un outil `schedule_create` est enregistré dans `crates/agent-tools/src/`, sur le patron de `crates/agent-tools/src/jobs.rs`, et rend `ToolOutput::text`.
- [ ] Given zéro ou deux sélecteurs fournis, when l'appel est fait, then `invalid_selector` est rendue **comme valeur de retour**, pas comme `Err`, et le message nomme les trois sélecteurs possibles.
- [ ] Given un texte de rappel vide ou dépassant `MAX_SCHEDULE_PROMPT_CHARS`, when l'appel est fait, then `invalid_prompt` est rendue et nomme la borne.
- [ ] Given une échéance dans le passé, when l'appel est fait, then `not_future` est rendue.
- [ ] Given un intervalle sous `MIN_EVERY_INTERVAL_SECONDS`, when l'appel est fait, then `frequency_too_high` est rendue et nomme la borne.
- [ ] Given `MAX_ACTIVE_SCHEDULES` rappels déjà actifs, when un de plus est demandé, then il est refusé par une erreur nommant la borne et l'action de libération, jamais par une attente.
- [ ] Given une erreur interne inattendue, when elle survient, then `internal_error` est rendue avec un message fixe qui ne divulgue rien, et la cause part en `tracing` de niveau `error`.
- [ ] Given une création acceptée, when l'appel rend, then l'identifiant et l'échéance résolue sont dans la réponse, et l'entrée durable est déjà commise.
- [ ] La description de l'outil dit que le rappel arrive dans **ce fil seulement** et que le fil doit être vivant à l'échéance ; c'est la frontière `session-local`.

#### US-159: schedule_list rend l'état plié sans qu'aucun identifiant ne soit à retenir
**Description:** As a modèle, I want lister mes rappels avec leur échéance et leur état so that je n'aie pas à me souvenir d'un identifiant à travers une compaction.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-158
**Source dsh:** `packages/schedule/schedule/src/tools.ts:118` pour `LIST_OUTPUT_SCHEMA`, `:156` pour la description de l'outil, et `:222` pour `foldForTool`, qui montre comment dsh rend une erreur de pli depuis une opération de lecture

**Acceptance Criteria:**
- [ ] Un outil `schedule_list` est enregistré, en lecture seule, et rend `ToolOutput::text`.
- [ ] Chaque ligne porte l'identifiant, la forme de la règle, l'échéance courante, l'état, et le texte du rappel borné et neutralisé.
- [ ] Given aucun rappel, when l'appel est fait, then une réponse explicite le dit, jamais une erreur ni une réponse vide.
- [ ] Given un rappel d'un autre fil, when l'appel est fait, then il n'apparaît pas ; la frontière est le fil, comme pour le registre de travaux de fond.
- [ ] Given un pli ayant rencontré des enregistrements illisibles, when l'appel est fait, then `corrupt_schedule_log` est rendue **en plus** de la liste des rappels lisibles, jamais à leur place.
- [ ] Given un texte de rappel portant des caractères de contrôle, when il est listé, then il est borné et neutralisé avant d'être rendu au modèle.
- [ ] La sortie est stable et ordonnée par échéance croissante ; un test le prouve.

#### US-160: schedule_delete traite un inconnu comme un succès
**Description:** As a modèle, I want qu'une suppression d'un rappel inexistant soit un succès portant un drapeau so that un réessai après une réponse perdue soit sûr.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-158
**Source dsh:** `packages/schedule/schedule/src/types.ts:206-208`, `ScheduleDeleteResult` : la branche `{id, deleted: false, code: 'schedule_not_found'}` vit dans le type de **succès** et non dans `ScheduleToolError` (`:187`) ; `packages/schedule/schedule/src/tools.ts:124` pour le schéma de sortie correspondant

**Acceptance Criteria:**
- [ ] Un outil `schedule_delete` est enregistré et rend `ToolOutput::text`.
- [ ] Given un identifiant existant, when il est supprimé, then l'entrée `ScheduleDeleted` est commise avant que l'appel ne rende, et la réponse dit que la suppression a eu lieu.
- [ ] Given un identifiant inconnu, when il est supprimé, then la réponse est un **succès** portant `schedule_not_found`, jamais une erreur ; un doc-comment dit que c'est ce qui rend un réessai sûr.
- [ ] Given un identifiant appartenant à un autre fil, when il est supprimé, then la réponse est le même succès `schedule_not_found` ; l'existence dans un autre fil n'est pas divulguée.
- [ ] Given un rappel supprimé alors que son bras de minuterie est armé, when la suppression est commise, then le bras se réarme sur la décision suivante et ne dispatche pas le rappel supprimé.
- [ ] Given une suppression pendant qu'un dispatch du même rappel est en cours d'écriture, when les deux se croisent, then l'ordre du journal tranche et aucun état incohérent n'est plié ; l'écrivain unique de l'acteur le garantit et un test le nomme.

#### US-161: Le rappel arrive dans un cadre qui se déclare non fiable
**Description:** As a humain, I want que le texte du rappel réentre dans un cadre explicite so that le modèle ne le confonde pas avec une nouvelle instruction de l'utilisateur au moment où il arrive hors de sa conversation d'origine.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-156
**Source dsh:** `packages/schedule/schedule/src/domain.ts:779`, `renderReminderFraming` : en-tête `[SCHEDULE REMINDER]`, ligne déclarant le contenu non fiable, champs dynamiques échappés par `JSON.stringify` ; `:794`, `renderEveryReminderBatchFraming`, la forme par lot

**Acceptance Criteria:**
- [ ] Le cadre est composé par une fonction **pure** du module de planification ; l'acteur ne compose aucune chaîne.
- [ ] Le cadre porte un en-tête fixe, une ligne disant explicitement que le contenu est un rappel non fiable et non une nouvelle instruction de l'utilisateur, l'identifiant, l'instant d'occurrence, et le texte.
- [ ] Les champs dynamiques sont échappés ; un test injecte un texte contenant l'en-tête lui-même, des sauts de ligne et des guillemets, et prouve qu'il ne peut pas se faire passer pour la structure du cadre.
- [ ] Une forme par lot existe pour un ensemble de récurrents dus simultanément, distincte de la forme unitaire ; les deux sont testées.
- [ ] Le README du crate qui compose le cadre gagne la section `## Model Experience` dans la forme fixée par `docs/model-experience.md`, avec le coût en jetons du cadre et son effet sur le cache.
- [ ] Les descriptions des trois outils sont recensées dans la même section, avec leur coût.
- [ ] `cargo test -p agent-doc-gates` passe, la porte d'expérience du modèle acceptant les nouvelles surfaces.

#### US-162: Le sélecteur at prend une date civile dans un fuseau nommé
**Description:** As a humain, I want dire « à 9 h, heure de Paris » so that je n'aie pas à convertir une heure locale en instant UTC de tête, ni à me tromper d'une heure deux fois par an.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-158
**Source dsh:** `packages/schedule/schedule/src/types.ts:53` pour `LocalAtInput {date, time, time_zone}` et `:143` pour `InvalidTimeZoneError` ; `packages/schedule/schedule/src/domain.ts:251` pour `canonicalizeTimeZone` et `:333` pour `resolveLocalInstant`, qui est l'endroit où dsh tranche le trou et le repli sans base tzdb, par projection `Intl`

**Acceptance Criteria:**
- [ ] La story tranche et écrit la décision de dépendance avant tout code : soit `jiff` entre au workspace, soit `at` reste un instant UTC et le sélecteur zoné est différé. Le PRD recommande `jiff`, et l'argument est dans Technical Considerations.
- [ ] Si une dépendance est ajoutée, son entrée du `Cargo.toml` du workspace porte le commentaire argumenté que `AGENTS.md` exige : ce qui la force, la contrainte de version qui compte, l'alternative écartée. Le fait que `crates/agent-tools/src/time.rs:193-195` justifie l'absence de dépendance « for one format string » est cité, avec la raison pour laquelle l'argument s'inverse ici.
- [ ] Si `jiff` entre, il entre en `default-features = false` avec la lecture du tzdb système ; Pyxis étant Linux seulement, rien n'est embarqué et `/usr/share/zoneinfo` est la source.
- [ ] Given un nom de fuseau inconnu, when l'appel est fait, then `invalid_time_zone` est rendue et nomme le fuseau reçu.
- [ ] Given une heure locale tombant dans un trou de passage à l'heure d'été, when elle est résolue, then la politique retenue est écrite dans un doc-comment et testée ; la story dit si elle suit le défaut de la bibliothèque, l'instant après le trou, ou si elle refuse comme dsh.
- [ ] Given une heure locale tombant dans un repli, when elle est résolue, then la première occurrence est retenue, et un test le prouve sur une date de repli réelle.
- [ ] Given une échéance au-delà de la borne de représentation, when elle est résolue, then `time_out_of_range` est rendue.
- [ ] Un test résout au moins trois fuseaux dont un de l'hémisphère sud, pour que l'inversion des saisons soit couverte.
- [ ] Given l'absence de `/usr/share/zoneinfo`, when un fuseau est résolu, then l'erreur est nommée et le sélecteur `after` continue de fonctionner ; le lot ne devient pas indisponible.

### EP-050: La notice humaine et les documents

Ce que l'humain lit à la reprise, et les documents que le lot rend faux s'il ne les touche pas.

**Definition of Done:** `just check` est vert, `git status --porcelain` est vide après, et les trois catalogues générés correspondent au code.

#### US-163: La notice de reprise nomme les rappels dus et les rappels à venir
**Description:** As a humain reprenant une session, I want savoir en une ligne ce qui va me revenir so that je ne sois pas surpris par un tour que je n'ai pas demandé.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-153, US-157
**Source dsh:** aucune. dsh n'a pas de notice de reprise : sa seule surface humaine est le rappel lui-même, encadré par `packages/schedule/schedule/src/domain.ts:779`

**Acceptance Criteria:**
- [ ] La notice de reprise d'`agent-cli` gagne une ligne par rappel dû et un décompte des rappels à venir avec la prochaine échéance.
- [ ] Given un rappel `Overdue` conservé par un refus de budget, when la notice est écrite, then elle le distingue d'un rappel simplement en retard par l'arrêt du processus.
- [ ] Given aucun rappel, when la notice est écrite, then aucune ligne n'est ajoutée ; la notice ne grossit pas pour rien.
- [ ] Given des enregistrements illisibles, when la notice est écrite, then leur nombre est dit, et la notice reste lisible.
- [ ] Le texte du rappel est borné et neutralisé avant affichage ; un test couvre un texte multi-ligne portant des séquences ANSI.
- [ ] Une story de rendu TUI touchée par cette ligne est prouvée par un instantané relu (`cargo insta review`), ou la story dit qu'aucun instantané ne bouge.

#### US-164: Les catalogues et les documents disent la vérité après le lot
**Description:** As a agent de codage, I want que les documents comptent juste so that je ne parte pas d'un inventaire faux à la prochaine tâche.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-158, US-159, US-160
**Source dsh:** aucune

**Acceptance Criteria:**
- [ ] `docs/tool-catalog.md` est régénéré par `PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis tool_catalog` et compte trente-trois outils ; il n'est jamais édité à la main.
- [ ] `docs/config-catalog.md` reste à quinze clés ; sa régénération ne produit aucune différence.
- [ ] `docs/crate-graph.md` est régénéré si une description de crate bouge, sinon il ne bouge pas.
- [ ] `AGENTS.md:171` cesse de dire « twenty-nine registered tools », périmé depuis le lot 9, et dit le compte du catalogue ; la ligne est en anglais, comme le fichier.
- [ ] La section pertinente de `docs/ARCHITECTURE.md` gagne la planification dans sa liste numérotée d'invariants ou dans son inventaire de sous-systèmes, selon ce que la relecture montre nécessaire.
- [ ] La table « Where new behavior goes » d'`AGENTS.md` gagne une ligne disant où va un producteur de tours non demandés, et renvoyant à ADR-17 pour le budget unique.
- [ ] `docs/CURRENT_STATUS.md` est mis à jour, en anglais, avec le périmètre livré et ce qui reste différé, US-162 nommément si elle n'est pas prise.
- [ ] Given `just check` lancé après le lot, when il se termine, then il est vert et `git status --porcelain` est vide.

## Functional Requirements

- FR-01: Le système doit représenter un rappel par une règle fermée à trois formes, `after`, `at` et `every`, et un état fermé à deux valeurs.
- FR-02: Le système doit recalculer l'état de planification par pli sur le journal du fil, et ne jamais le stocker.
- FR-03: Le système doit rendre le pli total : un enregistrement illisible est compté et ignoré, jamais fatal, et un fil reste ouvrable.
- FR-04: Le système doit calculer les échéances dans un domaine pur, sans lire aucune horloge, l'instant étant un paramètre.
- FR-05: Le système doit, pour un rappel récurrent en retard, délivrer la dernière occurrence seulement, et jamais énumérer un arriéré.
- FR-06: Le système doit refuser un intervalle de récurrence sous une borne qui est une constante de crate.
- FR-07: Le système doit refuser un rappel au-delà d'une borne de rappels actifs qui est une constante de crate, par une erreur nommant l'action de libération, jamais par une attente.
- FR-08: Le système doit rendre durable l'existence d'un rappel avant d'en acquitter la création à son appelant.
- FR-09: Le système doit rendre durable un dispatch avant de soumettre l'entrée correspondante.
- FR-10: Le système doit délivrer un rappel exactement une fois, en s'appuyant sur l'idempotence de la clé de message.
- FR-11: Le système doit réveiller un fil oisif sur échéance, par un bras de minuterie borné qui relit l'horloge murale à chaque segment.
- FR-12: Le système ne doit armer aucun bras de minuterie quand aucun rappel n'est actif.
- FR-13: Le système doit soumettre un rappel dispatché avec une origine `Runtime`, et n'en réarmer le budget que sur une entrée d'origine humaine.
- FR-14: Le système doit partager un unique budget de réveils entre tous les producteurs de tours non demandés.
- FR-15: Le système doit conserver en `overdue` un rappel dont le dispatch a été refusé, et ne jamais le perdre.
- FR-16: Le système ne doit ouvrir aucun tour pour un rappel sous `-p`.
- FR-17: Le système doit, à la reprise d'un fil, plier et rapporter sans ouvrir aucun tour ; la délivrance d'un rappel en retard appartient à l'acteur vivant.
- FR-18: Le système doit délivrer un rappel dans le fil qui l'a créé et dans aucun autre.
- FR-19: Le système doit rendre ses erreurs comme valeurs de retour nommées, et ne jamais divulguer une exception interne au modèle.
- FR-20: Le système doit traiter la suppression d'un rappel inconnu comme un succès portant un drapeau, jamais comme une erreur.
- FR-21: Le système doit encadrer le texte d'un rappel délivré par un en-tête déclarant son contenu non fiable, avec les champs dynamiques échappés.
- FR-22: Le système ne doit exposer aucune clé de configuration, aucun drapeau et aucune variable d'environnement pour ses bornes.

## Non-Functional Requirements

- **Empreinte durable:** au plus 1 entrée JSONL à la création, 1 par dispatch et 1 à la suppression ; `SESSION_SCHEMA_VERSION` reste à 1, `THREAD_RUNTIME_VERSION` reste à 1, et aucune migration n'est écrite.
- **Empreinte de configuration:** 0 clé ajoutée à `settings.toml`, 0 drapeau CLI, 0 variable `PYXIS_*` de comportement ; `docs/config-catalog.md` reste à quinze clés.
- **Empreinte de dépendances:** 0 dépendance ajoutée pour EP-046 à EP-050 hors US-162 ; au plus 1 si US-162 est prise, avec son commentaire argumenté.
- **Surface d'outils:** exactement 3 outils enregistrés ajoutés, portant le catalogue de 30 à 33.
- **Coût de réveil:** un fil sans rappel actif ne se réveille jamais sur horloge ; un fil avec au moins un rappel se réveille au plus une fois par `MAX_TIMER_SEGMENT`, soit 60 fois par heure, chaque réveil étant une lecture d'horloge et une décision pure.
- **Latence de délivrance:** un rappel est délivré au plus `MAX_TIMER_SEGMENT` après son échéance dans le pire cas d'un saut d'horloge, et au plus 100 ms dans le cas nominal sur une machine non suspendue.
- **Coût en requêtes modèle:** au plus 3 tours ouverts sans entrée humaine intercalée, tous producteurs confondus.
- **Coût en contexte:** le cadre d'un rappel délivré tient en au plus 5 lignes plus le texte, et son coût en jetons est chiffré dans `## Model Experience`.
- **Temps mur ajouté à `just test`:** ≤ 3 s sur cache chaud ; aucun test de ce lot n'attend une durée réelle supérieure à 50 ms, l'horloge étant injectée.
- **Sécurité:** un rappel d'un fil est invisible et inadressable d'un autre fil ; le texte d'un rappel est borné, neutralisé et encadré comme non fiable avant de réentrer chez le modèle ; `internal_error` ne divulgue aucune exception.
- **Fiabilité:** 0 rappel délivré deux fois ; 0 rappel perdu par un refus de budget ; 0 fil rendu inouvrable par un enregistrement illisible ; 0 tour ouvert par `resume.rs`.

## Edge Cases & Error States

| # | Scénario | Déclencheur | Comportement attendu | Message |
|---|----------|-------------|----------------------|---------|
| 1 | Deux sélecteurs fournis | Le modèle envoie `after` et `at` | Refus, rien n'est écrit | `invalid_selector`, nommant les trois sélecteurs |
| 2 | Aucun sélecteur | Appel incomplet | Refus, rien n'est écrit | `invalid_selector` |
| 3 | Texte de rappel vide | Argument manquant | Refus | `invalid_prompt` |
| 4 | Texte au-delà de la borne | Le modèle colle un document | Refus nommant la borne | `invalid_prompt`, avec `MAX_SCHEDULE_PROMPT_CHARS` |
| 5 | Échéance dans le passé | Décalage de fuseau mal calculé | Refus | `not_future` |
| 6 | Intervalle sous cinq minutes | `every: 60` | Refus nommant la borne | `frequency_too_high` |
| 7 | Fuseau inconnu | `Europe/Paris_2` | Refus nommant le fuseau reçu | `invalid_time_zone` |
| 8 | Heure locale dans un trou d'heure d'été | 2 h 30 le jour du passage | Politique écrite au doc-comment et testée | selon la politique de US-162 |
| 9 | Heure locale dans un repli | 2 h 30 le jour du retour | Première occurrence retenue | sans objet |
| 10 | Échéance au-delà de la représentation | Année 99999 | Refus | `time_out_of_range` |
| 11 | Registre de rappels plein | Dix-septième rappel | Refus nommant la borne et l'action de libération | nomme `MAX_ACTIVE_SCHEDULES` |
| 12 | Échec d'écriture à la création | Disque plein, magasin fermé | Refus, aucun bras armé | « rappel non enregistré : `<cause>` » |
| 13 | Échec d'écriture au dispatch | Disque plein en cours de session | Rien n'est soumis, le rappel reste dû, trace `warn` | sans objet |
| 14 | Arrêt entre l'écriture du dispatch et la soumission | Crash au pire moment | Rejoué à la réouverture, dédupliqué par la clé de message | sans objet |
| 15 | Budget de réveils épuisé | Quatre échéances d'affilée sur un fil oisif | Aucun tour, rappel conservé en `overdue`, notice le dit | « `<n>` rappels en attente d'une reprise de la conversation » |
| 16 | Échéance pendant un tour en cours | Rappel au milieu d'une chaîne d'outils | Entrée par pilotage au prochain point sûr, jamais entre un appel d'outil et son résultat | sans objet |
| 17 | File d'entrées pleine | Seize entrées déjà en attente | Dispatch refusé, rappel conservé en `overdue` | sans objet |
| 18 | Récurrent en retard d'une journée | Fil fermé vingt-quatre heures | Une seule occurrence délivrée, échéance suivante dans le futur | sans objet |
| 19 | Horloge murale reculée | Correction NTP, changement manuel | Aucun dispatch, réarmement ; le réveil ne déclenche jamais seul | sans objet |
| 20 | Horloge murale avancée | Veille de machine puis réveil | Détecté au segment suivant, délivré au plus `MAX_TIMER_SEGMENT` après | sans objet |
| 21 | Suppression d'un identifiant inconnu | Réessai après réponse perdue | **Succès** portant le drapeau, jamais une erreur | `schedule_not_found` |
| 22 | Suppression d'un rappel d'un autre fil | Identifiant deviné | Même succès `schedule_not_found` ; l'existence n'est pas divulguée | `schedule_not_found` |
| 23 | Suppression pendant l'écriture d'un dispatch | Course serrée | L'ordre du journal tranche ; écrivain unique, aucun état incohérent plié | sans objet |
| 24 | Enregistrement illisible dans le journal | Édition manuelle, corruption partielle | Compté, ignoré, rapporté ; le fil s'ouvre et les autres rappels marchent | `corrupt_schedule_log`, rendue **avec** la liste |
| 25 | Dispatch sans création correspondante | Journal tronqué | Ignoré comme illisible | sans objet |
| 26 | Journal antérieur au lot | Session écrite avant ce lot | Aucun rappel, aucune erreur, aucune migration | sans objet |
| 27 | Double reprise consécutive | `--resume` deux fois | Aucun dispatch réécrit | sans objet |
| 28 | Texte portant des séquences ANSI | Le modèle compose un texte coloré | Borné et neutralisé avant affichage et avant rendu au modèle | sans objet |
| 29 | Texte imitant l'en-tête du cadre | Tentative d'injection | Échappé ; ne peut pas se faire passer pour la structure | sans objet |
| 30 | Rappel sous `-p` | Pipeline sans humain | Aucun tour ouvert, `run_summary` reste dernière, le rappel reste dû | sans objet |
| 31 | `--ephemeral` avec un rappel | Pipeline sans fichier de session | Enregistrement sur le magasin en mémoire, ou refus nommé ; US-158 tranche | « rappels indisponibles en mode éphémère » |
| 32 | Absence de `/usr/share/zoneinfo` | Conteneur minimal | Erreur nommée pour `at` zoné, `after` continue de fonctionner | `invalid_time_zone` |
| 33 | Arrêt du fil pendant un dispatch | Ctrl+C au mauvais moment | Soit le dispatch est écrit et rejoué, soit il n'existe pas ; jamais une soumission sans entrée durable | sans objet |
| 34 | Deux récurrents dus simultanément | Deux rappels alignés | Une forme de cadre par lot, une seule soumission | sans objet |

## Risks & Mitigations

| # | Risque | Probabilité | Impact | Mitigation |
|---|--------|-------------|--------|------------|
| 1 | Un cinquième bras dans le `select!` change subtilement l'ordonnancement des quatre existants | Haute | Haut | US-155 le place après les quatre, conserve `biased;`, et prouve par un test d'ordonnancement qu'une fin de tour et une annulation gagnent toujours. C'est l'hypothèse HAUTE numéro un. |
| 2 | Le budget partagé rend fausse la garantie du lot 9 sans qu'aucun test existant ne casse | Haute | Haut | US-157 alterne les deux producteurs dans une même chaîne et compte les tours ; ADR-17 écrit la règle du budget unique pour qu'un troisième producteur ne la redécouvre pas. |
| 3 | ADR-16 est invoqué pour refuser la délivrance d'un rappel en retard, ou au contraire la reprise gagne un chemin d'exécution | Moyenne | Haut | US-154 écrit ADR-17, qui distingue un enregistrement écrit d'avance d'une exécution dont l'identité n'est pas prouvable, et US-153 place la délivrance dans l'acteur et non dans `resume.rs`. |
| 4 | Le temps réel entre dans les tests et les rend lents ou instables | Moyenne | Haut | Le domaine est pur et prend l'instant en paramètre ; l'acteur n'utilise que le `Clock` injecté. La NFR borne tout test de ce lot à 50 ms d'attente réelle. |
| 5 | La dépendance de temps civil s'installe pour un sélecteur et grossit ensuite | Moyenne | Moyen | US-162 est P1, isolée, et son premier critère est la décision écrite. Le signal de vérification du plan est tenu par `after` seul, donc rien ne bloque si elle est différée. |
| 6 | Un rappel récurrent devient une pompe à requêtes modèle | Moyenne | Haut | Trois protections composées : la borne de cinq minutes, la dernière occurrence seulement, et le budget de trois. US-157 teste leur composition sur le cas du retard d'une journée. |
| 7 | Le pli devient coûteux sur un fil très ancien | Basse | Moyen | US-149 exige un pli linéaire et une mesure sur mille entrées. La suppression retire l'enregistrement du pli plutôt que de le marquer. |
| 8 | Un enregistrement corrompu rend un fil inouvrable | Basse | Haut | US-149 rend le pli total et faillible en douceur, ce qui est l'écart 16 par rapport à dsh, dont le verrou `faulted` a le comportement opposé. |
| 9 | Le texte d'un rappel sert de vecteur d'injection en réentrant hors de sa conversation | Basse | Haut | US-161 reprend le cadre de dsh avec échappement, et teste un texte imitant l'en-tête. La section `## Model Experience` le rend relisible. |
| 10 | Le lot 10 casse la parité du contrat Codex | Basse | Haut | Aucune surface du fil Codex n'est touchée ; les trois variantes sont additives et `just parity` est une porte de qualité de chaque story. |

## Non-Goals

Frontières explicites de cette version.

- **Délivrer un rappel dans un autre fil que celui qui l'a créé.** dsh fixe `deliveryMode: 'session-local'` comme une valeur unique documentée « Fixed v1 delivery boundary: the original session must be live », et c'est la même frontière que la propriété par fil du lot 9. Un rappel inter-fils demanderait un routage, une autorisation et une définition de « le fil n'existe plus », c'est-à-dire un autre système. Le mode reste une valeur d'un type fermé, donc l'élargir plus tard sera l'ajout d'une variante.
- **Délivrer un rappel quand le processus est arrêté.** Le rappel est délivré à la réouverture du fil, pas pendant l'absence. Faire autrement signifierait un démon, un service de démarrage et une notification hors du terminal : le voisinage `at` et `systemd-run` fait exactement cela, et ne peut précisément pas rouvrir une conversation avec son contexte, ce qui est toute la valeur ici.
- **Rattraper les occurrences manquées d'un récurrent.** C'est la décision la plus délibérée du lot, et elle est reprise telle quelle de `resolveEveryOccurrence`. Un arriéré n'a aucune valeur pour un rappel et transformerait un fil rouvert en rafale.
- **Une expression de type `cron`.** Trois sélecteurs couvrent ce qu'un rappel de conversation demande. Une grammaire `cron` ajouterait un analyseur, un vocabulaire d'erreurs plus large et une classe entière de bugs de fuseau, sans requête actuelle.
- **Une validation du flux candidat avant écriture.** `invariant.ts` intercepte chaque événement avant qu'il n'entre au journal, ce qui rend la corruption impossible chez dsh. Pyxis n'a aucun crochet équivalent, en ajouter un serait un mécanisme général construit pour un cas unique, et le budget de complexité l'interdit. Le pli refuse en lecture ce que dsh refuse en écriture, avec le bénéfice supplémentaire qu'un journal déjà corrompu reste ouvrable.
- **Un verrou de défaillance sur journal corrompu.** dsh latche `faulted` et cesse de conduire. Pour Pyxis, un enregistrement de planification illisible ne doit pas coûter le fil : compter, rapporter, continuer.
- **Une chaîne de transactions par fil.** `transaction.ts` sérialise les opérations parce que Cordis ne le fait pas. `ThreadActor` est déjà l'écrivain unique de son journal ; construire la sérialisation qui existe déjà serait de la dette pure.
- **Une méthode d'app-server pour lister ou créer des rappels.** Un rappel produit une entrée ordinaire, donc un tour ordinaire, donc les événements que le protocole publie déjà. `docs/CURRENT_STATUS.md` pose que les surfaces hors du cycle fil/tour/item sont des non-buts.
- **Faire de `Sleep` un rappel.** L'outil garde son rôle, une pause dans le tour courant bornée à douze heures. Deux mécanismes se chevauchent seulement en apparence : l'un tient le tour, l'autre en ouvre un nouveau. Les fusionner retirerait au modèle une distinction qu'il doit faire explicitement.

## Files NOT to Modify

- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés et empreintés, jamais édités à la main. Les deux `BASELINE_COMMIT` ne bougent pas : ce lot ne déplace aucune ligne de base.
- `docs/crate-graph.md`, `docs/tool-catalog.md`, `docs/config-catalog.md` : rendus depuis le code. Les trois lignes d'outils naissent de leurs `.register(`, puis le document se régénère.
- `crates/agent-core/src/agent.rs` : `run_agent` est le seul moteur modèle-outils. Ce lot n'en ouvre pas un second.
- `crates/agent-core/src/clock.rs` : le trait garde `now_ms` et `sleep`. Ajouter une forme d'échéance serait élargir un trait de `agent-core` pour un besoin d'`agent-runtime` ; le segment borné de US-155 se compose depuis `sleep` seul. Si la story démontre le contraire, elle l'écrit avant de toucher au fichier.
- `crates/agent-runtime/src/inputs.rs` : `TurnInputs` reste une file non bornée, et la borne de seize que la ligne 26 du plan lui attribue vit en réalité dans `crates/agent-runtime/src/thread.rs:50`, appliquée par l'appelant à `:1142` et `:1194`. Ce lot n'écrit rien dans ce fichier, et ne corrige pas le plan, qui est un artefact daté d'intention.
- `docs/deepseek-harness-porting-plan.md` : artefact daté. Le défaut de sa ligne 26 est consigné dans ce PRD et, si la relecture le justifie, dans une note de l'arbre de décisions, sur le patron de celle du lot 9.
- `docs/DECISIONS.md`, ADR-16 : ADR-17 précise sa portée sans le modifier.
- Le clone Codex résolu par `$PYXIS_CODEX_BASELINE` : lecture seule, sans exception. Aucun `commit`, `checkout` ni `fetch`.
- `spikes/` : espace jetable exclu de la Phase 0.
- `.github/workflows/ci.yml` et `justfile` : aucune recette n'est ajoutée par ce lot, donc aucune étape ne l'est ; toute modification déclencherait la porte d'inventaire de recettes.

## Technical Considerations

Formulé comme des questions pour l'ingénierie, non comme des mandats.

- **Découpe du module:** un seul `schedule.rs` portant le vocabulaire, le pli, la décision et la récurrence, ou une découpe en deux modules imitant `domain.ts` et `runtime.ts` ? Recommandé : un seul module pur, la moitié effectue vivant dans `thread.rs`. C'est le patron du lot 9, où `jobs.rs` tient la comptabilité et `thread.rs` le câblage, et il a tenu sur 2 123 lignes.
- **Dépendance de temps civil:** `jiff`, `chrono` avec `chrono-tz`, ou aucune ? Recommandé : `jiff`, en `default-features = false` avec lecture du tzdb système. Trois raisons. D'abord l'argument du dépôt s'inverse : `crates/agent-tools/src/time.rs:193-195` justifie l'algorithme manuel parce qu'il s'agit « of one format string », et une base de fuseaux plus une politique de trou et de repli n'est pas une chaîne de format. Ensuite Pyxis est Linux seulement, donc `/usr/share/zoneinfo` est lisible et rien n'est embarqué, là où `chrono-tz` compile la base dans le binaire. Enfin `jiff` porte une politique de trou et de repli explicite et documentée, quand une implémentation manuelle la rendrait implicite, ce qui est le mode de défaillance le plus coûteux ici : une erreur d'une heure, deux fois par an, qu'aucun utilisateur ne peut diagnostiquer. Compromis : une dépendance de plus, dans un dépôt qui en a peu et qui argumente chacune.
- **Valeur de `MAX_TIMER_SEGMENT`:** soixante secondes, ou plus long ? Recommandé : soixante. Ce que le segment achète est la borne de retard après une veille système, puisque `CLOCK_MONOTONIC` n'avance pas pendant celle-ci. Un réveil par minute et par fil armé est une lecture d'horloge et une décision pure, donc négligeable ; une heure de segment donnerait une heure de retard possible.
- **Emplacement du bras de minuterie:** un cinquième bras du `select!` de l'acteur, ou une tâche séparée qui envoie une commande à la boîte aux lettres ? Recommandé : le bras. Une tâche séparée serait un nœud de l'arbre d'annulation à tenir, alors que le bras est annulé avec l'acteur par construction, ce que l'invariant 13 demande. Compromis : le corps de la boucle grandit, et l'échéance doit être copiée hors de `self` comme `straggler_deadline` l'est déjà.
- **Identifiant de rappel:** opaque comme `AgentId` et `JobId`, ou un entier lisible ? Recommandé : opaque, cohérent avec le reste du runtime, et le modèle l'obtient de `schedule_list` sans avoir à le retenir.
- **Forme rendue par les outils:** texte, comme `list_jobs`, ou un schéma de sortie comme les `oneOf` de dsh ? Recommandé : texte. Aucun outil de Pyxis ne porte de schéma de sortie ; en introduire un pour trois outils serait un nouveau patron. Le vocabulaire d'erreurs reste fermé côté Rust et se rend en texte stable, donc le modèle voit les mêmes codes nommés.
- **Politique de trou d'heure d'été:** suivre le défaut de la bibliothèque, l'instant après le trou, ou refuser comme dsh ? À trancher à US-162. Refuser est plus honnête et donne une erreur que l'utilisateur comprend ; suivre le défaut est plus simple et ne perd jamais un rappel.
- **Migration:** aucune. Les entrées sont additives, les deux versions restent à 1, et un journal antérieur se rouvre sans rappel. Retour arrière : retirer les trois outils et cesser d'armer le bras laisse des entrées qu'un lecteur ignore.

## Success Metrics

| Metric | Baseline (actuel) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|--------------|
| Mécanismes permettant de revenir dans un tour ultérieur | 0 | 1 | Month-1 | présence du module `schedule.rs` et de son test de bout en bout |
| Outils enregistrés | 30 | 33 | Month-1 | ligne de synthèse de `docs/tool-catalog.md` régénéré |
| Variantes de `ThreadEventPayload` | 12 | 15 | Month-1 | lecture de `crates/agent-runtime/src/event.rs` |
| Rappels délivrés deux fois | non applicable | 0 | Month-1 | test de coupure de US-156, dans les deux sens |
| Rappels perdus par un refus de budget | non applicable | 0 | Month-1 | assertion de US-157 |
| Occurrences délivrées pour un récurrent en retard de 24 h | non applicable | 1 | Month-1 | test nommé de US-151 |
| Tours ouverts par `resume.rs` | 0 | 0 | Month-6 | assertion de US-153 |
| Tours ouverts sans entrée humaine intercalée | non applicable | ≤ 3 | Month-1 | test de chaîne mixte de US-157 |
| Fils inouvrables après corruption d'un enregistrement | non applicable | 0 | Month-1 | test de pli faillible en douceur de US-149 |
| Réveils sur horloge d'un fil sans rappel | 0 | 0 | Month-6 | test d'armement de US-155 |
| Clés de configuration | 15 | 15 | Month-6 | `docs/config-catalog.md` |
| Dépendances du workspace | inchangé | +0, ou +1 avec US-162 | Month-6 | `git diff` du `Cargo.toml` racine |
| `SESSION_SCHEMA_VERSION` | 1 | 1 | Month-6 | lecture de la constante |
| Affirmation périmée du compte d'outils dans `AGENTS.md` | 1 | 0 | Month-1 | relecture de US-164 |
| Temps mur ajouté à `just test` | 0 s | ≤ 3 s | Month-1 | suite de planification chronométrée sur cache chaud |

## Open Questions

- `jiff` doit-il entrer au workspace, ou `at` doit-il rester un instant UTC jusqu'à ce qu'un usage réel force la question ? Arthur tranche avant US-162, dont c'est le premier critère d'acceptation. Le reste du lot est utile sans elle, et le signal de vérification du plan est tenu par `after` seul. Une réponse tardive ne coûte rien ; une réponse implicite coûterait une dépendance non argumentée.
- La politique de trou d'heure d'été doit-elle refuser, comme dsh, ou suivre le défaut de la bibliothèque ? À trancher dans la même story. Refuser produit une erreur que l'utilisateur comprend ; suivre le défaut ne perd jamais un rappel. Les deux sont défendables et le doc-comment doit dire laquelle a été choisie et pourquoi.
- `--ephemeral` doit-il accepter les rappels sur le magasin en mémoire, ou les refuser ? Question ouverte à US-158, exactement comme elle l'était à US-144 pour les travaux de fond ; la réponse devrait être la même pour les deux, et si elle diverge la story doit dire pourquoi.
- La borne de seize rappels actifs est-elle la bonne, ou faut-il l'aligner sur `MAX_ACTIVE_JOBS = 4` ? À constater à US-148. Un rappel est un enregistrement et non un processus, donc son coût n'est pas celui d'un travail de fond, ce qui argumente une borne plus haute ; mais deux bornes proches et différentes sont une charge de lecture.
- Le défaut de la ligne 26 du plan mérite-t-il une note de l'arbre de décisions, comme celui de la ligne 9 en a reçu une ? À trancher à US-164. Le critère d'`AGENTS.md` s'applique : rien dans `crates/` ne peut contredire « la ligne 26 nomme le mauvais fichier », donc si elle est écrite ce sera une note et jamais un ADR.
[/PRD]
