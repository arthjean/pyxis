[PRD]
# PRD: Pyxis : contrats du runtime frontier et fiabilité agentique

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-28 | Arthur Jean | Initial draft fondé sur le traçage bout en bout de Pyxis et Codex CLI |

## Problem Statement

1. Pyxis sélectionne encore les instructions, les capacités de requête, le parallélisme et plusieurs limites par heuristiques sur le slug. Le catalogue `/models` est réduit au nom, aux efforts et à la fenêtre de contexte (`crates/agent-provider/src/models.rs:21-35`), tandis que `model_profile` recrée un profil partiel (`crates/agent-provider/src/chatgpt.rs:51-77`). Le modèle réellement servi peut donc recevoir un prompt, un dialecte Responses ou des outils incompatibles avec son contrat.
2. Le provider ignore `response.end_turn`. Une réponse `completed` sans outil devient toujours `EndTurn` (`crates/agent-provider/src/chatgpt_events.rs:285-310`), alors que Codex poursuit explicitement lorsque `end_turn=false` (`/home/arthur/dev/codex/codex-rs/core/src/session/turn.rs:2369`). Pyxis peut arrêter un travail que le modèle déclare inachevé.
3. Le contexte de step contient les outils annoncés, mais l'exécution appelle le dispatcher global. Un changement de registre entre l'annonce et le dispatch peut donc faire exécuter un plan différent de celui vu par le modèle (`crates/agent-core/src/agent.rs:511`, `crates/agent-core/src/agent.rs:1011`). Les résultats reviennent ensuite sous une forme principalement textuelle, avec un plafond global en octets, ce qui perd le statut terminal, la structure MCP et la raison exacte d'une troncation.
4. Pyxis possède un historique canonique, des snapshots `TurnContext` et `StepContext`, une compaction et une reprise durable. Il ne possède cependant pas d'identité stable du prompt effectif ni de baseline explicite pour les changements de modèle, d'instructions, d'outils et de contexte. Une compaction ou un changement de modèle peut donc invalider implicitement des hypothèses sans transition durable.
5. Deux défauts de projection et de durabilité contredisent le contrat publié du runtime. Après avoir persisté et acquitté un input, `on_submit` le perd de la projection live si la promotion `Running` échoue (`crates/agent-runtime/src/thread.rs:810-835`, `crates/agent-runtime/src/thread.rs:997-1027`). `record_terminal` mute `last_turn`, publie un terminal synthétique et libère le slot même si l'append terminal échoue (`crates/agent-runtime/src/thread.rs:1083-1105`, `crates/agent-runtime/src/thread.rs:1190-1228`).
6. Au cold resume, les résultats synthétiques d'outils sont ajoutés au vecteur en mémoire, puis seuls les terminaux de recovery sont appendus (`crates/agent-runtime/src/resume.rs:158-175`, `crates/agent-runtime/src/thread.rs:384-404`). Un second crash avant le prochain sync peut donc reconstruire encore le transcript non réparé. Les retries réseau sont bornés et honorent `Retry-After`, mais ils restent génériques et invisibles au client.

**Why now:** les PRD précédents ont livré la surface du harness et le runtime durable. La parité fonctionnelle est élevée, mais l'audit ciblé aux commits Pyxis `e1f262d51928` et Codex `8e271dc02b23` montre que les écarts restants se situent désormais dans les contrats qui déterminent directement la qualité et la récupération. Continuer à ajouter des capacités avant de fermer ces ruptures consoliderait des invariants faux.

## Overview

Ce PRD durcit la chaîne verticale qui va du modèle sélectionné au terminal durable. Un `ResolvedModelRuntime` devient la source unique des instructions, du dialecte Responses, des modalités, du budget, du reasoning, du mode d'outils et de la politique de retry. Il est résolu avant le démarrage du turn, identifié par une empreinte stable et enregistré sans dupliquer les instructions inchangées à chaque turn.

Chaque sampling reçoit ensuite un `PromptSnapshot` et un `StepToolPlan` immuables. Le même plan produit les schémas envoyés au modèle et route les appels qui reviennent. Les outils produisent un `ModelToolResult` autoritatif et borné par tokens. Les changements de modèle, de profil, de contexte ou de baseline deviennent des transitions explicites. `run_agent` reste l'unique boucle modèle-outils, `Vec<Message>` reste l'historique canonique et `ThreadStore` reste l'unique journal.

Enfin, les mutations de l'actor deviennent commit-coupled. Un ack prouve que l'input est durablement `queued`; une promotion `Running` échouée conserve cette queue pour le prochain resume. Un append terminal échoué ne peut provoquer ni terminal observable ni libération de slot. La réparation de reprise est persistée comme une unité idempotente avant l'ouverture de l'admission. Les retries deviennent pilotés par le runtime effectif et visibles sans exposer le contenu des prompts.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Fidélité au contrat modèle | 100 % des fixtures connues résolvent le même profil en interactif et headless | 0 décision de capacité fondée sur un préfixe ou une famille de slug |
| Sémantique terminale | 100 % des fixtures `end_turn=false` poursuivent et 100 % des causes `incomplete` sont classifiées | 0 fin prématurée observée sur 100 sessions dogfood |
| Cohérence des outils | 100 % des appels sont validés et routés par le plan annoncé au step | 0 appel exécuté depuis une génération différente sur 10 000 courses |
| Durabilité actor | 0 ack ou terminal non durable sur 1 000 fautes injectées par point de commit | 0 divergence après 100 sessions reprises |
| Réparation après crash | 100 % des appels orphelins sont réparés une fois avant admission | 0 réparation dupliquée sur 1 000 séquences crash-resume-crash |
| Observabilité des retries | 100 % des tentatives portent cause, ordinal et délai sans contenu utilisateur | 0 sampling supplémentaire nécessaire pour identifier la cause sur 100 sessions |

## Target Users

### Arthur Jean, mainteneur et dogfooder principal

- **Role:** développeur de Pyxis et utilisateur quotidien de modèles frontier sur des tâches de code longues.
- **Behaviors:** alterne sessions interactives et headless, change de modèle, interrompt, reprend et compare le comportement à Codex CLI.
- **Pain points:** un modèle récent peut être sous-orchestré sans signal, un terminal peut arrêter trop tôt, et une panne de store peut afficher un état que le disque ne contient pas.
- **Current workaround:** revenir à Codex CLI, inspecter le JSONL et relancer manuellement une tâche ou une session.
- **Success looks like:** utiliser Pyxis sur vingt sessions consécutives sans bascule causée par un écart de contrat modèle, un appel d'outil incohérent ou une reprise ambiguë.

### Intégrateur d'un harness local

