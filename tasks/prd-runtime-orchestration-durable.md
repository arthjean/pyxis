[PRD]
# PRD: Pyxis : runtime d'orchestration durable de niveau Codex

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.2 | 2026-07-28 | Arthur Jean | US-011 recentrée sur le runtime et le store ; la surface de commande `/fork` et `/rewind`, que US-017 AC1 portait déjà, y est explicitement re-hébergée au lieu d'être décrite deux fois |
| 1.1 | 2026-07-27 | Arthur Jean | Ajout du codebase Codex CLI local comme référence primaire d'implémentation |
| 1.0 | 2026-07-27 | Arthur Jean | Draft initial fondé sur l'audit croisé de Codex CLI et Pyxis, la recherche concurrentielle et les primitives Tokio |

## Problem Statement

1. Pyxis possède une boucle agent headless, un transcript durable, des outils, un sandbox, MCP, des skills, des hooks et deux clients, mais aucun objet durable ne possède réellement une conversation. `agent-cli` lance un nouveau `run_agent` par tour et conserve l'activité dans un `ActiveTurn` process-local. Un redémarrage restaure des messages, pas l'état d'un thread, d'un turn ou d'une opération de contrôle.
2. Une saisie reçue pendant un turn est placée dans une FIFO et ne démarre qu'après l'événement terminal. Pyxis ne peut donc pas corriger la trajectoire d'un modèle en cours sans interrompre manuellement le turn puis reformuler.
3. L'annulation coopérative du cœur réconcilie le transcript, mais le CLI peut encore appeler `JoinHandle::abort`. Aucun superviseur ne garantit conjointement l'arrêt des requêtes modèle, des outils, des processus enfants et des futurs sous-agents avant d'écrire l'état terminal.
4. `/fork` copie actuellement le transcript sans identité de parent ni point de branche. Il n'existe pas de rewind non destructif, de soumission idempotente ni de reconstruction d'un cycle de vie explicite.
5. Pyxis ne possède aucun sous-agent. Ajouter directement des outils `spawn` dans le modèle actuel clonerait des `Deps` trop puissantes, sans quota global, filiation d'annulation, isolation de permissions ni provenance durable.
6. Les PRD précédents ont déjà livré le wire Codex, la TUI, les outils, le sandbox, MCP, les skills et les hooks. Continuer la parité par checklist produirait davantage de surface sans résoudre la lacune centrale: l'orchestrateur qui permet au modèle d'utiliser ces capacités dans un thread contrôlable et durable.

**Why now:** l'audit du 27 juillet 2026 montre que la parité de surface est désormais élevée tandis que la parité d'orchestration reste faible. Pyxis est dogfoodé au quotidien et les primitives basses sont stabilisées. C'est le moment où introduire un propriétaire unique du thread évite de multiplier les chemins ad hoc dans `interactive.rs`, avant l'ajout de sous-agents ou une intégration plus étroite avec Paneflow.

## Overview

Ce PRD ajoute un runtime local de thread au-dessus de `run_agent`. Le moteur actuel reste responsable d'un turn modèle-outils; le nouveau runtime possède l'identité durable, la mailbox de contrôle, le cycle de vie des turns, la persistance des opérations, les snapshots de contexte, l'annulation hiérarchique, les forks et les sous-agents. TUI et headless consomment la même interface `ThreadHandle` et le même flux d'événements.

La solution adopte cinq restrictions de scope. Il n'y a pas d'app-server distant, pas d'agent teams, pas de task board partagé, pas de nouveau provider et pas de nouveau réglage utilisateur. Les limites d'orchestration sont des constantes v1. Les forks sont matérialisés dans un fichier indépendant avec provenance, plutôt que référencés en copy-on-write. Les sous-agents sont parent-owned, profondeur 1, quatre actifs au maximum et lecture seule par défaut. Un enfant mutateur n'est pas livré: un spike vérifie d'abord si un worktree temporaire peut respecter Landlock et les protections Git existantes.

L'architecture recommandée est un crate `agent-runtime` entre `agent-core` et `agent-cli`, sous réserve du spike US-001. Il expose une interface étroite de soumission, observation, interruption, fork et shutdown. `agent-session` gagne un store d'événements local et un adapter mémoire de test. `tokio-util` remplace l'annulation maison afin d'obtenir des tokens parent-enfant et un suivi des tâches dynamiques sans maintenir deux mécanismes concurrents.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Cycle de vie durable des turns | 100 % des turns portent `ThreadId`, `TurnId`, `EventId` et exactement un état terminal | 0 divergence sur 100 sessions dogfood rejouées |
| Contrôle d'un turn actif | 100 % des steers acceptés sont persistés puis consommés au prochain point sûr | 0 message perdu sur 1 000 courses steer-terminal répétées |
| Interruption et shutdown | 0 processus enfant orphelin sur 1 000 courses d'annulation; état terminal sous 2 s pour les tâches coopératives | 0 régression sur 100 sessions dogfood |
| Reprise et branchement | Reprise de 10 000 événements en moins de 500 ms p95; 100 % des fixtures v1 lisibles | 100 % des forks portent parent et point de branche, même après suppression de la source |
| Sous-agents bornés | Registre et protocole parent-enfant validés avec deux enfants concurrents | Quatre enfants lecture seule concurrents, profondeur 1, huit créations maximum par thread racine |
| Sobriété de configuration | 0 nouvelle clé de configuration publique | 0 nouvelle clé liée à l'orchestration sans PRD ultérieur |

## Target Users

### Arthur Jean, créateur et dogfooder principal

- **Role:** mainteneur de Pyxis et utilisateur quotidien de modèles Codex sur des tâches longues.
- **Behaviors:** travaille dans un terminal, interrompt et redirige fréquemment un agent, compare Pyxis à Codex CLI et orchestre plusieurs agents dans Paneflow.
- **Pain points:** doit attendre la fin d'un turn pour corriger la trajectoire, ne peut pas déléguer une exploration à un enfant Pyxis et ne peut pas auditer précisément l'origine d'un fork.
- **Current workaround:** utilise Codex CLI pour les tâches qui exigent steering, sous-agents ou continuité de thread; utilise plusieurs processus externes dans Paneflow pour le parallélisme.
- **Success looks like:** vingt sessions Pyxis consécutives terminées sans bascule vers Codex CLI pour une raison d'orchestration.

### Développeur terminal-native et intégrateur Paneflow

