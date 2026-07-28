[PRD]
# PRD: Parité comportementale totale avec Codex CLI

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-28 | Arthur Jean | Périmètre initial fondé sur Codex `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5` |

## Problem Statement

1. Pyxis expose `gpt-5.6-sol`, mais refuse ce modèle avant l'appel provider parce que son mode `code_mode_only` n'est pas implémenté. Un prompt accepté par l'interface peut donc ne produire aucune réponse utile.
2. Le protocole provider de Pyxis ne représente que les outils function JSON. Il ne peut ni sérialiser un outil custom/freeform ni reconstruire les événements `custom_tool_call` nécessaires à Code Mode.
3. Pyxis possède un runtime durable et un superviseur de sous-agents, mais le binaire ne câble pas le protocole multi-agent v2 attendu par les modèles frontier.
4. Les causes terminales existent dans les événements runtime, mais certaines surfaces TUI et logs les abandonnent. Une défaillance ressemble alors à une absence de réaction.
5. Les audits de parité actuels décrivent des instantanés différents et vieillissent dès que Codex évolue. « Parité totale » n'est donc pas vérifiable sans baseline, matrice contractuelle et fixtures automatisées.

**Why now:** le catalogue Codex du 28 juillet 2026 marque `gpt-5.6-sol` comme `code_mode_only`, `multi_agent_version: v2` et `use_responses_lite: true`. Le défaut n'est plus théorique: il bloque le modèle frontier choisi par l'utilisateur et masque sa cause dans l'interface.

## Overview

Pyxis doit atteindre la parité comportementale du harness et de l'orchestrateur Codex CLI, figée sur le commit `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`. La parité porte sur les contrats observables: catalogue de modèles, outils function/custom, Code Mode, exécution terminale, multi-agent v2, persistance, app-server, MCP, événements, erreurs et surfaces minimales de pilotage.

L'implémentation conserve les invariants Pyxis: cœur headless, abstraction provider générique, journal JSONL commit-coupled, transitions typées, taint untrusted et permissions fail-closed. Elle adopte du code Codex seulement à des frontières autonomes et consignées. Le résultat n'est ni un fork ni une copie de `codex-core`.

Le programme est livré en deux gates. Le gate Harness couvre EP-001 à EP-004 et rend `gpt-5.6-sol` utilisable de bout en bout. Le gate Client couvre EP-005 et EP-006, rend le runtime pilotable par un protocole externe et prouve la parité par des contrats reproductibles.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Exécutions headless `gpt-5.6-sol` terminées sans incompatibilité locale | au moins 95 sur 100 scénarios | au moins 99 sur 100 scénarios |
| Contrats offline Codex applicables à Pyxis | 100 % des fixtures P0 | 100 % de la matrice baseline |
| Échecs terminaux avec cause exploitable sur chaque surface | 100 % en TUI, headless et JSONL | 100 % avec corrélation app-server et trace |
| Double exécution d'un outil après interruption ou reprise | 0 sur 1 000 scénarios injectés | 0 sur 10 000 scénarios injectés |

## Target Users

### Mainteneur et dogfooder Pyxis

- **Role:** Arthur, développeur Linux qui utilise Pyxis quotidiennement avec un abonnement ChatGPT.
- **Behaviors:** lance des prompts interactifs et headless, change de modèle, inspecte le dépôt et compare le comportement avec Codex CLI.
- **Pain points:** Sol est sélectionnable mais inutilisable; une erreur terminale peut être invisible; les écarts de parité sont décrits dans plusieurs documents non synchronisés.
- **Current workaround:** repasser sur `gpt-5.5`, lancer Codex CLI, lire les sessions JSONL ou comparer manuellement les deux dépôts.
- **Success looks like:** Sol, Code Mode et les sous-agents fonctionnent sans changement d'outil; tout échec indique immédiatement sa cause et son prochain diagnostic.

### Intégrateur d'un client terminal ou headless