- **Role:** développeur qui pilote Pyxis par le mode headless et ses événements machine.
- **Behaviors:** consomme le JSONL, attend des statuts terminaux stables et automatise retries, interruption et resume.
- **Pain points:** un retry invisible, un terminal non durable ou un résultat d'outil textuel oblige à inférer l'état réel.
- **Current workaround:** ajouter des timeouts, relire le fichier de session et dédupliquer côté appelant.
- **Success looks like:** chaque tentative, résultat et terminal est corrélé, durable et rejouable sans heuristique cliente.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- **Codex CLI:** `ModelInfo` porte instructions, capacités, `use_responses_lite`, `tool_mode`, `comp_hash` et géométrie de contexte (`/home/arthur/dev/codex/codex-rs/protocol/src/openai_models.rs:370-445`). Un `TurnContext` capture ce profil, un `StepContext` lie le plan d'outils au step et `end_turn=false` provoque une continuation. Son protocole app-server rend threads, turns, items, compactions et résultats de commandes observables. [Codex app-server protocol](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
- **Claude Code:** les sessions nommées et reprenables, le stream JSON bidirectionnel, les événements partiels et les limites de turns rendent le cycle agentique pilotable par une machine. [Claude Code CLI reference](https://code.claude.com/docs/en/cli-usage)
- **Gemini CLI:** le mode plan, le policy engine et le registre d'outils séparent explicitement intention, autorisation et exécution. [Gemini CLI plan mode](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/plan-mode.md), [Gemini CLI tools](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/tools.md)
- **Market gap:** Pyxis peut conserver une surface plus petite tout en offrant des contrats plus déterministes, un taint plus strict et une récupération plus explicite.

### Confirmed Gap Classification

| Finding | Classification | Evidence |
|---------|----------------|----------|
| `end_turn` ignoré | Écart fonctionnel | `chatgpt_events.rs:285-310` contre Codex `turn.rs:2369` |
| Profil modèle incomplet et heuristique | Écart architectural | `models.rs:21-35`, `chatgpt.rs:51-77`, Codex `openai_models.rs:370-445` |
| Specs de step et dispatcher non liés | Écart fonctionnel causé par l'architecture | `agent.rs:511`, `agent.rs:1011`, Codex `step_context.rs:12` |
| Résultat outil principalement textuel | Écart fonctionnel | `crates/agent-core/src/tools.rs:18-32` |
| Baseline et transitions de contexte implicites | Différence architecturale | `crates/agent-runtime/src/context.rs:157-212`, Codex `context_manager/history.rs:50-110` |
| Terminal publié après échec d'append | Défaut de fiabilité Pyxis | `thread.rs:1190-1228` |
| Réparation resume uniquement en mémoire | Défaut de fiabilité Pyxis | `resume.rs:158-190`, `thread.rs:384-404` |
| Taint, hooks restrictifs, historique canonique et annulation hiérarchique | Avantages Pyxis à préserver | `agent-tools/src/taint.rs`, `agent-tools/src/hooks.rs`, `agent-core/src/agent.rs`, `agent-runtime/src/thread.rs` |
| Sandbox filesystem par appel | Différence architecturale assumée | Landlock est process-wide et irréversible, `docs/CURRENT_STATUS.md:43` |

### Best Practices Applied

- Résoudre une configuration effective typée avant l'exécution et conserver son identité avec le turn.
- Utiliser un unique snapshot par requête pour instructions, contexte, outils, budget et routage.
- Persister avant ack, mutation observable, publication terminale et libération de ressource.
- Séparer sorties live et résultat autoritatif. Une delta streamée ne prouve jamais qu'un effet est commité.
- Fermer le `TaskTracker`, propager l'annulation puis attendre qu'il soit vide avant d'annoncer la fin. [Tokio graceful shutdown](https://tokio.rs/tokio/topics/shutdown)
- Utiliser `sync_data` ou `sync_all` pour la durabilité. `flush` seul garantit l'achèvement de l'I/O, pas sa persistance. [Tokio file documentation](https://docs.rs/tokio/latest/tokio/fs/struct.File.html)
- Ne jamais laisser une donnée de contexte, un résultat MCP ou une injection indirecte élargir permissions, sandbox ou outils.

*Full research sources are linked above. The local Codex checkout remains the primary source for behavior not specified by the public Responses reference.*

## Assumptions & Constraints

### Assumptions (to validate)

- Le canal ChatGPT `/models` sert les champs riches observés par Codex CLI, ou permet de les compléter par un fallback embarqué versionné. US-001 le tranche sur fixtures avant tout câblage.
- `response.end_turn` et `incomplete_details` gardent la sémantique observée dans le client de référence. La documentation publique actuelle ne couvre pas tous les champs propriétaires du canal abonnement.
- `use_responses_lite`, `tool_mode` et `comp_hash` sont des capacités obligatoires et non des suggestions. Un mode obligatoire inconnu doit rendre le modèle incompatible plutôt que déclencher un fallback silencieux.
- Le reasoning chiffré peut être rejoué sans risque uniquement lorsque le descriptor le permet et tant que le modèle, le hash de compatibilité et la fenêtre non compactée restent identiques.
- Une entrée JSONL complète, écrite par le writer unique et `sync_data`, constitue la plus petite unité durable du store actuel. Les transactions multi-lignes ne sont pas supposées crash-atomiques.
- Les profils et fixtures embarqués suffisent à démarrer hors ligne. Le refresh réseau ne doit jamais être une condition d'admission du runtime.

### Hard Constraints

- `run_agent` reste l'unique boucle modèle-outils. Retry, compaction, transition et dispatch ne sont pas réimplémentés dans `agent-runtime`.
- `Vec<Message>` reste l'historique canonique. Aucun second transcript ni clone de `ContextManager` Codex n'est introduit.
- `ThreadStore` reste l'unique journal du thread. Les nouvelles entrées sont additives et les sessions v1 restent lisibles.
- TUI et headless consomment les mêmes `ResolvedModelRuntime`, `PromptSnapshot`, événements et résultats.
- Aucun nouveau provider, protocole réseau, service, base de données, réglage CLI ou clé TOML.
- Le taint untrusted, les hooks restrictifs, les protections `.git` et `.pyxis`, les approbations et Landlock ne peuvent être affaiblis.
- Un secret, un bearer token, un encrypted reasoning ou le contenu d'un prompt ne peut apparaître dans un événement de retry ou un log sous le niveau `trace`.
- `/home/arthur/dev/codex` reste read-only. Tout code copié ou adapté suit `docs/codex-port-inventory.md` et `NOTICE-CODEX.md`.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatage du workspace
- `cargo check --workspace --all-targets` - compilation de toutes les cibles
- `cargo clippy --workspace --all-targets` - lints du workspace
- `cargo test --workspace` - tests unitaires, intégration, reprise et contrats

For fixture and fault-injection stories, additional gates:

- Les fixtures réseau ne déclenchent aucun appel live et portent leur provenance.
- Les tests de course utilisent IDs, horloge, store et délais déterministes.

## Epics & User Stories

### EP-001: Contrat runtime effectif du modèle

Cet epic remplace les décisions dispersées par un profil modèle résolu, durable et consommé par toute la requête.

**Definition of Done:** pour tout modèle sélectionnable, Pyxis peut expliquer le descriptor utilisé, produire une requête conforme et respecter sa sémantique terminale sans décision implicite fondée uniquement sur le slug.

#### US-001: Valider et ingérer le descriptor modèle riche

**Description:** As a mainteneur, I want transformer les réponses `/models` et le catalogue embarqué en descriptors complets so that chaque capacité utilisée par le runtime repose sur une source identifiable.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une fixture `/models` issue du canal courant, when elle est parsée, then instructions de base, contexte, compaction, modalités, reasoning, verbosité, parallélisme, dialecte Responses, mode d'outils et `comp_hash` connus sont conservés sans heuristique de slug.
- [ ] Given le catalogue distant et le fallback embarqué, when le même slug existe dans les deux, then la précédence et la provenance du descriptor effectif sont déterministes et testées.
- [ ] Given les modes interactif et headless, when un modèle est résolu, then ils passent par le même resolver et produisent la même empreinte pour les mêmes sources.
- [ ] Given un catalogue vide, mal formé ou indisponible, when la résolution se fait, then le dernier descriptor valide ou le fallback embarqué est utilisé avec diagnostic, sans descriptor partiel.
- [ ] Given un `tool_mode` ou une capacité obligatoire inconnue, when le descriptor est évalué, then le modèle est marqué incompatible avec une raison explicite, jamais rétrogradé silencieusement vers les outils directs.
- [ ] La fixture indique date, endpoint, commit Codex de comparaison et champs volontairement omis ou propriétaires.

#### US-002: Résoudre et capturer `ResolvedModelRuntime`

**Description:** As a moteur de turn, I want recevoir un profil effectif immuable et identifiable so that prompt, wire, budget et outils partagent les mêmes décisions.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given modèle, effort, configuration et descriptor, when un turn démarre, then un `ResolvedModelRuntime` unique est produit avant la transition `Running`.
- [ ] Given un profil résolu, when il est inspecté, then il contient au minimum slug, source, instructions, empreinte, fenêtre, limite de compaction, modalités, reasoning, verbosité, parallélisme, dialecte, mode d'outils, troncation, retry et `comp_hash`.
- [ ] Given plusieurs turns sans changement, when ils sont persistés, then les instructions complètes ne sont écrites qu'une fois par empreinte et chaque turn référence cette empreinte.
- [ ] Given une session ancienne sans profil résolu, when elle est reprise, then elle reste lisible et le prochain turn crée un profil effectif sans réécrire le préfixe historique.
- [ ] Given une instruction dépassant 64 KiB ou un champ obligatoire invalide, when la résolution a lieu, then le turn est refusé avant provider avec une erreur bornée.
- [ ] Une fois `Running` durable, le profil initial ne mute pas. Toute substitution ultérieure est une transition explicite avec ancien profil, nouveau profil et cause.

#### US-003: Piloter le prompt, le wire et les budgets par le profil

**Description:** As a modèle frontier, I want que toute la requête respecte mon descriptor so that Pyxis n'altère pas mes capacités par des défauts génériques.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un profil résolu, when `CanonicalRequest` est construit, then instructions, reasoning, verbosité, parallélisme, modalités, outils et limites proviennent de ce profil.
- [ ] Given un profil standard et un profil Responses Lite, when leurs bodies sont sérialisés, then les fixtures couvrent les différences d'instructions, outils, images et options attendues.
- [ ] Given une fenêtre et un seuil de compaction déclarés, when le budget est calculé, then estimation pré-tour, contrôle mid-turn et événement `ModelTurn` utilisent les mêmes valeurs.
- [ ] Given une image pour un modèle text-only, when le snapshot est validé, then l'appel provider est refusé avant sérialisation avec une raison explicite.
- [ ] Given un profil `code_mode_only` tant que le code mode est hors scope, when le modèle est sélectionné, then le turn ne démarre pas et le diagnostic nomme la capacité manquante.
- [ ] Les fonctions de production `model_profile` et `select_system_prompt` ne décident plus par préfixe ou famille de slug. Le slug exact ne sert qu'à sélectionner un descriptor versionné.

#### US-004: Respecter `end_turn` et les terminaux incomplets

**Description:** As a utilisateur, I want que la boucle suive l'intention terminale du backend so that le travail ne s'arrête ni trop tôt ni sur une cause mal classifiée.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `status=completed` et `end_turn=false` sans outil, when la réponse se termine, then le message assistant est commité et un nouveau sampling démarre sans fabriquer d'input utilisateur.
- [ ] Given `end_turn=true`, when la réponse se termine sans outil, then un unique `EndTurn` est produit.
- [ ] Given l'absence du champ sur une fixture legacy, when la réponse est mappée, then le comportement rétro-compatible est explicite et testé.
- [ ] Given `status=incomplete`, when `incomplete_details.reason` est servi, then max output, filtre de contenu et cause inconnue produisent des résultats distincts.
- [ ] Given `end_turn=false` répété jusqu'à la limite de modèle, when le garde-fou est atteint, then le run termine en `Exhausted` et non en succès.
- [ ] Given un type invalide pour `end_turn` ou un terminal contradictoire, when le provider décode l'événement, then il échoue fail-closed et ne publie aucun `EndTurn`.

---

### EP-002: Atomicité de l'actor et reprise durable

Cet epic aligne l'état mémoire, le flux live et le journal en présence d'une panne d'append, de sync ou d'un crash.

**Definition of Done:** aucun client ne peut observer un ack ou un terminal absent du journal, et tout cold resume persiste sa réparation complète avant d'accepter une commande.

#### US-005: Ajouter le contrat `StoreFailed` et le harness de fautes

**Description:** As a mainteneur, I want un état de santé typé et des fautes injectables so that les clients distinguent une panne du journal d'un terminal de turn.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un thread sain ou un writer en échec, when son statut est lu, then `ThreadStatus` porte `ThreadHealth::Healthy` ou `ThreadHealth::StoreFailed { operation, detail }`.
- [ ] Given une première faute durable, when l'actor la traite, then un unique `RuntimeEventPayload::StoreFailed` additif est publié; ce signal live ne prétend pas avoir été persisté dans le writer fautif.
- [ ] Given `StoreFailed`, when submit, steer, fork ou nouvelle admission est demandé, then l'opération retourne `SubmitError::StoreFailed`; status, interruption locale, drain et shutdown restent disponibles.
- [ ] Given le mode headless, when `StoreFailed` est publié, then un événement machine `thread_store_failed` est émis, le `run_summary` finit en error et le code de sortie est non nul.
- [ ] Given un resume sur un writer sain, when le snapshot durable est reconstruit, then la santé repart à `Healthy`; queued, running et needs_input sont traités depuis le log, jamais depuis l'ancien signal live.
- [ ] Given un `FailingThreadStore`, when il est configuré sur le N-ième create, append, flush, read ou close, then il échoue une fois avec une cause nommée et conserve la trace des appels.
- [ ] Given un append qui touche le writer puis échoue, when un second append est demandé, then le store de test reproduit l'état `Poisoned`.
- [ ] Given un cut après chaque entrée durable d'une séquence, when la snapshot est rejouée, then le test peut redémarrer un actor avec le même générateur d'IDs et la même horloge.
- [ ] Given 1 000 répétitions d'une séquence déterministe, when le harness tourne, then aucune dépendance au scheduler réel ou au réseau n'affecte le résultat.
- [ ] Given une faute non prévue par le scénario, when elle survient, then le test échoue au lieu de la convertir en succès ou timeout.

#### US-006: Coupler l'acceptation et le démarrage au commit

**Description:** As a client du runtime, I want qu'un input accepté reste durablement queued jusqu'à sa promotion so that un échec de démarrage ne le perde pas.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given un thread idle, when une soumission est acceptée, then `InputSubmitted` et l'état `queued` sont durables avant l'unique `Accepted`.
- [ ] Given un append `Running` en échec après la persistance de l'input, when `on_submit` termine, then il retourne l'`Accepted` original, conserve le turn `queued`, ne démarre aucun moteur et ferme l'admission avec `StoreFailed`.
- [ ] Given un input durable resté queued, when le thread est repris sur un store sain, then il est promu `Running` et démarré exactement une fois; seuls les turns déjà `running` ou `needs_input` sont fermés par recovery.
- [ ] Given une soumission dupliquée après une réponse client perdue, when le même `client_message_id` revient, then les identifiants originaux sont retournés et aucun second turn n'est créé.
- [ ] Given un store empoisonné après l'ack, when le client attend le turn, then il observe `StoreFailed` plutôt qu'un terminal de turn et peut récupérer l'identifiant accepté pour le resume.
- [ ] Given un store empoisonné, when une nouvelle soumission arrive, then elle est refusée avant mint d'identifiant observable et le thread reste inspectable.

#### US-007: Coupler terminal, publication et libération de slot

**Description:** As a observateur du runtime, I want qu'un terminal live corresponde toujours à un terminal durable so that état affiché, reprise et admission suivante ne divergent pas.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given un outcome terminal, when son append réussit, then `last_turn`, publication terminale et libération de slot surviennent après ce commit et dans cet ordre.
- [ ] Given un append terminal en échec, when `record_terminal` retourne, then aucun terminal synthétique n'est publié, `last_turn` ne passe pas au nouvel état et aucun turn suivant ne démarre.
- [ ] Given un store fermé ou empoisonné au terminal, when l'actor traite la faute, then le thread entre dans un état fatal inspectable qui n'accepte plus que status et shutdown.
- [ ] Given deux outcomes concurrents du même turn, when l'actor les sérialise, then un seul terminal est commité et le second ne libère aucune ressource supplémentaire.
- [ ] Given une tâche abortée après le délai, when ses destructeurs ne sont pas encore drainés, then aucun terminal n'est tenté avant la fin du drain ou son timeout borné.

#### US-008: Persister la réparation de resume comme une unité idempotente

**Description:** As a utilisateur qui reprend après crash, I want que les tool calls orphelins et les turns ouverts soient réparés durablement avant admission so that un second crash ne réintroduise pas l'anomalie.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006, US-007

**Acceptance Criteria:**
- [ ] Given un transcript avec appels d'outils sans résultat, when le cold resume démarre, then `ThreadStore::commit_recovery` écrit un unique `RecoveryCommit` contenant résultats synthétiques et fermetures de turns avant admission.
- [ ] Given plusieurs turns ouverts et plusieurs appels orphelins, when le `RecoveryCommit` est écrit, then il porte un repair ID, le `next_seq` attendu, les résultats et les fermetures dans un ordre déterministe.
- [ ] Given plusieurs fermetures dans un record, when leurs identités sont mintées, then chacune porte son propre `EventId` et un `seq` logique consécutif; le prochain `seq` suit le dernier embarqué.
- [ ] Given les adapters mémoire et JSONL, when le record est rejoué, then tous deux développent les fermetures en événements logiques ordinaires et projettent les mêmes messages, états et séquences.
- [ ] Given un fork vers un turn fermé par recovery, when son point est résolu, then il référence l'`EventId` logique de cette fermeture embarquée.
- [ ] Given un crash avant le commit de repair, when le thread est repris, then aucune recovery terminale partielle n'est visible et la même repair peut être retentée.
- [ ] Given un crash après le commit de repair, when le thread est repris, then aucun résultat synthétique ni terminal n'est ajouté une seconde fois.
- [ ] Given un résultat réel déjà présent, when la reconciliation analyse le transcript, then il n'est jamais remplacé par un résultat synthétique.
- [ ] Given un échec durable de repair, when l'ouverture est demandée, then aucun actor commandable n'est publié et l'erreur nomme l'offset ou l'opération fautive.

---

### EP-003: Plan d'outils et feedback autoritatif

Cet epic garantit que le modèle appelle exactement le catalogue qu'il a vu et reçoit un résultat structuré qui conserve les informations nécessaires à la prochaine décision.

**Definition of Done:** chaque sampling lie specs, génération et dispatcher dans un plan immuable, et chaque appel produit exactement un résultat modèle borné, corrélé et typé.

#### US-009: Lier exposition et dispatch dans `StepToolPlan`

**Description:** As a modèle, I want que les outils exécutables soient exactement ceux annoncés dans mon step so that un registre dynamique ne change pas rétroactivement le contrat.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un sampling, when le `StepToolPlan` est construit, then il porte une génération, une empreinte, les specs visibles et une vue de dispatch issue du même snapshot.
- [ ] Given un changement MCP ou une suppression staged pendant le stream, when un appel de l'ancienne réponse arrive, then il est routé par l'ancien plan et le plan suivant voit la nouvelle génération.
- [ ] Given un nom absent du plan, when le modèle l'appelle, then aucun outil global n'est exécuté et un résultat d'erreur corrélé est produit.
- [ ] Given un plan `direct` et un profil incompatible avec les appels parallèles, when les specs sont sérialisées et dispatchées, then les deux côtés appliquent la même restriction.
- [ ] Given deux sources identiques, when le plan est reconstruit, then ordre, schémas et empreinte sont byte-identiques.
- [ ] Le chemin de production ne dispatch plus directement par un registre mutable sans vérifier l'identité du plan.

#### US-010: Introduire `ModelToolResult`

**Description:** As a moteur agentique, I want un résultat d'outil autoritatif et typé so that modèle, transcript et clients dérivent leur état de la même source.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given tout outcome d'outil, when il est normalisé, then le statut appartient à success, error, rejected, cancelled, timed_out ou sandbox_denied.
- [ ] Given un résultat, when il est inspecté, then il porte call ID, texte, contenu structuré optionnel, taint, catégorie d'erreur, durée optionnelle et métadonnées de troncation.
- [ ] Given un `ModelToolResult`, when il entre dans l'historique canonique, then les champs typés sont projetés additivement dans `ContentBlock::ToolResult` et survivent à sync, resume et compaction autorisée.
- [ ] Given le modèle et un client humain, when ils consomment le résultat, then leur payload est dérivé de ce même bloc canonique et le transcript ne contient qu'un résultat apparié.
- [ ] Given un refus avant exécution, une annulation ou un panic d'adapter, when le pipeline termine, then un résultat corrélé est produit plutôt qu'un appel orphelin.
- [ ] Given des métadonnées de permission, sandbox ou secret internes, when le résultat est sérialisé pour le modèle, then seules la cause actionnable et les données autorisées sont exposées.
- [ ] Given un ancien message `tool_result`, when il est repris, then il reste lisible sans imposer le nouveau type au format historique.

#### US-011: Borner le feedback par tokens et préserver les terminaux d'exécution

**Description:** As a modèle, I want recevoir la partie décisionnelle d'une sortie volumineuse so that je puisse corriger ou poursuivre sans gaspiller le contexte.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-010

**Acceptance Criteria:**
- [ ] Given une limite de feedback du profil, when un résultat dépasse le budget, then la troncation utilise le compteur de tokens injecté et respecte aussi une borne dure en octets.
- [ ] Given une commande shell, when elle termine, then exit code ou signal, timeout ou cancel, durée et fin de stderr restent présents même si stdout est tronqué.
- [ ] Given un résultat MCP avec `structuredContent`, when il est rendu au modèle, then la structure JSON valide est conservée avec un fallback textuel borné.
- [ ] Given une troncation, when le résultat est sérialisé, then taille originale, taille conservée, stratégie head ou tail et hint de continuation sont explicites.
- [ ] Given Unicode multioctet ou JSON à la frontière de budget, when la coupe est appliquée, then aucune chaîne invalide ni structure JSON partielle n'est produite.
- [ ] Given des deltas live perdus ou ignorés par un client, when le résultat final arrive, then il reste l'unique source autoritative et contient toutes les métadonnées terminales retenues.

---

### EP-004: Identité du prompt et transitions de contexte

Cet epic donne une identité vérifiable à chaque requête et rend les changements de baseline, modèle et reasoning explicites sans dupliquer l'historique.

**Definition of Done:** toute requête peut être reliée à un snapshot unique, une baseline reconstruite et des transitions persistées; compaction, resume et changement de modèle invalident les éléments incompatibles de façon déterministe.

#### US-012: Construire un `PromptSnapshot` unique par sampling

**Description:** As a mainteneur, I want figer tout le contexte model-visible une fois par requête so that sérialisation, estimation et dispatch ne lisent pas des générations différentes.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-009

**Acceptance Criteria:**
- [ ] Given un sampling, when son `PromptSnapshot` est construit, then il référence profil modèle, contexte de step, plan d'outils, historique canonique et fragments éphémères dans un ordre stable.
- [ ] Given ce snapshot, when body, token budget et trace sont produits, then aucun composant dynamique n'est relu avant la fin de la tentative.
- [ ] Given deux samplings sans changement de source ni historique, when leurs snapshots sont comparés, then le préfixe stable et son empreinte sont identiques.
- [ ] Given une normalisation provider, when les appels et résultats orphelins sont filtrés, then seule une copie de prompt est modifiée et `Vec<Message>` reste byte-identique.
- [ ] Given une source dynamique illisible, when le snapshot est construit, then la dernière valeur valide ou l'omission bornée est utilisée avec diagnostic, sans élargir les capacités.
- [ ] Given un agrégat de contexte dépassant 64 KiB hors historique, when le snapshot est validé, then il est borné selon l'ordre de priorité documenté avant l'appel provider.

#### US-013: Versionner la baseline et persister les transitions de contexte

**Description:** As a runtime de thread, I want connaître la baseline qui a produit chaque prompt so that compaction, fork et resume ne diffèrent pas contre un état périmé.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-012

**Acceptance Criteria:**
- [ ] Given le premier snapshot d'une fenêtre, when il est accepté, then une baseline porte les empreintes du profil, des instructions, du contexte projet, des skills et du plan d'outils.
- [ ] Given une transition calculée par `run_agent`, when le provider doit être ouvert, then un appel attendu `Session::record_context_transition` persiste un `SessionEntry::ContextTransition` avant `provider.stream`.
- [ ] Given le même writer JSONL que le transcript, when l'entrée est relue, then `ThreadSnapshot` reconstruit la dernière baseline sans canal actor parallèle.
- [ ] Given une source inchangée, when un nouveau step démarre, then aucun événement de transition redondant n'est persisté.
- [ ] Given une source model-visible modifiée, when le step suivant est construit, then une transition ordonnée ancien vers nouveau fingerprint est durable avant l'appel provider.
- [ ] Given une compaction, un fork dont la coupe précède la baseline ou une session legacy, when le prochain step démarre, then une baseline complète remplace toute référence périmée.
- [ ] Given un resume, when le log est rejoué, then la dernière baseline valide est reconstruite sans relire un contenu distant.
- [ ] Given un append de transition en échec, when le step est prêt, then aucun appel provider ni terminal live ne démarre, le thread passe `StoreFailed` et ferme son admission.
- [ ] Given le resume suivant sur un writer sain, when il reconstruit ce turn ouvert, then la recovery le ferme `Interrupted` après reconciliation avant toute nouvelle admission.
- [ ] Les événements de baseline ne persistent ni prompt utilisateur, ni secret, ni résultat d'outil brut; ils portent identités, hashes et causes.

#### US-014: Encadrer changement de modèle, fallback et `comp_hash`

**Description:** As a utilisateur qui change de modèle, I want une transition compatible avec l'historique so that le nouveau modèle ne reçoit ni instructions ni reasoning de l'ancien contrat.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-012, US-013

**Acceptance Criteria:**
- [ ] Given un changement de modèle au prochain turn, when le nouveau profil est résolu, then la transition durable porte ancien profil, nouveau profil, cause et nouvelle baseline.
- [ ] Given des instructions différentes, when le premier prompt du nouveau profil est construit, then un fragment `<model_switch>` borné annonce le nouveau contrat sans persister deux copies du transcript.
- [ ] Given un changement de `comp_hash`, when le prochain sampling est demandé, then une compaction est terminée avant l'appel provider et la baseline est réinitialisée.
- [ ] Given un échec de compaction requis par `comp_hash`, when il survient, then le nouveau profil n'est pas appelé et l'ancien état durable reste inspectable.
- [ ] Given un fallback overload pendant un turn, when il est autorisé, then effort, budget, outils et modalités sont recalculés depuis le profil de fallback et la transition est observable.
- [ ] Given aucun fallback compatible, when l'overload persiste, then le turn échoue explicitement au lieu de muter seulement le slug.

#### US-015: Gater et durcir le reasoning replay existant

**Description:** As a modèle compatible, I want que le replay déjà implémenté soit activé seulement par mon descriptor so that son bénéfice ne crée pas de 400 ni de frontière incompatible.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] Given un profil qui prouve encrypted reasoning et replay stateless, when le runtime est résolu, then il active le chemin `reasoning_replay` existant sans flag de session séparé.
- [ ] Given un profil sans preuve de compatibilité, when le runtime est résolu, then le chemin reste désactivé et la raison est inspectable.
- [ ] Given compaction, changement de profil ou changement de `comp_hash`, when la baseline est remplacée, then les items reasoning en mémoire sont invalidés avant le sampling.
- [ ] Given un rejet backend imputable au replay, when la tentative échoue, then une seule reprise sans replay est permise, la capacité est désactivée pour le turn et l'événement est observable.
- [ ] Given les invariants déjà livrés de capture, ordre `reasoning` avant `function_call`, skip des orphelins et redaction at rest, when les tests historiques tournent, then ils restent inchangés et sont réutilisés comme préconditions plutôt que réimplémentés.
- [ ] Given un profil sans replay ou une reprise sans replay, when la requête est produite, then le chemin désactivé conserve ses fixtures byte-identiques.

---

### EP-005: Retry, récupération de credential et preuve verticale

Cet epic rend les tentatives visibles, aligne leur politique sur le runtime effectif et prouve la chaîne complète sur les fautes qui traversent plusieurs modules.

**Definition of Done:** chaque retry est classifié et corrélé, la récupération 401 est bornée, et une matrice E2E prouve qu'aucune combinaison de stream, outil, store et resume ne produit un faux terminal ou un effet dupliqué.

#### US-016: Centraliser et observer la politique de retry

**Description:** As a utilisateur, I want connaître chaque tentative et son prochain délai so that une latence réseau ne ressemble pas à une boucle agentique bloquée.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-004

**Acceptance Criteria:**
- [ ] Given un profil et un provider, when un sampling démarre, then `max_attempts`, backoff, jitter, délais serveur, fallback et erreurs retryables proviennent d'une politique résolue unique.
- [ ] `max_attempts` inclut la requête initiale et chaque nouvelle ouverture provider après erreur, refresh, retrait du reasoning replay ou fallback modèle.
- [ ] Given un fallback modèle, when il est appliqué, then l'ordinal reste monotone et le budget d'attempts ne redémarre pas.
- [ ] Given une erreur avant delta, après delta, pendant idle ou pendant backoff, when elle est retryable, then le même moteur d'attempt applique reset, attente et reconstruction du snapshot.
- [ ] Given une tentative planifiée, when elle est publiée, then l'événement porte turn, step, ordinal, cause, délai et fallback éventuel sans contenu utilisateur.
- [ ] Given des deltas non commit avant une erreur, when un retry survient, then un unique `StreamReset` précède la nouvelle tentative et aucun delta abandonné n'entre dans le transcript.
- [ ] Given une erreur terminale ou la dernière tentative, when elle est classifiée, then aucun retry supplémentaire n'est planifié et l'événement final porte la taxonomie exacte.
- [ ] Given une annulation pendant backoff, when le token est déclenché, then l'attente termine avant 100 ms sous horloge de test et aucun retry ne démarre.

#### US-017: Rendre la récupération 401 bornée et explicite

**Description:** As a utilisateur abonné, I want qu'un credential expiré soit rafraîchi une fois selon une taxonomie stable so that un corps d'erreur variable ne tue pas la session ni ne boucle.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-016

**Acceptance Criteria:**
- [ ] Given un 401 compatible avec un refresh, when il est reçu, then le credential manager tente au plus une récupération par sampling avant de reconstruire la requête.
- [ ] Given un corps sans le mot `expired` mais une classification 401 récupérable, when la réponse arrive, then la décision ne dépend pas d'une recherche textuelle seule.
- [ ] Given un refresh réussi, when la requête repart, then les nouveaux headers sont reconstruits et l'ancien bearer n'est ni loggé ni réutilisé.
- [ ] Given un refresh refusé, absent ou déjà tenté, when le 401 persiste, then l'erreur devient terminale avec une action de reconnexion et aucun second refresh.
- [ ] Given une annulation pendant refresh, when elle est déclenchée, then aucun retry provider ne démarre après la fin du refresh.
- [ ] Les événements indiquent tentative et résultat du refresh sans token, account ID complet ni corps sensible.

#### US-018: Prouver les contrats bout en bout et réconcilier la documentation

**Description:** As a mainteneur, I want une matrice de scénarios full-wiring so that les contrats inter-crates restent vérifiables après les prochaines évolutions du wire.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004, US-008, US-011, US-014, US-016, US-017

**Acceptance Criteria:**
- [ ] Given les scénarios `end_turn=false`, outil, retry après delta, fallback modèle et terminal, when ils s'exécutent sur fixtures, then chaque transition et chaque snapshot porte les IDs et fingerprints attendus.
- [ ] Given un registre modifié entre réponse et dispatch, when le scénario s'exécute, then seul le `StepToolPlan` annoncé peut produire un effet.
- [ ] Given une panne d'append sur input, start, terminal ou repair, when chaque cut est repris, then aucun ack fantôme, terminal fantôme, appel orphelin ou effet dupliqué n'apparaît.
- [ ] Given 1 000 courses terminal-retry-cancel et crash-repair-crash, when elles tournent avec horloge déterministe, then les projections mémoire et rejouées sont identiques.
- [ ] Given une fixture de session v1 et les PRD historiques, when la suite de compatibilité tourne, then le préfixe reste lisible et aucun status historique n'est modifié.
- [ ] Given la livraison, when la documentation est relue, then `CURRENT_STATUS.md`, `ARCHITECTURE.md`, `EVENT_SCHEMA.md` et `PROVIDERS.md` distinguent comportements confirmés, capacités incompatibles et limites encore différées.

## Functional Requirements

- FR-01: Le système doit résoudre un descriptor modèle complet avant tout turn.
- FR-02: Le profil effectif doit avoir une empreinte stable, une provenance et une représentation durable dédupliquée.
- FR-03: Prompt, request body, budget, modalités, reasoning, outils et retries doivent consommer le même profil.
- FR-04: Le chemin de production ne doit prendre aucune décision de capacité depuis un préfixe, un suffixe ou une famille de slug; un slug exact ne sélectionne qu'un descriptor.
- FR-05: Un mode obligatoire non supporté doit rendre le modèle incompatible avant appel provider.
- FR-06: `end_turn=false` doit poursuivre la boucle et `end_turn=true` doit produire un terminal unique.
- FR-07: Les causes `incomplete` doivent être conservées dans une taxonomie typée.
- FR-08: Chaque sampling doit posséder un `PromptSnapshot` unique.
- FR-09: Chaque sampling doit posséder un `StepToolPlan` liant exposition et dispatch.
- FR-10: Un outil absent du plan ne doit jamais être exécuté depuis le registre global.
- FR-11: Tout appel d'outil doit produire exactement un `ModelToolResult` apparié.
- FR-12: Les résultats doivent préserver statut, taint, structure utile, durée et troncation.
- FR-13: La troncation modèle doit être bornée par tokens et octets et préserver le terminal d'exécution.
- FR-14: Les transitions de contexte doivent être persistées avant le sampling qu'elles affectent.
- FR-15: Compaction, fork et resume doivent invalider toute baseline incompatible.
- FR-16: Un changement de `comp_hash` doit déclencher une compaction avant utilisation du nouveau profil.
- FR-17: Le reasoning replay ne doit être actif que sous preuve de compatibilité et jamais survivre à une frontière incompatible.
- FR-18: Aucun ack de soumission ne doit précéder un `InputSubmitted` durable; un échec de promotion doit conserver ce turn `queued` pour le resume.
- FR-19: Aucun terminal live ni libération de slot ne doit précéder le commit terminal.
- FR-20: Un échec de commit terminal doit fermer l'admission et préserver un état inspectable.
- FR-21: La réparation de cold resume doit être durable et idempotente avant admission.
- FR-22: Chaque retry doit porter ordinal, cause, délai et IDs sans contenu sensible.
- FR-23: Une récupération 401 doit être bornée à une tentative par sampling.
- FR-24: Les sessions v1 doivent rester lisibles sans réécriture de leur préfixe.
- FR-25: Le système ne doit introduire ni seconde boucle, ni second historique, ni nouveau réglage public.
- FR-26: `StoreFailed` doit être un état de santé live typé, distinct de tout état terminal durable d'un turn.

## Non-Functional Requirements

- **Résolution modèle:** p95 inférieur à 5 ms sur catalogue en mémoire pour 10 000 résolutions, hors refresh réseau.
- **Construction prompt:** p95 inférieur à 20 ms pour 1 000 `PromptSnapshot` avec 64 KiB de contexte éphémère, hors tokenization de l'historique.
- **Plan d'outils:** ordre et empreinte identiques sur 10 000 reconstructions depuis les mêmes sources; 0 allocation non bornée par description ou schéma.
- **Feedback outil:** payload model-visible inférieur ou égal au budget du profil et à 64 KiB; terminal shell présent dans 100 % des résultats tronqués.
- **Admission:** ack ou refus sous 100 ms p95 sur store local hors faute injectée.
- **Annulation:** backoff et refresh annulés sous 100 ms avec horloge de test; shutdown reste sous 3 s.
- **Durabilité:** 0 ack, terminal ou repair non durable sur 1 000 fautes injectées par point de commit.
- **Fiabilité:** 0 doublon d'effet, résultat ou terminal sur 1 000 séquences crash-resume-crash et retry-cancel-terminal.
- **Compatibilité:** 100 % des fixtures v1 et v2 existantes lisibles; 100 % des événements additifs ignorables par un ancien consommateur.
- **Sécurité:** 100 % des branches d'outil et de retry gardent le taint et les hooks restrictifs; 0 secret dans les logs `error` à `debug`.
- **Configuration:** 0 nouvelle clé TOML, variable d'environnement ou option CLI.
- **Réseau de test:** 0 appel externe dans la suite par défaut; toute validation live reste explicitement autorisée et séparée.

## Edge Cases & Error States

Systematic coverage of unhappy paths:

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Catalogue absent | Offline au démarrage | Descriptor embarqué versionné, provenance visible | "Catalogue distant indisponible, profil embarqué utilisé." |
| 2 | Descriptor incompatible | `code_mode_only` sans code mode | Refus avant turn, aucun outil direct silencieux | "Modèle incompatible: code mode requis." |
| 3 | Terminal demande continuation | `completed,end_turn=false` | Commit assistant puis nouveau sampling | Aucun |
| 4 | Terminal contradictoire | Type ou statut incohérent | Decode fail-closed, aucun `EndTurn` | "Réponse modèle hors contrat." |
| 5 | Registre change mid-step | MCP staged pendant stream | Ancien plan route la réponse, nouveau plan au step suivant | Aucun |
| 6 | Outil non annoncé | Call vers nom absent du plan | Aucun effet, résultat d'erreur apparié | "Outil indisponible dans ce step." |
| 7 | Sortie shell volumineuse | Résultat au-dessus du budget | Tail terminal conservé avec métadonnées | "[sortie tronquée: terminal conservé]" |
| 8 | Structured MCP invalide | JSON mal formé ou trop grand | Fallback textuel borné, statut d'erreur si inutilisable | "Résultat MCP invalide." |
| 9 | Append start échoue | Input durable, `Running` refusé | Ack original conservé, turn queued, admission fermée jusqu'au resume | "Turn accepté mais non démarré: reprise requise." |
| 10 | Append terminal échoue | Outcome moteur reçu | Aucun terminal live, admission fermée | "Session en erreur durable, reprise requise." |
| 11 | Crash pendant repair | Cut avant ou après unité de repair | Zéro repair partielle ou dupliquée | "Session récupérée après arrêt précédent." |
| 12 | Baseline absente | Legacy, fork ancien ou compaction | Réinjection complète et nouvelle baseline | Aucun |
| 13 | `comp_hash` change | Même slug, nouveau hash | Compaction avant sampling | "Compatibilité modèle mise à jour, contexte compacté." |
| 14 | Compaction de transition échoue | Summarizer ou store failed | Nouveau profil non appelé | "Changement de modèle suspendu: compaction échouée." |
| 15 | Reasoning replay rejeté | Backend refuse item chiffré | Une reprise sans replay, puis désactivation du turn | "Reasoning replay désactivé pour ce turn." |
| 16 | Retry après delta | Stream coupé après texte live | `StreamReset`, snapshot reconstruit, aucun delta persisté | "Connexion interrompue, nouvelle tentative." |
| 17 | 401 non récupérable | Refresh absent ou déjà tenté | Échec terminal, aucune boucle | "Authentification expirée, reconnecte Pyxis." |
| 18 | Annulation pendant attente | Backoff ou refresh actif | Arrêt sous 100 ms de test, aucun nouvel appel | "Turn interrompu." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Le schema `/models` propriétaire dérive | High | High | Resolver tolérant, fixtures datées, fallback versionné, capacité inconnue fail-closed |
| 2 | `ResolvedModelRuntime` devient une seconde configuration | Med | High | Source unique, aucune clé publique, test statique supprimant les décisions de slug dispersées |
| 3 | `PromptSnapshot` dérive vers un second historique | Med | High | Référencer `Vec<Message>`, normaliser une copie, interdire toute persistance de transcript parallèle |
| 4 | Fix actor laisse une soumission durable sans exécution | Med | High | `InputSubmitted` définit l'ack, promotion séparée, queued rejouable, idempotency key |
| 5 | Une transaction multi-lignes est supposée atomique | Med | High | `RecoveryCommit` est un seul record rejouable avec repair ID et séquences logiques |
| 6 | Feedback structuré consomme plus de contexte | Med | Med | Budget tokens du profil, sérialisation compacte, métriques original/kept |
| 7 | Reasoning replay produit des 400 ou fuite at rest | Med | High | P2, descriptor opt-in, retry unique sans replay, redaction persistante |
| 8 | Refuser `code_mode_only` réduit le catalogue utilisable | High | Med | Diagnostic explicite et mesure; code mode traité dans un PRD séparé seulement si le dogfood justifie son coût |
| 9 | Les événements de retry exposent du contenu sensible | Low | High | Champs allow-listés, tests de redaction, contenu autorisé uniquement à `trace` |
| 10 | Le scope transversal crée une migration big-bang | Med | High | Ordre par epics, adapters compatibles, fixtures byte-identiques et cutover seam par seam |

## Non-Goals

Explicit boundaries: what this version does NOT include:

- Code mode JavaScript, appels imbriqués depuis un runtime JS et `code_mode_only`. Ce sous-système exige un moteur et un modèle de sécurité propres; v1 refuse explicitement les modèles qui l'imposent.
- Recherche différée d'outils, BM25, namespaces lazy ou catalogue distant d'outils. À réévaluer si le catalogue model-visible dépasse le budget mesuré.
- PTY, émulation terminal, limites multi-processus de Codex ou nouveau protocole `exec`. Les pipes persistants actuels restent le choix.
- Sandbox filesystem par appel et escalade Landlock. Le confinement process-wide est conservé; une granularité par commande exige une frontière de processus séparée.
- WebSocket, `previous_response_id`, app-server, Responses stateful ou compaction distante.
- Stop hooks capables de forcer une continuation. Les hooks Pyxis restent restrictifs et observationnels après terminal.
- Refonte TUI, nouveau provider, nouveau système d'auth, nouvelle base de données ou nouveau réglage utilisateur.
- Port ou fork du cœur Codex. Le dépôt local reste une référence read-only.
- Remplacement de `Vec<Message>`, `run_agent`, `ThreadStore` ou du format JSONL existant.

## Files NOT to Modify

- `/home/arthur/dev/codex/**` - référence primaire read-only
- `tasks/prd-pyxis.md`, `tasks/prd-codex-orchestration.md`, `tasks/prd-runtime-orchestration-durable.md` et tous leurs status JSON - historiques, aucune réécriture rétroactive
- `tasks/prd-harness-parity.md`, `tasks/prd-harness-capabilities.md`, `tasks/prd-parite-codex-par-le-code.md` et leurs status JSON - périmètres déjà suivis ailleurs
- `crates/agent-tui/src/render.rs`, `crates/agent-tui/src/composer.rs` et snapshots TUI - aucune refonte visuelle
- `crates/agent-sandbox/**` - modèle Landlock et proxy hors scope
- `crates/agent-auth/**` - flux OAuth et stockage credential hors scope; US-017 réutilise l'interface de refresh existante
- `LICENSE`, `NOTICE-CODEX.md` et `docs/codex-port-inventory.md` - ne modifier que si du code Codex est effectivement copié ou adapté
- `spikes/**` - artefacts jetables historiques

## Technical Considerations

Frame as questions for engineering input, not mandates:

- **Ownership du type:** `ResolvedModelRuntime` doit-il vivre dans `agent-core` comme contrat canonique ou dans `agent-runtime` avec un DTO provider? Recommandation: type canonique dans `agent-core`, parsing du wire dans `agent-provider`, capture durable dans `agent-runtime`.
- **Déduplication:** faut-il persister le profil complet dans chaque `TurnContext` ou une entrée `ModelRuntimeResolved` référencée par fingerprint? Recommandation: entrée dédupliquée, car les instructions peuvent atteindre 64 KiB et changent moins souvent que les turns.
- **État de santé:** quelle quantité de détail exposer dans `StoreFailed`? Recommandation: type et événement sont obligatoires; sérialiser l'opération et une cause bornée/redacted, jamais le contenu en cours.
- **Commit d'admission:** comment relancer le queued au resume? Recommandation: `InputSubmitted` reste l'unité d'ack et la source du texte; une promotion absente distingue le queued récupérable des turns déjà actifs.
- **Repair:** quelle forme wire donner à `RecoveryCommit`? Recommandation: une entrée JSONL unique, écrite par `ThreadStore::commit_recovery`, avec un `EventId` et un `seq` logique par fermeture afin de préserver les points de fork.
- **Plan d'outils:** faut-il cloner un dispatcher generation-bound ou passer un `plan_id` à `dispatch`? Recommandation: vue immuable restreinte, car une validation d'ID seule suivie d'un lookup global garde une course.
- **Context baseline:** faut-il garder `record_context_transition` sur `Session` ou extraire un trait plus étroit? Recommandation v1: étendre `Session`, déjà attendu et implémenté par le même writer que `ThreadStore`; persister fingerprints, versions et causes uniquement.
- **Responses Lite:** quelles différences sont propriétaires au canal abonnement? Recommandation: figer des fixtures de Codex au commit de référence et isoler la sérialisation derrière le dialecte du profil.
- **Incompatibilité modèle:** faut-il masquer un modèle incompatible ou l'afficher disabled? Recommandation: conserver sa visibilité avec raison inspectable; ne pas effectuer de fallback sans cause durable.
- **Retry 401:** l'interface actuelle de credential suffit-elle à classifier la récupération sans toucher `agent-auth`? Recommandation: adapter côté provider et ne rouvrir `agent-auth` que si une preuve de test l'exige.
- **Dépendances:** une nouvelle crate est-elle nécessaire? Recommandation: non. `serde`, `tokio`, `tokio-util`, le tokenizer injecté et les stores existants couvrent le scope.
- **Migration:** comment garder les anciens lecteurs? Recommandation: variantes JSONL additives, champs serde optionnels et fixtures qui prouvent que le préfixe v1 reste byte-identique.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Décisions de capacité par slug en production | 2 chemins confirmés | 0 | Fin EP-001 | Recherche statique + fixtures descriptor |
| `completed,end_turn=false` traité comme fin | 100 % | 0 % | Fin EP-001 | Tests SSE terminal |
| Appels routés hors plan annoncé | Course possible, non mesurée | 0 sur 10 000 | Fin EP-003 | Test registre staged |
| Résultats avec statut terminal typé | 0 % | 100 % | Fin EP-003 | Tests `ModelToolResult` |
| Inputs queued perdus après échec de promotion | 1 chemin confirmé | 0 sur 1 000 fautes | Fin EP-002 | `FailingThreadStore` |
| Terminaux publiés après échec d'append | 1 chemin confirmé | 0 sur 1 000 fautes | Fin EP-002 | Harness actor |
| Repairs persistées avant admission | 0 % | 100 % | Fin EP-002 | E2E crash-resume-crash |
| Retries visibles par événements structurés | 0 % | 100 % | Fin EP-005 | Schéma et tests provider |
| Sessions dogfood sans bascule pour écart de contrat | Non mesuré | 20 consécutives | Month-6 | Journal de dogfood |
| Régressions de session v1 | 0 connue | 0 sur 100 % des fixtures | Chaque epic | Suite compatibilité |

## Open Questions

- US-001, owner provider, avant US-002: quels champs riches le canal abonnement sert-il réellement aujourd'hui, et lesquels doivent venir du fallback embarqué?
- US-001, owner core, avant sélection du prochain modèle par défaut: `gpt-5.6-sol` est-il `code_mode_only` sur le compte dogfood, et doit-il rester visible mais disabled?
- US-004, owner provider, avant merge: l'absence de `end_turn` doit-elle signifier `true` pour toutes les fixtures legacy ou dépendre du statut et des tool calls?
- US-008, owner session, avant implémentation: quel tag JSONL additif permet à un ancien lecteur d'ignorer `RecoveryCommit` sans considérer la ligne comme corrompue?
- US-014, owner core, avant activation: un changement de `comp_hash` sur le même slug est-il observé dans les fixtures et exige-t-il toujours une compaction complète?
- US-015, owner provider, avant passage P2 à production: une validation live explicitement autorisée confirme-t-elle le replay et le retry sans replay sur le canal abonnement?
- Après vingt sessions dogfood, owner Arthur: le refus de `code_mode_only` justifie-t-il un PRD dédié au code mode, ou les modèles directs couvrent-ils le besoin?
[/PRD]
