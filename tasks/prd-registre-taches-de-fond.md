[PRD]
# PRD: Registre de tâches de fond

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-22 | Arthur Jean | Rédaction initiale, lot #9 du plan de portage DeepSeek Harness |
| 1.1 | 2026-08-22 | Arthur Jean | Contexte concurrentiel étendu à Gemini CLI, coût du sondage chiffré, sink fichier appuyé sur `pipe(7)` |
| 1.2 | 2026-08-22 | Arthur Jean | Racines dsh et Pyxis déclarées avant la première citation ; les seize ancres dsh relues sur le disque |

## Problem Statement

Pyxis laisse déjà des processus tourner après la fin d'un tour, et rien dans le dépôt ne les tient. Six défaillances mesurées sur l'état de l'arbre au 2026-08-22 :

1. **Rien ne nomme au modèle ce qu'il a laissé tourner.** `docs/tool-catalog.md:6` compte les vingt-neuf outils qu'une session expose ; aucun ne répond à « qu'est-ce qui tourne pour moi en ce moment ». La donnée existe pourtant : `ExecSessions::open_sessions()` (`crates/agent-tools/src/exec_session.rs:366`) rend la liste des sessions ouvertes avec leur commande, et son unique appelant est `left_running_notice` (`crates/agent-cli/src/interactive/mod.rs:1387`), qui écrit sur l'écran de l'humain. Le modèle, lui, doit se souvenir de ses `session_id` d'un tour à l'autre, à travers une compaction éventuelle. Un identifiant oublié est une session que plus personne ne peut adresser jusqu'à `shutdown()`.

2. **Le résultat d'un travail long se perd dès que le modèle cesse de le relever.** Le commentaire de `MIN_POLL_YIELD` (`crates/agent-tools/src/exec_session.rs:56-70`) consigne l'observation qui a fixé la constante : « Observed on `cargo test --workspace`: seven polls at 1000 ms, six of them returning zero bytes, then the model gave up and left the session running ». Le plancher de 5 s corrige le coût du sondage ; il ne corrige pas l'abandon. Il n'existe aucune poussée : la seule façon dont un résultat revient est que le modèle pense à redemander, et un modèle qui n'y pense pas ne perd pas seulement du temps, il perd le résultat. Le sondage lui-même n'est pas gratuit : chaque relève est un tour d'API complet, historique compris, ce que Codex [#13733](https://github.com/openai/codex/issues/13733) chiffre et ce que [#29865](https://github.com/openai/codex/issues/29865) demande de remplacer par un réveil sur sortie, sans que cela soit implémenté. Le plancher de sondage arbitre donc entre deux coûts et n'en supprime aucun.

3. **Deux mécanismes de fond coexistent sans vocabulaire commun.** `AgentSupervisor` (`crates/agent-runtime/src/supervisor.rs`) tient les enfants avec un état fermé, un journal durable, des bornes et une livraison exactement-une-fois. `ExecSessions` (`crates/agent-tools/src/exec_session.rs`) tient les terminaux avec un cap de quatre, aucun état durable et aucune livraison. La question « qu'est-ce qui tourne » n'a pas une réponse, elle en a deux, dont une seule est adressable et journalisée. Le plan appelle cela « un enfant d'une autre nature, pas un nouveau sous-système » ; le dépôt a aujourd'hui les deux natures et aucun genre commun.

4. **Le porteur des terminaux est par exécution, celui des enfants est par fil.** `crates/agent-cli/src/main.rs:1755` construit un unique `ExecSessions::new()` pour tout le processus ; `crates/agent-cli/src/runtime.rs:727-730` construit un `AgentSupervisor` par fil, sous le commentaire « One supervisor per thread ». L'écart est sans conséquence observable aujourd'hui, et seulement parce que `crates/agent-app-server/src/server.rs:289` publie `max_open_threads: 1`. Le jour où cette capacité bouge, deux fils se partagent quatre emplacements et se voient mutuellement les sessions. La barrière de propriété de dsh (`assertAccess`, `packages/jobs/jobs-local/src/index.ts:356`) n'est donc pas superflue pour Pyxis : elle est absente.

5. **Un redémarrage ne trouve rien et surtout n'en dit rien.** `Drop for Session` (`crates/agent-tools/src/exec_session.rs:256`) tue le groupe de processus et `shutdown()` (`:381`) le fait pour tous : un arrêt propre ne laisse rien. Un arrêt brutal laisse des processus dont le journal du fil ne porte aucune trace, puisque rien n'y est écrit. Le fil rouvert reconstruit son graphe d'agents, `AgentGraph::restore` (`crates/agent-runtime/src/agent.rs:512-525`) passant tout enregistrement actif en `AgentState::Interrupted` avec `RESTART_CAUSE`, et ne reconstruit rien pour les terminaux, parce qu'il n'y a rien à reconstruire. Le `bash` de trois heures que le modèle avait lancé disparaît sans laisser de phrase.

6. **La troisième clause du signal de vérification n'a pas de source, et contredit un critère livré.** Le plan demande qu'« un redémarrage le retrouve ». Le fichier que le plan cite comme source dit le contraire dès sa première ligne : `packages/jobs/jobs-local/src/index.ts:1-10` énonce « Process-local provider for the background-job capability seam (`ctx.jobs`). It keeps every record in memory and hands out fresh snapshots, never live state ». dsh ne fait survivre aucun travail à un redémarrage, et son producteur `bash` (`packages/shell/tool-bash/src/index.ts:349-380`) engendre un enfant ordinaire. Sous la règle du plan, où ce qui se reprend sont les décisions de conception, cette clause n'a rien à reprendre. Elle contredit en outre `AgentSupervisor::restore` (`crates/agent-runtime/src/supervisor.rs:344-367`), qui marque `delivered: true` sur tout relais restauré, avec son commentaire : « A restored handoff is already history: marking it delivered is what keeps a replayed terminal from being injected twice (US-015 AC6) ». Le `reported` de dsh et le `Pending.delivered` de Pyxis sont structurellement le même drapeau, avec des sémantiques de redémarrage **opposées par décision**. La clause se renégocie, elle ne se comble pas.

Trois défauts documentaires accompagnent l'ensemble et se corrigent dans ce lot. `docs/ARCHITECTURE.md:596-626` (§8) est périmé sur trois points : il annonce « cinq outils » quand le catalogue en compte six pour le multi-agent, il nomme `send_agent` quand l'outil enregistré est `send_message`, et il affirme « Le binaire, lui, ne les enregistre pas encore et démarre son thread sans superviseur » quand `crates/agent-cli/src/main.rs:1757-1780` construit le câblage sans condition. Le doc-comment d'`open_sessions()` (`crates/agent-tools/src/exec_session.rs:359-365`) invoque un « idle watchdog » qui « reaps it five minutes later », que la décision portée par `MAX_SESSIONS` (`:47-52`) refuse explicitement : « A session is NOT closed for being quiet ». Enfin la colonne « Source dsh à lire » de la ligne 25 du plan omet `packages/shell/tool-bash/src/index.ts` et `packages/shell/tool-bash/src/background.ts`, où vit la moitié producteur de la conception.