- **Role:** utilisateur OSS ou intégrateur qui veut embarquer un agent local dans un workflow de développement.
- **Behaviors:** attend des événements structurés, une reprise déterministe, des limites de permissions inspectables et des processus qui s'arrêtent réellement.
- **Pain points:** le contrat actuel `run_agent` est adapté à un turn, pas à la possession d'une conversation ou d'un arbre d'agents.
- **Current workaround:** supervise lui-même plusieurs processus CLI et infère leur état depuis leur sortie.
- **Success looks like:** contrôle un thread Pyxis par une interface in-process stable, observe chaque transition et récupère un état terminal sans analyser le rendu TUI.

## Research Findings

Key findings that informed this PRD:

### Reference Codebase

- **Codex CLI local source of truth:** `/home/arthur/dev/codex`
- Ce dépôt est la référence primaire pour comparer les responsabilités, invariants, flux d'exécution et décisions d'orchestration pendant US-001 et les stories de parité suivantes.
- Il doit rester strictement read-only. Toute source copiée ou adaptée dans Pyxis doit être inscrite dans `docs/codex-port-inventory.md`; une réimplémentation inspirée doit être distinguée d'un port.

### Competitive Context

- **Codex CLI:** son app-server expose des threads et turns identifiés, des opérations typées, des événements streamés, des interruptions terminales et des forks persistés. La profondeur vient du contrat thread, pas du nombre de commandes. [Codex app-server protocol](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
- **Claude Code:** les sous-agents sont des enfants isolés et parent-owned; les agent teams ajoutent task board, communication pair-à-pair, shutdown distribué et coût de contexte. Ce PRD retient le premier modèle et exclut le second. [Claude subagents](https://code.claude.com/docs/en/sub-agents), [Claude agent teams](https://code.claude.com/docs/en/agent-teams)
- **Agent Client Protocol:** la trajectoire du protocole converge vers sessions listables et reprenables, cancellation, métadonnées, coûts et workspace roots. Ces capacités confirment la nécessité d'un cycle de vie explicite, sans imposer un protocole réseau à Pyxis. [ACP updates](https://agentclientprotocol.com/updates)
- **Market gap:** Pyxis peut proposer un runtime Rust local, inspectable et borné avec une surface de configuration plus petite que les harness généralistes.

### Best Practices Applied

- Persister les opérations et transitions avant de les acquitter, avec IDs stables et soumissions idempotentes.
- Séparer steering, interruption et nouveau turn; définir leur arbitrage dans l'actor plutôt que dans le TUI.
- Utiliser une mailbox bornée pour les commandes, un signal de dernier état pour l'observation et un event log pour la livraison sans perte.
- Propager l'annulation du parent vers les enfants sans permettre à un enfant d'annuler son parent ou ses frères.
- Fermer l'admission, annuler, attendre la fin des tâches suivies, puis seulement aborter les stragglers.
- Traiter les workspace roots comme contexte, jamais comme frontière de sécurité; le sandbox et les permissions restent l'autorité. [MCP security best practices](https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices)

*Full research sources are linked above and the code evidence is captured in `docs/codex-harness-parity-audit-2026-07-27.md`.*

## Assumptions & Constraints

### Assumptions (to validate)

- `run_agent` peut rester le moteur unique d'un turn si on lui injecte une source de contexte par requête et une file de steering. US-001 doit prouver ce seam avant toute migration.
- `tokio-util::sync::CancellationToken` et `tokio-util::task::TaskTracker` couvrent l'annulation hiérarchique et la comptabilité des tâches sans imposer un second superviseur.
- Une copie matérialisée au fork est suffisamment petite pour les sessions locales actuelles. Le benchmark de US-010 mesure le coût au lieu de pré-optimiser par références entre fichiers.
- Quatre enfants actifs, huit créations et profondeur 1 couvrent le dogfood initial. Les limites restent fixes jusqu'à vingt sessions instrumentées.
- Le TUI et le mode headless peuvent consommer des variantes additives d'`AgentEvent` et des champs JSONL supplémentaires sans casser les clients existants.
- Un adapter mémoire et le store JSONL local constituent deux adapters réels justifiant une interface `ThreadStore`.

### Hard Constraints

- `agent-core` reste headless, sans Ratatui, HTTP ni accès disque direct.
- `run_agent` reste l'unique boucle modèle-outils. Aucun second moteur ne peut réimplémenter retry, compaction ou dispatch.
- Le format de session v1 reste lisible. Une session v1 ouverte puis poursuivie ne perd aucun message.
- Un input, une transition terminale et une relation parent-enfant sont persistés avant leur publication au client.
- L'autorité d'un enfant est l'intersection de l'autorité du parent et de la demande de spawn. Elle ne peut jamais être plus large.
- Les sorties de sous-agents sont marquées untrusted avant injection au parent.
- Les limites v1 de mailbox, enfants et profondeur ne créent aucune clé dans `settings.toml`, les profils ou la CLI.
- Le provider `OpenAiChatGpt`, son wire, l'auth et le sandbox Linux ne sont pas refondus par ce PRD.
- Toute source Codex copiée ou adaptée est ajoutée à `docs/codex-port-inventory.md`; une réimplémentation inspirée est signalée comme telle.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatage du workspace
- `cargo clippy --workspace --all-targets` - lints du workspace sur bibliothèques, binaires et tests
- `cargo test --workspace` - tests unitaires, intégration, reprises et snapshots existants

For UI stories, additional gates:

- `cargo test -p agent-tui --test render_snapshots` - snapshots ciblés du TUI
- Inspection de chaque fichier `*.snap.new`; aucun snapshot en attente à la fin de la story

## Epics & User Stories

### EP-001: Socle du runtime de thread

Cet epic crée l'identité durable, le vocabulaire d'événements, le store et l'actor qui possèdent une conversation locale.

**Definition of Done:** un test peut créer un thread, soumettre un turn, observer ses transitions, fermer le runtime et reconstruire le même état depuis le store sans dépendre du TUI.

#### US-001: Valider le seam entre runtime et `run_agent`

**Description:** As a mainteneur, I want prouver qu'un runtime de thread peut piloter `run_agent` sans dupliquer sa boucle so that l'architecture soit fixée avant la migration.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given le code actuel, when le spike cartographie les responsabilités, then il produit une décision écrite distinguant runtime de thread, moteur de turn, contexte par requête et clients
- [ ] Given un fake provider et un fake store, when un prototype pilote un `run_agent`, then les événements du moteur traversent une interface `TurnRunner` sans réimplémenter retry, compaction ni dispatch
- [ ] Given un token parent, un token enfant et une tâche suivie, when le parent est annulé puis le tracker fermé, then la tâche enfant termine et son destructeur est observé avant la fin du test
- [ ] Given un enfant annulé seul, when son token est déclenché, then le parent et un frère restent actifs
- [ ] Given que le prototype exigerait une seconde boucle modèle ou casserait la réconciliation des tool calls, when le spike conclut, then US-002 et les stories dépendantes sont marquées `BLOCKED` avec l'alternative documentée plutôt que d'empiler deux orchestrateurs

#### US-002: Introduire les identifiants et états de cycle de vie

**Description:** As a client du runtime, I want identifier chaque thread, turn, step, événement et agent so that je puisse corréler commandes, reprise et télémétrie sans état process-local.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given une création de thread ou de turn, when l'identifiant est généré, then il est opaque, sérialisable, comparable et unique sur 100 000 générations de test
- [ ] Given des tests déterministes, when un générateur d'identifiants injecté est utilisé, then les valeurs produites sont reproductibles sans horloge globale
- [ ] Given un turn, when il évolue, then son état appartient à `queued`, `running`, `needs_input`, `completed`, `interrupted` ou `failed`
- [ ] Given un état terminal, when une seconde transition terminale est demandée, then elle est refusée par une erreur typée et aucun événement supplémentaire n'est persisté
- [ ] Given un identifiant vide, mal formé ou d'un autre type, when il est désérialisé, then la validation échoue sans panic

#### US-003: Ajouter un store d'événements d'orchestration

**Description:** As a runtime, I want persister des événements de thread et de turn derrière une interface so that reprise et tests partagent le même contrat.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given `ThreadStore`, when le runtime crée, append, flush, lit et ferme un thread, then les adapters JSONL local et mémoire satisfont le même test de contrat
- [ ] Given un input, un changement d'état, un fork ou une relation d'agent, when l'opération est acceptée, then un événement portant `EventId` et les IDs propriétaires est durable avant l'acquittement
- [ ] Given une session v1 contenant seulement `Meta`, `Message` et compactions, when elle est lue, then tous ses messages sont reconstruits dans le même ordre
- [ ] Given une session v1 poursuivie, when le premier événement v2 est appendu, then l'ancien préfixe reste byte-identique et la reprise suivante reconstruit les deux formats
- [ ] Given une dernière ligne partielle, when le store reprend le fichier, then elle est tronquée au dernier offset valide comme aujourd'hui
- [ ] Given une corruption au milieu du fichier ou un `sync_data` en échec, when une opération est tentée, then le store retourne une erreur nommée, empoisonne le writer si nécessaire et n'acquitte pas l'opération

#### US-004: Posséder le thread dans un actor local

**Description:** As a client Pyxis, I want soumettre des opérations à un propriétaire unique du thread so that l'ordre, l'admission et le shutdown soient déterministes.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001, US-002, US-003

**Acceptance Criteria:**
- [ ] Given un `ThreadHandle`, when un client soumet une opération, observe les événements, lit le dernier état ou demande le shutdown, then il ne manipule directement ni `JoinHandle` ni writer JSONL
- [ ] Given plusieurs producteurs, when ils soumettent simultanément, then une mailbox `mpsc` de capacité 64 sérialise leur ordre d'acceptation
- [ ] Given un thread, when un turn régulier est actif, then aucun second turn régulier ne démarre avant son état terminal
- [ ] Given une opération acceptée, when l'acquittement revient au client, then son événement de soumission est déjà durable
- [ ] Given une mailbox pleine, when une nouvelle réservation est demandée, then elle échoue en moins de 100 ms avec `QueueFull` et aucun identifiant n'est annoncé comme accepté
- [ ] Given un shutdown, when l'admission est fermée, then les tâches suivies sont annulées puis attendues; les stragglers sont abortés après 2 s et drainés avant la fermeture du store

### EP-002: Tours, contexte et contrôle actif

Cet epic branche le moteur existant dans l'actor, sépare l'état par durée de vie et rend steering et interruption observables.

**Definition of Done:** pendant un turn réel, le runtime peut rafraîchir le contexte avant chaque sampling, accepter un steer, interrompre toutes les sous-tâches et produire exactement un état terminal durable.

#### US-005: Exécuter `run_agent` comme moteur de turn

**Description:** As a runtime, I want lancer le moteur actuel dans un cycle de vie de turn explicite so that toute sortie et toute erreur possèdent une identité durable.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Given un `StartTurn`, when le moteur démarre, then `TurnStarted` est persisté avant le premier appel provider et le turn passe de `queued` à `running`
- [ ] Given chaque `AgentEvent`, when il est reçu, then il est corrélé au `ThreadId`, `TurnId` et à un `EventId` sans modifier son contenu canonique
- [ ] Given `EndTurn`, `Interrupted`, `Exhausted` ou `Error`, when le moteur termine, then le runtime produit exactement un état terminal correspondant
- [ ] Given des tool calls sans résultat au moment d'une interruption, when la terminaison est persistée, then la réconciliation existante a déjà écrit leurs résultats synthétiques
- [ ] Given une erreur du provider, du store ou du pipeline d'outils, when elle survient, then le turn devient `failed`, le thread reste commandable et aucun second moteur ne continue en arrière-plan

#### US-006: Capturer `TurnContext` et `StepContext`

**Description:** As a modèle, I want recevoir un état cohérent à chaque requête so that les changements d'outils et de contexte soient visibles sans rendre le turn incohérent.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given le démarrage d'un turn, when `TurnContext` est capturé, then modèle, effort, permissions, sandbox, workspace et limites restent immuables jusqu'à l'état terminal
- [ ] Given chaque requête modèle du turn, when `StepContext` est construit, then il capture une génération immuable du registre d'outils, du contexte projet, des skills invoquées et des fragments environnementaux
- [ ] Given deux steps sans changement de source, when leur contexte model-visible est sérialisé, then l'ordre et les octets du préfixe stable sont identiques
- [ ] Given une modification MCP staged pendant un sampling, when le sampling continue, then le step actif garde son catalogue et le step suivant voit la nouvelle génération
- [ ] Given un fragment contextuel dépassant 32 KiB ou l'agrégat nouveau dépassant 64 KiB, when le contexte est construit, then il est borné avec diagnostic et aucun item injecté ne dépasse la limite
- [ ] Given une source dynamique illisible ou mal formée, when le step est capturé, then le runtime conserve la dernière valeur valide ou omet la section avec warning, sans élargir de permissions ni faire échouer le transcript

#### US-007: Steerer un turn actif

**Description:** As a utilisateur, I want envoyer une correction au turn en cours so that le modèle adapte sa prochaine requête sans attendre une fin de turn artificielle.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005, US-006

**Acceptance Criteria:**
- [ ] Given un turn `running`, when un steer portant l'`expected_turn_id` correct est accepté, then l'input est persisté et visible dans l'état pending en moins de 100 ms
- [ ] Given un sampling provider avec des deltas non commit, when un steer arrive, then seul ce sampling est annulé, `StreamReset` est émis, l'input est appendu et un nouveau step est samplé
- [ ] Given un outil en cours, when un steer arrive, then il reste en file jusqu'à la persistance des résultats de l'outil puis entre avant le sampling suivant
- [ ] Given plusieurs steers, when ils sont drainés, then leur ordre d'acceptation est conservé et chacun n'apparaît qu'une fois dans le transcript
- [ ] Given un `expected_turn_id` périmé, when le steer est soumis, then il est refusé avec l'état courant et ne devient pas silencieusement un nouveau turn
- [ ] Given une course entre steer et événement terminal, when l'actor les sérialise, then le steer appartient soit au turn actif avant son terminal, soit à un nouveau turn queued après lui, sans perte ni double consommation

#### US-008: Unifier interruption et shutdown hiérarchiques

**Description:** As a utilisateur, I want qu'une interruption termine le modèle, les outils et les processus descendants so that l'état affiché corresponde aux tâches réellement arrêtées.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004, US-005

**Acceptance Criteria:**
- [ ] Given le runtime racine, when threads, turns, outils et agents sont créés, then chacun reçoit un child token et aucun clone ne crée accidentellement un domaine d'annulation indépendant
- [ ] Given une interruption utilisateur, when elle est acceptée, then l'acquittement arrive en moins de 100 ms et la propagation n'annule ni thread frère ni parent
- [ ] Given un provider stream, un backoff, une compaction ou un outil coopératif, when le turn est annulé, then la branche en cours sort et le turn devient `interrupted` après réconciliation
- [ ] Given un processus shell ou une session exec, when le turn est annulé, then l'arbre de processus est terminé avant l'événement terminal
- [ ] Given une tâche non coopérative, when elle dépasse 2 s après annulation, then elle est abortée, drainée et signalée dans la trace; le shutdown complet reste inférieur à 3 s
- [ ] Given deux interruptions du même turn ou une interruption après terminal, when elles sont soumises, then l'opération est idempotente et aucun second terminal n'est écrit

### EP-003: Reprise et branches

Cet epic rend la reprise idempotente et transforme fork et rewind en opérations de thread traçables et non destructives.

**Definition of Done:** un thread peut être repris après crash, forké à un turn terminal et rewound sans modifier les octets durables de sa source.

#### US-009: Reprendre un thread et dédupliquer les soumissions

**Description:** As a utilisateur, I want reprendre exactement le dernier état durable so that un crash ou un retry client ne répète ni input ni effet.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-003, US-005

**Acceptance Criteria:**
- [ ] Given un thread fermé proprement, when il est repris, then transcript, dernier état, turn courant, config capturée et relations d'agents sont reconstruits
- [ ] Given un `TurnStarted` sans terminal après crash, when le thread est repris, then un unique événement de recovery le marque `interrupted` après réconciliation des appels sans résultat
- [ ] Given un `client_message_id` déjà accepté, when le client le resoumet, then le runtime retourne l'identifiant original et n'append ni message ni turn supplémentaire
- [ ] Given une session v1, when elle est reprise, then un `ThreadId` stable est dérivé ou matérialisé une seule fois et tous les messages existants restent visibles
- [ ] Given 10 000 événements locaux, when le store les rejoue en build release sur la machine de référence, then le p95 de 20 reprises est inférieur à 500 ms
- [ ] Given une corruption non terminale, when la reprise est demandée, then elle échoue en nommant l'offset et ne démarre aucun actor sur un état partiel

#### US-010: Forker à une frontière de turn matérialisée

**Description:** As a utilisateur, I want créer une branche indépendante à un turn précis so that j'explore une alternative sans coupler le cycle de vie des deux threads.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given un turn terminal, when un fork est demandé, then le store source est flushé avant la copie du préfixe durable
- [ ] Given le thread enfant, when il est inspecté, then il porte `parent_thread_id`, `fork_turn_id`, `fork_event_id` et un nouvel identifiant
- [ ] Given le fork terminé, when parent et enfant reçoivent ensuite des turns, then leurs fichiers et états divergent sans écriture croisée
- [ ] Given la suppression ou l'archivage ultérieur du parent, when l'enfant est repris, then il reste lisible car son préfixe est matérialisé
- [ ] Given un turn actif, un identifiant inconnu ou une erreur de copie, when le fork est demandé, then l'opération échoue sans publier d'enfant partiel et le parent reste byte-identique
- [ ] Given une session de 10 000 événements, when elle est forkée, then la durée et les octets copiés sont consignés pour décider ultérieurement si un store référencé est justifié

#### US-011: Rewind non destructif et provenance des branches

**Description:** As a utilisateur, I want revenir à un turn antérieur par création de branche so that l'historique original reste auditable et récupérable.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-010

**Scope note (v1.2):** cette story possède l'opération de branche et sa provenance, au niveau du runtime et du store. La surface de commande (`/fork` sans argument, `/fork <turn-id>`, `/rewind <turn-id>`, bascule du client vers la branche) appartient à US-017, dont l'AC1 la nommait déjà ; elle y est reprise mot pour mot plutôt que décrite deux fois. Aucun critère n'est abandonné : les trois critères de commande sont listés dans US-017.

**Acceptance Criteria:**
- [ ] Given un turn terminal nommé du thread, when une branche est demandée au runtime, then elle est matérialisée à cette frontière et le thread source n'est ni tronqué, ni réécrit, ni supprimé
- [ ] Given un turn actif, un turn non terminal ou un identifiant étranger, when une branche est demandée, then elle est refusée avec une raison typée et aucun fichier n'est créé
- [ ] Given la liste des sessions, when une branche y figure, then son `ThreadId`, son `parent_thread_id` et son point de fork sont inspectables sans ouvrir le thread ni lire son transcript
- [ ] Given une branche dont la source a été supprimée, when la liste est reconstruite, then la branche garde sa provenance

### EP-004: Sous-agents bornés

Cet epic réutilise le même runtime pour des enfants parent-owned, avec limites fixes, autorité réduite et handoff structuré.

**Definition of Done:** un parent peut créer, observer, steerer et interrompre jusqu'à quatre enfants lecture seule; chaque enfant possède un thread durable et ne peut élargir ni son périmètre ni celui de ses frères.

#### US-012: Persister le graphe et réserver les leases d'agents

**Description:** As a runtime, I want réserver atomiquement les places d'agents et persister leur filiation so that le parallélisme reste borné après reprise.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004, US-008, US-009

**Acceptance Criteria:**
- [ ] Given un spawn, when une place est disponible, then un `AgentId`, une edge parent-enfant et un lease sont réservés atomiquement avant la création du thread enfant
- [ ] Given un thread racine, when quatre enfants sont actifs, huit ont déjà été créés ou la profondeur dépasserait 1, then le spawn est refusé sans appel provider
- [ ] Given deux spawns concurrents pour la dernière place, when ils sont arbitrés, then un seul réussit
- [ ] Given un échec de création du store ou du moteur enfant, when le spawn échoue, then le lease est libéré et un événement d'échec durable conserve la cause
- [ ] Given une reprise, when le graphe est reconstruit, then les enfants qui n'ont aucun terminal sont marqués interrompus et aucune place fantôme ne reste occupée
- [ ] Given les limites v1, when le runtime démarre, then elles proviennent de constantes testées et d'aucune clé de configuration

#### US-013: Spawn, list et wait d'enfants lecture seule

**Description:** As a modèle parent, I want déléguer une tâche isolée et attendre son résultat so that les explorations parallèles ne polluent pas mon contexte.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006, US-012

**Acceptance Criteria:**
- [ ] Given une tâche de spawn, when elle est acceptée, then l'enfant reçoit un thread, un transcript, un token enfant, le modèle du parent et un `TurnContext` propre
- [ ] Given l'autorité par défaut, when l'enfant construit ses outils, then seuls les outils sans mutation et les capacités explicitement autorisées sont exposés
- [ ] Given `list_agents`, when il est appelé, then il retourne ID, parent, état, tâche, turn actif et temps écoulé sans injecter les transcripts
- [ ] Given `wait_agent`, when aucun enfant n'est terminal dans la fenêtre de 10 s, then l'appel rend un état `running` plutôt que de bloquer indéfiniment
- [ ] Given la fin ou l'annulation du parent, when elle survient, then tous ses enfants actifs reçoivent l'annulation avant le terminal du parent
- [ ] Given un échec provider ou store dans un enfant, when il survient, then le parent et les frères restent actifs et l'échec est observable

#### US-014: Envoyer, poursuivre et interrompre un enfant

**Description:** As a modèle parent, I want envoyer une précision ou interrompre mon enfant so that je puisse corriger sa trajectoire sans créer un second enfant.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007, US-008, US-013

**Acceptance Criteria:**
- [ ] Given un enfant idle, when le parent envoie un follow-up, then un nouveau turn enfant est créé
- [ ] Given un enfant running, when le parent envoie une précision, then elle emprunte le même protocole de steering que l'utilisateur
- [ ] Given un enfant actif, when le parent demande son interruption, then seule sa branche d'annulation est déclenchée
- [ ] Given un AgentId inconnu, terminal ou appartenant à un autre parent, when une opération est demandée, then elle est refusée sans révéler le transcript de l'agent
- [ ] Given un `client_message_id` dupliqué, when le follow-up est rejoué, then il n'est exécuté qu'une fois
- [ ] Given plusieurs messages parentaux concurrents, when ils sont acceptés, then leur ordre de mailbox est conservé

#### US-015: Handoff borné et provenance des résultats

**Description:** As a modèle parent, I want recevoir un résumé et des références d'artefacts so that le travail enfant soit exploitable sans copier tout son contexte.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-013

**Acceptance Criteria:**
- [ ] Given un enfant terminal, when le handoff est produit, then il porte AgentId, ThreadId, état, résumé, chemins relatifs d'artefacts et hash du diff quand ils existent
- [ ] Given un résumé, when il dépasse 8 000 caractères, then il est tronqué avec une indication et le transcript complet reste accessible seulement via le thread enfant
- [ ] Given le handoff injecté au parent, when il entre dans le contexte, then il est marqué untrusted et ne modifie aucune permission
- [ ] Given une sortie contenant secret, token ou valeur d'environnement scrubbed, when le handoff est construit, then la valeur n'est ni persistée dans le résumé ni journalisée
- [ ] Given un enfant failed ou interrupted sans résumé, when il termine, then le parent reçoit un handoff structuré portant l'état et la cause bornée plutôt qu'un silence
- [ ] Given un handoff déjà publié, when le terminal enfant est rejoué après reprise, then aucun doublon n'est injecté

#### US-016: Valider un enfant mutateur dans un worktree isolé

**Description:** As a mainteneur, I want vérifier la faisabilité d'un worktree enfant sous le sandbox actuel so that la mutation parallèle ne soit pas livrée sur une hypothèse de sécurité.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-012, US-013

**Acceptance Criteria:**
- [ ] Given un dépôt Git de fixture, when un worktree est créé dans le répertoire temporaire déjà writable, then l'enfant peut modifier sa copie sans toucher au worktree parent
- [ ] Given Landlock et les gardes de `.git`, when création, exécution et cleanup sont testés dans un child process, then les écritures internes nécessaires sont identifiées et aucune permission générale n'est ajoutée aux outils
- [ ] Given un dépôt sale, un dépôt non Git, un worktree déjà verrouillé ou un cleanup en échec, when le spike s'exécute, then chaque cas produit un verdict et une procédure de récupération
- [ ] Given une mutation enfant, when le résultat est rendu, then aucun merge, commit, apply ou suppression n'est automatique
- [ ] Given que l'isolation exige d'élargir le sandbox global ou de laisser écrire `.git` au modèle, when le spike conclut, then la capacité mutatrice est rejetée et un follow-up PRD n'est pas ouvert comme livré
- [ ] Given le résultat du spike, when US-016 se termine, then une décision go/no-go chiffrée est ajoutée à `docs/DECISIONS.md`; aucune surface de production n'est exposée par cette story

### EP-005: Clients et vérification

Cet epic fait consommer le runtime par les deux clients, retire l'orchestration legacy et rend les invariants mesurables.

**Definition of Done:** TUI et headless utilisent exclusivement `ThreadHandle`; aucun `ActiveTurn` ou FIFO post-turn legacy ne subsiste; les races critiques et la reprise sont couvertes par un harness reproductible.

#### US-017: Migrer le TUI vers `ThreadHandle`

**Description:** As a utilisateur interactif, I want que saisie, état et interruption passent par le runtime so that le TUI n'ait plus sa propre sémantique de turn.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-007, US-008, US-009

**Acceptance Criteria:**
- [ ] Given le mode interactif, when un prompt, steer, interrupt, fork, rewind ou shutdown est demandé, then le TUI soumet une opération au `ThreadHandle`
- [ ] Given `/fork` sans argument, when il est exécuté, then il crée une branche au dernier turn terminal via le runtime *(re-hébergé depuis US-011 en v1.2)*
- [ ] Given `/fork <turn-id>` ou `/rewind <turn-id>`, when le turn appartient au thread et est terminal, then la commande demande la branche au runtime à cette frontière *(re-hébergé depuis US-011 en v1.2)*
- [ ] Given `/rewind`, when la branche est prête, then le client bascule vers elle sans tronquer, réécrire ni supprimer le thread source *(re-hébergé depuis US-011 en v1.2)*
- [ ] Given les événements du runtime, when ils arrivent, then l'état TUI affiche thread, turn, état, nombre d'inputs pending et activité sans relire le store
- [ ] Given un prompt envoyé pendant `running`, when Enter est pressé, then il devient un steer et non une FIFO post-turn
- [ ] Given la migration terminée, when le code est recherché, then `ActiveTurn`, son compteur `u64`, son `JoinHandle` direct et la FIFO de prompts legacy sont supprimés
- [ ] Given une fermeture du canal runtime ou un store failed, when le TUI le détecte, then il restaure le terminal et affiche une erreur terminale sans panic
- [ ] Given un terminal de 40 colonnes, when les nouveaux états sont rendus, then aucun overflow horizontal n'apparaît et chaque diff snapshot est inspecté

#### US-018: Migrer le headless et le schéma machine

**Description:** As un intégrateur, I want exécuter le même runtime sans TUI so that les scripts observent les mêmes IDs, transitions et codes de sortie.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005, US-009, US-010, US-011

**Acceptance Criteria:**
- [ ] Given `pyxis -p`, when un prompt est fourni, then le client crée ou reprend un thread par `ThreadHandle` et attend son état terminal
- [ ] Given `--output-format json`, when des événements sont émis, then chaque ligne conserve le schéma existant et ajoute de façon additive thread, turn et event IDs
- [ ] Given un client qui ignore les nouveaux champs, when il lit les lignes existantes, then son comportement reste inchangé et `run_summary` reste la dernière ligne
- [ ] Given `--ephemeral`, when le run s'exécute, then l'adapter mémoire est utilisé et aucun fichier de thread, fork ou agent n'est créé
- [ ] Given Ctrl+C, un stdin vide ou une reprise corrompue, when le cas survient, then l'interruption ou l'erreur passe par le runtime et le code de sortie documenté est non nul
- [ ] Given `--output-last-message`, when le turn est interrupted ou failed après texte partiel, then le fichier contient le dernier message assistant commité et non des deltas reset

#### US-019: Prouver les invariants et rendre l'orchestration observable

**Description:** As a mainteneur, I want un harness de courses et des métriques de lifecycle so that chaque régression d'orchestration soit localisable.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-012, US-013, US-014, US-015, US-017, US-018

**Acceptance Criteria:**
- [ ] Given les courses steer-terminal, interrupt-tool, shutdown-mailbox et spawn-last-slot, when chacune est répétée 1 000 fois avec horloge et IDs déterministes, then aucun input, terminal, lease ni processus n'est perdu ou dupliqué
- [ ] Given les scénarios resume, fork, rewind, enfant failed et parent cancelled, when les E2E full-wiring s'exécutent, then le store reconstruit le même graphe et le même transcript
- [ ] Given `/status`, when il est demandé, then thread ID, turn ID et état, profondeur de queue, nombre d'agents actifs et limites fixes sont visibles sans requête réseau
- [ ] Given `PYXIS_LOG=debug`, when une opération traverse le runtime, then submission ID, transition, latence d'admission, latence d'interruption et durée de cleanup sont tracées sans contenu de prompt
- [ ] Given aucune variable de trace, when le runtime tourne, then aucune nouvelle sortie ni écriture de log n'est produite
- [ ] Given la livraison de l'epic, when la documentation est relue, then `CURRENT_STATUS.md`, `ARCHITECTURE.md` et `EVENT_SCHEMA.md` distinguent état livré et travaux différés, sans modifier les PRD historiques

## Functional Requirements

- FR-01: Le système doit posséder chaque conversation dans un actor local à mailbox bornée.
- FR-02: Un thread ne doit exécuter qu'un turn régulier à la fois.
- FR-03: Thread, turn, step, événement et agent doivent porter des identifiants opaques et stables.
- FR-04: Chaque turn doit suivre un état explicite et produire exactement un terminal.
- FR-05: Toute opération acceptée doit être durable avant son acquittement.
- FR-06: Le store d'orchestration doit avoir un adapter JSONL local et un adapter mémoire de test.
- FR-07: Toutes les sessions v1 doivent rester lisibles et poursuivables.
- FR-08: La configuration stable du turn doit être séparée du contexte rafraîchi avant chaque requête modèle.
- FR-09: Un steer doit être distinct d'un nouveau turn et d'une interruption.
- FR-10: Une interruption doit se propager du parent vers les descendants, jamais dans le sens inverse.
- FR-11: Un terminal interrupted ou failed ne doit être écrit qu'après réconciliation et cleanup.
- FR-12: Une soumission portant un `client_message_id` déjà accepté doit être idempotente.
- FR-13: Un fork doit être matérialisé à une frontière terminale et porter sa provenance.
- FR-14: Un rewind doit créer une branche et ne jamais tronquer le thread source.
- FR-15: Le runtime doit limiter un thread racine à quatre enfants actifs, huit créations et une profondeur.
- FR-16: Un enfant doit recevoir une autorité égale à l'intersection de celle du parent et de sa demande.
- FR-17: Seul le parent propriétaire doit pouvoir steerer, interrompre ou lire le handoff d'un enfant.
- FR-18: Un handoff enfant doit être borné, structuré et marqué untrusted.
- FR-19: TUI et headless doivent utiliser la même interface de runtime.
- FR-20: L'orchestration v1 ne doit ajouter aucune clé de configuration publique.
- FR-21: Les canaux de commandes et d'événements doivent être bornés; aucune file non bornée n'est autorisée.
- FR-22: Le runtime doit exposer ses IDs, états, queues, agents et latences par événements ou traces structurés.
- FR-23: Le runtime ne doit pas introduire app-server distant, teams, task board ou nouveau provider.

## Non-Functional Requirements

- **Admission:** acquittement ou refus d'une commande en moins de 100 ms p95 sur stockage local, mesuré sur 1 000 opérations.
- **Reprise:** reconstruction de 10 000 événements en moins de 500 ms p95 sur 20 runs release sur la machine de référence.
- **Contexte:** construction d'un `StepContext` chaud en moins de 20 ms p95 sur 1 000 steps, hors lecture initiale des fichiers projet.
- **Shutdown:** état terminal sous 2 s pour les tâches coopératives; runtime fermé sous 3 s après abort et drain des stragglers.
- **Files bornées:** mailbox de commandes à 64, flux d'événements live à 256, inputs pending par turn à 16, enfants actifs à 4, enfants créés à 8, profondeur à 1.
- **Fiabilité:** zéro perte, doublon de terminal, lease fantôme ou processus orphelin sur 1 000 répétitions de chacune des quatre courses critiques.
- **Compatibilité:** 100 % des fixtures de session v1 et des lignes JSONL machine existantes restent lisibles.
- **Sécurité:** 100 % des branches qui calculent l'autorité enfant possèdent au moins un test négatif; aucune branche ne peut produire une autorité plus large que le parent.
- **Confidentialité:** aucun prompt, résultat d'outil, token ou variable sensible dans les traces `error`, `warn`, `info` ou `debug`; contenu autorisé uniquement à `trace`.
- **Observabilité:** 100 % des opérations acceptées et transitions portent IDs et durée; chaque terminal porte sa cause.
- **Configuration:** zéro nouvelle clé publique liée à l'orchestration pendant ce PRD.

## Edge Cases & Error States

Systematic coverage of unhappy paths:

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Thread vide | Nouvelle session sans input | Actor idle, aucun turn synthétique | "Prêt." |
| 2 | Mailbox pleine | 64 commandes réservées | Refus avant acceptation et persistance | "File de contrôle pleine, réessaie." |
| 3 | Soumission dupliquée | Même `client_message_id` | Retour de l'ID original, aucun doublon | "Entrée déjà acceptée." |
| 4 | Steer contre turn périmé | `expected_turn_id` ne correspond plus | Refus avec ID et état courants | "Le turn ciblé est terminé." |
| 5 | Course steer-terminal | Terminal et steer arrivent ensemble | Ordre actor déterministe, input jamais perdu | Aucun si accepté; erreur nommée sinon |
| 6 | Interruption pendant stream | Deltas non commit | Sampling annulé, stream reset, terminal après cleanup | "Turn interrompu." |
| 7 | Interruption pendant outil | Processus ou écriture en cours | Résultat synthétique, arbre de processus terminé | "Interruption en cours..." |
| 8 | Tâche non coopérative | Pas de sortie après annulation | Abort à 2 s, drain avant 3 s | "Tâche forcée à s'arrêter." |
| 9 | Ligne finale partielle | Crash pendant append | Troncation au dernier offset valide | "Session récupérée après écriture interrompue." |
| 10 | Corruption médiane | Ligne invalide suivie de données | Reprise refusée, aucun actor démarré | "Session corrompue à l'offset {n}." |
| 11 | Turn incomplet au resume | `TurnStarted` sans terminal | Recovery interrupted écrit une fois | "Le dernier turn a été interrompu par l'arrêt précédent." |
| 12 | Fork pendant turn actif | `/fork` avant terminal | Refus sans fichier partiel | "Interromps ou termine le turn avant de forker." |
| 13 | Parent supprimé | Reprise d'un fork matérialisé | Enfant autonome et lisible | Aucun |
| 14 | Limite d'agents | Quatre actifs, huit créés ou profondeur >1 | Spawn refusé avant provider | "Limite de sous-agents atteinte." |
| 15 | Enfant qui échoue | Provider, store ou outil failed | Handoff failed, frères et parent actifs | "Sous-agent {id} en échec: {cause}." |
| 16 | Commande sur agent étranger | AgentId d'un autre parent | Refus sans fuite de transcript | "Sous-agent inaccessible depuis ce parent." |
| 17 | Permission parent réduite | Changement avant nouveau turn enfant | Nouveau TurnContext prend l'intersection réduite | "Périmètre du sous-agent mis à jour." |
| 18 | Client live déconnecté | Receiver d'événements fermé | Événements restent durables, actor continue ou shutdown selon propriétaire | "Client déconnecté; thread conservé." |
| 19 | Worktree impossible | Hors Git ou sandbox incompatible | Spike no-go, aucune mutation exposée | "Sous-agent mutateur indisponible dans ce workspace." |
| 20 | Shutdown avec commandes pending | Quit pendant une file non vide | Admission fermée, commandes non acceptées refusées, tâches suivies drainées | "Arrêt de Pyxis en cours." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | `agent-runtime` duplique progressivement `run_agent` | Medium | High | US-001 bloque la suite si retry, compaction ou dispatch doivent être recodés; `TurnRunner` reste le seul seam |
| 2 | Migration laissant deux orchestrateurs actifs | High | High | US-017 exige la suppression d'`ActiveTurn`, du `JoinHandle` direct et de la FIFO legacy |
| 3 | Race entre terminal, steer et persistance | High | High | Actor unique, persist-before-ack, IDs idempotents et harness 1 000 répétitions |
| 4 | Annulation laissant un processus ou un lease fantôme | Medium | High | Tokens hiérarchiques, TaskTracker, timeout 2 s, abort puis drain avant terminal |
| 5 | Sous-agent héritant trop d'autorité | Medium | Critical | Intersection parent-demande, lecture seule par défaut, tests négatifs sur chaque branche |
| 6 | Copy-on-fork trop coûteux pour les longues sessions | Low | Medium | Benchmark 10 000 événements; le `ThreadStore` permet une optimisation future sans changer l'interface |
| 7 | Reprise v1 ambiguë faute d'IDs historiques | Medium | Medium | Dérivation stable ou matérialisation unique, fixtures de migration et aucune réécriture du préfixe |
| 8 | Le scope dérive vers app-server et agent teams | Medium | High | Non-goals, zéro protocole réseau, profondeur 1 et aucun task board |
| 9 | `tokio-util` ajoute une abstraction sans remplacer l'ancienne | Medium | Medium | US-008 supprime l'annulation maison après migration; interdiction de conserver deux arbres de tokens |
| 10 | Worktree mutateur contourne les protections Git | Medium | Critical | US-016 est un spike sans surface de production; no-go si `.git` doit être exposé au modèle |

## Non-Goals

Explicit boundaries: what this version does NOT include:

- App-server JSON-RPC, WebSocket, remote relay ou protocole ACP exposé.
- Agent teams, task board partagé, communication pair-à-pair, élection de leader ou shutdown distribué.
- Automatisations, tâches cloud, marketplace, plugins, mémoire vectorielle ou recherche web intégrée.
- Nouvel adapter provider, changement du wire ChatGPT ou mode `previous_response_id`.
- Refonte visuelle du TUI; seules les surfaces d'état nécessaires au runtime sont ajoutées.
- Rollback automatique des fichiers, merge automatique de worktrees ou commit produit par Pyxis.
- Sous-agent mutateur livré. US-016 produit seulement un go/no-go pour un PRD ultérieur.
- Configuration utilisateur des tailles de mailbox, limites d'agents, profondeur, délais ou stratégie de fork.
- Store SQLite, index de recherche ou fork référencé copy-on-write dans cette version.
- Compatibilité multi-OS du sandbox. Le runtime reste portable en données, mais le support produit demeure Linux-first.

## Files NOT to Modify

- `/home/arthur/dev/codex/**` - dépôt de référence strictement read-only
- `tasks/prd-codex-orchestration.md` et son status JSON - historique déjà livré, centré sur le wire et le contexte modèle
- `tasks/prd-harness-parity.md`, `tasks/prd-harness-capabilities.md`, `tasks/prd-parite-codex-par-le-code.md` et leurs status JSON - historiques de livraison, aucune réécriture rétroactive
- `tasks/prd-codex-tui-parity.md` et son status JSON - les deux stories en review restent suivies dans leur propre workflow
- `spikes/**` - artefacts Phase 0 jetables, jamais réintégrés
- `crates/agent-auth/**` - OAuth et keyring hors scope
- `crates/agent-provider/src/chatgpt.rs` et `chatgpt_events.rs` - wire SSE hors scope, sauf changement minimal démontré indispensable par US-001
- `crates/agent-sandbox/src/fs.rs` et `proxy.rs` - politiques et backends de confinement hors scope
- `LICENSE` et `NOTICE-CODEX.md` - aucune réécriture; ajout de provenance uniquement si une source est copiée ou adaptée

## Technical Considerations

Frame as questions for engineering input, not mandates:

- **Architecture:** le nouveau module doit-il être `agent-runtime` ou une sous-partie d'`agent-core`? Recommandation: crate dédié, car TUI, headless et sous-agents sont trois appelants réels et `agent-core` doit rester le moteur de turn. US-001 confirme ou bloque ce choix.
- **Interface moteur:** `run_agent` doit-il accepter un `StepContextSource` et un `InputQueue`, ou faut-il extraire un `AgentRun` pilotable? Recommandation: commencer par les deux interfaces minimales et refuser toute duplication de la boucle.
- **Store:** `ThreadStore` doit-il adapter le `Session` existant ou le remplacer? Recommandation: le runtime possède `ThreadStore`; un adapter de turn implémente le trait `Session` existant pour `run_agent`.
- **IDs:** faut-il ajouter `uuid` avec UUIDv7 ou utiliser `rand` déjà présent derrière un `IdGenerator`? Recommandation: décider en US-002 selon coût de dépendance, en gardant les newtypes indépendants du générateur.
- **Cancellation:** `tokio-util` doit-il être ajouté avec la feature `rt`? Recommandation: oui pour `CancellationToken` et `TaskTracker`; `JoinSet` reste réservé aux ensembles bornés dont le superviseur doit lire les résultats.
- **État client:** le dernier état doit-il voyager par `watch` et les événements par `mpsc`? Recommandation: oui; `watch` ne doit jamais transporter un event log et ses borrows doivent rester hors des awaits.
- **Fork:** faut-il référencer le parent ou matérialiser le préfixe? Recommandation v1: matérialiser, mesurer, puis conserver l'interface de store si une optimisation devient nécessaire.
- **Contexte:** faut-il porter tout le `WorldState` de Codex? Recommandation: non; définir seulement des sections bornées déjà consommées par Pyxis, dans un ordre stable, sans registry de contributors spéculative.
- **Worktree:** un worktree temporaire peut-il être créé sans exposer `.git` au modèle et sans élargir Landlock? US-016 est le seul décideur; no-go par défaut.
- **Migration:** comment dériver un `ThreadId` stable pour une session v1? Recommandation: hash du chemin canonique et de la première entrée, puis matérialisation d'un meta v2 au premier append; confirmer la portabilité des chemins dans US-009.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Inputs envoyés pendant un turn qui influencent ce turn | 0 %, FIFO post-terminal | 100 % des steers acceptés au prochain point sûr | Fin EP-002 | Tests fake provider et trace de 20 sessions |
| Turns avec identité durable et terminal unique | 0 %, compteur process-local | 100 % | Fin EP-002 | Validation du store et invariant de transition |
| Processus orphelins après interrupt/shutdown | Non garanti sur `JoinHandle::abort` | 0 sur 1 000 courses | Fin EP-002 | Harness de processus enfant |
| Reprise de 10 000 événements | Non mesurée, messages seulement | <500 ms p95 sur 20 runs release | Fin EP-003 | Benchmark reproductible |
| Forks avec provenance parent et point de branche | 0 % | 100 % | Fin EP-003 | Inspection des métadonnées et tests reprise |
| Soumissions dupliquées exécutées deux fois | Non détectées | 0 sur 1 000 replays | Fin EP-003 | Test `client_message_id` |
| Sous-agents Pyxis actifs | 0 | 4 concurrents lecture seule, 8 créations, profondeur 1 | Fin EP-004 | Tests registry et session dogfood |
| Autorités enfant plus larges que le parent | N/A | 0 sur matrice complète des modes | Fin EP-004 | Tests négatifs de permission |
| Clients utilisant une orchestration distincte | 2, TUI et headless câblés séparément | 0, tous sur `ThreadHandle` | Fin EP-005 | Recherche statique et E2E |
| Nouvelles clés de configuration | Baseline 13 clés reconnues | 0 | Fin PRD | Diff de `KNOWN_KEYS` et aide CLI |
| Sessions dogfood sans bascule pour manque d'orchestration | 0 mesurée | 20 consécutives | Month-6 | Journal manuel de sessions |

## Open Questions

- US-001, owner engineering, avant EP-002: le seam minimal garde-t-il `run_agent` intact ou exige-t-il l'extraction d'un `AgentRun` pilotable?
- US-002, owner engineering, avant US-003: UUIDv7 justifie-t-il une dépendance ou le générateur injectable peut-il réutiliser `rand` sans perdre l'inspectabilité?
- US-009, owner engineering, avant migration: la dérivation d'ID des sessions v1 doit-elle dépendre du chemin ou uniquement de leur premier contenu durable?
- US-016, owner engineering et security review, avant tout PRD de mutation parallèle: un worktree temporaire respecte-t-il Landlock et la protection de `.git` sans exception générale?
- Après vingt sessions dogfood, owner Arthur: les limites 4 actifs, 8 créations et profondeur 1 restent-elles fixes ou une exigence mesurée justifie-t-elle une configuration?
- Après le benchmark 10 000 événements, owner engineering: le store JSONL nécessite-t-il un index ou un checkpoint supplémentaire, ou le scan reste-t-il sous la cible?
[/PRD]
