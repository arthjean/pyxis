[PRD]
# PRD: Pyxis : Capacités du harness (suite de la parité Codex CLI)

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-25 | Arthur Jean | Rédaction initiale, dérivée de `docs/codex-harness-parity-audit-2026-07-25.md`. Absorbe et redécoupe EP-006 de `tasks/prd-harness-parity.md`. |

## Problem Statement

La passe d'audit du 2026-07-25 a mesuré 23 écarts pertinents fermés sur 125 depuis la passe du 24. Les trois problèmes bloquants d'alors (corruption de session, composer mono-ligne, statuts non fiables) sont fermés et vérifiables par une CI verte. Ce qui reste tient sur un seul axe : ce que l'agent sait faire, et ce que l'utilisateur voit de ce qu'il fait.

1. **La seule métrique produit affichée est connue pour être fausse, alors que la vraie transite déjà.** `AgentEvent::ModelTurn` porte les compteurs de tokens réels du backend depuis US-017, et les deux consommateurs les jettent : `crates/agent-tui/src/app_event.rs:515` renvoie `Vec::new()`, `crates/agent-tui/src/state.rs:1121` matche sur `{}`. L'affichage repose sur `turn_chars` (`state.rs:638`), une estimation caractères sur quatre. En parallèle, `render.rs:1581` dessine une jauge de contexte conditionnée à `state.context_pct`, champ jamais assigné hors `examples/` et `crates/agent-tui/tests/render_snapshots.rs:423,433` : en usage réel il vaut `None` et la jauge n'apparaît jamais. La pièce manquante est côté provider : le catalogue `/models` sert `context_window` (visible dans l'échantillon `crates/agent-provider/src/models.rs:84`) et ni `WireModel` (`models.rs:28`) ni `CatalogModel` (`models.rs:13`) ne le déclarent, donc la valeur est silencieusement abandonnée.

2. **Chaque appel `bash` demande une confirmation, y compris `ls`.** `crates/agent-tools/src/bash.rs:94` déclare `fn permission(&self, _input: &Self::Input, ...)` : l'entrée est ignorée et la décision est fixe. `resolve_permission` (`crates/agent-tools/src/permission.rs:103`) est une fonction pure sans état de session, donc aucune réponse n'est mémorisée, ni pour la session ni pour un préfixe. C'est l'écart restant qui se paie le plus souvent, et la recherche montre que c'est exactement la pression qui pousse les utilisateurs vers les modes de contournement global.

3. **Les serveurs MCP configurés ne changent rien à ce que l'agent sait faire.** `grep -rn 'call_tool\|CallTool\|tools/call' crates/ --include='*.rs'` ne retourne aucun résultat. `crates/agent-mcp/src/client.rs` expose `connect` (l. 50), `connect_hardened` (l. 57), `list_tools` (l. 95) et `cancel` (l. 130), rien d'autre. Toute connexion est en outre verrouillée derrière `PYXIS_EXPERIMENTAL_MCP_CONNECT` (`crates/agent-cli/src/interactive.rs:1439`), donc même la découverte est inaccessible par défaut. Le README annonce la configuration MCP.

4. **Une skill installée ne produit qu'un mot dans le prompt.** `crates/agent-cli/src/main.rs:623` (`read_skills`) ne lit que des noms de répertoires sous `~/.agents/skills`. Sélectionner une skill exécute `self.set_input(format!("/{} ", item.id))` (`crates/agent-tui/src/state.rs:1796`), donc le nom part littéralement au modèle. Aucun `SKILL.md` n'est lu : grep sur `crates/` ne trouve rien. Les skills déjà installées pour Codex ou Claude Code sont inertes.

5. **Aucune extension utilisateur ne peut intervenir sur un appel d'outil, et rien n'est traçable après coup.** Aucun moteur de hooks n'existe. Aucune dépendance `tracing` dans le workspace. Aucun `std::panic::set_hook` hors du harness de test (`crates/agent-tui/tests/harness/mod.rs:23`), donc un panic laisse le terminal en mode raw sans trace exploitable. La sonde `PYXIS_DEBUG_USAGE` écrit encore par `eprintln!` depuis le cœur (`crates/agent-core/src/agent.rs:620`), en tension avec ADR-3.

6. **Un quota d'abonnement épuisé se découvre au moment où la session casse.** Aucune capture de fenêtre de quota côté provider : `crates/agent-provider/src/chatgpt.rs:325` ne lit que `Retry-After`, et la classification 429 (`chatgpt.rs:376`) distingue le terminal du transitoire sans jamais exposer le pourcentage consommé ni l'heure de reset.

**Why now:** l'audit fournit les preuves `chemin:ligne` des deux côtés, donc le travail est cadrable sans exploration supplémentaire. Trois des six problèmes sont des câblages sur des données déjà produites, ce qui ne restera pas vrai longtemps : plus la TUI évolue autour d'une estimation fausse, plus le recâblage coûte. Et la recherche 2026 documente deux CVE qui portent exactement sur les mécanismes introduits ici, CVE-2026-22708 sur la mémorisation d'approbation et CVE-2025-6514 sur un serveur MCP malveillant, ce qui impose de les concevoir défensivement dès la première version plutôt que de durcir après.

## Overview

Ce PRD termine le rattrapage de harness commencé par `tasks/prd-harness-parity.md`, dont les releases R1 à R3 (EP-001 à EP-005) sont livrées. Il **absorbe et redécoupe** l'unique epic resté `TODO` de ce PRD, EP-006 « Extensibilité utilisateur » : ses quatre user stories US-019 à US-022 sont remplacées par les dix stories des EP-003, EP-004 et EP-005 ci-dessous, parce que l'audit montre que MCP seul porte quatorze écarts et ne tient pas en deux stories.

La solution est ordonnée en trois releases. **R1 rend visible ce que le système sait déjà** : la fenêtre de contexte descend du catalogue jusqu'à la jauge, les compteurs de tokens réels remplacent l'estimation, les quotas d'abonnement remontent, et quatre commandes exposent des données déjà produites. R1 traite aussi la friction d'approbation, seul écart restant qui se paie à chaque tour. **R2 ouvre les deux canaux d'extension attendus** : les outils MCP deviennent appelables par le modèle avec namespacing et taint, et les skills suivent la spec ouverte agentskills au lieu d'un format propriétaire. **R3 livre le contrôle et la traçabilité** : un moteur de hooks avec droit de veto sur le contrat Claude Code, et l'observabilité de processus.

Quatre décisions structurantes méritent d'être explicites. La classification des commandes shell tokenise l'argv et compare des **séquences de tokens de préfixe**, jamais des sous-chaînes : la recherche documente que la mémorisation par chaîne est exactement le vecteur de CVE-2026-22708, où `git` autorisé ouvre `git push --force`. Le contrat de hook reprend celui de Claude Code (`hookSpecificOutput.permissionDecision`, code de sortie 2 bloquant) parce que c'est celui vers lequel l'écosystème converge et qu'un format maison n'apporterait rien. Les noms d'outils MCP sont préfixés `mcp__{serveur}__{outil}` avec troncature déterministe sous 64 octets, le débordement de préfixe étant le premier mode d'échec documenté de cette intégration. Enfin `tracing` est émis par les crates et le souscripteur est installé par le binaire, ce qui lève la tension d'`agent.rs:620` sans faire entrer d'I/O dans le cœur.

Le cœur reste headless. Comme dans le PRD précédent, les nouvelles capacités passent par des variantes ajoutées à `AgentEvent` et par des traits injectés dans `Deps`, jamais par une dépendance concrète d'entrée-sortie dans `agent-core`.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Fidélité du compteur de contexte affiché | Écart inférieur à 5 % entre l'affichage et l'usage backend sur 20 tours mesurés | Écart inférieur à 2 %, jauge alimentée sur 100 % des tours |
| Friction d'approbation en dogfood | 0 confirmation demandée pour une commande classée sans effet de bord | Moins de 3 confirmations par session de 50 tours |
| Outils MCP appelables par le modèle | 100 % des outils listés par un serveur connecté | Serveur indisponible sans effet sur la session, sur 100 % des démarrages |
| Skills interopérables | Une skill installée pour un autre agent fonctionne sans adaptation | 0 format propriétaire, conformité `name` plus `description` à la spec |
| Diagnostic après incident | Un panic produit une trace exploitable et rend le terminal | Trace structurée activable par variable d'environnement sur 100 % des crates |
| Visibilité des quotas | Pourcentage et heure de reset affichés dès qu'ils sont connus | 0 session cassée sans message expliquant le plan et le reset |

## Target Users

### Arthur Jean, créateur et dogfooder principal

- **Role:** Solo indie maker, mainteneur de Pyxis, utilisateur quotidien de Codex CLI et de Claude Code.
- **Behaviors:** Sessions longues d'audit et de refactor, beaucoup de commandes shell de lecture (`ls`, `git status`, `cargo tree`), skills et serveurs MCP déjà installés pour d'autres agents.
- **Pain points:** Chaque `ls` demande une confirmation. Le compteur de contexte affiché est faux et la jauge n'apparaît jamais. Les serveurs MCP configurés ne servent à rien. Les skills présentes dans `~/.agents/skills` n'injectent aucune instruction.
- **Current workaround:** Lancer avec `--yes` pour supprimer toute confirmation, ce qui supprime aussi la défense de taint. Copier manuellement le contenu d'un `SKILL.md` dans le prompt. Basculer sur Codex CLI quand un outil MCP est nécessaire.
- **Success looks like:** Les commandes inoffensives passent sans question, la jauge de contexte dit la vérité, et les skills et serveurs MCP déjà installés fonctionnent sans configuration spécifique à Pyxis.

### Développeur Rust early adopter du dépôt

- **Role:** Contributeur ou évaluateur qui clone le dépôt et juge le projet sur une session réelle.
- **Behaviors:** Compare le README aux capacités réelles, teste les fonctions annoncées, lit les statuts pour juger la maturité.
- **Pain points:** Le README annonce la configuration MCP alors qu'aucun outil MCP n'est appelable et que la connexion exige une variable d'environnement expérimentale non documentée.
- **Current workaround:** Lire le code pour vérifier chaque affirmation.
- **Success looks like:** Configurer un serveur MCP change ce que l'agent sait faire, sans drapeau expérimental.

### Futur client embarquant `agent-core`

- **Role:** Client riche qui embarquera `agent-core` en process, sans IPC.
- **Behaviors:** Consomme le flux d'`AgentEvent` pour rendre l'état complet d'une session.
- **Pain points:** L'usage de contexte n'est exposé qu'en valeur absolue, sans la fenêtre du modèle, donc aucun pourcentage n'est calculable côté client. Les quotas d'abonnement ne sont pas modélisés.
- **Current workaround:** Aucun, l'intégration n'a pas commencé.
- **Success looks like:** Le flux d'événements suffit à rendre la consommation de contexte et l'état de quota sans appeler le provider.

## Research Findings

Constats issus de la recherche qui ont façonné ce PRD.

### Competitive Context

- **Codex CLI :** `execpolicy` porte des règles Starlark avec précédence `forbidden > prompt > allow` et correspondance par tokens de préfixe, la plus stricte gagnant. `/status` affiche jetons et limites. Le reproche récurrent porte sur l'ergonomie d'écriture des règles, ce qui plaide contre un langage de règles complet pour un premier jet.
- **Claude Code :** la commande `/context` avec ventilation par composant (prompt système, outils, MCP, skills, réserve d'auto-compaction) est la référence d'UX sur ce point. Les hooks avec droit de veto et le format Skills en sont issus. Faiblesses documentées : erreurs 400 sur longueur de nom d'outil MCP, et un plafond de règles de refus contournable.
- **Cursor :** mémorisation d'allow-list détournée par empoisonnement d'environnement, CVE-2026-22708.
- **Market gap :** aucun standard inter-agents pour les flux d'événements ; les Agent Skills sont en revanche le standard ouvert qui converge, avec plus de seize outils l'ayant adopté en 2026.

### Best Practices Applied

- **Spec Agent Skills.** Répertoire plus `SKILL.md` à frontmatter YAML. Champs requis : `name` (minuscules, chiffres et tirets, 64 caractères au plus, sans tiret en tête ni en fin, identique au nom du répertoire) et `description` (1024 caractères au plus, disant quoi et quand). Champs optionnels ignorés s'ils sont inconnus. Seuls `name` et `description` sont préchargés au démarrage. Recommandation de sécurité : refuser `<` et `>` dans le frontmatter, qui partirait sinon dans le prompt système.
- **Contrat de hook.** Événement JSON sur l'entrée standard ; sortie standard JSON portant `hookSpecificOutput.permissionDecision` valant `allow`, `deny` ou `ask` avec sa raison ; code de sortie 2 valant blocage avec la sortie d'erreur transmise au modèle. Un refus de hook prime sur les modes de contournement de permissions.
- **Classification de commandes.** Tokeniser l'argv, apparier par programme puis par séquence de tokens de préfixe, jamais par sous-chaîne. La décision la plus stricte gagne.
- **Nommage MCP.** Les noms de fonctions de l'API Responses doivent respecter `^[a-zA-Z0-9_-]+$` et 64 octets. Le préfixe serveur plus le nom d'outil déborde régulièrement : il faut mesurer et tronquer de façon déterministe.
- **Erreurs MCP.** Le protocole distingue l'échec fonctionnel (`isError` dans le résultat, destiné au modèle) de l'erreur JSON-RPC (destinée au harness). Le SDK `rmcp` matérialise exactement cette séparation : `Ok(CallToolResult { is_error, .. })` contre `Err(ServiceError)`.
- **Tracing.** Une bibliothèque émet des spans et des événements, le binaire installe le souscripteur.

### Security Findings

- **CVE-2026-22708 (Cursor) :** une allow-list mémorisée a été détournée par empoisonnement de l'environnement, la charge utile passant par une sortie de `git branch`. Conséquence directe : mémoriser une séquence de tokens d'argv, jamais une chaîne, et ne jamais mémoriser une commande dont un argument provient d'une substitution.
- **CVE-2025-6514 (mcp-remote, score 9.6) :** un serveur MCP malveillant a obtenu l'exécution de code chez le client. Conséquence : le contenu d'un outil MCP reste `untrusted` à 100 %, et un serveur ne peut jamais élargir un périmètre de sécurité.
- **OWASP LLM01 :** l'injection indirecte reste le mode d'échec dominant en production. Les sorties MCP, le texte de terminal et le frontmatter des skills sont tous du contenu non fiable.
- **Configuration contrôlée par le workspace :** motif générique déjà retenu par le PRD précédent, ici étendu aux hooks et aux skills, qui ne peuvent être déclarés que globalement.

*Sources complètes conservées dans `docs/codex-harness-parity-audit-2026-07-25.md` et dans l'historique de recherche de la session de rédaction.*

## Assumptions & Constraints

### Assumptions (to validate)

- Le catalogue `/models` sert bien `context_window` en production, et pas seulement dans l'échantillon de test de `crates/agent-provider/src/models.rs:84`. US-001 le vérifie sur une réponse réelle avant de câbler quoi que ce soit.
- Le backend Codex renvoie des en-têtes ou un corps portant l'état de quota d'abonnement sur une réponse ordinaire. US-003 le mesure ; si rien n'est servi, la story se réduit à l'exploitation du 429 terminal et le constat est enregistré.
- Les skills présentes dans `~/.agents/skills` sur la machine de développement suivent la spec agentskills et sont parsables telles quelles.
- `rmcp` 1.7.0 nomme le paramètre d'appel `CallToolRequestParams` au singulier ou au pluriel selon la version : la documentation consultée porte sur la version courante et l'ancienne API s'appelait `CallToolRequestParam`. US-010 tranche sur les sources locales.
- `rmcp` 1.7.0 n'expose pas d'API publique documentée de timeout par requête. Le repli est `tokio::time::timeout` autour de l'appel, plus la notification d'annulation du SDK.

### Hard Constraints

- **ADR-3, cœur headless :** `agent-core` ne dépend ni d'`agent-tui`, ni d'`agent-provider`, et n'émet jamais d'ANSI. Toute nouvelle capacité passe par un trait injecté dans `Deps` ou par une variante ajoutée à `AgentEvent`.
- **Contrats en extension seulement :** `AgentEvent` et `StreamEvent` sont consommés par la TUI et le mode headless. Les variantes s'ajoutent, elles ne se refondent pas, et le schéma JSONL n'incrémente sa version que si une ligne déjà émise change de forme (`docs/EVENT_SCHEMA.md`).
- **ADR-11 :** Linux uniquement, provider unique `OpenAiChatGpt`. Aucune story n'introduit macOS, Windows ou multi-provider.
- **ADR-8 :** crates internes nommées `agent-*`, binaire et commande publics nommés `pyxis`.
- **Sécurité de la configuration de projet :** `<workspace>/.pyxis/config.toml` ne peut déclarer ni hooks, ni racines writables, ni mode de permission (`crates/agent-cli/src/settings.rs:52`). Cette liste s'étend aux règles d'approbation et aux racines de skills.
- **Outils MCP :** `returns_untrusted()` vaut toujours `true`, description plafonnée à 2048 caractères, taint propagé intégralement (`docs/ARCHITECTURE.md` section 6).
- **Budget de complexité :** aucune abstraction spéculative. Une dépendance nouvelle doit être justifiée par un besoin présent.
- **`tasks/prd-harness-parity.md` EP-006 :** absorbé par ce PRD. Ses US-019 à US-022 doivent être marquées `CANCELLED` avec renvoi ici, pas réimplémentées.

## Quality Gates

Ces commandes doivent passer pour chaque user story :

- `cargo fmt --all -- --check` - formatage conforme
- `cargo clippy --workspace --all-targets` - lints du workspace, dont les `deny` sur `panic`, `unimplemented` et `dbg`. Volontairement sans `-D warnings` : le workspace déclare `unwrap_used` et `expect_used` en `warn` par décision documentée (`Cargo.toml`), un durcissement global rendrait la commande rouge sur du code sain
- `cargo test --workspace` - suite complète, 693 tests au point de départ

Pour les stories touchant au rendu terminal, gate supplémentaire :

- `cargo insta test --review` puis validation visuelle des snapshots modifiés, avec justification écrite de chaque diff accepté dans le message de commit

## Epics & User Stories

Release 1 couvre EP-001 et EP-002, release 2 couvre EP-003 et EP-004, release 3 couvre EP-005 et EP-006. Le phasage est structurel : EP-001 et EP-002 sont des câblages sur des données existantes et rendent le dogfood quotidien supportable, ce qui conditionne la qualité du retour d'usage sur tout le reste.

### EP-001: Visibilité de la consommation et surfaces de session

Ce que le système mesure déjà doit devenir visible. Aucune de ces stories n'introduit de nouvelle source de données, elles câblent des données produites et jetées.

**Definition of Done:** la jauge de contexte est alimentée par la fenêtre réelle du modèle, le compteur de tokens affiché vient du backend, l'état de quota est visible, et quatre commandes exposent l'état de session.

#### US-001: Fenêtre de contexte du modèle actif
**Description:** As a utilisateur, I want que Pyxis connaisse la taille de fenêtre du modèle que j'utilise, so that toute mesure de remplissage du contexte repose sur une valeur réelle et non sur une constante.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une réponse du catalogue `/models`, when elle est désérialisée, then la fenêtre de contexte de chaque modèle est conservée jusque dans le type exposé aux clients
- [ ] Given une réponse réelle du backend capturée pendant cette story, when elle est inspectée, then la présence effective du champ est confirmée par écrit dans la story, ou son absence est enregistrée et la story s'arrête là
- [ ] Given un modèle du catalogue sans fenêtre de contexte déclarée, when il est sélectionné, then l'absence est représentée explicitement et aucune valeur par défaut inventée n'est substituée
- [ ] Given une réponse de catalogue portant des champs inconnus, when elle est désérialisée, then la tolérance actuelle est préservée et aucun champ existant ne cesse d'être lu
- [ ] Given le mode headless sans découverte de catalogue, when un tour s'exécute, then l'absence de fenêtre connue ne provoque ni erreur ni message parasite

#### US-002: Consommation de contexte réelle dans la boucle et le contrat client
**Description:** As a client du cœur, I want recevoir la consommation de contexte rapportée par le backend rapportée à la fenêtre du modèle, so that je puisse afficher un remplissage sans réimplémenter d'estimation.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given un tour de modèle terminé, when l'événement de fin de round-trip est émis, then il porte la fenêtre de contexte du modèle actif en plus des compteurs déjà présents
- [ ] Given cette extension, when un consommateur existant ignore le nouveau champ, then son comportement est inchangé et la version du schéma JSONL n'est pas incrémentée
- [ ] Given un provider qui ne rapporte aucun usage sur un tour, when l'événement est émis, then il indique explicitement que la mesure est absente au lieu de rapporter zéro
- [ ] Given une fenêtre de contexte inconnue, when l'événement est émis, then aucun pourcentage n'est calculé dans le cœur, le calcul restant une décision de présentation
- [ ] Given la sonde de calibration actuellement écrite sur la sortie d'erreur depuis le cœur, when cette story s'achève, then elle ne passe plus par une écriture directe depuis `agent-core`

#### US-003: Remontée des limites d'usage de l'abonnement
**Description:** As a utilisateur sur abonnement, I want voir ma consommation de quota et l'heure de reset, so that je découvre la limite avant qu'elle ne casse ma session.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une réponse du backend portant un état de quota, when elle est reçue, then la fenêtre de quota, le pourcentage consommé et l'heure de reset sont capturés et exposés comme événement structuré
- [ ] Given l'inspection d'une réponse réelle pendant cette story, when aucun état de quota n'est servi, then le constat est enregistré par écrit et la story se limite à l'exploitation du refus 429 terminal
- [ ] Given un quota épuisé, when le backend refuse la requête, then le message présenté nomme la limite atteinte et l'heure de reset quand elle est connue, au lieu du corps HTTP brut
- [ ] Given un état de quota partiel ou mal formé, when il est lu, then il est ignoré sans faire échouer le tour et sans affichage trompeur
- [ ] Given aucune donnée de quota reçue, when la session tourne, then aucun indicateur vide ni valeur nulle n'est affiché

#### US-004: Jauge de contexte et compteur de tokens alimentés dans la TUI
**Description:** As a utilisateur, I want voir le remplissage réel de ma fenêtre de contexte, so that je sache quand la compaction approche au lieu de la subir.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un tour de modèle terminé, when l'événement correspondant arrive, then l'indicateur de contexte est mis à jour à partir des compteurs backend et de la fenêtre du modèle
- [ ] Given une session sans usage rapporté ou sans fenêtre connue, when la frame est dessinée, then l'indicateur reste absent et aucune valeur estimée ne le remplace
- [ ] Given l'estimation locale caractères sur quatre actuellement affichée, when cette story s'achève, then elle n'est plus la source de l'affichage de consommation
- [ ] Given une compaction déclenchée, when elle se produit, then l'indicateur reflète la baisse de remplissage au tour suivant
- [ ] Given les snapshots de rendu existants, when ils s'exécutent, then leurs diffs sont volontaires, revus un par un et justifiés dans le message de commit
- [ ] Given un terminal étroit, when l'indicateur est rendu avec les nouvelles valeurs, then aucun débordement horizontal ni troncature illisible n'apparaît

#### US-005: Commandes d'état de session
**Description:** As a utilisateur, I want interroger l'état de ma session sans quitter Pyxis, so that je puisse vérifier ma configuration, ma consommation et mon quota d'un coup.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-003, US-004

**Acceptance Criteria:**
- [ ] Given une session en cours, when l'utilisateur demande l'état, then il obtient le modèle actif, l'effort de raisonnement, le mode de permission, la portée du sandbox, l'espace de travail et l'identifiant de session
- [ ] Given une session en cours, when l'utilisateur demande sa consommation, then il obtient les jetons cumulés, le remplissage de contexte et l'état de quota quand il est connu
- [ ] Given une donnée non disponible, when l'état est affiché, then la ligne concernée indique explicitement l'indisponibilité au lieu d'être omise en silence
- [ ] Given ces commandes, when elles sont invoquées, then elles n'émettent aucune requête réseau et n'entrent pas dans le transcript envoyé au modèle
- [ ] Given le menu de commandes, when il est ouvert, then les nouvelles entrées y figurent avec leur description

#### US-006: Commandes de diff et de compaction manuelle
**Description:** As a utilisateur, I want voir les modifications en cours et déclencher une compaction quand je le décide, so that je garde la main sur le contexte et sur ce que l'agent a changé.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given un espace de travail modifié, when l'utilisateur demande le diff, then il obtient les modifications courantes avec le même périmètre que le diff agrégé de tour déjà implémenté
- [ ] Given un répertoire qui n'est pas un dépôt git, when le diff est demandé, then l'utilisateur reçoit un message expliquant la limite au lieu d'un résultat vide sans explication
- [ ] Given une session en cours, when l'utilisateur demande une compaction, then elle s'exécute et le transcript résultant est persisté comme une compaction automatique
- [ ] Given une compaction manuelle demandée pendant un tour actif, when la commande est reçue, then elle est refusée avec un message plutôt que d'entrer en concurrence avec le tour
- [ ] Given une compaction qui échoue, when l'erreur remonte, then la session reste utilisable et le transcript n'est pas tronqué

---

### EP-002: Fin de la friction d'approbation

Une confirmation doit signaler un risque, pas ponctuer chaque commande. La défense de taint reste intacte : elle est orthogonale à cette story et prime toujours.

**Definition of Done:** une commande sans effet de bord s'exécute sans question, une réponse peut être mémorisée pour une séquence de commande précise, et aucune mémorisation ne peut être détournée par un argument différent.

#### US-007: Classification du risque des commandes shell
**Description:** As a utilisateur, I want que les commandes manifestement sans effet de bord s'exécutent sans confirmation, so that je ne valide plus chaque `ls` et chaque `git status`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une commande shell, when sa permission est résolue, then la décision dépend de la commande elle-même et non plus d'une valeur fixe indépendante de l'entrée
- [ ] Given une commande figurant dans la liste des programmes sans effet de bord avec des arguments eux-mêmes sans effet de bord, when elle est exécutée en mode par défaut, then aucune confirmation n'est demandée
- [ ] Given une commande contenant un opérateur de composition, une redirection, une substitution de commande ou une expansion, when elle est classée, then elle n'est jamais classée sans effet de bord, quel que soit le programme invoqué
- [ ] Given une commande dont le programme est inconnu de la classification, when elle est exécutée, then le comportement actuel est conservé et une confirmation est demandée
- [ ] Given un contexte marqué non fiable, when une commande classée sans effet de bord est exécutée, then la classification ne contourne jamais la défense de taint existante
- [ ] Given la classification, when elle s'exécute, then elle opère sur des séquences de tokens et jamais sur une correspondance de sous-chaîne de la commande brute

#### US-008: Mémorisation d'approbation par séquence de commande
**Description:** As a utilisateur, I want répondre une fois pour une commande que j'approuve régulièrement, so that je ne revalide pas la même chose vingt fois dans une session.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007

**Acceptance Criteria:**
- [ ] Given une confirmation acceptée avec mémorisation, when la même séquence de tokens de préfixe se représente dans la session, then aucune confirmation n'est demandée
- [ ] Given une commande mémorisée, when une commande partageant seulement le début de sa chaîne mais différant en tokens se présente, then la confirmation est demandée normalement
- [ ] Given une commande contenant une substitution de commande, une variable ou une redirection, when l'utilisateur l'approuve, then la mémorisation est refusée et l'approbation ne vaut que pour cet appel, avec la raison affichée
- [ ] Given une mémorisation acquise, when la session se termine, then elle n'est pas persistée sur disque et ne survit pas au redémarrage
- [ ] Given un contexte marqué non fiable, when une commande mémorisée se présente, then la confirmation est de nouveau demandée, la défense de taint primant sur la mémorisation
- [ ] Given un refus mémorisé, when la même séquence se représente, then elle est refusée sans question et la raison est transmise au modèle

#### US-009: Dialogue d'approbation étendu et surface d'inspection
**Description:** As a utilisateur, I want choisir la portée de ma réponse au moment où je réponds, so that la mémorisation soit un acte explicite et vérifiable.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-008

**Acceptance Criteria:**
- [ ] Given un dialogue d'approbation, when il s'affiche pour une commande mémorisable, then il propose au moins accepter une fois, accepter pour la session et refuser
- [ ] Given un dialogue d'approbation pour une commande non mémorisable, when il s'affiche, then l'option de mémorisation est absente et la raison est visible
- [ ] Given des approbations mémorisées, when l'utilisateur demande à les inspecter, then il obtient la liste des séquences mémorisées de la session et peut la vider
- [ ] Given le dialogue étendu, when il est rendu, then il reste lisible sur un terminal de 40 colonnes et les snapshots correspondants sont mis à jour et justifiés
- [ ] Given une réponse par une touche ne correspondant à aucune option, when elle est pressée, then le dialogue reste ouvert et rien n'est approuvé

---

### EP-003: Outils MCP appelables par le modèle

Configurer un serveur MCP doit changer ce que l'agent sait faire. Cet epic remplace US-019 et US-020 de `tasks/prd-harness-parity.md`.

**Definition of Done:** un serveur MCP configuré expose des outils que le modèle invoque réellement, sans drapeau expérimental, avec taint intégral et sans qu'une panne du serveur affecte la session.

#### US-010: Adaptateur d'appel d'outil MCP
**Description:** As a mainteneur, I want pouvoir invoquer un outil d'un serveur MCP et récupérer son résultat, so that la découverte existante devienne une capacité réelle.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une connexion MCP établie, when un outil est invoqué avec ses arguments, then son résultat est retourné sous une forme textuelle exploitable par le registre d'outils
- [ ] Given un outil qui échoue fonctionnellement, when il est invoqué, then l'échec est rendu comme résultat d'outil en erreur et non comme erreur de protocole, conformément à la séparation imposée par le SDK
- [ ] Given un résultat portant du contenu non textuel, when il est rendu, then chaque élément est représenté par un descripteur borné plutôt que par son contenu brut
- [ ] Given un serveur qui ne répond pas, when un appel est émis, then il expire après un délai borné et l'échec nomme le serveur
- [ ] Given un serveur qui se déconnecte pendant un appel, when la déconnexion survient, then l'appel retourne une erreur nommant le serveur et la connexion repasse dans un état non connecté sans panique
- [ ] Given la version du SDK figée par le workspace, when les signatures sont écrites, then elles sont vérifiées sur les sources locales et non déduites d'une documentation portant sur une autre version

#### US-011: Enregistrement des outils MCP dans le registre avec nommage sûr
**Description:** As a mainteneur, I want que les outils MCP entrent dans le registre sous un nom unique et valide, so que deux serveurs ne se masquent pas et qu'aucune requête ne soit refusée pour un nom trop long.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**
- [ ] Given des outils venant de plusieurs serveurs, when ils sont enregistrés, then chaque nom est préfixé par son serveur d'origine et l'unicité est garantie
- [ ] Given un nom composé dépassant la limite de l'API modèle, when il est enregistré, then il est raccourci de façon déterministe et reste unique, et la correspondance vers le serveur et l'outil d'origine est conservée
- [ ] Given un nom d'outil contenant des caractères hors du jeu accepté par l'API modèle, when il est enregistré, then il est assaini et l'assainissement ne crée pas de collision
- [ ] Given un outil MCP enregistré, when ses métadonnées sont lues, then il déclare toujours retourner du contenu non fiable et n'est jamais marqué sûr pour la concurrence sans preuve
- [ ] Given une description d'outil dépassant la limite, when elle est exposée, then elle est tronquée à 2048 caractères
- [ ] Given deux serveurs exposant un outil de même nom, when le modèle en appelle un, then l'appel atteint le bon serveur

#### US-012: Connexion des serveurs au démarrage sans drapeau expérimental
**Description:** As a utilisateur, I want que mes serveurs MCP configurés soient connectés au lancement, so that je n'aie pas à connaître une variable d'environnement non documentée.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-011

**Acceptance Criteria:**
- [ ] Given des serveurs déclarés dans la configuration, when Pyxis démarre, then ils sont connectés sans variable d'environnement expérimentale
- [ ] Given un serveur indisponible au démarrage, when la liste d'outils est composée, then les outils des serveurs sains restent disponibles et l'indisponibilité est signalée sans bloquer la session
- [ ] Given un serveur lent à démarrer, when le délai de connexion est dépassé, then la session démarre sans lui et le délai global de démarrage n'augmente pas de plus de deux secondes
- [ ] Given le mode headless sans serveur configuré, when il démarre, then le comportement et la sortie sont inchangés
- [ ] Given un serveur qui tombe pendant la session, when un appel lui est adressé, then l'échec est attribué au serveur nommé et les autres serveurs restent utilisables
- [ ] Given la sandbox active, when les serveurs sont lancés, then leur lancement respecte le durcissement déjà appliqué aux sous-processus

#### US-013: Approbation et taint des appels d'outils MCP
**Description:** As a utilisateur, I want que le contenu venu d'un serveur MCP soit traité comme non fiable, so qu'un serveur compromis ne se transforme pas en exécution sur ma machine.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-011

**Acceptance Criteria:**
- [ ] Given un outil MCP appelé par le modèle, when son résultat revient, then le contenu est marqué non fiable et la propagation de taint force une confirmation avant toute action destructrice ou réseau dans le même tour
- [ ] Given un appel d'outil MCP, when la permission est résolue, then elle demande une confirmation par défaut, sans que la classification des commandes shell s'applique
- [ ] Given un serveur MCP, when il est configuré, then il ne peut élargir aucun périmètre de sécurité, ni racine writable, ni mode de permission
- [ ] Given un résultat MCP volumineux, when il est rendu, then il est tronqué selon la politique existante des sorties d'outils
- [ ] Given un serveur déclaré dans la configuration de projet, when la configuration est chargée, then les mêmes restrictions que pour les autres clés de sécurité s'appliquent

---

### EP-004: Skills conformes à la spécification ouverte

Une skill installée pour un autre agent doit fonctionner sans adaptation. Cet epic remplace US-021 de `tasks/prd-harness-parity.md`.

**Definition of Done:** un `SKILL.md` conforme est lu, sa description est connue du modèle, et l'invoquer injecte ses instructions au lieu d'envoyer son nom.

#### US-014: Lecture du frontmatter SKILL.md
**Description:** As a utilisateur, I want que Pyxis lise mes skills existantes, so que je n'aie pas à maintenir un format spécifique.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un répertoire de skill contenant un `SKILL.md` avec frontmatter, when Pyxis démarre, then le nom et la description sont lus et la skill est enregistrée
- [ ] Given un nom ne respectant pas les contraintes de la spec ou différent du nom du répertoire, when la skill est lue, then elle est rejetée avec une trace et le démarrage n'échoue pas
- [ ] Given une description dépassant la limite de la spec, when elle est lue, then elle est tronquée à cette limite
- [ ] Given un frontmatter contenant des chevrons, when il est lu, then ils sont neutralisés avant toute injection dans un prompt, le frontmatter étant du contenu non fiable
- [ ] Given un `SKILL.md` absent, mal formé, sans frontmatter exploitable ou portant des clés inconnues, when le répertoire est lu, then les clés inconnues sont ignorées, une skill inexploitable est écartée avec une trace, et le démarrage n'échoue pas
- [ ] Given l'absence totale de répertoire de skills, when Pyxis démarre, then aucun avertissement n'est émis

#### US-015: Catalogue de skills exposé au modèle
**Description:** As a utilisateur, I want que le modèle sache quelles skills sont disponibles, so qu'il puisse en proposer une sans que je la connaisse par cœur.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] Given un catalogue de skills chargé, when le contexte est composé, then le modèle reçoit le nom et la description de chaque skill disponible
- [ ] Given un catalogue volumineux, when il est exposé, then l'ensemble reste borné par un budget d'octets explicite et la troncature est signalée
- [ ] Given un catalogue vide, when le contexte est composé, then aucune section de skills n'est injectée
- [ ] Given une description issue d'un fichier du disque, when elle est injectée, then elle est encadrée comme contenu de niveau utilisateur et non comme autorité système, à l'image du bloc d'instructions de projet existant

#### US-016: Injection des instructions d'une skill invoquée
**Description:** As a utilisateur, I want qu'invoquer une skill injecte réellement ses instructions, so que la sélection produise un effet au lieu d'envoyer un mot au modèle.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-015

**Acceptance Criteria:**
- [ ] Given une skill sélectionnée, when elle est invoquée, then le corps de son `SKILL.md` est injecté dans le contexte du tour au lieu que son nom soit envoyé littéralement
- [ ] Given une skill dont le corps dépasse le budget d'octets, when elle est injectée, then elle est tronquée sur une frontière de caractère et la troncature est visible pour l'utilisateur et pour le modèle
- [ ] Given une skill supprimée du disque entre le démarrage et son invocation, when elle est invoquée, then l'échec est signalé sans panique et le tour n'est pas envoyé
- [ ] Given le corps injecté, when il est composé, then il est traité comme contenu non fiable et ne peut pas se présenter comme instruction de niveau système
- [ ] Given une skill invoquée, when le tour se déroule, then la session persistée permet de savoir quelle skill a été injectée

---

### EP-005: Hooks avec droit de veto

Ouvrir le troisième canal d'extension, sur le contrat vers lequel l'écosystème converge. Cet epic remplace US-022 de `tasks/prd-harness-parity.md`.

**Definition of Done:** un hook utilisateur reçoit un appel d'outil avant son exécution, peut le refuser, et un hook défaillant refuse au lieu d'autoriser.

#### US-017: Moteur de hooks et contrat d'échange
**Description:** As a utilisateur, I want déclarer des commandes exécutées autour des appels d'outils, so que je puisse automatiser mes propres règles.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un hook déclaré dans la configuration globale, when un outil est sur le point de s'exécuter, then le hook reçoit sur son entrée standard un objet JSON portant au moins le nom de l'événement, le nom de l'outil et ses arguments
- [ ] Given un hook qui écrit un objet JSON de décision sur sa sortie standard, when il se termine, then la décision est interprétée selon le contrat documenté et une décision inconnue est traitée comme un refus
- [ ] Given des hooks déclarés dans la configuration de projet, when la configuration est chargée, then ils sont ignorés avec un avertissement, seule la configuration globale pouvant en déclarer
- [ ] Given un hook qui écrit un volume important sur sa sortie, when il se termine, then la sortie est bornée avant d'être interprétée
- [ ] Given aucune déclaration de hook, when un outil s'exécute, then aucun processus n'est lancé et la latence par appel d'outil n'augmente pas de plus d'une milliseconde
- [ ] Given un hook déclaré vers un exécutable introuvable, when il est appelé, then l'appel d'outil est refusé et la raison nomme le hook

#### US-018: Veto avant appel d'outil
**Description:** As a utilisateur, I want qu'un hook puisse bloquer un appel d'outil, so que je puisse interdire une action selon mes propres règles.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-017

**Acceptance Criteria:**
- [ ] Given un hook qui retourne un refus, when il se termine, then l'outil n'est pas exécuté et la raison du refus est transmise au modèle
- [ ] Given un hook qui retourne une demande de confirmation, when il se termine, then l'utilisateur est sollicité même si le mode actif n'aurait pas demandé de confirmation
- [ ] Given un hook qui expire ou se termine en erreur, when l'événement précédant un appel est traité, then l'appel est refusé, conformément au principe fail-closed du projet
- [ ] Given un refus émis par un hook, when le mode de permission actif contourne normalement les confirmations, then le refus prime et l'outil n'est pas exécuté
- [ ] Given un hook qui refuse une commande, when le tour continue, then la session reste utilisable et le transcript reste valide

#### US-019: Événement suivant un appel d'outil
**Description:** As a utilisateur, I want qu'un hook s'exécute après un appel d'outil, so que je puisse déclencher un formatage ou une vérification automatique.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-018

**Acceptance Criteria:**
- [ ] Given un hook déclaré pour l'événement suivant un appel d'outil, when l'outil s'est exécuté, then le hook reçoit le nom de l'outil, ses arguments et son résultat
- [ ] Given un hook postérieur qui échoue ou expire, when il se termine, then l'échec est signalé sans invalider le résultat de l'outil déjà exécuté
- [ ] Given un résultat d'outil volumineux, when il est transmis au hook, then il est borné selon la même politique que les autres sorties
- [ ] Given un hook postérieur, when il se termine, then il ne peut pas modifier le résultat déjà transmis au modèle

---

### EP-006: Observabilité de processus

Rendre un incident diagnosticable sans instrumenter le cœur avec des écritures directes.

**Definition of Done:** un panic rend le terminal et laisse une trace exploitable, et une trace structurée est activable sans changer le comportement par défaut.

#### US-020: Trace structurée et filet de panic
**Description:** As a mainteneur, I want qu'un incident laisse une trace exploitable, so que je puisse diagnostiquer sans reproduire à l'aveugle.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un panic pendant une session interactive, when il survient, then le terminal est restauré dans un état utilisable avant que le message ne soit affiché
- [ ] Given un panic, when il survient, then le message et la localisation sont écrits dans une trace persistante hors de l'espace de travail
- [ ] Given une variable d'environnement de trace définie, when Pyxis s'exécute, then les crates émettent des événements structurés collectés par un souscripteur installé par le binaire seul
- [ ] Given aucune variable de trace définie, when Pyxis s'exécute, then aucune sortie supplémentaire n'est produite et la latence par tour n'augmente pas de plus d'une milliseconde
- [ ] Given l'émission de traces, when elle est ajoutée, then aucune crate autre que le binaire n'installe de souscripteur ni n'écrit directement sur une sortie de processus
- [ ] Given une trace émise pendant un tour, when elle contient le contenu d'un message ou d'un résultat d'outil, then ce contenu n'est inclus qu'au niveau de verbosité le plus élevé

## Functional Requirements

- FR-01: Le système doit connaître la fenêtre de contexte du modèle actif à partir du catalogue servi par le backend.
- FR-02: Le système doit exposer aux clients la consommation de contexte rapportée par le backend, rapportée à la fenêtre du modèle.
- FR-03: Le système ne doit PAS afficher une consommation de contexte dérivée d'une estimation locale une fois la mesure backend disponible.
- FR-04: Le système doit exposer l'état de quota d'abonnement quand le backend le sert, et nommer la limite atteinte quand il la refuse.
- FR-05: Le système doit résoudre la permission d'une commande shell à partir de la commande elle-même.
- FR-06: Le système ne doit PAS classer sans effet de bord une commande portant une composition, une redirection, une substitution ou une expansion.
- FR-07: Le système doit permettre de mémoriser une approbation pour une séquence de tokens de préfixe, pour la durée de la session uniquement.
- FR-08: Le système ne doit PAS permettre qu'une mémorisation d'approbation autorise une commande dont la séquence de tokens diffère.
- FR-09: Le système doit exposer les outils des serveurs MCP connectés à la boucle du modèle, sous un nom unique et valide pour l'API modèle.
- FR-10: Le système doit traiter tout contenu issu d'un serveur MCP comme non fiable.
- FR-11: Le système doit lire un `SKILL.md` conforme à la spécification ouverte et injecter ses instructions lorsqu'une skill est invoquée.
- FR-12: Le système doit permettre à un hook utilisateur de refuser un appel d'outil avant son exécution, et doit refuser lorsqu'un hook échoue ou expire.
- FR-13: Le système ne doit PAS honorer de hooks, de racines de skills ou de règles d'approbation déclarés par un fichier contrôlé par l'espace de travail.
- FR-14: Le système ne doit PAS faire échouer le démarrage à cause d'une skill, d'un hook ou d'un serveur MCP invalide.
- FR-15: Aucune crate autre que le binaire ne doit installer de souscripteur de trace ni écrire directement sur une sortie de processus.

## Non-Functional Requirements

- **Performance :** surcoût nul mesurable en l'absence de hook déclaré, et au plus 1 ms de latence ajoutée par appel d'outil quand un hook est déclaré. Délai d'appel d'outil MCP borné à 60 secondes par défaut. Délai d'exécution d'un hook borné à 5 secondes. Connexion des serveurs MCP au démarrage bornée à 2 secondes ajoutées au temps de lancement total. Classification d'une commande shell sous 1 ms. Rendu de frame P95 sous 16 ms avec l'indicateur de contexte alimenté.
- **Sécurité :** 100 % des résultats d'outils MCP marqués non fiables. 0 mémorisation d'approbation applicable à une séquence de tokens différente de celle approuvée. 0 mémorisation persistée sur disque. Un hook qui expire ou échoue avant un appel d'outil provoque un refus, jamais une autorisation. Une configuration contrôlée par l'espace de travail ne peut modifier aucune clé de sécurité. Aucun chevron issu d'un frontmatter de skill n'atteint le prompt.
- **Fiabilité :** un serveur MCP indisponible n'empêche jamais le démarrage d'une session. Une skill invalide n'empêche jamais le démarrage. 0 panique sur déconnexion d'un serveur MCP pendant un appel. 100 % des paniques restaurent le terminal.
- **Observabilité :** 100 % des variantes d'`AgentEvent` sérialisables en JSON. Trace structurée désactivée par défaut, avec 0 octet de sortie supplémentaire quand elle l'est. Contenu de message inclus dans les traces au seul niveau de verbosité maximal.
- **Maintenabilité :** CI complète sous 15 minutes, cache compris. Aucune régression sur les 693 tests existants. Aucune nouvelle dépendance de production sans justification écrite dans la story qui l'introduit.
- **Compatibilité :** la version du schéma d'événements JSONL n'est incrémentée que si une ligne déjà émise change de forme. La sortie textuelle du mode headless par défaut reste identique octet pour octet.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Fenêtre de contexte inconnue | Modèle absent du catalogue ou catalogue non découvert | Aucun pourcentage calculé, indicateur absent | aucun |
| 2 | Usage backend non rapporté sur un tour | Réponse sans bloc d'usage | Mesure signalée absente, pas de zéro affiché | aucun |
| 3 | Quota non servi par le backend | Réponse sans état de quota | Constat enregistré, aucun indicateur vide | aucun |
| 4 | Quota épuisé | Refus 429 terminal | Message nommant la limite et le reset connu | "Limite d'abonnement atteinte, reset à {heure}." |
| 5 | Commande composée déguisée en lecture | `ls && rm -rf build` | Jamais classée sans effet de bord, confirmation demandée | "Confirmation requise : commande composée." |
| 6 | Mémorisation détournée par argument | `git status` approuvé puis `git push --force` | Confirmation demandée, la séquence de tokens diffère | "Confirmation requise : commande différente." |
| 7 | Commande à substitution approuvée | `rm $(cat liste)` | Approbation valable pour cet appel seul, mémorisation refusée | "Non mémorisable : la commande contient une substitution." |
| 8 | Taint actif sur commande mémorisée | Lecture de contenu non fiable puis commande approuvée | Confirmation redemandée, le taint prime | "Contenu non fiable lu : confirmation requise." |
| 9 | Nom d'outil MCP trop long | Serveur au nom long exposant un outil au nom long | Raccourcissement déterministe, unicité préservée | aucun |
| 10 | Deux serveurs, même nom d'outil | Deux serveurs exposant `search` | Préfixe garantit l'unicité, l'appel atteint le bon serveur | aucun |
| 11 | Serveur MCP indisponible au démarrage | Serveur configuré qui ne répond pas | Outils des serveurs sains disponibles, indisponibilité signalée | "Serveur MCP '{x}' indisponible, ses outils sont absents." |
| 12 | Serveur MCP tombe pendant un appel | Processus enfant tué en vol | Erreur nommant le serveur, connexion remise à l'état non connecté | "Serveur MCP '{x}' déconnecté pendant l'appel." |
| 13 | Outil MCP en échec fonctionnel | Outil retournant un résultat d'erreur | Résultat d'outil en erreur transmis au modèle, pas d'erreur de protocole | aucun |
| 14 | `SKILL.md` sans frontmatter exploitable | Fichier vide ou YAML invalide | Skill écartée avec trace, démarrage normal | "Skill '{x}' ignorée : frontmatter illisible." |
| 15 | Nom de skill différent du répertoire | `name: foo` dans le répertoire `bar` | Skill rejetée avec trace | "Skill '{x}' ignorée : nom incohérent avec le répertoire." |
| 16 | Frontmatter portant des chevrons | Description contenant des balises | Chevrons neutralisés avant injection | aucun |
| 17 | Skill supprimée entre démarrage et invocation | Répertoire effacé en cours de session | Échec signalé, tour non envoyé | "Skill '{x}' introuvable." |
| 18 | Hook déclaré dans la configuration de projet | Dépôt cloné portant une configuration hostile | Clé ignorée avec avertissement | "Hooks ignorés : déclarés dans la configuration de projet." |
| 19 | Hook qui expire | Hook bloqué au-delà de son délai | Appel d'outil refusé, fail-closed | "Hook expiré, appel d'outil refusé." |
| 20 | Hook retournant une décision inconnue | Sortie JSON avec une valeur non prévue | Traitée comme un refus | "Décision de hook inconnue, appel refusé." |
| 21 | Répertoire sans dépôt git | `/diff` demandé hors dépôt | Message expliquant la limite | "Diff indisponible : ce répertoire n'est pas un dépôt git." |
| 22 | Compaction demandée pendant un tour | `/compact` pendant un tour actif | Refus explicite, pas de concurrence | "Un tour est en cours." |
| 23 | Panic pendant une session interactive | Bug de rendu | Terminal restauré, trace persistée | "Pyxis a rencontré une erreur fatale, trace écrite dans {chemin}." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | La classification des commandes crée un faux sentiment de sécurité et laisse passer une commande destructrice | Med | High | La classification n'autorise que par liste explicite de programmes, et tout opérateur de composition, redirection, substitution ou expansion disqualifie la commande. La défense de taint reste orthogonale et prime toujours. Tests dédiés sur les contournements documentés |
| 2 | La mémorisation d'approbation reproduit CVE-2026-22708 | Med | High | Mémorisation sur séquence de tokens d'argv et jamais sur chaîne, refus de mémoriser toute commande portant une substitution ou une variable, portée limitée à la session sans persistance disque |
| 3 | Le backend ne sert pas d'état de quota, rendant US-003 sans objet | Med | Med | La story impose la mesure avant l'implémentation et prévoit explicitement le repli sur l'exploitation du 429 terminal, avec constat écrit |
| 4 | La signature d'appel du SDK MCP diffère de la documentation consultée | High | Low | US-010 impose la vérification sur les sources locales de la version figée, la documentation consultée portant sur la version courante |
| 5 | Le raccourcissement des noms d'outils MCP crée une collision silencieuse | Med | High | L'unicité est un critère d'acceptation explicite et la correspondance inverse vers le serveur et l'outil d'origine est conservée. Test avec deux serveurs aux noms longs partageant un préfixe |
| 6 | Un serveur MCP malveillant obtient une exécution, comme dans CVE-2025-6514 | Low | High | Contenu MCP non fiable à 100 %, confirmation par défaut sur tout appel MCP, aucun élargissement de périmètre de sécurité par un serveur, lancement sous le durcissement de sous-processus existant |
| 7 | Le périmètre de 20 stories dépasse le seuil au-delà duquel les PRD échouent | High | Med | Phasage explicite en trois releases livrables indépendamment. R1 seule résout la friction quotidienne et rend la mesure de contexte honnête |
| 8 | L'ajout de champs aux événements casse un consommateur | Low | Med | Les champs sont ajoutés, jamais modifiés, la version du schéma n'est pas incrémentée, et le contrat est couvert par les tests des trois consommateurs actuels |
| 9 | La spec agentskills évolue et le format lu devient obsolète | Low | Low | Seuls `name` et `description` sont exploités, socle stable de la spec. Les clés inconnues sont ignorées comme la spec l'impose |
| 10 | L'ajout de tracing dégrade la latence ou fait entrer de l'I/O dans le cœur | Low | Med | Émission seule dans les crates, souscripteur installé par le binaire, critère d'acceptation mesurant le surcoût à moins d'une milliseconde et vérifiant l'absence de sortie quand la trace est inactive |

## Non-Goals

- **Langage de règles déclaratif pour les commandes shell.** Codex utilise Starlark, et l'ergonomie d'écriture de ces règles est le reproche qui lui est fait. Une liste explicite de programmes sans effet de bord couvre le besoin présent. Un langage de règles serait à reconsidérer quand un utilisateur autre qu'Arthur devra encoder sa propre politique.
- **Persistance des approbations mémorisées entre sessions.** La portée session est un choix de sécurité, pas une limitation technique. Une allow-list persistante est exactement le vecteur de CVE-2026-22708 et demande un modèle de confiance qui n'existe pas encore.
- **Transport MCP distant et OAuth par serveur.** Seul stdio reste supporté, conformément à la roadmap Phase 2.
- **Ressources MCP, élicitation, notifications de progression.** Les outils sont la capacité qui manque ; le reste double le périmètre d'EP-003 sans besoin présent.
- **Répertoires `scripts/`, `references/` et `assets/` des skills.** Seuls le frontmatter et le corps du `SKILL.md` sont exploités en v1.
- **Modification d'un appel d'outil par un hook.** Le contrat de référence permet à un hook de réécrire les arguments. Autoriser, refuser et demander couvre le besoin présent ; réécrire ouvre une surface de confusion entre ce que le modèle a demandé et ce qui a été exécuté.
- **Ventilation du contexte par composant.** La commande `/context` de Claude Code ventile prompt système, outils, MCP et skills. Ce PRD livre le total et le pourcentage ; la ventilation demande une comptabilité par section qui n'existe nulle part dans le cœur.
- **Support macOS et multi-provider.** Exclus par ADR-11.
- **Steering en cours de tour, retour arrière sur un message, fork de session, reflow du scrollback.** Écarts réels et documentés par l'audit, aucun ne bloque l'usage quotidien. Ils relèvent d'un PRD ultérieur.

## Files NOT to Modify

- `crates/agent-core/src/event.rs` et `crates/agent-core/src/provider.rs` : contrats consommés par la TUI et le mode headless. Extension par ajout de variantes ou de champs optionnels uniquement, jamais de refonte.
- `crates/agent-core/src/cancel.rs` et la frontière d'arrêt de `crates/agent-core/src/agent.rs` : annulation coopérative livrée par EP-001 du PRD précédent, couverte par des tests d'intégrité de session. Ne pas réordonner la réconciliation par rapport à la persistance.
- `crates/agent-sandbox/src/fs.rs` autour de `restrict_self` : séquence irréversible exécutée avant le runtime tokio.
- `crates/agent-tools/src/path.rs` : sous-chemins protégés contre l'exécution différée, couverts par des tests de sécurité. Extensibles, jamais affaiblis.
- `crates/agent-tools/src/permission.rs` : la logique fail-closed et la propagation de taint sont étendues par EP-002, jamais contournées. La priorité du taint sur toute autorisation reste inviolable.
- `docs/ARCHITECTURE.md` invariants 1 à 9 : amendables uniquement par un ADR.
- `docs/codex-harness-parity-audit.md` et `docs/codex-harness-parity-audit-2026-07-25.md` : constats datés servant de référence.
- `tasks/prd-pyxis.md`, `tasks/prd-codex-orchestration.md`, `tasks/prd-response-rendering.md`, `tasks/prd-codex-tui-parity.md` et leurs fichiers de statut : archives historiques.

## Technical Considerations

- **Fenêtre de contexte :** le champ doit-il vivre sur le type de catalogue exposé aux clients, ou sur la configuration de run résolue au démarrage ? Le premier suit la source de vérité, le second évite de propager le catalogue jusqu'au cœur. Recommandation : porter la valeur dans la configuration de run, le cœur n'ayant pas à connaître le catalogue.
- **Classification des commandes :** la tokenisation doit-elle utiliser un analyseur de shell existant, ou une reconnaissance restreinte aux constructions qui disqualifient ? Recommandation : la seconde, parce que la décision recherchée est un refus par défaut et qu'un analyseur complet crée une surface d'erreur bien plus large que le besoin. L'ingénierie doit confirmer que la reconnaissance couvre les opérateurs de composition, les redirections, les substitutions, les expansions et les guillemets.
- **Clé de mémorisation :** la séquence de tokens de préfixe doit-elle être bornée en longueur, et le répertoire de travail doit-il faire partie de la clé ? Une même commande dans un autre répertoire n'a pas le même effet.
- **Appel MCP :** la signature exacte du SDK à la version figée doit être lue sur les sources locales avant écriture. Le timeout par requête n'ayant pas d'API publique documentée, le repli recommandé est une enveloppe de délai plus la notification d'annulation du SDK. L'ingénierie doit confirmer quelle variante d'erreur remonte quand le transport enfant meurt en vol.
- **Raccourcissement des noms d'outils :** troncature avec empreinte courte, ou index par serveur ? Le premier reste lisible pour le modèle, le second garantit l'unicité sans calcul. Recommandation : troncature plus empreinte, avec l'unicité vérifiée à l'enregistrement.
- **Frontmatter YAML :** le workspace n'a aucune dépendance YAML. Faut-il une bibliothèque maintenue, ou un analyseur restreint aux deux clés scalaires utilisées ? Le précédent de `parse_tomlish_string` invite à la prudence, mais deux clés scalaires ne sont pas un langage. Recommandation : analyseur restreint, avec rejet explicite de tout ce qui n'est pas une paire clé-valeur simple.
- **Exécution des hooks :** un hook s'exécute-t-il sous la sandbox du processus, sachant que `restrict_self` s'applique à tout l'arbre ? Conséquence à confirmer : un hook ne pourra pas écrire hors des racines writables, ce qui doit être documenté plutôt que découvert.
- **Trace structurée :** quelle granularité de spans, et le cœur doit-il émettre des événements de trace alors qu'ADR-3 le veut sans effet de bord ? Émettre un événement `tracing` n'est pas une I/O tant qu'aucun souscripteur n'est installé, mais l'ingénierie doit trancher si cela reste conforme à l'esprit de l'invariant.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Écart entre consommation affichée et usage backend | Facteur 3 à 24 (estimation caractères sur quatre) | Moins de 5 % | Month-1 | Comparaison sur 20 tours avec la sonde de calibration |
| Tours affichant une jauge de contexte alimentée | 0 % | 100 % des tours où le backend rapporte un usage | Month-1 | Snapshot de rendu plus vérification manuelle en session |
| Confirmations demandées pour des commandes sans effet de bord | 100 % | 0 % | Month-1 | Journal d'une session de 50 tours en dogfood |
| Confirmations totales par session de 50 tours | Non mesuré, estimé au-dessus de 30 | Moins de 3 | Month-6 | Journal d'usage personnel |
| Outils MCP appelables par le modèle | 0 sur N listés | 100 % des outils listés | Month-6 | Test d'intégration avec serveur MCP simulé |
| Skills produisant une injection d'instructions | 0 sur N installées | 100 % des skills conformes à la spec | Month-6 | Test sur le contenu de `~/.agents/skills` |
| Paniques restaurant le terminal | 0 % | 100 % | Month-1 | Test déclenchant un panic sous harness |
| Écarts pertinents ouverts de l'audit du 2026-07-25 | 102 | 102 moins ceux couverts par ce PRD, recomptés | Month-6 | Nouvelle passe d'audit sur les dimensions traitées |

## Open Questions

- Faut-il marquer US-019 à US-022 de `tasks/prd-harness-parity.md` en `CANCELLED` avec renvoi vers ce PRD, ou clore EP-006 en le déclarant absorbé ? Le second préserve mieux l'historique mais laisse un epic `DONE` sans code livré. À trancher par Arthur avant le démarrage d'EP-003, parce que cela change ce que le tracker signifie.
- La classification des commandes doit-elle être extensible par configuration globale dès la première version, ou rester une liste interne ? Une liste interne est plus sûre et plus simple ; une liste configurable évite un cycle de release pour ajouter un outil personnel. À trancher après une semaine de dogfood sur EP-002.
- Le corps d'une skill invoquée doit-il entrer dans le transcript persisté, ou rester un message éphémère recomposé à la reprise ? Le premier rend la session autonome, le second évite de dupliquer un contenu qui peut être volumineux. À trancher avant US-016, car cela change le contrat d'`agent-session`.
- Un hook doit-il pouvoir observer les événements de cycle de vie de session en plus des appels d'outils ? Le contrat de référence en déclare une douzaine ; ce PRD n'en implémente que deux. À réévaluer après le premier usage réel d'EP-005.
[/PRD]