**Why now:** les deux prérequis nommés par le plan sont livrés. Le lot 4 a apporté le déversement (`ADR-15`, `NEVER_SPILLED`, `MAX_SPILL_ROOT_BYTES`), sans lequel chaque relève d'un travail volumineux empoisonnerait le contexte ; le lot 5 a apporté les catalogues générés, donc la porte qui rendra un nouvel outil visible sans qu'on l'écrive à la main. Le coût de l'attente est mesurable ailleurs : sept issues ouvertes de Claude Code décrivent un seul mode de défaillance, le rappel de complétion ré-émis indéfiniment parce que rien n'enregistre qu'il a déjà été rendu, dont [#11190](https://github.com/anthropics/claude-code/issues/11190) (« two background jobs finished early but generated reminders for 2+ hours across 50+ responses, wasting thousands of context tokens »). C'est très exactement le champ que dsh appelle `reported`. Construire le registre sans ce drapeau, ou après coup, coûte la même dette.

## Overview

**Racines et convention de chemins.** Tout chemin en `crates/`, `docs/` ou `tasks/` est relatif à `/home/arthur/dev/pyxis`. Tout chemin en `packages/` est relatif à `/home/arthur/dev/deepseek-harness`, le dépôt DeepSeek Harness, cité en **lecture seule** : il est en TypeScript sur Cordis, Pyxis est en Rust, et rien ne s'y copie. Ce qui se reprend sont les décisions de conception, chacune ancrée sur la ligne qui la porte pour qu'elle se relise. Le champ `**Source dsh:**` de chaque story nomme la ligne à ouvrir avant de l'implémenter, et dit « aucune » quand la story n'a pas de source, ce qui est en soi une information.

Le lot fait entrer dans `agent-runtime` un registre de travaux de fond : un vocabulaire fermé, une comptabilité bornée, propriété d'un fil, et trois entrées durables dans le journal de ce fil. Le registre ne lance rien lui-même. Il enregistre ce que le dépôt fait déjà tourner, en commençant par les sessions de terminal d'`ExecSessions`, et il devient la seule source de vérité sur ce qui est en cours : `open_sessions()` en redevient une projection.

Quatre décisions de conception se reprennent de DeepSeek Harness, et aucune ligne ne se copie, la source étant du TypeScript sur Cordis. La première est le vocabulaire fermé : `packages/jobs/jobs/src/types.ts:17` ferme `JobStatus` à cinq valeurs, dont `stopping`, un état intermédiaire que Pyxis n'a nulle part et qui est précisément ce qui distingue « j'ai demandé l'arrêt » de « c'est arrêté ». La deuxième est le drapeau `reported` (`types.ts:127`), qui distingue un travail terminé d'un travail dont le résultat a atteint le modèle ; c'est le champ qui empêche à la fois la double livraison et la livraison jamais faite. La troisième est l'ordre du règlement : `packages/jobs/jobs-local/src/index.ts:416-440` marque, prend l'instantané, libère les attentes, puis annonce la complétion **en dernier**, « because a reporter may open a model turn synchronously » (`packages/jobs/jobs/src/index.ts:41-60`), et `cancelForTeardown` (`:507-517`) met `reported = true` **avant** d'annuler pour qu'une annulation qui lève ne puisse pas annoncer une complétion non rapportée dans un propriétaire en cours de démontage. La quatrième est la barrière de propriété : les identifiants sont prédictibles, donc « authorization, not secrecy, is the boundary ».

Trois divergences volontaires viennent des invariants de Pyxis. Le registre ne fait **pas** survivre un travail à un redémarrage : il le rapporte. Un fil rouvert ferme tout travail encore actif en `interrupted` avec sa cause durable, comme `AgentGraph::restore` le fait déjà pour les enfants, et il le dit à l'humain et au modèle. C'est l'objet d'ADR-16, parce qu'une pull request sur les crates peut violer cette frontière. Ensuite le budget de réveils est une constante de crate et jamais une clé de configuration, ce qu'ADR-12 et l'invariant 15 imposent, là où dsh en fait une option de déploiement (`packages/jobs/tool-jobs/src/index.ts:49-52`). Enfin le genre `subagent` de dsh n'entre pas : `AgentSupervisor` tient déjà pour ses enfants un état durable, des bornes et une livraison exactement-une-fois, et les replier dans un registre générique serait un refactor sans gain observable. `JobKind` porte donc une variante, et en ajouter une reste l'ajout d'une variante.

Le périmètre de production est étroit et nommé. Un module de registre dans `agent-runtime`, un trait de lanceur qu'il définit et que `agent-cli` implémente sur le patron de `AgentSpawner` et `SubAgentSpawner`, trois variantes additives de `ThreadEventPayload`, un outil `list_jobs` en lecture seule, et le câblage qui fait qu'`exec_command` réserve son emplacement puis enregistre son travail dans cet ordre. Le reste est du test, du document et une décision.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Travaux de fond qu'un modèle peut lister sans se souvenir d'un identifiant | 100 % | 100 % |
| Complétions livrées au modèle exactement une fois | 100 % | 100 % |
| Complétions livrées deux fois ou jamais | 0 | 0 |
| Travaux actifs restés `running` après une reprise de fil | 0 | 0 |
| Reprises nommant ce qu'elles ont interrompu, avec la cause | 100 % | 100 % |
| Réveils consécutifs sans entrée humaine intercalée | ≤ 3 | ≤ 3 |
| Clés de configuration utilisateur ajoutées | 0 | 0 |
| Bumps de `SESSION_SCHEMA_VERSION` | 0 | 0 |
| Dépendances ajoutées au workspace | 0 | 0 |
| Outils enregistrés ajoutés | 1 | 1 |
| Affirmations périmées de `docs/ARCHITECTURE.md` §8 | 0/3 | 0 |

## Target Users

### Le modèle conduisant un travail long
- **Role:** l'agent qui lance `cargo test --workspace`, un `docker compose up`, un serveur de développement ou une migration, puis continue à travailler pendant que cela tourne.
- **Behaviors:** ouvre une session avec `exec_command`, sonde avec `write_stdin`, mène d'autres appels d'outils entre deux sondages, et finit le tour.
- **Pain points:** il doit retenir un `session_id` numérique à travers les tours et une compaction éventuelle. S'il l'oublie, la session est inatteignable. S'il cesse de sonder, le résultat n'arrive jamais : rien ne le lui pousse. Le plancher de sondage de 5 s l'oblige par ailleurs à choisir entre attendre et travailler.
- **Current workaround:** sonder en boucle, ce que `crates/agent-tools/src/exec_session.rs:669-674` a dû exempter du garde-fou de boucle pour rendre supportable, ou abandonner, ce que le commentaire de `MIN_POLL_YIELD` documente comme observé.
- **Success looks like:** un appel qui liste ce qui tourne avec son état et son âge, et une complétion qui revient d'elle-même quand il ne l'a pas relevée.

### L'humain qui reprend une session interrompue
- **Role:** Arthur rouvrant un fil après une fermeture de terminal, un crash ou une mise à jour du binaire.
- **Behaviors:** `pyxis --resume`, lit le transcript reconstruit, reprend là où il était.
- **Pain points:** ce qui tournait a disparu sans phrase. Le journal du fil porte les tours, les états et les enfants, et rien sur le terminal de trois heures. Il ne peut pas savoir si le travail a fini, s'il a été tué, ni s'il tourne encore quelque part.
- **Current workaround:** `ps`, et de la mémoire.
- **Success looks like:** la reprise dit, en une ligne par travail, ce qu'elle a trouvé actif, avec sa commande et la cause de son interruption ; et le modèle le sait aussi, sans qu'on le lui raconte.

### L'agent de codage ajoutant un producteur de fond
- **Role:** Claude Code ou Codex recevant une tâche qui ajoute un outil capable de laisser quelque chose tourner.
- **Behaviors:** lit `AGENTS.md`, suit la table « Where new behavior goes », écrit le module, lance `just check`.
- **Pain points:** la table ne dit pas où déclarer un travail de fond, et le graphe de crates rend l'endroit non évident : `agent-tools` dépend d'`agent-runtime` et jamais l'inverse (`crates/agent-tools/Cargo.toml:22-26`). Rien ne l'empêche de tenir sa propre comptabilité dans son module, ce qui donnerait une troisième nature.
- **Current workaround:** aucun ; il copie ce que fait `ExecSessions`.
- **Success looks like:** une ligne de la table `AGENTS.md`, un trait de lanceur à implémenter, et une porte qui devient rouge s'il enregistre un travail sans le rendre durable.

## Research Findings

### Contexte concurrentiel

Claude Code expose `Bash(run_in_background)`, `BashOutput` et `KillShell`, plus `/bashes` côté humain. Sept issues publiques décrivent un unique mode de défaillance, et c'est celui que ce lot ferme : le rappel de complétion est ré-émis à chaque réponse parce que rien n'enregistre qu'il a déjà été rendu. [#11190](https://github.com/anthropics/claude-code/issues/11190) le mesure (« reminders for 2+ hours across 50+ responses, wasting thousands of context tokens ») ; [#12302](https://github.com/anthropics/claude-code/issues/12302) le montre persistant après la relève de la sortie ; [#13249](https://github.com/anthropics/claude-code/issues/13249) après un `KillShell` ; [#13091](https://github.com/anthropics/claude-code/issues/13091) et [#14049](https://github.com/anthropics/claude-code/issues/14049) montrent un statut `running` périmé pour un processus terminé ; [#11716](https://github.com/anthropics/claude-code/issues/11716) reprend le coût en contexte.

Claude Code prend en outre une décision que ce lot ne reprend pas : un dépassement de délai au premier plan bascule la commande en fond automatiquement, sauf `sleep`, sauf toute commande contenant `git`, sauf les composées non analysables, et l'ensemble se désactive par `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` ([référence des outils](https://code.claude.com/docs/en/tools-reference)). La même référence dit que les commandes de la conversation principale survivent au tour, et que le mode `-p` les termine peu après le résultat final.

Codex CLI route ses commandes par `unified_exec`, une session PTY par défaut sur Linux et macOS ([référence des commandes](https://developers.openai.com/codex/cli/reference)). `/ps` liste les terminaux de fond, `/stop` les arrête **tous**, et [#17821](https://github.com/openai/codex/issues/17821) demande encore un `/stop <id>` pour n'en viser qu'un ; [#13858](https://github.com/openai/codex/issues/13858) demande un moyen de voir le contenu d'un terminal de fond. Côté protocole, la ligne de base porte `BackgroundTerminalInfo { item_id, process_id, command, cwd }` (`codex-rs/core/src/codex_thread.rs:162-167`) et `list_background_terminals()` (`:471`), consommé par `app-server/src/request_processors/thread_processor.rs:2319` : c'est une API **client**, jamais un outil que le modèle appelle, ce qu'une recherche de la chaîne dans le catalogue d'outils du clone confirme. Codex borne la sortie par relève avec `max_output_tokens` plutôt que par déversement, et son identifiant de session est un `i32` tiré au hasard, là où dsh assume des identifiants prédictibles gardés par l'autorisation.

Gemini CLI n'a pas de registre du tout : `run_shell_command` accepte un `&` final et rend un champ « Background PIDs » ([documentation du shell](https://google-gemini.github.io/gemini-cli/docs/tools/shell.html)), sans relecture ni terminaison exposées au modèle, et [#13594](https://github.com/google-gemini/gemini-cli/issues/13594) montre le mode de défaillance qui en découle : un script de fond lancé au premier plan bloque le CLI. C'est la borne basse de ce qu'un registre évite. Cursor n'entre pas dans la comparaison : ses agents de fond sont des machines distantes, pas des travaux shell locaux.

Aucun de ces systèmes ne fait survivre un travail à un redémarrage du processus agent. Claude Code [#65925](https://github.com/anthropics/claude-code/issues/65925) rapporte l'inverse du service attendu : des tâches de fond qui « persist as running after full process restart », c'est-à-dire des fantômes qu'une reprise ne réconcilie pas, et [#75037](https://github.com/anthropics/claude-code/issues/75037) rapporte des « lost background-task completion records ». Le seul patron qui survit réellement est externe au processus agent : une session `tmux` détachée pilotée par un service de démarrage. dsh non plus ne survit pas, et le dit dans le doc-comment de son fournisseur.

### Bonnes pratiques reprises

Un état fermé plutôt qu'un booléen, avec un état intermédiaire explicite pour la demande d'arrêt, ce qui rend distinguables « j'ai demandé » et « c'est fait ». Un drapeau de rapport séparé de l'état terminal, seul moyen de rendre la livraison exactement-une-fois sans dépendre de l'ordre d'observation. Un règlement premier-arrivé, où une seconde issue n'écrase pas la première. Une annonce de complétion émise en dernier, après que l'enregistrement est commis, parce que l'écouteur peut ouvrir un tour modèle de façon synchrone. Une autorisation par propriétaire plutôt qu'un identifiant secret. Un budget de réveils consécutifs réarmé par une entrée d'origine humaine, sans quoi une série de travaux courts s'auto-alimente en tours.

### Correspondance dsh vers Pyxis

Racine du dépôt source : `/home/arthur/dev/deepseek-harness`.

| # | Décision | Source dsh | Reprise dans Pyxis | Écart |
|---|----------|-----------|--------------------|-------|
| 1 | État fermé à cinq valeurs, dont `stopping` | `packages/jobs/jobs/src/types.ts:17` | `JobStatus` en enum Rust, exhaustif au `match` | aucun ; le compilateur rend la fermeture vérifiable, ce que la fusion de déclarations TypeScript ne fait pas |
| 2 | Genre extensible en déclaration, fermé à l'usage | `types.ts:23` (`JobKindMap`) | `JobKind` à une variante, `Terminal` | dsh en a deux ; `subagent` reste chez `AgentSupervisor` (voir Non-Goals) |
| 3 | Instantané rendu par valeur, jamais l'état vivant | `packages/jobs/jobs-local/src/index.ts:1-10` | `JobSnapshot` `Clone`, rendu par copie | aucun |
| 4 | Drapeau `reported` distinct de l'état terminal | `types.ts:127` | champ du registre **et** entrée durable `JobReported` | dsh le garde en mémoire ; ici il est journalisé, sinon une reprise le perdrait |
| 5 | Règlement premier-arrivé | `jobs-local/src/index.ts:416` (`private settle`) | `settle` idempotent, seconde issue ignorée avec une trace | aucun |
| 6 | Complétion annoncée en dernier, après commit | `packages/jobs/jobs/src/index.ts:41-60`, `jobs-local:422-440` | l'événement durable est écrit avant toute notification | aucun ; c'est déjà l'invariant 11 de Pyxis |
| 7 | Démontage : `reported = true` avant l'annulation, faute de lecteur restant | `jobs-local/src/index.ts:507,517`, motif en `jobs/src/index.ts:44-46` | `shutdown()` marque avant de tuer | aucun |
| 8 | Autorisation par propriétaire, pas par secret | `jobs-local/src/index.ts:356` (`assertAccess`) | registre possédé par un fil ; un travail d'un autre fil est introuvable, pas refusé | Pyxis n'a pas de session propriétaire distincte du fil, donc la barrière est le fil lui-même |
| 9 | Plafond de travaux simultanés par propriétaire | `jobs-local/src/index.ts:28` (`DEFAULT_MAX_CONCURRENT_TASKS_PER_OWNER = 10`) | constante de crate, alignée sur `MAX_SESSIONS = 4` | dsh en fait un défaut configurable ; l'invariant 15 l'interdit ici |
| 10 | Budget de réveils réarmé par une entrée humaine | `packages/jobs/tool-jobs/src/index.ts:49-52,228,279-300` | constante de crate, réarmée par un `submit` d'origine humaine | même écart : constante, jamais clé |
| 11 | Livraison `quiet` contre `wakeup` selon l'oisiveté | `tool-jobs/src/index.ts:279-300` (`owner.followup` contre `owner.inject`) | `submit` quand le fil est oisif, `steer` quand un tour court | correspondance directe ; `Submission.client_message_id` (`crates/agent-runtime/src/thread.rs:56-60`) donne en plus une clé d'idempotence que dsh n'a pas |
| 12 | Sortie de fond : lecture consommante contre sortie finale idempotente | `packages/jobs/jobs/src/types.ts:131` (`JobRead`) | `write_stdin` garde son curseur consommant ; la relève finale est idempotente | aucun |
| 13 | Code de sortie non nul rapporté, pas échoué | `packages/shell/tool-bash/src/background.ts` | `completed` avec le code en détail ; `failed` réservé à l'échec du lancement | aucun ; identique au rendu au premier plan des deux côtés |
| 14 | Le registre est en mémoire et local au processus | `jobs-local/src/index.ts:1-10` | **écart assumé** : les enregistrements sont durables, les processus ne le sont pas | voir ADR-16 ; c'est la renégociation de la clause 3 |

## Assumptions & Constraints

### Assumptions (to validate)

- **HAUTE** : l'écriture de trois entrées durables par travail ne change pas le profil d'écriture du journal de fil de façon perceptible. Un travail écrit au plus une entrée d'enregistrement, une par transition d'état et une de rapport, contre une entrée par tour aujourd'hui. Validée par US-133.
- **MOYENNE** : faire d'`ExecSessions` une projection du registre ne change aucun comportement observé par les tests existants du terminal, y compris le test de parité `a_finished_command_reports_the_baseline_wire` (`docs/parity/offline-suite.md:31`). Validée par US-136.
- **MOYENNE** : un réveil ouvrant un tour est acceptable pour l'humain quand le fil est oisif et que le budget est de trois. Non réfutable par un test ; US-142 la borne et la rend désactivable par la constante mise à zéro, ce qui donne le comportement `quiet` de dsh.
- **BASSE** : aucune surface app-server n'a besoin de changer. `max_open_threads: 1` (`crates/agent-app-server/src/server.rs:289`) rend la question théorique en v1.

### Hard Constraints

- **Invariant 11** : un tour produit exactement un état terminal, persisté avant publication. Un travail suit la même règle : un règlement, écrit avant d'être annoncé.
- **Invariant 12** : une opération acceptée est durable avant d'être acquittée ; un `client_message_id` déjà accepté rend les identifiants d'origine et ne ré-exécute rien.
- **Invariant 13** : un seul arbre d'annulation. Un travail est un nœud ENFANT du fil ; l'annulation descend et ne remonte jamais, et aucun `JoinHandle::abort` côté client n'est permis.
- **Invariant 15 et ADR-12** : toute limite d'orchestration est une constante de crate. Zéro clé de configuration publique, zéro drapeau, zéro variable d'environnement.
- **Graphe de crates** : `agent-tools` dépend d'`agent-runtime`, jamais l'inverse (`crates/agent-tools/Cargo.toml:22-26`). Le registre ne peut donc pas vivre dans `agent-tools`.
- **Format durable** : les entrées sont additives et `SESSION_SCHEMA_VERSION` reste à 1, comme le doc-comment de `crates/agent-runtime/src/event.rs:8-12` l'exige déjà pour les entrées existantes.
- **`agent-core`** n'émet que des `AgentEvent` structurés, jamais d'ANSI ni de couleur ; toute nouveauté visible d'un client est une variante d'`AgentEvent`.
- **Le clone Codex** résolu par `$PYXIS_CODEX_BASELINE` est en lecture seule, sans exception.
- **Défauts fermés** : les défauts du trait `Tool` restent fermés ; la sortie d'un travail est non fiable par construction et la souillure se propage.

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

### EP-041: Le registre et son écriture durable

Le vocabulaire, la comptabilité bornée possédée par un fil, les trois entrées durables et le rattachement à l'arbre d'annulation. Ferme les défaillances 3 et 4.

**Definition of Done:** un test enregistre un travail, le règle, et un fil rouvert sur le même journal retrouve l'enregistrement, son état terminal et son drapeau de rapport, sans qu'aucun processus n'ait été relancé.

#### US-131: Le vocabulaire d'un travail de fond est fermé et vérifié par le compilateur
**Description:** As a agent de codage, I want un état, un genre et un instantané fermés dans un seul module so that un `match` non exhaustif refuse de compiler au lieu de laisser passer un état oublié.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Source dsh:** `packages/jobs/jobs/src/types.ts:17` pour `JobStatus`, `:23` pour `JobKindMap`, `:32` pour `JobOutcome`, `:97` pour `JobSnapshot`, `:127` pour `reported`

**Acceptance Criteria:**
- [ ] Un module `crates/agent-runtime/src/jobs.rs` porte `JobId`, `JobKind`, `JobStatus`, `JobRecord` et `JobSnapshot` ; aucun de ces types n'est marqué `#[non_exhaustive]`.
- [ ] `JobStatus` porte exactement `Running`, `Stopping`, `Completed`, `Killed`, `Failed` ; `Stopping` est documenté comme distinguant « l'arrêt est demandé » de « l'arrêt est fait ».
- [ ] `JobKind` porte exactement `Terminal` ; un doc-comment nomme le genre `Subagent` de dsh et dit pourquoi il n'entre pas (voir Non-Goals).
- [ ] `JobSnapshot` est `Clone` et rendu par valeur : aucune méthode publique du registre ne prête une référence sur l'état vivant.
- [ ] `reported` est un champ distinct de `status`, et son doc-comment énonce l'invariant : un travail terminé peut ne pas être rapporté, un travail rapporté est nécessairement terminé.
- [ ] Given un `JobSnapshot` sérialisé puis désérialisé, when il est comparé à l'original, then il est égal ; un test de tour complet le prouve.
- [ ] Given une nouvelle variante ajoutée à `JobStatus`, when le crate est compilé, then au moins un `match` du registre échoue à compiler ; le test est une note de la story, pas un `#[test]`.
- [ ] Aucune clé de configuration, aucun drapeau, aucune variable `PYXIS_*` n'est ajouté.

#### US-132: Le registre est borné, possédé par un fil, et règle au premier arrivé
**Description:** As a mainteneur, I want un registre dont la capacité est une constante et dont chaque travail appartient à un fil so that deux fils ne se voient pas mutuellement et qu'un règlement concurrent n'écrase pas le premier.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-131
**Source dsh:** `packages/jobs/jobs-local/src/index.ts:28` pour le plafond par propriétaire, `:356` pour `assertAccess`, `:416` pour `settle`

**Acceptance Criteria:**
- [ ] Le plafond de travaux actifs est une constante de crate, alignée sur `MAX_SESSIONS = 4` (`crates/agent-tools/src/exec_session.rs:47`), avec un doc-comment qui dit pourquoi les deux valeurs sont la même.
- [ ] Given un registre à plein, when un enregistrement est demandé, then il est refusé par une erreur nommant la borne et l'action de libération, jamais par une attente.
- [ ] Given un travail enregistré par un fil, when un autre fil liste ses travaux, then il ne le voit pas ; le test ouvre deux fils sur deux journaux et le prouve.
- [ ] Given deux règlements concurrents sur le même travail, when ils arrivent, then le premier gagne, le second est ignoré, et une trace `tracing` de niveau `debug` le consigne.
- [ ] Given un règlement sur un identifiant inconnu, when il arrive, then il rend une erreur nommée et n'insère rien.
- [ ] La capacité est réservée AVANT tout effet, sur le patron de `ExecSessions::reserve()` (`crates/agent-tools/src/exec_session.rs:398`) : un refus postérieur libère l'emplacement.
- [ ] Aucune clé de configuration n'est ajoutée ; `/status` peut lire la borne comme il lit déjà les autres.

#### US-133: Un enregistrement est durable avant d'être acquitté
**Description:** As a humain reprenant une session, I want que l'existence d'un travail, ses transitions et son rapport soient dans le journal du fil so that un redémarrage sache ce qui existait, sans qu'aucun format existant ne change.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-131
**Source dsh:** `packages/jobs/jobs/src/index.ts:41-60` pour « Completion is announced last, after the record is committed » ; aucune source pour la durabilité elle-même, dsh étant en mémoire (`jobs-local/src/index.ts:1-10`)

**Acceptance Criteria:**
- [ ] `ThreadEventPayload` (`crates/agent-runtime/src/event.rs:77`) gagne exactement trois variantes : `JobRegistered`, `JobStateChanged`, `JobReported`, calquées sur `AgentLinked`, `AgentStateChanged` et `AgentMessageDelivered`.
- [ ] `SESSION_SCHEMA_VERSION` reste à 1 ; un test lit un fichier de session écrit avant ce lot et le rouvre sans erreur.
- [ ] Given un lecteur v1 rencontrant l'une des trois entrées, when il la lit, then elle est mappée sur `SessionEntry::Unknown` et ignorée, comme le doc-comment du module l'exige déjà.
- [ ] `ThreadEvent::turn_id()` traite les trois nouvelles variantes explicitement ; le `match` reste exhaustif sans bras générique.
- [ ] Given un enregistrement accepté, when l'appelant reçoit son `JobId`, then l'entrée `JobRegistered` est déjà commise ; le test coupe l'écriture entre les deux et prouve que l'acquittement n'arrive pas.
- [ ] Given un travail réglé, when un observateur est notifié, then l'entrée `JobStateChanged` terminale est déjà commise, dans cet ordre, conformément à l'invariant 11.
- [ ] Given un échec d'écriture du journal, when il survient à l'enregistrement, then l'enregistrement est refusé avec une erreur nommée et rien n'est lancé.
- [ ] La commande d'écriture passe par les opérations existantes de `crates/agent-runtime/src/store.rs` ; aucune variante de `StoreOperation` n'est ajoutée, ou la story explique laquelle et pourquoi.

#### US-134: L'annulation descend du fil au travail et ne remonte jamais
**Description:** As a mainteneur, I want qu'un travail soit un nœud enfant de l'arbre d'annulation du fil so that une interruption atteigne ses processus sans qu'aucun client n'ait à connaître un handle.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-132
**Source dsh:** `packages/jobs/jobs/src/types.ts:46` (`JobStart`, dont le `cancel`) et `jobs-local/src/index.ts:507` (`cancelForTeardown`), pour la forme d'une annulation confinée

**Acceptance Criteria:**
- [ ] Chaque travail dérive son jeton d'annulation de celui du fil ; aucun `JoinHandle::abort` n'apparaît sur le chemin, conformément à l'invariant 13.
- [ ] Given une interruption du fil, when elle est émise, then chaque travail actif passe en `Stopping` puis en `Killed`, dans cet ordre, et les deux transitions sont durables.
- [ ] Given une annulation qui lève ou expire, when elle survient, then le travail est quand même réglé, le drapeau de rapport est posé AVANT la tentative d'annulation, et la trace nomme le travail. Reprend `jobs-local/src/index.ts:517`, dont `jobs/src/index.ts:44-46` donne la raison : « a record its owner is being destroyed for has no reader left ».
- [ ] Given une annulation d'un travail unique, when elle est demandée, then les autres travaux du même fil ne sont pas touchés ; c'est ce que Codex [#17821](https://github.com/openai/codex/issues/17821) demande encore et que `/stop` ne fait pas.
- [ ] Given l'arrêt du fil, when le délai de grâce existant est atteint, then le comportement suit `SHUTDOWN_DEADLINE` (`crates/agent-runtime/src/thread.rs:51`) sans introduire une seconde échéance.
- [ ] Un test de course prouve qu'une interruption pendant un enregistrement laisse le registre cohérent : soit le travail existe et est `Killed`, soit il n'existe pas, jamais un enregistrement sans processus.

---

### EP-042: Les terminaux entrent au registre

Le trait de lanceur, l'enregistrement des sessions d'`ExecSessions` et le règlement à la sortie du processus. Ferme les défaillances 1 et 4 pour le genre `Terminal`.

**Definition of Done:** ouvrir une session par `exec_command` crée un travail visible dans le registre du fil ; la fin du processus le règle une fois, avec son code de sortie, et `open_sessions()` rend la même liste que le registre.

#### US-135: Le lanceur est un trait du runtime, implémenté par le binaire
**Description:** As a agent de codage, I want que le registre définisse ce qu'il sait lancer sans dépendre des outils so that le graphe de crates reste à sens unique et que la politique reste testable sans registre d'outils.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-132
**Source dsh:** `packages/jobs/jobs/src/types.ts:46` (`JobStart`, avec `run`, `cancel`, `done`, `readOutput`) et `packages/shell/tool-bash/src/index.ts:349-380`, où le producteur `bash` fournit ces quatre fonctions au registre sans que le registre connaisse le shell

**Acceptance Criteria:**
- [ ] Un trait de lanceur est déclaré dans `agent-runtime` et implémenté dans `agent-cli`, sur le patron documenté par `crates/agent-tools/Cargo.toml:22-26` pour `AgentSpawner` et `SubAgentSpawner`.
- [ ] `crates/agent-runtime/Cargo.toml` ne gagne aucune dépendance vers `agent-tools` ; le graphe reste à sens unique et `docs/crate-graph.md` régénéré le montre.
- [ ] Given un registre construit sans lanceur, when un enregistrement est demandé, then il est refusé par une erreur nommée, sur le précédent « pas de dépôt, pas de comportement » de `ToolCtx.spill` (`crates/agent-tools/src/tool.rs:60-105`).
- [ ] Le trait expose la lecture de sortie et l'annulation comme des fonctions fournies par le producteur, jamais comme une connaissance du registre : le registre ignore ce qu'est un shell.
- [ ] Given un lanceur dont le démarrage échoue, when il échoue, then le travail est réglé `Failed` avec la cause, l'emplacement est libéré, et aucun enregistrement `Running` ne subsiste.
- [ ] Un test du crate `agent-runtime` exerce le registre avec un lanceur factice, sans processus ni shell.

#### US-136: `exec_command` réserve puis enregistre, dans cet ordre
**Description:** As a modèle, I want que la session que j'ouvre soit un travail du registre so that elle porte un identifiant durable et que je puisse la retrouver sans m'en souvenir.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-135
**Source dsh:** `packages/shell/tool-bash/src/index.ts:349-380` pour l'ordre « le registre d'abord, le processus ensuite » et pour le retour d'un identifiant plutôt que d'une sortie

**Acceptance Criteria:**
- [ ] `ExecSessions` enregistre chaque session ouverte auprès du registre du fil ; le registre devient la source de vérité et `open_sessions()` (`crates/agent-tools/src/exec_session.rs:366`) en devient une projection.
- [ ] L'ordre est : réservation d'emplacement, enregistrement durable, puis lancement du processus. Un échec à l'une des deux premières étapes ne lance rien.
- [ ] Given un enregistrement refusé pour cause de plafond, when il est refusé, then le message existant de `reserve()` (`:403-412`) reste le message rendu au modèle, inchangé.
- [ ] Le doc-comment périmé d'`open_sessions()` (`:360-366`), qui invoque un « idle watchdog » que `MAX_SESSIONS` (`:47-52`) refuse, est corrigé dans cette story.
- [ ] Given les tests existants du terminal, when ils tournent, then ils restent verts sans changement de sémantique ; `a_finished_command_reports_the_baseline_wire` (`docs/parity/offline-suite.md:31`) inclus, et `just parity` reste vert.
- [ ] Le fil vers lequel `ExecSessions` enregistre est explicite : soit `ExecSessions` devient par fil comme `AgentSupervisor` (`crates/agent-cli/src/runtime.rs:727-730`), soit la story écrit pourquoi le rattachement suffit et ce que `max_open_threads: 1` (`crates/agent-app-server/src/server.rs:289`) garantit d'ici là.
- [ ] Le contrat de fil Codex (`chunk_id`, `session_id`, `exit_code`) est inchangé sur le fil ; le `JobId` est un ajout, jamais un remplacement.

#### US-137: La fin d'un processus règle le travail une fois, avec son code de sortie
**Description:** As a modèle, I want qu'un travail terminé porte un état terminal exact so that je distingue un échec de lancement d'une commande qui a rendu un code non nul.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-136
**Source dsh:** `packages/shell/tool-bash/src/background.ts` (27 lignes) : « A nonzero command exit is reported, not failed, exactly like the foreground rendering », et `killed` reste `killed`

**Acceptance Criteria:**
- [ ] Given un processus qui sort avec 0, when il sort, then l'état est `Completed` et le code est porté en détail.
- [ ] Given un processus qui sort avec un code non nul, when il sort, then l'état est `Completed`, pas `Failed`, et le code est porté ; `Failed` est réservé à l'échec du lancement.
- [ ] Given un processus tué, when il l'est, then l'état est `Killed` et le reste : aucune conversion en `Completed`.
- [ ] Given un processus tué par un signal, when il l'est, then le signal apparaît dans le détail et l'état reste `Killed`.
- [ ] Given la sortie d'un processus déjà réglé, when elle arrive en retard, then elle est ignorée par le règlement premier-arrivé de US-132, sans panique ni écrasement.
- [ ] Given un `Drop for Session` (`crates/agent-tools/src/exec_session.rs:256`) déclenché, when il tue le groupe, then le travail est réglé `Killed` avec sa cause, et l'entrée durable est écrite avant que le registre n'annonce quoi que ce soit.
- [ ] Le règlement écrit d'abord, notifie ensuite : un test ordonne les deux et échoue si l'ordre s'inverse.

---

### EP-043: Ce que le modèle voit

L'outil de liste, la relève de sortie idempotente et le drapeau de rapport qui empêche la double livraison. Ferme les défaillances 1 et 2 côté modèle.

**Definition of Done:** un modèle qui a tout oublié appelle un outil, voit ses travaux avec leur état et leur âge, relève une sortie deux fois et obtient deux fois la même chose, et un travail terminé n'est annoncé qu'une fois.

#### US-138: `list_jobs` dit ce qui tourne, sans effet et sans souillure supplémentaire
**Description:** As a modèle, I want un appel qui liste mes travaux de fond so that un identifiant oublié cesse d'être une session perdue.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-136
**Source dsh:** `packages/jobs/tool-jobs/src/index.ts` pour la forme d'un outil de registre ; contre-modèle Codex `list_background_terminals` (`codex-rs/core/src/codex_thread.rs:471`), qui est une API client et non un outil du modèle

**Acceptance Criteria:**
- [ ] Un outil `list_jobs` est enregistré dans `crates/agent-tools/`, avec les propriétés de politique de `list_agents` (`docs/tool-catalog.md:53`) : lecture seule, concurrent, non sensible, non sensible à la souillure, sortie non fiable.
- [ ] Given un registre vide, when l'outil est appelé, then il rend une réponse explicite « aucun travail », jamais une chaîne vide ni une erreur.
- [ ] Given des travaux existants, when l'outil est appelé, then chacun porte son identifiant, son genre, son état, son âge et sa commande tronquée sur une frontière de caractère, comme `format_left_running` (`crates/agent-cli/src/interactive/mod.rs:1391`) le fait déjà pour l'écran.
- [ ] La commande listée est traitée comme du texte non fiable : elle est bornée en octets et ses caractères de contrôle sont neutralisés.
- [ ] Given un appel répété sans rien entre, when il est répété, then il est exempté du garde-fou de boucle par `loop_guard_exempt`, sur le précédent des sondages vides (`crates/agent-tools/src/exec_session.rs:669-674`), ou la story dit pourquoi il ne l'est pas.
- [ ] L'outil n'a aucun effet : il ne règle rien, ne pose aucun drapeau de rapport et ne libère aucun emplacement.
- [ ] Given un fil qui n'a pas de registre, when l'outil est appelé, then il rend une erreur nommée, jamais une liste vide qui mentirait.

#### US-139: La relève finale est idempotente, la lecture incrémentale ne l'est pas
**Description:** As a modèle, I want distinguer « donne-moi ce qui est nouveau » de « donne-moi le résultat » so that relire un résultat ne le consomme pas et que sonder ne le duplique pas.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-138
**Source dsh:** `packages/jobs/jobs/src/types.ts:131` (`JobRead`) pour la séparation des deux lectures

**Acceptance Criteria:**
- [ ] La lecture incrémentale reste celle qui existe : `write_stdin` garde son curseur consommant et son comportement actuel, bornes de rendement comprises.
- [ ] Given un travail terminé, when sa sortie finale est relevée deux fois, then les deux relèves rendent exactement les mêmes octets.
- [ ] Given un travail terminé dont la sortie dépasse la borne, when elle est relevée, then elle passe par le déversement existant (`crates/agent-tools/src/registry.rs:992`, ADR-15) et le modèle reçoit le chemin, pas le contenu. Codex borne la même sortie par `max_output_tokens` et par relève ; Pyxis a déjà le déversement, donc n'ajoute pas une seconde borne.
- [ ] La séparation des deux lectures est le point où ce lot dépasse la ligne de base : `BashOutput` chez Claude Code et `write_stdin` vide chez Codex n'offrent qu'un curseur consommant côté serveur, et aucun agent étudié n'expose une relève idempotente du résultat.
- [ ] Given un travail encore actif, when sa sortie finale est demandée, then la réponse dit qu'il tourne encore et rend ce qui est disponible, sans bloquer plus que la borne de rendement existante.
- [ ] Given une sortie contenant des octets non UTF-8, when elle est relevée, then le rendu ne panique pas et la story dit quelle politique de remplacement s'applique.
- [ ] La borne d'octets d'un travail est une constante de crate ; aucune clé de configuration n'est ajoutée.

#### US-140: Un travail terminé n'est annoncé qu'une fois, et le drapeau survit au redémarrage
**Description:** As a humain, I want que le résultat d'un travail atteigne le modèle exactement une fois so that mon contexte ne soit pas mangé par un rappel qui se répète, et qu'aucun résultat ne disparaisse en silence.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-137, US-139
**Source dsh:** `packages/jobs/jobs/src/types.ts:127` pour `reported`, `packages/jobs/jobs-local/src/index.ts:422` (`if (job.waiters > 0) job.reported = true`) pour le moment où il se pose

**Acceptance Criteria:**
- [ ] `reported` bascule à vrai quand, et seulement quand, le résultat a atteint le modèle : une relève de sortie finale, une attente satisfaite ou une livraison de EP-044.
- [ ] Given une attente en cours au moment du règlement, when le travail se règle, then `reported` est posé dans le même passage, avant la libération des attentes ; c'est la ligne `jobs-local:422`.
- [ ] Given un travail rapporté, when le fil produit sa notice de fin de tour, then il n'y figure plus ; c'est le mode de défaillance que Claude Code [#12302](https://github.com/anthropics/claude-code/issues/12302) et [#13249](https://github.com/anthropics/claude-code/issues/13249) décrivent, et le test le reproduit à l'envers.
- [ ] Given un travail terminé jamais rapporté, when le tour se termine, then il figure dans la notice ; un résultat n'est jamais perdu en silence.
- [ ] `reported` est durable : l'entrée `JobReported` de US-133 est écrite au moment de la bascule, et un fil rouvert la relit.
- [ ] Given un travail tué, when il est tué, then il compte comme rapporté dès que le modèle en a reçu l'accusé, exactement comme un travail terminé ; [#13249](https://github.com/anthropics/claude-code/issues/13249) montre le contraire chez le concurrent.
- [ ] Un test compte les annonces : sur cinquante tours suivant un règlement, le travail est annoncé exactement une fois.

#### US-141: Les catalogues, le graphe de crates et l'expérience du modèle sont régénérés
**Description:** As a mainteneur, I want que les documents rendus depuis le code montrent le nouvel outil et le nouveau module so that la revue voie la surface ajoutée dans un diff au lieu de la deviner.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-138
**Source dsh:** aucune ; c'est un mécanisme propre à Pyxis, livré au lot 5

**Acceptance Criteria:**
- [ ] `docs/tool-catalog.md` est régénéré par `PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis tool_catalog` ; le compte passe de 29 à 30 et la ligne de synthèse suit.
- [ ] `crates/agent-tools/README.md:16`, qui porte le littéral « 29 tools today » vérifié par une porte, est mis à jour dans la même modification.
- [ ] `docs/crate-graph.md` est régénéré si une arête change ; sinon la story dit qu'aucune arête ne change et pourquoi.
- [ ] La section `## Model Experience` du README du crate qui compose la description de `list_jobs` est écrite dans la forme que `docs/model-experience.md` fixe, et `cargo test -p agent-doc-gates` la valide.
- [ ] Aucun de ces fichiers n'est édité à la main : le diff montre exactement ce que la commande de l'en-tête produit.
- [ ] Given une régénération lancée deux fois de suite, when la seconde tourne, then `git status --porcelain` reste vide.

---

### EP-044: La complétion qui revient d'elle-même

La poussée d'une complétion non relevée, son budget et son inertie sur les surfaces où il n'y a personne à réveiller. Ferme la défaillance 2.

**Definition of Done:** un travail qui finit alors que le modèle ne le relève plus ouvre un tour, au plus trois fois de suite sans entrée humaine, et n'ouvre rien du tout sous `-p`.

#### US-142: Une complétion non relevée ouvre un tour, dans un budget constant
**Description:** As a modèle, I want qu'un résultat que j'ai cessé d'attendre me revienne so that un travail long cesse de dépendre de ma discipline de sondage.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-140
**Source dsh:** `packages/jobs/tool-jobs/src/index.ts:279-300` pour le bloc de livraison, `:49-52` pour les valeurs par défaut (`completionDelivery: 'wakeup'`, `maxConsecutiveWakes: 3`)

**Acceptance Criteria:**
- [ ] Le budget de réveils consécutifs est une constante de crate d'`agent-runtime`, valeur 3, avec un doc-comment nommant la valeur dsh et la raison du choix. Aucune clé, aucun drapeau, aucune variable.
- [ ] Given un fil oisif et un travail qui se règle sans être rapporté, when le règlement est commis, then un tour est ouvert par le chemin `submit` existant, avec un message dont l'origine est marquée comme non humaine.
- [ ] Given un tour en cours, when un travail se règle, then le résultat entre par `steer` au prochain point sûr, jamais entre un `tool_use` et son résultat ; c'est la sémantique de `inject` chez dsh.
- [ ] Given un budget épuisé, when un travail se règle, then rien n'est ouvert, le travail reste non rapporté et il figure dans la notice de fin de tour ; c'est le mode `quiet`.
- [ ] La livraison porte un `client_message_id` (`crates/agent-runtime/src/thread.rs:56-60`) dérivé du `JobId` : une livraison rejouée rend les identifiants d'origine et n'ouvre pas un second tour.
- [ ] L'annonce suit le commit : l'entrée durable terminale est écrite avant que la livraison ne soit tentée, parce que le destinataire peut ouvrir un tour de façon synchrone (`packages/jobs/jobs/src/index.ts:41-60`).
- [ ] Given une constante mise à zéro, when un travail se règle, then aucun tour n'est jamais ouvert ; le comportement dégradé est celui d'aujourd'hui et un test le prouve.
- [ ] Given une livraison dont l'écriture échoue, when elle échoue, then le travail reste non rapporté et sera annoncé plus tard ; jamais l'inverse.

#### US-143: Le budget se réarme sur une entrée d'origine humaine
**Description:** As a humain, I want que mes propres messages remettent le compteur à zéro so that une série de travaux courts ne s'auto-alimente pas en tours pendant que je ne regarde pas.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-142
**Source dsh:** `packages/jobs/tool-jobs/src/index.ts:228` : `if (message.source.kind === 'user') spentWakes.delete(agent)`

**Acceptance Criteria:**
- [ ] Le compteur est remis à zéro par une entrée dont l'origine est humaine, et par elle seule.
- [ ] Given trois réveils consécutifs puis un message de l'humain, when un quatrième travail se règle, then il réveille.
- [ ] Given trois réveils consécutifs sans message humain, when un quatrième travail se règle, then il ne réveille pas.
- [ ] L'origine d'une entrée est portée par une donnée, pas devinée : la story nomme le champ, et un tour ouvert par une livraison ne peut pas se faire passer pour humain.
- [ ] Given une livraison ouvrant un tour qui règle un autre travail, when cela arrive, then le compteur ne se réarme pas ; le test construit la chaîne et prouve qu'elle s'arrête.
- [ ] Le compteur n'est pas durable et la story dit pourquoi : une reprise repart d'un budget plein, ce qui est le comportement sûr.

#### US-144: En headless et en app-server, la livraison ne réveille personne
**Description:** As a intégrateur, I want qu'un `pyxis -p` ne se prolonge pas parce qu'un travail a fini so that le code de sortie et la dernière ligne du flux restent ce que mon script attend.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-142
**Source dsh:** `packages/jobs/tool-jobs/src/index.ts:279-300`, dont la branche de repli `owner.inject` quand le propriétaire n'est pas oisif

**Acceptance Criteria:**
- [ ] Given `pyxis -p`, when un travail se règle après le dernier tour, then aucun tour n'est ouvert, `run_summary` reste la dernière ligne, et le code de sortie est inchangé. Claude Code documente le même arbitrage : en `-p`, les commandes de fond sont terminées peu après le résultat final.
- [ ] Given `pyxis -p`, when le tour se termine avec des travaux actifs, then ils sont tués par le chemin d'arrêt existant et leur état terminal est durable avant la sortie.
- [ ] Given `--ephemeral`, when un travail est enregistré, then il l'est sur le magasin en mémoire sans qu'aucun fichier de session ne soit ouvert, ou l'enregistrement est refusé par une erreur nommée ; la story tranche et le teste.
- [ ] Given l'app-server, when un travail se règle, then l'événement traverse le contrat existant sans nouvelle méthode de protocole, ou la story nomme la méthode ajoutée et régénère les schémas.
- [ ] La transcription JSONL gelée d'un scénario sans travail de fond est inchangée octet pour octet ; `cargo test -p agent-cli --bin pyxis transcript` reste vert.
- [ ] Si une variante d'`AgentEvent` est ajoutée, `docs/EVENT_SCHEMA.md` la documente dans la même modification, la porte de comptage l'exige déjà.

---

### EP-045: Le redémarrage qui rapporte au lieu de ressusciter

La reprise qui réconcilie, la phrase qui le dit, et la décision qui fixe la frontière. Ferme les défaillances 5 et 6, et renégocie la troisième clause du signal de vérification.

**Definition of Done:** un fil rouvert ne porte aucun travail `Running`, chaque travail actif est devenu `Interrupted` avec une cause durable, l'humain et le modèle le lisent, et ADR-16 dit pourquoi ce n'est pas une résurrection.

#### US-145: Une reprise ferme tout travail actif avec sa cause durable
**Description:** As a humain reprenant une session, I want qu'aucun travail ne reste marqué en cours après un redémarrage so that je ne lise pas un état qui ment.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-133
**Source dsh:** aucune ; dsh n'a pas de reprise. Précédent interne : `AgentGraph::restore` (`crates/agent-runtime/src/agent.rs:512-525`) et `RESTART_CAUSE` (`:35`)

**Acceptance Criteria:**
- [ ] Given un journal portant des travaux actifs, when le fil est rouvert, then chacun devient `Interrupted` avec une cause durable, sur le patron exact de `AgentGraph::restore`.
- [ ] La transition de réconciliation est elle-même durable : un second redémarrage ne la réécrit pas.
- [ ] Given un travail déjà terminal dans le journal, when le fil est rouvert, then son état et son drapeau de rapport sont relus tels quels, sans transition supplémentaire.
- [ ] Given un travail terminé mais non rapporté au moment du crash, when le fil est rouvert, then il reste non rapporté et sera annoncé une fois, exactement une ; c'est l'écart délibéré avec `AgentSupervisor::restore` (`crates/agent-runtime/src/supervisor.rs:344-367`), qui marque `delivered: true`, et la story écrit pourquoi les deux règles diffèrent.
- [ ] Given une reprise, when elle se termine, then aucun processus n'est relancé et aucun pid n'est ré-attaché ; un test le prouve par l'absence de tout appel de lancement.
- [ ] Given un journal écrit avant ce lot, when il est rouvert, then la reprise réussit et ne trouve aucun travail ; aucune migration n'est nécessaire.
- [ ] Aucun état `Running` ne peut survivre à une reprise : l'assertion est un test, pas une revue.

#### US-146: La reprise, la fin de tour et `/status` nomment ce qui reste ouvert
**Description:** As a humain, I want lire en une phrase ce qui tourne et ce qui vient d'être interrompu so that je décide de relancer, sans exécuter `ps`.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-145, US-138
**Source dsh:** aucune ; précédent interne `left_running_notice` (`crates/agent-cli/src/interactive/mod.rs:1387`)

**Acceptance Criteria:**
- [ ] `left_running_notice` lit le registre plutôt qu'`ExecSessions` directement, et la troncature existante sur frontière de caractère est conservée.
- [ ] Given une reprise ayant interrompu des travaux, when le fil s'ouvre, then une ligne par travail nomme sa commande tronquée et sa cause ; s'il n'y en a aucun, rien n'est écrit.
- [ ] Le modèle reçoit la même information que l'écran : la reprise la lui rend accessible par `list_jobs`, pas seulement par un texte d'interface.
- [ ] `/status` montre la borne du registre et le nombre de travaux actifs, à côté des autres constantes d'orchestration qu'il affiche déjà.
- [ ] Given un travail rapporté, when la notice est composée, then il n'y figure pas ; c'est US-140 vue depuis l'écran.
- [ ] Given une commande contenant des séquences ANSI ou des caractères de contrôle, when elle est affichée, then elle est neutralisée : c'est du texte que le modèle a composé.
- [ ] Un instantané de rendu du TUI couvre la notice avec au moins un travail et avec aucun, revu par `cargo insta review`.

#### US-147: ADR-16 fixe la frontière de survie, et la note consigne l'écart au plan
**Description:** As a mainteneur, I want que « rapporter, pas ressusciter » soit une décision opposable so that une pull request qui tenterait la ré-attache par pid soit refusée sur un texte, pas sur un avis.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-145
**Source dsh:** `packages/jobs/jobs-local/src/index.ts:1-10`, le doc-comment qui dit que tout est en mémoire, c'est-à-dire l'absence de source de la clause 3

**Acceptance Criteria:**
- [ ] `docs/DECISIONS.md` gagne ADR-16, numérotée après ADR-15 (`docs/DECISIONS.md:560`), dans la forme des précédentes : statut, date, lot du plan, et ce qu'elle n'altère pas.
- [ ] ADR-16 énonce la frontière : les enregistrements sont durables, les processus ne le sont pas ; une reprise rapporte et n'exécute rien.
- [ ] ADR-16 nomme ce que coûterait la survie réelle, sans la construire : un puits fichier sous `.pyxis/`, déjà protégé par `PROTECTED_SUBPATHS` (`crates/agent-tools/src/path.rs:33`), un groupe de processus détaché par `setsid`, et une ré-attache par pid qui n'a aucune garantie d'identité après un recyclage de pid. Le puits fichier n'est pas un choix de confort : une écriture dans un tube sans lecteur lève `SIGPIPE` ou `EPIPE` ([`pipe(7)`](https://man7.org/linux/man-pages/man7/pipe.7.html)), donc un travail qui survit à son parent ne peut pas garder un tube.
- [ ] ADR-16 dit pourquoi ce n'est pas ADR-13 : ADR-13 traite l'isolation noyau d'un enfant mutateur, `landlock::restrict_self` étant irréversible et propre au processus ; la durée de vie est une autre question.
- [ ] Une note sous `docs/notes/implemented/` consigne l'écart au plan et l'ordre de lecture : la clause 3 du signal de vérification n'a pas de source dsh, et le plan omet deux fichiers producteurs dans sa colonne source.
- [ ] La note lie ADR-16 et ADR-16 ne lie pas la note, conformément à la règle réciproque de `docs/notes/README.md`.
- [ ] `cargo test -p agent-doc-gates` valide la forme des deux documents.
- [ ] `docs/ARCHITECTURE.md` §8 (`:596-626`) est corrigé sur ses trois affirmations périmées, et sa liste numérotée d'invariants (`:693-708`) reste inchangée : ce lot n'en ajoute aucun.

## Functional Requirements

- FR-01: Le système doit tenir un registre des travaux de fond d'un fil, avec un état fermé à cinq valeurs et un genre fermé.
- FR-02: Le système doit refuser un enregistrement au-delà d'une borne qui est une constante de crate, par une erreur nommant l'action de libération, jamais par une attente.
- FR-03: Le système doit rendre durable l'existence d'un travail avant d'en acquitter l'enregistrement à son appelant.
- FR-04: Le système doit rendre durable l'état terminal d'un travail avant d'annoncer sa complétion à quiconque.
- FR-05: Le système doit régler un travail une seule fois : un second règlement est ignoré, jamais appliqué.
- FR-06: Le système doit distinguer un travail terminé d'un travail dont le résultat a atteint le modèle, et rendre cette distinction durable.
- FR-07: Le système doit livrer le résultat d'un travail terminé au modèle exactement une fois, ni zéro ni deux.
- FR-08: Le système doit permettre au modèle de lister ses travaux sans connaître d'identifiant préalable.
- FR-09: Le système doit rendre une relève de sortie finale idempotente, la lecture incrémentale existante restant consommante.
- FR-10: Le système doit propager l'annulation du fil vers chaque travail, et interdire tout chemin d'annulation remontant.
- FR-11: Le système doit permettre l'annulation d'un travail unique sans toucher aux autres.
- FR-12: Le système doit, lors de la reprise d'un fil, transformer tout travail actif en travail interrompu portant une cause durable, sans relancer aucun processus.
- FR-13: Le système ne doit ouvrir un tour pour une complétion que lorsque le budget de réveils consécutifs le permet, et ce budget doit se réarmer sur une entrée d'origine humaine.
- FR-14: Le système ne doit ouvrir aucun tour pour une complétion sous `-p`.
- FR-15: Le système doit rapporter un code de sortie non nul comme une complétion, et réserver l'échec au défaut de lancement.
- FR-16: Le système ne doit exposer aucune clé de configuration, aucun drapeau et aucune variable d'environnement pour ses bornes.

## Non-Functional Requirements

- **Empreinte durable:** au plus 3 entrées JSONL par travail dans la vie normale d'un travail, plus 1 par transition de réconciliation à la reprise ; `SESSION_SCHEMA_VERSION` reste à 1 et aucune migration n'est écrite.
- **Empreinte de configuration:** 0 clé ajoutée à `settings.toml`, 0 drapeau CLI, 0 variable `PYXIS_*` de comportement ; `docs/config-catalog.md` reste à quinze clés.
- **Empreinte de dépendances:** 0 dépendance ajoutée au `Cargo.toml` du workspace.
- **Surface d'outils:** exactement 1 outil enregistré ajouté, portant le catalogue de 29 à 30.
- **Latence:** l'enregistrement d'un travail ajoute au plus 1 écriture au journal avant le lancement du processus ; le temps mesuré entre l'appel d'`exec_command` et le premier octet lisible ne se dégrade pas de plus de 10 ms sur un disque local.
- **Coût en contexte:** un travail terminé occupe au plus une annonce dans tout le fil ; l'assertion est un test de comptage sur cinquante tours, pas une revue.
- **Coût en requêtes modèle:** au plus 3 tours ouverts par livraison sans entrée humaine intercalée.
- **Temps mur ajouté à `just test`:** ≤ 4 s sur cache chaud, aucun test n'attendant une durée réelle.
- **Sécurité:** un travail d'un fil est invisible d'un autre fil ; la commande listée est du texte non fiable, bornée et neutralisée ; la sortie d'un travail reste souillée et propage la souillure.
- **Fiabilité:** 0 état `Running` survivant à une reprise ; 0 processus relancé par une reprise ; 0 `JoinHandle::abort` sur le chemin d'annulation.

## Edge Cases & Error States

| # | Scénario | Déclencheur | Comportement attendu | Message |
|---|----------|-------------|----------------------|---------|
| 1 | Registre à plein | Cinquième travail demandé | Refus, rien n'est lancé, l'emplacement n'est pas consommé | reprend le message existant de `reserve()` |
| 2 | Échec d'écriture du journal à l'enregistrement | Disque plein, magasin fermé | Refus de l'enregistrement, aucun processus lancé | « travail non enregistré : `<cause>` » |
| 3 | Deux règlements concurrents | Le processus sort pendant une annulation | Premier arrivé gagne, second ignoré, trace `debug` | sans objet |
| 4 | Annulation qui lève ou expire | Processus qui ignore le signal | Travail réglé quand même, rapport posé AVANT la tentative | trace nommant le travail |
| 5 | Code de sortie non nul | `cargo test` qui échoue | `Completed`, code en détail, jamais `Failed` | sans objet |
| 6 | Échec de lancement | Binaire absent, droit refusé | `Failed` avec la cause, emplacement libéré | « lancement impossible : `<cause>` » |
| 7 | Sortie tardive d'un travail déjà réglé | Course entre le lecteur et l'arrêt | Ignorée par le règlement premier-arrivé, aucune panique | sans objet |
| 8 | Sortie dépassant la borne | Build très bavard | Déversement ADR-15, le modèle reçoit un chemin | message de déversement existant |
| 9 | Sortie non UTF-8 | Programme binaire | Remplacement documenté, aucune panique | sans objet |
| 10 | Reprise avec travaux actifs | Crash puis `--resume` | Tous en `Interrupted` avec cause durable, rien de relancé | « `<n>` travaux interrompus par le redémarrage » |
| 11 | Reprise d'un journal antérieur au lot | Fichier écrit avant ce lot | Aucun travail trouvé, aucune erreur, aucune migration | sans objet |
| 12 | Double reprise | `--resume` deux fois de suite | La réconciliation n'est pas réécrite la seconde fois | sans objet |
| 13 | Travail terminé non rapporté au crash | Crash entre le règlement et la livraison | Rapporté une fois après la reprise, jamais zéro ni deux | sans objet |
| 14 | Budget de réveils épuisé | Quatre travaux courts d'affilée | Aucun tour ouvert, les travaux figurent dans la notice | « `<n>` travaux terminés non relevés » |
| 15 | Livraison pendant un tour en cours | Règlement au milieu d'une chaîne d'outils | Entrée par `steer` au prochain point sûr, jamais entre un `tool_use` et son résultat | sans objet |
| 16 | Livraison rejouée | Même `client_message_id` resoumis | Rend les identifiants d'origine, n'ouvre pas un second tour | sans objet |
| 17 | Complétion sous `-p` après le dernier tour | Script de pipeline | Aucun tour ouvert, `run_summary` reste dernière | sans objet |
| 18 | `list_jobs` sur un registre vide | Premier appel de la session | Réponse explicite « aucun travail », jamais une erreur | « aucun travail de fond » |
| 19 | Commande portant des caractères de contrôle | Heredoc composé par le modèle | Bornée et neutralisée avant affichage et avant rendu au modèle | sans objet |
| 20 | Travail d'un autre fil | Deux fils ouverts, un identifiant deviné | Introuvable, pas « refusé » : la réponse ne révèle pas l'existence | « travail inconnu » |
| 21 | Arrêt du fil pendant un enregistrement | Ctrl+C au mauvais moment | Soit le travail existe et est `Killed`, soit il n'existe pas ; jamais un enregistrement sans processus | sans objet |
| 22 | `--ephemeral` avec un travail | Pipeline sans fichier de session | Enregistrement sur le magasin en mémoire, ou refus nommé ; la story tranche | « travaux de fond indisponibles en mode éphémère » |

## Risks & Mitigations

| # | Risque | Probabilité | Impact | Mitigation |
|---|--------|-------------|--------|------------|
| 1 | Le réveil automatique dépense une requête modèle que l'humain n'a pas demandée | Haute | Haut | Budget de 3 en constante, réveil seulement sur fil oisif, inertie totale sous `-p`, et la constante à zéro rend le comportement d'aujourd'hui. US-142 et US-144 portent les critères. |
| 2 | Le registre double une comptabilité que `ExecSessions` tient déjà, et les deux divergent | Haute | Haut | US-136 fait du registre la seule source de vérité et d'`open_sessions()` une projection. La divergence devient impossible plutôt que surveillée. |
| 3 | La clause 3 du plan est lue comme livrée alors qu'elle est renégociée | Moyenne | Haut | US-147 l'écrit en ADR-16, une note consigne l'écart, et la section Non-Goals le dit en toutes lettres. La décision est opposable à une pull request. |
| 4 | Une pull request ultérieure tente la ré-attache par pid | Moyenne | Haut | ADR-16 nomme le coût et le mode de défaillance (recyclage de pid, absence de garantie d'identité) ; sans texte, le refus serait un avis. |
| 5 | Trois entrées durables par travail alourdissent le journal de fil | Moyenne | Moyen | La NFR d'empreinte durable la borne et US-133 la mesure ; un travail écrit moins d'entrées qu'un tour. |
| 6 | Le rattachement d'`ExecSessions` au fil se révèle plus profond que prévu | Moyenne | Moyen | US-136 pose explicitement l'alternative et exige une réponse écrite ; `max_open_threads: 1` borne le risque d'ici là. |
| 7 | Une chaîne de livraisons s'auto-alimente en tours | Basse | Haut | US-143 exige que l'origine d'une entrée soit une donnée portée, pas devinée, et teste la chaîne jusqu'à son arrêt. |
| 8 | Le nouvel outil casse la parité du contrat terminal | Basse | Haut | US-136 laisse le fil Codex inchangé, le `JobId` est un ajout ; `just parity` est une porte de qualité de chaque story. |
| 9 | Le drapeau de rapport se pose au mauvais moment et perd un résultat | Basse | Haut | US-140 reprend la ligne exacte de dsh (`jobs-local:422`) et la teste dans les deux sens : jamais deux fois, jamais zéro. |

## Non-Goals

Frontières explicites de cette version.

- **Faire survivre un processus à l'arrêt du binaire.** C'est la troisième clause du signal de vérification du plan, et elle n'a aucune source : le fournisseur que le plan cite dit lui-même « keeps every record in memory » (`packages/jobs/jobs-local/src/index.ts:1-10`), et le producteur `bash` de dsh engendre un enfant ordinaire. La construire signifierait un puits fichier, un groupe de processus détaché et une ré-attache par pid, c'est-à-dire un autre système. ADR-16 fixe la frontière retenue : les enregistrements survivent, les processus non, et une reprise rapporte. C'est aussi ce que font Claude Code et Codex, à ceci près qu'aucun des deux ne réconcilie : [#65925](https://github.com/anthropics/claude-code/issues/65925) montre des tâches qui restent `running` après un redémarrage complet.
- **Faire entrer les sous-agents dans le registre.** `AgentSupervisor` tient déjà pour ses enfants un état fermé, un journal durable, des bornes (`MAX_ACTIVE_AGENTS = 4`, `MAX_AGENTS_PER_ROOT = 8`, `MAX_AGENT_DEPTH = 1`, `crates/agent-runtime/src/agent.rs:25-30`) et une livraison exactement-une-fois (`Pending.delivered`, `crates/agent-runtime/src/supervisor.rs:190-192`). Les replier dans un registre générique est un refactor sans gain observable et avec un risque de régression sur un chemin livré. `JobKind` reste donc à une variante ; en ajouter une plus tard est l'ajout d'une variante.
- **Un `run_in_background` sur `bash`.** dsh le fait (`packages/shell/tool-bash/src/index.ts:349-380`) parce qu'il n'a rien d'autre. Pyxis a `exec_command`, dont la session « outlives the turn that opened it, on purpose » (`crates/agent-tools/src/exec_session.rs:359-365`). Ajouter un second mécanisme de fond pour un service déjà rendu contredirait le budget de complexité, et donnerait au modèle deux façons de faire la même chose.
- **La bascule automatique en fond sur dépassement de délai.** Claude Code la fait, avec trois exceptions et une variable d'échappement. Elle retire au modèle une décision qu'il prend explicitement chez Pyxis, elle demanderait une liste d'exceptions à maintenir, et elle ouvrirait la variable d'environnement que l'invariant 15 refuse. Le registre rend la bascule inutile : ce que le modèle a lancé reste adressable sans qu'on ait deviné à sa place.
- **Un moissonneur de sessions oisives.** La décision portée par `MAX_SESSIONS` (`crates/agent-tools/src/exec_session.rs:48-52`) est explicite : « A session is NOT closed for being quiet », parce que surveiller un build est du silence délibéré. Ce lot corrige le doc-comment qui la contredit, il ne renverse pas la décision.
- **Un outil de tuerie dédié.** `write_stdin` porte déjà la terminaison d'une session. Un `kill_job` serait un second chemin vers le même effet ; US-134 exige seulement que l'annulation d'un travail unique n'atteigne pas les autres.
- **Une méthode d'app-server pour lister les travaux.** La ligne de base l'expose comme une API client (`codex-rs/core/src/codex_thread.rs:471`), mais `docs/CURRENT_STATUS.md` pose que les surfaces hors du cycle fil/tour/item sont des non-buts. Si le besoin apparaît, il passera par le contrat existant.
- **Une planification de travaux.** C'est le lot 10 du plan, explicitement ordonné après celui-ci.

## Files NOT to Modify

- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés et empreintés, jamais édités à la main. `BASELINE_COMMIT` ne bouge pas : ce lot ne déplace aucune ligne de base.
- `docs/crate-graph.md`, `docs/tool-catalog.md`, `docs/config-catalog.md` : rendus depuis le code. La ligne de `list_jobs` naît de son `.register(`, puis le document se régénère.
- `crates/agent-core/src/agent.rs` : `run_agent` est le seul moteur modèle-outils. Ce lot n'en ouvre pas un second et ne le modifie pas.
- `crates/agent-core/src/event.rs` : à ne toucher que si une variante d'`AgentEvent` s'avère nécessaire (US-144), et alors avec `docs/EVENT_SCHEMA.md` dans la même modification.
- Le clone Codex résolu par `$PYXIS_CODEX_BASELINE` : lecture seule, sans exception. Aucun `commit`, `checkout` ni `fetch`.
- `spikes/` : espace jetable exclu de la Phase 0.
- `.github/workflows/ci.yml` et `justfile` : aucune recette n'est ajoutée par ce lot, donc aucune étape ne l'est ; toute modification déclencherait la porte d'inventaire de recettes.
- `crates/agent-runtime/src/inputs.rs` : `TurnInputs` est une file non bornée, et la borne de 16 que le plan lui attribue au lot 10 vit en réalité dans `crates/agent-runtime/src/thread.rs:47`, où elle borne `pending_steers`. Ce lot ne corrige pas le plan, il ne se fonde pas dessus.

## Technical Considerations

Formulé comme des questions pour l'ingénierie, non comme des mandats.

- **Portée d'`ExecSessions`:** devient-il par fil comme `AgentSupervisor`, ou reste-t-il par exécution avec un rattachement explicite au fil de chaque session ? Recommandé : par fil, parce que c'est la seule forme qui rend la barrière de propriété structurelle plutôt que conventionnelle. Compromis : `crates/agent-cli/src/main.rs:1755` et `crates/agent-cli/src/runtime.rs:727-730` bougent tous les deux, et `ToolCtx` porte déjà `sessions` par valeur clonée.
- **Forme du trait de lanceur:** une fonction rendant un quadruplet (lancer, annuler, terminaison, lire), comme le `JobStart` de dsh, ou quatre méthodes de trait ? Recommandé : le trait, parce qu'il se documente et se factice mieux en Rust ; dsh choisit les fonctions parce que TypeScript n'a pas de trait objet à cet endroit.
- **Emplacement du drapeau de rapport:** champ du `JobRecord` en mémoire avec entrée durable, ou déduit de la présence de l'entrée `JobReported` ? Recommandé : les deux, le champ étant le cache de l'entrée, comme `AgentGraph` le fait déjà pour l'état d'un enfant.
- **Identifiant de travail:** dérivé du `session_id` numérique existant, ou un identifiant opaque comme `AgentId` ? Recommandé : opaque, cohérent avec le reste du runtime, le `session_id` restant ce que le fil Codex porte. Compromis : deux identifiants pour la même chose, ce que la story doit rendre lisible au modèle.
- **Origine d'une entrée:** un champ de `Submission`, ou un type de soumission distinct ? La question conditionne US-143 ; sans un porteur explicite, le réarmement du budget serait deviné.
- **Migration:** aucune. Les entrées sont additives, `SESSION_SCHEMA_VERSION` reste à 1, et un journal antérieur se rouvre sans travail. Retour arrière : retirer l'outil et cesser d'enregistrer laisse des entrées qu'un lecteur ignore.

## Success Metrics

| Metric | Baseline (actuel) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|--------------|
| Outils permettant au modèle de lister ce qui tourne | 0 | 1 | Month-1 | `docs/tool-catalog.md` régénéré |
| Outils enregistrés | 29 | 30 | Month-1 | ligne de synthèse de `docs/tool-catalog.md` |
| Appelants d'`open_sessions()` hors interface humaine | 0 | ≥ 1 | Month-1 | `grep` des appelants dans `crates/` |
| Complétions annoncées plus d'une fois | non mesuré | 0 | Month-1 | test de comptage de US-140, sur 50 tours |
| Complétions jamais annoncées | non mesuré | 0 | Month-1 | test de US-140 dans l'autre sens |
| Travaux `Running` après une reprise | non applicable | 0 | Month-1 | assertion de US-145 |
| Processus relancés par une reprise | non applicable | 0 | Month-1 | absence de tout appel de lancement, prouvée par le lanceur factice |
| Tours ouverts sans entrée humaine intercalée | non applicable | ≤ 3 | Month-1 | test de chaîne de US-143 |
| Clés de configuration | 15 | 15 | Month-6 | `docs/config-catalog.md` |
| Dépendances du workspace | inchangé | +0 | Month-6 | `git diff` du `Cargo.toml` racine |
| `SESSION_SCHEMA_VERSION` | 1 | 1 | Month-6 | lecture de la constante |
| Affirmations périmées de `docs/ARCHITECTURE.md` §8 | 3 | 0 | Month-1 | relecture de US-147 |
| Temps mur ajouté à `just test` | 0 s | ≤ 4 s | Month-1 | suite du registre chronométrée sur cache chaud |

## Open Questions

- `ExecSessions` doit-il devenir par fil dans ce lot, ou le rattachement suffit-il tant que `max_open_threads` vaut 1 ? Arthur tranche à l'implémentation de US-136, qui exige une réponse écrite dans les deux cas. Une réponse tardive coûte une seconde migration du câblage de `main.rs`.
- Le réveil automatique doit-il être livré, ou la constante doit-elle naître à zéro et passer à trois dans un lot ultérieur ? Arthur tranche avant US-142. C'est la seule story qui dépense une requête modèle sans demande humaine, et le reste du lot est utile sans elle.
- `--ephemeral` doit-il accepter les travaux de fond sur le magasin en mémoire, ou les refuser ? Question ouverte à US-144 ; les refuser est plus simple et plus honnête, les accepter est plus cohérent avec `exec_command`, qui fonctionne déjà dans ce mode.
- Le `JobId` opaque et le `session_id` numérique doivent-ils coexister durablement, ou l'un doit-il finir par absorber l'autre ? À constater à US-136 ; le fil Codex fixe le second, donc l'absorption ne peut aller que dans un sens.
[/PRD]