- **Role:** développeur qui pilote Pyxis depuis une TUI, un script, un IDE ou un orchestrateur externe.
- **Behaviors:** démarre ou reprend un thread, envoie un tour, observe le flux d'items, répond aux approvals et récupère l'historique.
- **Pain points:** le runtime n'a pas de contrat externe stable; les événements internes ne suffisent pas à construire un client découplé.
- **Current workaround:** lier directement les crates Pyxis ou analyser une sortie de processus non versionnée.
- **Success looks like:** un protocole JSON-RPC versionné permet de piloter les threads avec des identifiants stables, une reprise idempotente et des schémas générés.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- [Codex app-server](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md): le comparable canonique expose des threads persistants, des tours interruptibles, des items ordonnés, un historique paginé et un protocole client versionné.
- [Responses API](https://developers.openai.com/api/reference/resources/responses/methods/create): les outils custom acceptent une entrée texte libre; le provider doit séparer leur wire de celui des fonctions JSON.
- [Model Context Protocol](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation): la compatibilité MCP exige négociation de capacités, réponses explicites aux demandes et corrélation durable.
- **Market gap:** Pyxis peut combiner le comportement attendu de Codex avec un provider générique, un taint untrusted et des hooks fail-closed que le fork de `codex-core` compromettrait.

### Best Practices Applied

- [V8 Embedding](https://v8.dev/docs/embed) et [V8 untrusted-code mitigations](https://v8.dev/docs/untrusted-code-mitigations): un isolate sépare l'état JavaScript mais ne remplace pas une frontière de capacités. Le runtime doit ajouter quotas, interruption externe et ponts natifs minimaux.
- [OWASP secure MCP development](https://genai.owasp.org/resource/a-practical-guide-for-secure-mcp-server-development/): les appels d'outils doivent valider les entrées, lier l'identité à l'autorisation et éviter les secrets transportés dans le contenu modèle.
- [OWASP LLM01 Prompt Injection](https://genai.owasp.org/llmrisk/llm01-prompt-injection/): un contenu non fiable ne doit jamais contourner les permissions d'un outil imbriqué.
- Une parité mouvante doit être remplacée par une baseline de commit, une matrice versionnée et des contract tests observables.

## Assumptions & Constraints

### Assumptions (to validate)

- Le contrat visible de Codex au commit baseline suffit pour exécuter `gpt-5.6-sol` via le canal ChatGPT subscription sans endpoint privé supplémentaire.
- `rusty_v8 = 149.2.0` et l'ICU associé peuvent être intégrés au toolchain Rust 1.95 de Pyxis sur Linux x86_64 avec un coût binaire et mémoire acceptable.
- Le runtime V8 in-process peut respecter les quotas requis; sinon un hôte processus est nécessaire.
- Les sessions JSONL existantes peuvent être migrées en lecture sans réécrire les événements historiques.
- Les limites actuelles de quatre agents actifs, huit agents créés par racine et une profondeur de un restent le profil par défaut de Pyxis.

### Hard Constraints

- La baseline normative est `/home/arthur/dev/codex` au commit `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`.
- `/home/arthur/dev/codex` est une source read-only et ne doit jamais être modifiée.
- Le wire Responses reste dans `agent-provider`; `agent-core` conserve des types provider-neutral.
- Toute reprise de code Apache-2.0 est consignée dans `NOTICE-CODEX.md` et `docs/codex-port-inventory.md`.
- Linux x86_64 est la plateforme obligatoire de cette version.
- La persistance reste single-writer et commit-coupled; aucune migration ne réécrit les journaux utilisateur.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - format Rust déterministe.
- `cargo clippy --workspace --all-targets` - lints du workspace sur toutes les cibles.
- `cargo test --workspace --no-fail-fast` - suite complète sans masquer les échecs suivants.

Additional gates:

- Toute modification TUI ajoute ou met à jour un snapshot textuel et prouve qu'aucun état n'est communiqué uniquement par la couleur.
- Toute modification de wire ajoute une fixture golden fragmentée et un round-trip sérialisation/désérialisation.
- Toute reprise de code Codex met à jour l'inventaire de provenance dans la même story.

## Epics & User Stories

### EP-001: Contrat provider et baseline de parité

Figer la cible et rendre le protocole Pyxis capable de représenter les outils function et custom sans coupler le cœur à Responses.

**Definition of Done:** la baseline est vérifiable automatiquement; les outils custom/freeform effectuent un round-trip complet et les fixtures de wire détectent toute divergence.

#### US-001: Figer la matrice contractuelle Codex

**Description:** As a mainteneur Pyxis, I want une matrice de parité liée à un commit Codex so that chaque écart et chaque preuve aient une définition stable.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**

- [ ] Given le clone Codex au commit baseline, when le vérificateur inventorie modèles, modes d'outils, multi-agent et méthodes app-server, then il produit une matrice versionnée et déterministe.
- [ ] Given un clone Codex absent ou à un autre commit, when le vérificateur démarre, then il échoue avec le chemin attendu, le commit attendu et aucune modification du clone.
- [ ] La matrice distingue `Direct`, `CodeMode`, `CodeModeOnly`, Responses Lite, multi-agent v1/v2 et les méthodes app-server incluses.
- [ ] Les audits historiques sont référencés comme contexte et ne restent pas des sources normatives concurrentes.

#### US-002: Généraliser l'algèbre des outils

**Description:** As a développeur provider, I want représenter les outils function et freeform dans des types provider-neutral so that un modèle Code Mode puisse être décrit sans fuite du wire Responses.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given un outil function existant, when il traverse le nouveau type canonique, then son nom, sa description et son schéma JSON restent byte-equivalent dans les fixtures actuelles.
- [ ] Given un outil freeform, when il est projeté vers le provider ChatGPT, then son format texte et sa grammaire optionnelle sont conservés sans faux `input_schema`.
- [ ] Given un provider qui ne supporte pas les outils freeform, when un plan en contient un, then le provider retourne une incompatibilité typée avant tout appel réseau.
- [ ] Aucun type de `agent-core` ne contient un nom d'événement ou un payload propre à l'API Responses.

#### US-003: Supporter le cycle `custom_tool_call`

**Description:** As a runtime agentique, I want sérialiser et reconstruire les appels custom streamés so that Code Mode puisse dispatcher une entrée texte et enregistrer son résultat.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given un flux SSE fragmenté `custom_tool_call`, when les deltas arrivent, then l'accumulateur produit exactement un appel terminal avec `call_id`, nom et entrée texte.
- [ ] Given une sortie d'outil custom, when le tour suivant est construit, then le payload de sortie référence le bon `call_id` et effectue un round-trip sans perte.
- [ ] Given des deltas dupliqués suivis d'un item terminal autoritaire, when le flux se ferme, then aucun texte ni dispatch n'est dupliqué.
- [ ] Given un nom absent, un UTF-8 invalide ou un ordre d'événements impossible, when l'accumulateur traite le flux, then il retourne une erreur de contrat typée et aucun outil n'est exécuté.

#### US-004: Installer les fixtures de conformité provider

**Description:** As a mainteneur, I want des fixtures golden dérivées du contrat baseline so that une régression function/custom soit détectée offline.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-003

**Acceptance Criteria:**

- [ ] Given les cas function, freeform, custom call, sortie custom et stream fragmenté, when la suite de conformité tourne, then chaque requête et événement correspond à la fixture baseline.
- [ ] Given une fixture dont le résultat terminal contredit un delta intermédiaire, when elle est lue, then le résultat terminal gagne sans double émission.
- [ ] Given un nouvel item Responses non mappé, when il entre dans la suite, then le test échoue avec le type et la position de l'item au lieu de l'ignorer.
- [ ] Les fixtures ne contiennent aucun token, identifiant de compte ou contenu de session réel.

---

### EP-002: Runtime Code Mode V8

Fournir une session JavaScript durable par thread, un cycle `exec/wait` et un pont contrôlé vers les outils Pyxis.

**Definition of Done:** `CodeModeOnly` exécute des cellules isolées, attend leur sortie, appelle des outils imbriqués sous la politique Pyxis, survit aux erreurs de cellule et s'arrête de façon bornée.

#### US-005: Valider l'enveloppe V8 et la frontière de sécurité

**Description:** As a mainteneur, I want mesurer et interrompre `rusty_v8` sur le toolchain réel so that l'architecture in-process ou process-owned soit décidée sur des preuves.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given `rusty_v8 = 149.2.0`, ICU et Rust 1.95 sur Linux x86_64, when le spike compile, then il mesure taille binaire, démarrage froid, démarrage chaud, heap V8 et mémoire native.
- [ ] Given une boucle JavaScript infinie, when le budget CPU expire, then un handle externe interrompt l'exécution en moins de 1 seconde sans bloquer le runtime Tokio.
- [ ] Given un dépassement heap et un dépassement mémoire native simulé, when chaque limite est franchie, then le rapport distingue les deux et aucun processus Pyxis n'est tué silencieusement.
- [ ] Given qu'un seuil obligatoire n'est pas atteint, when le verdict est publié, then US-006 est bloquée avec une recommandation process-owned; aucun fallback non documenté n'est activé.

#### US-006: Porter le protocole de session Code Mode

**Description:** As a runtime de thread, I want un protocole session/cellule indépendant de V8 so that l'exécution, l'attente et l'arrêt soient testables sans moteur JavaScript.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-005

**Acceptance Criteria:**

- [ ] Given un thread actif, when sa première cellule est soumise, then une session unique produit un identifiant de cellule stable et des états `running`, `yielded`, `completed` ou `failed`.
- [ ] Given une cellule yielded, when `wait` est appelé, then seule la sortie nouvelle depuis le précédent yield est retournée.
- [ ] Given `terminate` ou `shutdown`, when des cellules sont actives, then toutes atteignent un état terminal dans le délai configuré et les waiters sont réveillés.
- [ ] Given un identifiant inconnu, dupliqué ou appartenant à un autre thread, when une commande est reçue, then elle échoue de façon typée sans fuite de sortie inter-session.

#### US-007: Implémenter l'isolate et le cycle de vie V8

**Description:** As a utilisateur Code Mode, I want une exécution JavaScript isolée et bornée so that le modèle puisse orchestrer sans compromettre le processus.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**

- [ ] Given deux threads, when ils modifient leurs globals simultanément, then chaque isolate conserve son état sans visibilité croisée.
- [ ] Given le premier démarrage V8, when le mode JIT ou JITless est fixé, then tout démarrage incompatible ultérieur échoue explicitement avant création d'une session.
- [ ] Given une exception JavaScript, une limite CPU ou une limite heap, when la cellule termine, then son erreur est typée, la session reste utilisable si l'isolate est sain et le thread Rust reste vivant.
- [ ] Given un arrêt du thread dédié qui dépasse le délai, when le shutdown expire, then le runtime signale le worker non joint et applique le verdict process-owned de US-005 au lieu de détacher silencieusement le travail.

#### US-008: Exposer `exec`, `wait` et les outils imbriqués

**Description:** As a modèle Code Mode, I want exécuter du JavaScript et déléguer des outils via une API unique so that l'orchestration complexe ne gonfle pas le schéma visible du modèle.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-003, US-007

**Acceptance Criteria:**

- [ ] Given une entrée conforme à la grammaire Lark baseline, when l'outil freeform `exec` est appelé, then la cellule retourne texte, image, audio ou référence yielded selon le contrat Codex.
- [ ] Given un appel `tools.<name>(args)` depuis JavaScript, when il est dispatché, then il traverse le même `ToolDispatchSnapshot`, le taint, les hooks, l'approbation et la cancellation qu'un appel direct.
- [ ] Given plusieurs appels imbriqués autorisés, when le script les attend, then leurs résultats restent corrélés et l'ordre terminal est déterministe.
- [ ] Given une syntaxe invalide, un outil absent ou une permission refusée, when la cellule l'appelle, then aucun effet n'a lieu et une erreur structurée revient dans la cellule.

#### US-009: Intégrer Code Mode au `ThreadRuntime`

**Description:** As a utilisateur Sol, I want que la session Code Mode suive le thread, sa persistance et son arbre d'annulation so that démarrage, reprise et arrêt aient les mêmes garanties que le reste de Pyxis.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-008

**Acceptance Criteria:**

- [ ] Given un modèle `CodeModeOnly`, when le plan d'outils est construit, then seuls `exec`, `wait` et les exceptions directes baseline sont visibles; les outils imbriqués restent dispatchables mais cachés.
- [ ] Given un modèle `CodeMode`, when le plan est construit, then le runtime applique exactement la combinaison direct/Code Mode définie par la matrice US-001.
- [ ] Given une reprise après arrêt propre, when le thread recharge son journal, then la session JavaScript repart vide mais le transcript, les identifiants et les résultats terminaux restent cohérents et documentés.
- [ ] Given un crash avec cellule active, when le thread est repris, then la cellule devient un échec interrompu persistant et aucun appel imbriqué n'est rejoué automatiquement.

---

### EP-003: Modèles frontier et orchestration multi-agent v2

Aligner le catalogue de capacités et brancher le superviseur existant sur les outils v2 visibles par les modèles frontier.

**Definition of Done:** `gpt-5.6-sol` résout ses capacités sans refus local; les six opérations multi-agent v2 fonctionnent en direct et depuis Code Mode avec autorité, reprise et cancellation.

#### US-010: Aligner le catalogue et la résolution des capacités

**Description:** As a utilisateur, I want que le modèle sélectionné porte ses capacités exactes so that le runtime active le bon mode d'outils et la bonne version multi-agent.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given un descriptor baseline, when il est résolu, then `Direct`, `CodeMode`, `CodeModeOnly`, `use_responses_lite` et `multi_agent_version` sont conservés dans le runtime résolu.
- [ ] Given `gpt-5.6-sol` et un runtime Code Mode disponible, when le modèle est sélectionné, then aucune incompatibilité locale n'est retournée avant l'appel provider.
- [ ] Given une valeur inconnue ou une capacité obligatoire absente, when le catalogue est chargé, then l'entrée est rejetée avec le champ fautif et aucun défaut silencieux vers `Direct`.
- [ ] Given un modèle direct historique, when le nouveau catalogue est chargé, then ses requêtes et outils restent compatibles avec les fixtures antérieures.

#### US-011: Câbler les outils multi-agent v2 dans le binaire

**Description:** As a modèle frontier, I want `spawn_agent`, `send_message`, `followup_task`, `list_agents`, `wait_agent` et `interrupt_agent` so that je puisse orchestrer les sous-tâches avec le contrat Codex v2.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**

- [ ] Given un thread racine initialisé, when le registre CLI est construit, then les six outils v2 pointent vers un `AgentSupervisor` et un spawner réels.
- [ ] Given `spawn_agent` avec `task_name`, when le child démarre, then son nom canonique, sa filiation et son autorité intersectée sont persistés.
- [ ] Given un appel v2 alors que le spawner ou le runtime enfant est indisponible, when l'outil s'exécute, then il retourne une erreur typée et aucun nœud orphelin n'est ajouté.
- [ ] Given les limites actuelles de quatre actifs, huit créés et profondeur un, when une limite est dépassée, then le spawn est refusé sans consommer de slot.

#### US-012: Rendre le cycle multi-agent durable et interruptible

**Description:** As a utilisateur d'orchestration, I want reprendre, attendre, relancer et interrompre les enfants so that leur état ne dépende pas de la durée de vie d'une commande CLI.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-011

**Acceptance Criteria:**

- [ ] Given des descendants actifs et des messages en attente, when le processus redémarre, then le graphe, les noms canoniques, les messages durables et les états terminaux sont reconstruits.
- [ ] Given `followup_task` sur un agent idle, when le message est accepté, then un nouveau tour démarre; sur un agent running, le message est livré à une frontière sûre sans second tour concurrent.
- [ ] Given l'interruption du parent, when la cancellation se propage, then tous les descendants actifs terminent ou signalent leur timeout dans un ordre causal persistant.
- [ ] Given un journal enfant corrompu ou manquant, when le parent reprend, then le child devient `failed` avec une cause visible et les autres descendants restent pilotables.

#### US-013: Unifier multi-agent direct et Code Mode

**Description:** As a modèle Code Mode, I want appeler les outils multi-agent depuis JavaScript avec les mêmes règles que le mode direct so that le choix de mode ne change pas la sémantique d'orchestration.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-008, US-012

**Acceptance Criteria:**

- [ ] Given le même appel v2 en direct et via `tools`, when il termine, then les deux chemins produisent les mêmes événements, états et erreurs normalisées.
- [ ] Given un child qui répond pendant que la cellule JavaScript attend, when la réponse arrive, then elle réveille uniquement la cellule et le call corrélés.
- [ ] Given une tentative de cycle, de profondeur excessive ou d'escalade d'autorité depuis Code Mode, when elle est dispatchée, then elle est refusée avant création du child.
- [ ] Given l'arrêt de la cellule appelante, when un child autonome continue selon sa politique, then son ownership et son devenir sont explicites dans le journal et dans `list_agents`.

---

### EP-004: Exécution terminale et sessions unifiées

Aligner le wire terminal nécessaire aux outils directs et imbriqués sans remplacer les invariants d'exécution déjà livrés par Pyxis.

**Definition of Done:** commandes pipe et PTY, yield, attente, écriture stdin, interruption et limites de sortie correspondent aux contrats baseline et restent soumises aux permissions Pyxis.

#### US-014: Aligner `exec_command` et le support PTY

**Description:** As a agent de code, I want lancer une commande pipe ou PTY avec le schéma baseline so that les programmes interactifs et non interactifs se comportent comme dans Codex.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**

- [ ] Given une commande, un cwd, un shell et un mode TTY valides, when `exec_command` démarre, then le résultat expose sortie, code, session, chunk et temps écoulé selon le wire baseline.
- [ ] Given une session PTY, when `write_stdin` envoie des octets ou un signal de terminaison, then le processus et sa sortie restent corrélés à cette seule session.
- [ ] Given une commande réseau ou mutante issue d'un contenu untrusted, when elle est évaluée, then les hooks, le taint, le sandbox et l'approbation restent obligatoires.
- [ ] Given un cwd absent, un shell refusé ou la cinquième session ouverte, when l'appel arrive, then il échoue avant spawn et ne laisse aucun processus.

#### US-015: Garantir yield, wait, limites et nettoyage

**Description:** As a utilisateur d'une commande longue, I want attendre uniquement la nouvelle sortie et terminer proprement la session so that le transcript reste borné et sans doublon.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**

- [ ] Given une session yielded, when elle est polled sans entrée, then seuls les octets postérieurs au chunk précédent sont retournés avec un identifiant monotone.
- [ ] Given un timeout d'appel, when le processus continue, then la session reste accessible; given `terminate: true`, then le groupe de processus atteint un état terminal en moins de 2 secondes.
- [ ] Given une sortie supérieure à 10 MiB, when elle est collectée, then elle est tronquée avec compte exact des octets omis sans dépassement mémoire non borné.
- [ ] Given une session inconnue, expirée ou déjà terminée, when `write_stdin` est appelé, then l'erreur distingue ces états et aucun autre processus ne reçoit l'entrée.

---

### EP-005: App-server et contrat client externe

Exposer le runtime durable par un protocole JSON-RPC bidirectionnel compatible avec les concepts thread, turn, item et approval de Codex.

**Definition of Done:** un client externe initialise la connexion, crée ou reprend un thread, lance ou interrompt un tour, répond aux approvals, parcourt l'historique et reçoit un flux ordonné sous backpressure.

#### US-016: Exposer le cycle thread/turn/item sur stdio

**Description:** As a intégrateur, I want piloter Pyxis via JSON-RPC stdio so that mon client ne dépende pas des crates internes.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009, US-012, US-015

**Acceptance Criteria:**

- [ ] Given une connexion neuve, when `initialize` négocie une version supportée, then le serveur expose ses capacités avant toute mutation.
- [ ] Given un thread créé ou repris, when un tour démarre, then les événements `thread`, `turn` et `item` portent des identifiants stables et un ordre causal.
- [ ] Given deux demandes d'écriture concurrentes sur le même thread, when elles arrivent, then une seule obtient l'ownership et l'autre reçoit un conflit typé sans mutation partielle.
- [ ] Given une méthode inconnue, une version incompatible ou un message JSON invalide, when il est reçu, then le serveur répond selon JSON-RPC et reste disponible pour la requête suivante.

#### US-017: Corréler approvals, outils dynamiques et interruptions

**Description:** As a client app-server, I want répondre aux demandes bidirectionnelles et interrompre un tour so that aucune approval ni exécution ne devienne orpheline.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-016

**Acceptance Criteria:**

- [ ] Given une approval ou une elicitation MCP, when elle est émise, then `threadId`, `turnId`, `itemId` et `requestId` permettent une unique résolution `accept`, `decline` ou `cancel`.
- [ ] Given un outil dynamique enregistré pour un thread, when le modèle l'appelle, then sa définition, son autorité et son résultat traversent le même dispatch que les outils statiques.
- [ ] Given l'interruption d'un tour, when des cellules, commandes ou enfants sont actifs, then la cancellation se propage et un événement terminal unique ferme chaque item.
- [ ] Given une réponse tardive, dupliquée ou liée à un tour terminé, when elle arrive, then elle est refusée sans réouvrir l'item ni exécuter l'outil.

#### US-018: Ajouter historique paginé, WebSocket et schémas

**Description:** As a auteur de client, I want pagination, transport WebSocket et schémas générés so that je puisse construire un client robuste sans reverse engineering.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-016, US-017

**Acceptance Criteria:**

- [ ] Given un historique supérieur à une page, when le client suit les curseurs, then chaque item apparaît exactement une fois dans l'ordre persistant.
- [ ] Given une connexion WebSocket autorisée, when elle reprend un thread, then elle observe le même contrat et les mêmes identifiants que stdio.
- [ ] Given les types protocolaires Rust, when la génération tourne, then JSON Schema et TypeScript sont déterministes et couvrent 100 % des méthodes et notifications exposées.
- [ ] Given un client lent ou déconnecté, when la file atteint 1 024 événements ou 16 MiB, then le serveur applique une backpressure bornée, ferme avec une cause exploitable et ne perd aucun résultat déjà commité.

---

### EP-006: Observabilité et preuve de parité

Rendre chaque échec diagnostiquable et transformer la parité en suite de preuves reproductibles, maintenue avec la documentation et les obligations de licence.

**Definition of Done:** aucune transition terminale n'est silencieuse; le scénario Sol passe en live opt-in; la matrice baseline, les fixtures, la documentation et l'inventaire de provenance convergent.

#### US-019: Projeter causes, cellules et traces sur toutes les surfaces

**Description:** As a utilisateur qui diagnostique un prompt, I want voir la cause, la corrélation et l'état Code Mode partout so that une absence de réponse devienne une erreur actionnable.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009, US-012, US-015, US-017

**Acceptance Criteria:**

- [ ] Given un `TurnStateChanged` failed avec cause, when il atteint TUI, headless, JSONL et app-server, then chaque surface affiche la même catégorie, le même message actionnable et les mêmes identifiants.
- [ ] Given une cellule running, yielded, completed ou failed, when son état change, then la TUI et le flux externe le représentent textuellement sans dépendre de la couleur.
- [ ] Given `PYXIS_LOG=debug`, when un provider, un outil, V8 ou un child échoue, then une trace structurée corrèle thread, turn, item, call et cell sans token ni contenu sensible.
- [ ] Given un export OTLP non configuré ou indisponible, when Pyxis démarre ou émet des traces, then aucun réseau d'observabilité n'est ouvert par défaut et le runtime utilisateur continue sans perte d'erreur locale.

#### US-020: Prouver et maintenir la parité baseline

**Description:** As a mainteneur, I want une recette offline et live de parité so that la livraison soit fondée sur des preuves et que la dérive amont soit visible.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004, US-013, US-015, US-018, US-019

**Acceptance Criteria:**

- [ ] Given la suite offline, when elle tourne, then elle couvre catalogue, function/custom wire, Code Mode, terminal, multi-agent v2, reprise, app-server, MCP et erreurs sans accès réseau.
- [ ] Given des credentials ChatGPT opt-in, when le scénario live lance un prompt `gpt-5.6-sol`, then il observe au moins une cellule Code Mode, un résultat terminal et un transcript reprenable.
- [ ] Given aucun credential ou une panne OpenAI, when la recette live est demandée, then elle rapporte `skipped` ou l'erreur externe exacte sans convertir ce résultat en succès de parité.
- [ ] Given un nouveau HEAD Codex, when le vérificateur de dérive est lancé, then il produit un diff lisible contre la baseline sans modifier Pyxis ni Codex.
- [ ] `docs/CURRENT_STATUS.md`, les audits de parité, `NOTICE-CODEX.md` et `docs/codex-port-inventory.md` décrivent le même état livré et les mêmes composants adoptés.

## Functional Requirements

- FR-01: Le système doit fixer chaque campagne de parité à un commit Codex explicite.
- FR-02: Le système doit représenter séparément les outils function et custom/freeform.
- FR-03: Le système doit traiter les items terminaux comme autoritaires face aux deltas streamés.
- FR-04: Le système doit refuser un outil custom avant l'appel réseau si le provider ne le supporte pas.
- FR-05: Une session Code Mode doit appartenir à un seul thread et ne partager aucun global JavaScript avec un autre thread.
- FR-06: `exec`, `wait`, `terminate` et `shutdown` doivent produire des états de cellule typés et persistables.
- FR-07: Tous les appels d'outils imbriqués doivent traverser les permissions, hooks, taint, sandbox et cancellation Pyxis.
- FR-08: Une reprise après crash ne doit jamais rejouer automatiquement un outil dont l'issue est inconnue.
- FR-09: Le catalogue doit conserver `tool_mode`, `use_responses_lite` et `multi_agent_version`.
- FR-10: Le binaire doit enregistrer les six outils multi-agent v2 quand le modèle les supporte.
- FR-11: L'autorité d'un child doit être l'intersection de l'autorité parent et de la demande de spawn.
- FR-12: Le graphe multi-agent, les messages et les causes terminales doivent survivre à une reprise.
- FR-13: L'exécution terminale doit supporter pipe et PTY avec cwd, shell, timeout, stdin et terminaison.
- FR-14: La sortie incrémentale doit être bornée, corrélée et dépourvue de doublons.
- FR-15: L'app-server doit négocier une version et exposer un cycle thread/turn/item sur stdio.
- FR-16: Les approvals et elicitations doivent être résolues exactement une fois.
- FR-17: L'historique doit être paginé par curseur opaque et stable.
- FR-18: stdio et WebSocket doivent partager les mêmes types protocolaires et la même sémantique.
- FR-19: Toute transition failed doit conserver et projeter sa cause sur TUI, headless, JSONL et app-server.
- FR-20: L'observabilité distante doit être opt-in et ne jamais transporter de secrets par défaut.
- FR-21: La suite offline doit prouver les contrats sans compte OpenAI.
- FR-22: Le scénario live Sol doit être séparé, opt-in et incapable de produire un faux succès.
- FR-23: Toute reprise de code Codex doit conserver la provenance et les obligations Apache-2.0.
- FR-24: Le système ne doit pas importer `codex-core` ni déplacer le wire Responses dans `agent-core`.

## Non-Functional Requirements

- **Performance:** sur Linux x86_64 de référence, le dispatch d'un événement provider déjà reçu vers le journal ajoute moins de 10 ms au P95 sur 10 000 événements; une cellule V8 chaude commence en moins de 100 ms au P95 sur 1 000 cellules; une session froide devient prête en moins de 1 000 ms au P95 sur 100 sessions.
- **Security:** le heap V8 par défaut est limité à 256 MiB, le budget CPU d'une cellule à 30 secondes et l'interruption prend moins de 1 seconde; 100 % des ponts natifs sont enregistrés par allowlist; 100 % des outils mutants ou réseau passent par la politique d'autorité existante.
- **Accessibility:** 100 % des états, approvals et erreurs visibles dans la TUI possèdent un libellé textuel; aucun snapshot critique ne dépend uniquement d'une couleur; toutes les actions de diagnostic sont accessibles au clavier.
- **Scalability:** le profil initial supporte quatre agents actifs, huit agents créés par racine, quatre sessions terminales et 32 lectures app-server concurrentes, avec un seul writer par thread.
- **Reliability:** zéro double exécution sur 10 000 scénarios d'interruption/reprise; 100 % des items actifs reçoivent un état terminal lors d'un shutdown; les groupes de processus terminent en moins de 2 secondes après `terminate`.
- **Resource bounds:** une sortie d'outil ou de cellule est limitée à 10 MiB par résultat; une file client app-server est limitée à 1 024 événements ou 16 MiB; tout dépassement produit une erreur structurée.
- **Compatibility:** 100 % des sessions JSONL créées par les versions Pyxis présentes dans le dépôt restent lisibles; Linux x86_64 avec Rust 1.95 est obligatoire; les autres plateformes peuvent échouer à la compilation avec un message documenté.
- **Privacy:** sans configuration OTLP explicite, Pyxis ouvre zéro connexion d'observabilité; tokens, cookies et secrets sont absents de 100 % des logs et fixtures.

## Edge Cases & Error States

Systematic coverage of unhappy paths. Evidence shows earlier defect discovery significantly reduces cost (Boehm 1981, NIST 2002).

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Aucun runtime Code Mode | Sol sélectionné avant installation ou après verdict bloquant | Refus avant réseau, modèle et composant manquant identifiés | "Le modèle exige Code Mode, mais le runtime n'est pas disponible." |
| 2 | Initialisation V8 en cours | Première cellule pendant le démarrage froid | État loading corrélé, timeout borné, aucune seconde initialisation | "Initialisation de Code Mode..." |
| 3 | Boucle JavaScript infinie | Budget CPU de 30 secondes dépassé | Interruption externe, cellule failed, thread encore utilisable | "Cellule interrompue: budget CPU dépassé." |
| 4 | Heap ou mémoire native dépassé | Script ou pont natif consomme au-delà du quota | Limite distinguée, session arrêtée proprement si nécessaire | "Cellule arrêtée: limite mémoire dépassée." |
| 5 | Flux provider malformé | Delta custom incomplet, dupliqué ou hors ordre | Aucun dispatch, erreur de contrat persistée | "Réponse provider invalide: custom tool call incomplet." |
| 6 | Réseau OpenAI dégradé | Timeout ou déconnexion SSE | Résultat partiel non terminal conservé, retry sans double outil | "Connexion interrompue avant la fin du tour." |
| 7 | Permission révoquée | Hook, approval ou autorité change avant dispatch imbriqué | Décision réévaluée au dispatch, effet refusé | "Outil refusé par la politique actuelle." |
| 8 | Reprise concurrente | Deux clients réclament le même thread | Un writer, conflit explicite pour l'autre | "Ce thread est déjà piloté par un autre client." |
| 9 | Limite de descendants | Cinquième agent actif ou neuvième créé | Spawn refusé sans nœud orphelin | "Limite de sous-agents atteinte." |
| 10 | Interruption en cascade | Parent interrompu avec cellule, shell et enfants actifs | Cancellation causale, un état terminal par item | "Tour interrompu; ressources enfants arrêtées." |
| 11 | Panne MCP | Serveur absent, réponse invalide ou elicitation orpheline | Erreur isolée à l'appel, thread disponible | "Serveur MCP indisponible ou réponse invalide." |
| 12 | Client app-server lent | File supérieure à 1 024 événements ou 16 MiB | Backpressure puis fermeture typée, journal intact | "Client trop lent; reconnectez-vous et reprenez le thread." |
| 13 | Session historique | Journal antérieur sans métadonnée Code Mode ou v2 | Valeur historique explicite, aucune réécriture | "Session reprise avec le profil de compatibilité historique." |
| 14 | Arrêt du processus | Shutdown pendant V8, PTY, approval et child | Délai borné, ressources jointes ou cause de timeout persistée | "Arrêt incomplet: une ressource n'a pas terminé dans le délai." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|-------------|--------|------------|
| 1 | La baseline Codex dérive rapidement | High | High | Commit figé, matrice générée, diff amont explicite et aucune mise à jour automatique |
| 2 | V8 in-process donne une fausse impression de sandbox | High | High | Spike bloquant, quotas CPU/heap/native, allowlist, interruption externe et option process-owned |
| 3 | `rusty_v8` augmente fortement build, binaire ou mémoire | Med | High | Mesures US-005 avant port, dépendance exacte, cache CI et verdict d'architecture documenté |
| 4 | Le custom wire régresse les outils function existants | Med | High | Algèbre provider-neutral, fixtures golden historiques et refus typé par capability |
| 5 | Une reprise rejoue un effet externe | Med | High | Résultats commit-coupled, appels in-flight convertis en état interrupted et tests par injection de crash |
| 6 | Le scope de vingt stories dépasse une seule campagne | High | Med | Deux gates de release, ordre de dépendances strict, aucune story XL et review par epic |
| 7 | App-server crée deux writers sur un thread | Low | High | Ownership exclusif, identifiants stables, conflit JSON-RPC et tests de course |
| 8 | OTLP ou logs divulguent des secrets | Low | High | Export opt-in, redaction structurée, fixtures sans données réelles et tests négatifs |
| 9 | Les tests live masquent une panne externe | Med | Med | Suite offline normative, live opt-in et statuts distincts pass/fail/skipped |

## Non-Goals

Explicit boundaries, what this version does NOT include:

- Forker Codex, importer `codex-core` ou reproduire son architecture interne.
- Reproduire les cloud tasks, analytics, politiques enterprise, comptes OpenAI internes ou autres services non nécessaires au comportement local du harness.
- Copier la TUI Codex pixel par pixel; seules les surfaces requises pour piloter et diagnostiquer les capacités sont concernées.
- Garantir Windows ou macOS dans cette version; la cible reste Linux x86_64.
- Activer automatiquement une télémétrie distante ou envoyer le contenu des prompts.
- Suivre automatiquement les commits Codex postérieurs à la baseline; toute nouvelle baseline exige une décision explicite.
- Modifier les sémantiques propres à Pyxis: provider générique, taint untrusted, hooks fail-closed, cœur headless et persistance commit-coupled.

## Files NOT to Modify

- `/home/arthur/dev/codex/**` - source normative read-only; les fichiers non suivis de son `.codex/` sont également hors périmètre.
- `/home/arthur/dev/pyxis/.pyxis/sessions/**` - sessions utilisateur et preuves de dogfood, jamais migrées ou réécrites en place.
- `/home/arthur/.codex/**`, `/home/arthur/.claude/**` et caches de plugins - état géré par les applications, hors périmètre de l'implémentation.

## Technical Considerations

Frame as questions for engineering input, not mandates:

- **Architecture:** faut-il adopter `code-mode-protocol` presque tel quel puis adapter son dispatch, ou réimplémenter son contrat sur les types Pyxis? Recommandation: adopter la frontière protocolaire autonome et réécrire l'adapter, après inventaire de licence.
- **Isolation:** l'isolate doit-il vivre in-process sur un thread dédié ou dans un hôte processus? Recommandation: décider avec US-005; process-owned devient obligatoire si l'interruption, la mémoire native ou le join ne sont pas bornés.
- **Data Model:** faut-il ajouter de nouveaux événements JSONL ou enrichir les payloads existants pour cellules, lineage et approvals? Recommandation: événements additifs versionnés, avec lecteurs tolérant les champs absents.
- **Provider API:** l'union `Function | Freeform` doit-elle vivre dans `agent-core::provider` ou dans un module de protocole séparé? Recommandation: type canonique provider-neutral dans `agent-core`, projections wire exclusivement dans chaque provider.
- **Cancellation:** faut-il utiliser un `CancellationToken` racine avec children et `TaskTracker`, ou conserver les primitives actuelles? Recommandation: aligner sur les primitives déjà présentes; un token enfant ne doit jamais annuler son parent.
- **Terminal:** faut-il étendre `ExecSessions` ou adopter le moteur unified exec Codex? Recommandation: étendre Pyxis pour préserver permissions et session store; n'adopter que les contrats de wire et les portions PTY autonomes.
- **App-server:** stdio et WebSocket doivent-ils partager un seul actor ou deux adapters? Recommandation: un actor protocolaire unique avec transports minces; engineering doit confirmer la stratégie de backpressure.
- **Schemas:** les types TypeScript doivent-ils être générés depuis Rust ou depuis JSON Schema? Recommandation: Rust vers JSON Schema, puis TypeScript, avec diff déterministe en CI.
- **Migration:** les sessions historiques doivent-elles recevoir une migration en mémoire ou un profil legacy? Recommandation: profil legacy en lecture, aucun rewrite; rollback par suppression des nouveaux événements non commitées.
- **Dependencies:** `rusty_v8 = 149.2.0`, ICU associé et `tokio-util` doivent-ils être obligatoires ou feature-gated? Recommandation: Code Mode obligatoire dans le binaire Linux de parité, crates isolées pour limiter le coût des consommateurs headless.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Prompt Sol rejeté localement | 1 sur 1 scénario observé | au plus 1 sur 100 pour cause locale | Month-1 | recette headless répétée avec catalogue baseline |
| Contrats custom/Code Mode offline | 0 fixture supportée | 100 % des fixtures US-004 et US-020 | Gate Harness | suite Rust sans réseau |
| Outils multi-agent v2 enregistrés dans le binaire | 0 sur 6 | 6 sur 6 | Gate Harness | test du registre CLI et scénarios runtime |
| Causes failed visibles en TUI | 0 % du champ `cause` projeté par le handler concerné | 100 % des catégories terminales | Month-1 | snapshots TUI et tests JSONL/headless |
| Double exécution après reprise | Non mesuré | 0 sur 10 000 injections | Month-6 | harness déterministe crash/restart |
| Méthodes app-server P0 conformes | 0 % | 100 % de la matrice US-001 | Gate Client | contract tests stdio et WebSocket |
| Délai de détection d'une dérive amont | Audit manuel ponctuel | un diff en moins d'un run local | Month-6 | vérificateur de baseline |
| Provenance des lignes adoptées | Inventaire partiel existant | 100 % des composants adoptés | Chaque epic | revue de `NOTICE-CODEX.md` et port inventory |

## Open Questions

- Le verdict in-process ou process-owned doit être tranché par US-005 avant US-006; responsable: engineering Pyxis.
- La liste exacte des méthodes app-server P0 doit être générée depuis la baseline par US-001 avant US-016; responsable: engineering Pyxis.
- Le scénario live US-020 peut-il utiliser un compte de test distinct du compte personnel? Décision d'Arthur requise avant son exécution, pas avant les fixtures offline.
- La limite de profondeur multi-agent doit-elle évoluer après la parité v2? Hors gate Harness; à réévaluer avec des données de dogfood.
- macOS devient-il une cible après le gate Client? Décision produit postérieure à cette version.
[/PRD]
