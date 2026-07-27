[PRD]
# PRD: Pyxis : parité Codex CLI par le code

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-27 | Arthur Jean | Rédaction initiale. Chaque story nomme l'implémentation Codex de référence (`path:line`) et le fichier Pyxis à modifier. Dérivé de `docs/codex-harness-parity-audit-2026-07-27.md` pour le constat et de `docs/strategie-parite-codex-2026-07-27.md` pour le refus du fork. |

## Problem Statement

L'audit du 2026-07-27 laisse 82 écarts. La lecture de l'implémentation Codex montre que la majorité ne sont pas des fonctionnalités manquantes mais des **modèles de données absents** : Pyxis a un booléen là où Codex a une énumération, une constante là où Codex a une couche, une décision fixe là où Codex a une politique. Six écarts structurent tous les autres.

1. **Le confinement est un booléen.** `crates/agent-cli/src/main.rs:115` ne connaît que `--no-sandbox`, et `crates/agent-sandbox/src/fs.rs` applique un périmètre unique. Codex modélise `SandboxPolicy` en quatre variantes porteuses de données (`codex-rs/protocol/src/protocol.rs:995`) : `DangerFullAccess`, `ReadOnly { network_access }`, `ExternalSandbox { network_access }`, `WorkspaceWrite { writable_roots, network_access, exclude_tmpdir_env_var, exclude_slash_tmp }`. Sans ce type, ni le mode lecture seule, ni le contrôle réseau par mode ne sont exprimables.

2. **Les sous-chemins protégés ne couvrent pas `bash`.** `crates/agent-tools/src/path.rs:29` (`PROTECTED_SUBPATHS`) refuse `.git/` et `.pyxis/` pour `write` et `edit`, et `docs/CURRENT_STATUS.md` documente le trou : les règles Landlock étant additives, un droit d'écriture accordé sur le workspace ne peut pas être soustrait pour un sous-chemin, donc une commande shell y écrit encore. Codex résout au niveau **politique** et non noyau : `WritableRoot { root, read_only_subpaths, protected_metadata_names }` avec `is_path_writable` (`protocol.rs:1050-1078`), évalué avant l'exécution.

3. **La politique d'approbation n'a pas d'états.** `crates/agent-tools/src/permission.rs:20` porte cinq modes globaux, sans granularité par catégorie de demande. Codex a `AskForApproval::Granular(GranularApprovalConfig)` (`protocol.rs:908,935`) avec cinq drapeaux indépendants : `sandbox_approval`, `rules`, `skill_approval`, `request_permissions`, `mcp_elicitations`. Et sa classification de commandes rend trois états (`Decision { Allow, Prompt, Forbidden }`, `codex-rs/execpolicy/src/decision.rs:9`) là où `crates/agent-tools/src/command.rs:59` n'en rend que deux : il n'existe aucun moyen d'interdire une commande.

4. **La configuration n'a pas de provenance.** `crates/agent-cli/src/settings.rs` fusionne cinq couches en dur, sans type qui les nomme. Codex a `ConfigLayerSource` avec une fonction `precedence() -> i16` explicite (`codex-rs/config/src/config_layer_source.rs:6,31`), des profils nommés (`ConfigProfile`, `profile_toml.rs:24`, 15+ champs surchargeables) et une couche de surcharges de session (`build_cli_overrides_layer`, `overrides.rs:7`). Pyxis n'a ni profils, ni `-c cle=valeur`, ni moyen de dire d'où vient une valeur effective.

5. **Le modèle ne dispose que de six outils.** `crates/agent-cli/src/main.rs:1013-1018` enregistre `Read`, `Glob`, `Grep`, `Write`, `Edit`, `Bash`. Quatre familles Codex manquent et changent le comportement, pas seulement la couverture : `update_plan` (`handlers/plan_spec.rs:7`), `apply_patch` (`codex-rs/apply-patch/`, grammaire `apply_patch.lark`), `view_image` (`handlers/view_image_spec.rs:15`, alors que `ContentBlock::Image` existe déjà dans `crates/agent-core/src/message.rs:109` et n'est jamais produit), et l'exécution shell persistante avec stdin (`handlers/unified_exec/exec_command.rs`, `write_stdin.rs`) là où `bash` est one-shot.

6. **MCP s'arrête au transport stdio.** `crates/agent-mcp/src/lib.rs:8` documente le report des transports HTTP, des ressources et de l'élicitation. Codex a l'adaptateur Streamable HTTP (`codex-rs/rmcp-client/src/http_client_adapter.rs:54`), l'OAuth par serveur, le filtrage d'outils (`enabled_tools` / `disabled_tools`) et un mode d'approbation par serveur et par outil (`codex-rs/config/src/mcp_types.rs`). Côté extensibilité, `crates/agent-tools/src/hooks.rs:49-50` implémente 2 événements contre 11 (`codex-rs/hooks/src/lib.rs:20`).

**Why now:** l'option du fork a été évaluée et rejetée sur mesures (`WireApi` à un seul variant, aucune tranche extractible, 707 commits amont sur 30 jours). La voie retenue est le portage sélectif, ce qui suppose de savoir **exactement quoi porter et où**. Ce travail de repérage est fait : chaque story ci-dessous nomme sa source Codex et sa cible Pyxis. Le repérage se périme au rythme de l'amont, donc il se consomme maintenant.

## Overview

Ce PRD est un plan de portage, story par story, de la structure de données ou du mécanisme Codex vers le fichier Pyxis correspondant. Il ne fait pas de la parité de surface : il porte les **modèles** qui rendent les surfaces exprimables. Une story typique remplace un booléen par une énumération porteuse de données, puis câble les deux ou trois points qui la consomment.

L'ordre suit la dépendance technique, pas la valeur perçue. **EP-001 pose la politique d'exécution et le périmètre de sandbox**, parce que `SandboxPolicy` est le type dont dépendent le mode lecture seule, le contrôle réseau, l'escalade et la protection réelle des sous-chemins. **EP-002 pose la configuration en couches**, parce que sans provenance nommée ni profils il n'y a nulle part où déclarer les modes qu'EP-001 vient de rendre possibles. **EP-003 étend la suite d'outils**, **EP-004 complète MCP**, **EP-005 ouvre les extensions**, **EP-006 expose tout cela en surface**.

Trois décisions d'adaptation, prises après lecture du code Codex et non par défaut. D'abord, `codex-execpolicy` **n'est pas vendorisé** : il dépend de `starlark`, un interpréteur de langage complet, pour classer des commandes shell. Seul son modèle `Decision` à trois états est repris, exprimé en TOML, format déjà dans le graphe. Ensuite, la protection des sous-chemins est portée **au niveau politique** comme chez Codex (`is_path_writable` évalué avant l'exécution), ce qui ferme le trou `bash` que Landlock seul ne peut pas fermer. Enfin, `codex-file-search` sera pris avec `nucleo` depuis crates.io et non avec le rev git épinglé par Codex (`codex-rs/Cargo.toml:354`), pour ne pas hériter d'une dépendance de fork.

Les invariants du dépôt tiennent : `agent-core` ne gagne aucune dépendance d'entrée-sortie, aucune clé élargissant un périmètre ne peut venir du workspace, et toute reprise de code Codex est inscrite dans `docs/codex-port-inventory.md` sous le protocole de `NOTICE-CODEX.md`.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Écarts de parité fermés sur les 42 relevant du harness interactif local | 20 fermés, prouvés par test | 38 fermés |
| Modes de sandbox exprimables | 4 variantes de politique, chacune couverte par un test | Escalade après échec sandbox opérationnelle |
| Sous-chemins protégés effectifs contre `bash` | `.git/` et `.pyxis/` non écrivables par une commande shell | Métadonnées de workspace protégées par déclaration |
| Outils exposés au modèle | 10 (6 actuels plus 4 familles portées) | 10, plus outils MCP filtrés par serveur |
| Événements de hooks | 6 sur 11 | 11 sur 11 |
| Couverture de tests du code porté | 100 % des nouvelles branches de décision | Aucun chemin de sécurité sans test négatif |

## Target Users

### Mainteneur de Pyxis

- **Role:** Arthur, auteur unique, qui porte du code depuis un dépôt de référence de 1,22 M lignes.
- **Behaviors:** lit l'implémentation Codex, l'adapte aux invariants de Pyxis, refuse ce qui ne passe pas le budget de complexité.
- **Pain points:** l'écart se mesure en modèles de données absents, ce qui rend chaque fonctionnalité isolée artificiellement coûteuse tant que le type sous-jacent manque.
- **Current workaround:** ajouter des booléens et des constantes au coup par coup, ce qui a produit six modes de permission mais aucun mode de sandbox.
- **Success looks like:** chaque story remplace une constante par un type, et la surface qui la consomme devient triviale à ajouter.

### Utilisateur de l'agent en terminal

- **Role:** développeur qui pilote Pyxis sur un dépôt réel, sous confinement.
- **Behaviors:** alterne entre phases de lecture, d'édition et d'exécution ; ajuste le périmètre selon la confiance qu'il a dans la tâche.
- **Pain points:** ne peut ni restreindre à la lecture seule, ni interdire une commande, ni laisser passer un sous-domaine, ni reprendre une commande bloquée par le sandbox sans tout désactiver.
- **Current workaround:** `--no-sandbox`.
- **Success looks like:** ne jamais avoir de raison de passer `--no-sandbox`.

## Research Findings

Lecture directe des deux dépôts clonés localement, le 2026-07-27. Codex à `95637f7056`, Pyxis à `0c1cf17`.

### Competitive Context

- **Codex CLI** : 1,22 M lignes, 107 crates, ~90 atteints depuis le binaire. Mono-wire (`WireApi` à un seul variant, `codex-rs/model-provider-info/src/lib.rs:57-61`). Mûr sur le harness, non forkable en solo (707 commits sur 30 jours, 176 contributeurs sur 90 jours).
- **Market gap** : Pyxis garde un trait `Provider` réellement générique (7 méthodes dont 5 par défaut, `crates/agent-core/src/provider.rs:408`), un taint OWASP LLM01 et des hooks fail-closed que Codex n'a pas. Le portage doit préserver ces trois propriétés, pas les diluer.

### Best Practices Applied

- **Politique d'exécution à trois états** plutôt que deux : `Decision { Allow, Prompt, Forbidden }`.
- **Protection de sous-chemin au niveau politique** et non noyau : `WritableRoot::is_path_writable`, seul moyen de couvrir un chemin d'exécution que les règles additives du noyau ne peuvent pas restreindre.
- **Provenance de configuration nommée** avec `precedence() -> i16` : rend l'origine d'une valeur effective inspectable au lieu d'implicite.
- **Approbation granulaire par catégorie de demande** plutôt qu'un mode global unique.
- **Filtrage d'outils MCP par serveur** (`enabled_tools` puis `disabled_tools`, dans cet ordre) : réduit la surface exposée au modèle sans désactiver le serveur.

### Risk Areas

- Le canal d'abonnement ChatGPT n'a jamais été déclaré conforme par OpenAI pour un client tiers, et le précédent Anthropic (interdiction officielle des tokens OAuth Pro/Max dans les outils tiers, enforcement en avril 2026) montre qu'il peut fermer sans préavis. Hors périmètre de ce PRD, mais il conditionne la valeur du trait `Provider` générique que le portage doit préserver.

## Assumptions & Constraints

### Assumptions (to validate)

- La protection des sous-chemins peut être portée au niveau politique sans coût de latence perceptible, parce que l'évaluation est un test de préfixe de chemin. Validée par US-002.
- Le format TOML suffit à exprimer les règles d'exécution utiles, sans interpréteur. Fondé sur le fait que la classification actuelle porte déjà sur des séquences de tokens et non sur du calcul. Validée par US-006.
- Les modèles `*-codex` produisent de meilleurs résultats d'édition avec `apply_patch` qu'avec `edit`. Fondé sur le fait qu'ils y sont entraînés, **non mesuré sur Pyxis**. Validée par le critère de mesure d'US-010.

### Hard Constraints

- `agent-core` ne doit gagner aucune dépendance d'entrée-sortie (ADR-3). Toute capacité nouvelle passe par une variante d'`AgentEvent` ou un trait injecté dans `Deps`.
- Aucune clé élargissant un périmètre (`permission_mode`, `writable_roots`, `hooks`, et les clés ajoutées par ce PRD qui les rejoignent) ne peut venir d'un fichier contrôlé par le workspace (`SECURITY_KEYS`, `crates/agent-cli/src/settings.rs:55`).
- Le taint untrusted (OWASP LLM01) ne peut être affaibli par aucune politique ajoutée ici. Une règle qui autorise ne peut jamais l'emporter sur une confirmation forcée par le taint.
- Les hooks restent restrictifs uniquement : `allow` se lit « pas d'objection », jamais comme un contournement. Cette divergence délibérée avec Codex est préservée.
- Toute reprise de code Codex est inscrite dans `docs/codex-port-inventory.md` : licence et mentions conservées, fichiers modifiés signalés (Apache-2.0 §4), marque « Codex » non réutilisée (§6).
- Aucun fork du dépôt Codex, aucune dépendance git pointant vers un fork Codex.
- Aucune régression des 835 tests et 37 snapshots existants.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatage conforme
- `cargo clippy --workspace --all-targets` - aucun avertissement sur les lints obligatoires du workspace
- `cargo test --workspace` - suite complète verte

## Epics & User Stories

### EP-001: Politique d'exécution et périmètre de sandbox

Porter le modèle de confinement de Codex : une politique à variantes porteuses de données, des racines writables avec sous-chemins soustraits, un contrôle réseau par mode, et une escalade après échec imputé au sandbox.

**Definition of Done:** les quatre variantes de politique sont exprimables et testées, `bash` ne peut plus écrire dans un sous-chemin protégé, et aucun scénario de travail normal n'impose `--no-sandbox`.

#### US-001: Modéliser la politique de sandbox en variantes

**Description:** En tant qu'utilisateur, je veux choisir entre lecture seule, écriture sur le workspace et accès complet, afin d'adapter le confinement à la phase de travail au lieu de tout couper.

**Référence Codex:** `codex-rs/protocol/src/protocol.rs:995-1042` (`enum SandboxPolicy`), `codex-rs/protocol/src/config_types.rs:86` (`enum SandboxMode`).
**Cible Pyxis:** `crates/agent-sandbox/src/fs.rs`, `crates/agent-sandbox/src/lib.rs`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Un type de politique remplace le booléen de confinement actuel, avec au minimum les variantes accès complet, lecture seule et écriture sur workspace, chacune portant ses données propres.
- [ ] Given la variante lecture seule, when un outil tente une écriture, then l'écriture est refusée et le refus nomme la variante en cause.
- [ ] Given la variante écriture sur workspace, when Pyxis démarre, then le périmètre est identique au comportement actuel, prouvé par les tests de sandbox existants inchangés.
- [ ] Given la variante accès complet, when Pyxis démarre, then aucun confinement filesystem n'est posé et l'état est visible en permanence dans la ligne de statut.
- [ ] Given une plateforme sans Landlock, when une variante confinée est demandée, then la dégradation est annoncée explicitement et la variante réellement appliquée est nommée, jamais silencieusement.
- [ ] Given les règles Landlock additives, when la variante lecture seule est appliquée, then elle est posée comme un jeu de règles distinct et non par soustraction d'un droit déjà accordé.

#### US-002: Soustraire des sous-chemins d'une racine writable

**Description:** En tant qu'utilisateur, je veux que `.git/` et `.pyxis/` restent non écrivables même par une commande shell, afin qu'une écriture différée ne contourne pas le confinement.

**Référence Codex:** `codex-rs/protocol/src/protocol.rs:1050-1078` (`struct WritableRoot`, `is_path_writable`, `read_only_subpaths`, `protected_metadata_names`).
**Cible Pyxis:** `crates/agent-tools/src/path.rs:29` (`PROTECTED_SUBPATHS`), `crates/agent-tools/src/bash.rs`, `crates/agent-sandbox/src/fs.rs`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Une racine writable porte une liste de sous-chemins en lecture seule, évaluée avant l'exécution et non par le noyau.
- [ ] Given une commande `bash` qui écrit dans `.git/hooks/`, when elle est évaluée, then elle est refusée avant exécution, ce qui ferme le trou documenté dans `docs/CURRENT_STATUS.md`.
- [ ] Given une commande `bash` qui écrit dans `.pyxis/config.toml`, when elle est évaluée, then elle est refusée avant exécution.
- [ ] Given une écriture atteignant un sous-chemin protégé par un lien symbolique, when elle est évaluée, then elle est refusée : la résolution des liens précède la décision, comme le fait déjà `path.rs:84`.
- [ ] Given une commande dont la cible d'écriture n'est pas déterminable statiquement, when elle est évaluée, then le comportement est fail-closed et documenté, jamais une autorisation par défaut.
- [ ] Given la protection ajoutée, when les tests de performance tournent, then le surcoût par appel d'outil reste sous 1 ms.

#### US-003: Contrôler le réseau par la politique et par suffixe de domaine

**Description:** En tant qu'utilisateur, je veux qu'autoriser un domaine couvre ses sous-domaines et que l'accès réseau soit une propriété de la politique, afin de ne plus désactiver le sandbox pour laisser passer un appel légitime.

**Référence Codex:** `codex-rs/protocol/src/protocol.rs:1003-1006,1028-1030` (`network_access` porté par les variantes de politique).
**Cible Pyxis:** `crates/agent-sandbox/src/proxy.rs:30` (égalité stricte d'hôte), `crates/agent-sandbox/src/fs.rs`.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] L'accès réseau devient une donnée portée par la variante de politique, et non un réglage indépendant.
- [ ] Given une autorisation sur `github.com`, when un sous-processus contacte `api.github.com`, then la connexion est autorisée.
- [ ] Given une autorisation sur `github.com`, when un sous-processus contacte `notgithub.com` ou `evil-github.com`, then la connexion est refusée : la correspondance se fait sur une frontière d'étiquette de domaine, jamais sur une sous-chaîne.
- [ ] Given une autorisation sur `api.github.com`, when un sous-processus contacte `github.com`, then la connexion est refusée : l'autorisation descend, elle ne remonte pas.
- [ ] Given une politique dont l'accès réseau est fermé, when une allow-list est fournie, then le conflit est résolu de façon déterministe et documentée.
- [ ] Given un hôte refusé, when le refus se produit, then il est restitué à l'utilisateur avec le nom d'hôte et l'allow-list active, et non seulement journalisé.

#### US-004: Reprendre une commande échouée à cause du sandbox

**Description:** En tant qu'utilisateur, je veux qu'une commande bloquée par le confinement soit identifiée comme telle et puisse être relancée avec un périmètre élargi pour ce seul appel, afin que l'agent cesse de boucler sur des variantes de la même commande.

**Référence Codex:** `codex-rs/shell-escalation/`.
**Cible Pyxis:** `crates/agent-tools/src/bash.rs`, `crates/agent-tools/src/permission.rs`.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given une commande dont l'échec est imputable au confinement, when elle échoue, then la cause est distinguée d'un échec applicatif et nommée dans le résultat rendu au modèle.
- [ ] Given une cause identifiée comme confinement, when l'utilisateur est sollicité, then il peut autoriser une réexécution au périmètre élargi pour ce seul appel, sans changer la politique de session.
- [ ] Given une escalade accordée, when la commande se relance, then l'élargissement ne survit pas à cet appel.
- [ ] Given le mode de permission le plus permissif, when une escalade est proposée, then elle reste une décision explicite et n'est jamais automatique.
- [ ] Given un taint untrusted récent sur le tour, when une escalade est proposée, then la confirmation est forcée quel que soit le mode, conformément à la défense existante.
- [ ] Given une cause d'échec ambiguë, when elle est classée, then le classement est fail-closed : pas d'escalade proposée sur un doute.

---

### EP-002: Configuration en couches, profils et surcharges

Porter le modèle de configuration de Codex : une provenance nommée avec précédence explicite, des profils, une couche de surcharges de session, et les drapeaux CLI qui la pilotent.

**Definition of Done:** l'origine de toute valeur effective est inspectable, un profil nommé change modèle, effort, approbation et sandbox en une option, et les modes d'EP-001 sont sélectionnables en ligne de commande.

#### US-005: Nommer la provenance de chaque couche de configuration

**Description:** En tant qu'utilisateur, je veux savoir d'où vient une valeur de configuration effective, afin de diagnostiquer un comportement inattendu sans lire le code.

**Référence Codex:** `codex-rs/config/src/config_layer_source.rs:6-50` (`enum ConfigLayerSource`, `fn precedence() -> i16`).
**Cible Pyxis:** `crates/agent-cli/src/settings.rs` (fusion actuelle en dur des cinq couches).

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Un type nomme chaque couche de configuration et expose une précédence explicite, remplaçant l'ordre implicite actuel.
- [ ] Given la configuration résolue, when l'utilisateur l'inspecte par `/status`, then chaque valeur non par défaut affiche la couche dont elle provient.
- [ ] Given une clé définie dans plusieurs couches, when la résolution s'exécute, then la couche de plus forte précédence gagne et la comparaison est faite sur la précédence, jamais sur l'ordre d'insertion.
- [ ] Given une clé de sécurité définie par la couche projet, when la résolution s'exécute, then elle est écartée avec un avertissement nommant la couche, comportement actuel préservé.
- [ ] Given une nouvelle couche ajoutée au code, when elle n'a pas de précédence déclarée, then la compilation échoue.

#### US-006: Déclarer des profils nommés

**Description:** En tant qu'utilisateur, je veux basculer entre des jeux de réglages nommés, afin de passer d'une configuration de revue en lecture seule à une configuration d'implémentation sans réécrire quatre clés.

**Référence Codex:** `codex-rs/config/src/profile_toml.rs:24-48` (`struct ConfigProfile` : modèle, politique d'approbation, mode de sandbox, effort de raisonnement, verbosité, instructions).
**Cible Pyxis:** `crates/agent-cli/src/settings.rs:36` (`KNOWN_KEYS`), `crates/agent-cli/src/main.rs:115`.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Un profil nommé regroupe au minimum le modèle, l'effort de raisonnement, le mode de permission et la politique de sandbox.
- [ ] Given un profil sélectionné, when Pyxis démarre, then ses valeurs s'appliquent à une précédence supérieure à la configuration utilisateur nue et inférieure aux surcharges de session.
- [ ] Given un profil définissant une clé de sécurité dans un fichier global, when il est chargé, then la clé est appliquée ; given le même profil dans un fichier de projet, then elle est écartée avec avertissement.
- [ ] Given un profil inexistant, when il est demandé, then Pyxis refuse de démarrer en nommant le profil et en listant ceux qui existent.
- [ ] Given un profil dont une clé est invalide, when il est chargé, then la clé est écartée seule et le démarrage aboutit sur les autres.

#### US-007: Surcharger une clé pour un run

**Description:** En tant qu'utilisateur, je veux surcharger n'importe quelle clé non sécuritaire par un argument, afin d'essayer un réglage sans éditer de fichier.

**Référence Codex:** `codex-rs/config/src/overrides.rs:7` (`build_cli_overrides_layer`), `ConfigLayerSource::SessionFlags` de précédence 30.
**Cible Pyxis:** `crates/agent-cli/src/main.rs:115` (`parse_args_from`), `crates/agent-cli/src/settings.rs`.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given `-c cle=valeur` sur une clé connue non sécuritaire, when Pyxis démarre, then la valeur l'emporte sur toutes les couches de fichier, par précédence et non par cas particulier.
- [ ] Given `-c` sur une clé de sécurité, when Pyxis démarre, then la surcharge est refusée en nommant la clé, sans empêcher le démarrage : un argument peut provenir d'un script du dépôt et n'est donc pas plus fiable qu'un fichier de workspace.
- [ ] Given `-c` sur une clé inconnue, when Pyxis démarre, then un avertissement nomme la clé et le démarrage se poursuit, cohérent avec le traitement des fichiers.
- [ ] Given une valeur non convertible, when Pyxis démarre, then l'erreur nomme la clé, la valeur reçue et le type attendu.
- [ ] Given plusieurs `-c` sur la même clé, when Pyxis démarre, then la dernière occurrence gagne et le comportement est documenté dans l'aide.

#### US-008: Piloter sandbox et approbation depuis la ligne de commande

**Description:** En tant qu'utilisateur, je veux choisir la politique de sandbox et le mode de permission en argument, afin de lancer une session adaptée à la tâche sans modifier ma configuration globale.

**Référence Codex:** `codex-rs/protocol/src/protocol.rs:908-928` (`enum AskForApproval`), `config_types.rs:86` (`SandboxMode`).
**Cible Pyxis:** `crates/agent-cli/src/main.rs:115`, `crates/agent-tools/src/permission.rs:20`.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-001, US-007

**Acceptance Criteria:**
- [ ] Given un drapeau de mode de permission avec une valeur valide, when Pyxis démarre, then le mode actif est celui demandé et l'emporte sur la configuration globale.
- [ ] Given un drapeau de politique de sandbox, when Pyxis démarre, then la variante d'US-001 correspondante est appliquée.
- [ ] Given une valeur inconnue sur l'un des deux drapeaux, when Pyxis démarre, then il refuse de démarrer en nommant la valeur reçue et les valeurs acceptées.
- [ ] Given les drapeaux utilisés en mode headless `-p`, when le run s'exécute, then ils s'appliquent comme en interactif.
- [ ] Given `--no-sandbox` conservé, when il est utilisé, then il se comporte comme un alias documenté de l'accès complet, sans troisième sémantique.
- [ ] Given un mode plus permissif que le défaut headless, when il est appliqué, then l'élargissement est annoncé sur stderr, comme le fait déjà la configuration globale.

---

### EP-003: Suite d'outils exposée au modèle

Porter les quatre familles d'outils Codex dont l'absence change ce que le modèle fait, et non seulement ce qu'il peut faire.

**Definition of Done:** dix outils sont exposés, chacun traversant le pipeline de permissions, de taint et de hooks existant.

#### US-009: Structurer une tâche par `update_plan`

**Description:** En tant qu'utilisateur, je veux que l'agent expose un plan qu'il met à jour, afin de suivre une tâche longue sans relire tout le transcript.

**Référence Codex:** `codex-rs/core/src/tools/handlers/plan_spec.rs:7-58` (`create_update_plan_tool` ; items `{ step, status ∈ pending|in_progress|completed }`, au plus un `in_progress`).
**Cible Pyxis:** `crates/agent-tools/`, `crates/agent-core/src/event.rs:13` (`AgentEvent`), `crates/agent-tui/`.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Le schéma d'entrée reprend celui de Codex : une liste d'items portant une étape et un statut parmi trois valeurs, avec explication facultative.
- [ ] Given un plan déclarant deux étapes `in_progress`, when il est reçu, then il est rejeté avec un retour au modèle nommant la contrainte, sans casser le tour.
- [ ] Given un plan valide, when il arrive, then il est porté par une variante d'`AgentEvent` et rendu par la TUI comme par le flux JSONL.
- [ ] Given un plan mis à jour en cours de tour, when la mise à jour arrive, then l'affichage reflète le nouvel état sans dupliquer l'ancien.
- [ ] Given un client qui ignore la nouvelle variante d'événement, when il consomme le flux, then son comportement est inchangé.

#### US-010: Éditer par `apply_patch`

**Description:** En tant qu'utilisateur d'un modèle `*-codex`, je veux que l'agent édite au format sur lequel ce modèle a été entraîné, afin de réduire les échecs d'édition.

**Référence Codex:** `codex-rs/apply-patch/` (4 553 lignes, grammaire `apply_patch.lark`), `codex-rs/core/src/tools/handlers/apply_patch.rs`.
**Cible Pyxis:** `crates/agent-tools/`, en coexistence avec `crates/agent-tools/src/edit.rs`.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given un patch bien formé, when le modèle l'émet, then les modifications sont appliquées et le diff de tour les reflète.
- [ ] Given un patch dont le contexte ne correspond plus au fichier, when il est appliqué, then l'échec est rendu au modèle avec la raison, **sans modification partielle** du fichier.
- [ ] Given un patch touchant un sous-chemin protégé, when il est appliqué, then il est refusé avant toute décision de permission, comme le font déjà `write` et `edit`.
- [ ] Given l'outil `edit` existant, when `apply_patch` est ajouté, then `edit` reste disponible et le choix entre les deux est déterministe et documenté.
- [ ] Given la reprise de code Codex, when elle est faite, then `docs/codex-port-inventory.md` porte la provenance et les fichiers modifiés sont signalés.
- [ ] Un comparatif chiffré du taux d'échec d'édition entre `edit` et `apply_patch` est produit sur au moins 20 éditions réelles, et consigné : c'est la validation de l'hypothèse déclarée en assumptions.

#### US-011: Faire lire une image au modèle

**Description:** En tant qu'utilisateur, je veux que l'agent puisse lire une capture d'écran ou un diagramme du dépôt, afin de traiter les tâches où l'information n'est pas textuelle.

**Référence Codex:** `codex-rs/core/src/tools/handlers/view_image_spec.rs:15-40` (`create_view_image_tool`, paramètre `path`, niveau de détail facultatif).
**Cible Pyxis:** `crates/agent-tools/`, `crates/agent-core/src/message.rs:109` (`ContentBlock::Image`, déjà présent et jamais produit).

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un chemin vers une image du workspace, when le modèle appelle l'outil, then un bloc image entre dans le transcript et est transmis au provider.
- [ ] Given un provider dont les capabilities ne déclarent pas la vision, when l'outil est appelé, then l'appel est refusé avec une raison lisible, sans envoi.
- [ ] Given un chemin hors du périmètre de lecture, when l'outil est appelé, then il est refusé par le pipeline de permissions existant.
- [ ] Given un fichier qui n'est pas une image ou dépasse la taille maximale, when l'outil est appelé, then l'échec nomme la cause et la limite.
- [ ] Given une compaction pleine, when elle s'exécute, then les blocs image sont retirés, comportement déjà implémenté et préservé.

#### US-012: Exécuter dans une session shell persistante avec stdin

**Description:** En tant qu'utilisateur, je veux que l'agent puisse ouvrir une session shell, y écrire sur l'entrée standard et lire la sortie au fil de l'eau, afin de traiter les commandes interactives et les processus longs.

**Référence Codex:** `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` et `write_stdin.rs`.
**Cible Pyxis:** `crates/agent-tools/src/bash.rs` (one-shot, sans PTY ni stdin), `crates/agent-tools/src/shell.rs`.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-004

**Acceptance Criteria:**
- [ ] Given une session ouverte, when le modèle y écrit sur l'entrée standard, then la sortie produite lui est rendue au fil de l'eau par le canal de fragments existant.
- [ ] Given une commande qui attend une saisie, when elle s'exécute dans la session, then elle n'échoue plus par fermeture immédiate de l'entrée.
- [ ] Given une session inactive au-delà du délai configuré, when le délai expire, then la session est fermée et le processus terminé, sans processus orphelin.
- [ ] Given une session ouverte, when la session Pyxis se termine ou est annulée, then tous les processus enfants sont terminés.
- [ ] Given le confinement actif, when une session est ouverte, then elle hérite du périmètre de la politique en vigueur, US-002 comprise.
- [ ] Given l'outil `bash` one-shot existant, when la session persistante est ajoutée, then `bash` reste disponible et inchangé.

---

### EP-004: MCP au niveau du protocole

Compléter l'intégration MCP au-delà du transport stdio : transport HTTP, filtrage d'outils par serveur, approbation par outil, enregistrement en cours de session.

**Definition of Done:** un serveur MCP distant est utilisable, sa surface d'outils est réductible par configuration, et une connexion en cours de session change ce que le modèle peut appeler.

#### US-013: Se connecter à un serveur MCP par HTTP

**Description:** En tant qu'utilisateur, je veux connecter un serveur MCP distant, afin d'utiliser les serveurs qui ne s'exécutent pas en sous-processus local.

**Référence Codex:** `codex-rs/rmcp-client/src/http_client_adapter.rs:38-85` (`StreamableHttpClient`, SSE).
**Cible Pyxis:** `crates/agent-mcp/src/client.rs:50,57`, `crates/agent-mcp/src/config.rs`, `crates/agent-mcp/src/lib.rs:8` (report documenté à lever).

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une déclaration de serveur en transport HTTP, when Pyxis démarre, then la connexion s'établit et les outils sont listés comme pour un serveur stdio.
- [ ] Given un serveur HTTP injoignable ou lent, when la connexion est tentée, then l'échec est borné par un délai, nommé, et n'empêche pas le démarrage de la session.
- [ ] Given un jeton porteur déclaré par variable d'environnement, when la connexion s'établit, then le jeton n'apparaît jamais dans les journaux ni dans le transcript.
- [ ] Given un serveur HTTP déclaré par le workspace, when il est chargé, then il reste soumis au gate de confiance existant.
- [ ] Given un résultat d'outil venant d'un serveur HTTP, when il revient, then il est untrusted par construction, comme pour stdio.
- [ ] Given une URL en clair non chiffrée, when elle est déclarée, then elle est refusée ou exige une confirmation explicite, décision documentée.

#### US-014: Réduire la surface d'outils exposée par un serveur

**Description:** En tant qu'utilisateur, je veux n'exposer au modèle qu'une partie des outils d'un serveur MCP, afin de réduire le bruit et la surface de risque sans désactiver le serveur.

**Référence Codex:** `codex-rs/config/src/mcp_types.rs` (`enabled_tools` en allow-list puis `disabled_tools` en deny-list, appliquées dans cet ordre).
**Cible Pyxis:** `crates/agent-mcp/src/config.rs`, `crates/agent-mcp/src/tool.rs:195` (`dyn_tools`).

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-013

**Acceptance Criteria:**
- [ ] Given une allow-list déclarée, when les outils sont enregistrés, then seuls les outils nommés entrent dans le registre.
- [ ] Given une allow-list et une deny-list, when les deux sont déclarées, then la deny-list s'applique après l'allow-list, ordre repris de Codex et documenté.
- [ ] Given un nom d'outil listé qui n'existe pas sur le serveur, when les outils sont enregistrés, then un avertissement le nomme sans faire échouer la connexion.
- [ ] Given un filtrage déclaré par le workspace, when il restreint, then il est appliqué ; when il élargit, then il est écarté avec avertissement.
- [ ] Given un filtrage qui vide entièrement un serveur, when il est appliqué, then le serveur reste connecté et l'état est visible dans `/mcp`.

#### US-015: Décider l'approbation par serveur et par outil

**Description:** En tant qu'utilisateur, je veux régler le niveau d'approbation d'un outil MCP précis, afin de ne pas confirmer chaque lecture d'un serveur en qui j'ai confiance.

**Référence Codex:** `codex-rs/config/src/mcp_types.rs` (`default_tools_approval_mode` par serveur, table `tools` par outil).
**Cible Pyxis:** `crates/agent-mcp/src/config.rs`, `crates/agent-tools/src/permission.rs:112` (`resolve_permission`).

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] Given un mode d'approbation déclaré par serveur, when un outil de ce serveur est appelé, then le mode s'applique en l'absence de réglage par outil.
- [ ] Given un réglage par outil, when il existe, then il l'emporte sur le mode du serveur.
- [ ] Given un taint untrusted récent sur le tour, when un outil MCP auto-approuvé est appelé, then la confirmation est forcée : aucun réglage MCP ne peut affaiblir la défense taint.
- [ ] Given un réglage d'approbation déclaré par le workspace, when il élargit, then il est écarté avec avertissement.
- [ ] Given un serveur sans réglage, when un de ses outils est appelé, then le comportement actuel est préservé : la confirmation est demandée.

#### US-016: Enregistrer les outils d'un serveur connecté en session

**Description:** En tant qu'utilisateur, je veux qu'un serveur connecté en cours de session rende ses outils appelables, afin que `/mcp connect` change ce que l'agent sait faire et pas seulement un état affiché.

**Référence Codex:** enregistrement dynamique dans `codex-rs/core/src/tools/` (`tool_discovery.rs`, `dynamic_tool.rs`).
**Cible Pyxis:** `crates/agent-cli/src/main.rs:1020` (`register_dyn`, appelé au démarrage uniquement), `crates/agent-tools/src/registry.rs:596`.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-013

**Acceptance Criteria:**
- [ ] Given un serveur connecté en cours de session, when la connexion aboutit, then ses outils entrent dans le registre et le modèle peut les appeler au tour suivant.
- [ ] Given un serveur déconnecté en cours de session, when la déconnexion aboutit, then ses outils quittent le registre et un appel ultérieur échoue proprement.
- [ ] Given une collision de nom avec un outil déjà enregistré, when l'enregistrement se produit, then la règle de raccourcissement déterministe existante s'applique sans renommer un outil déjà exposé au modèle.
- [ ] Given un enregistrement pendant qu'un tour est en cours, when il se produit, then le registre vu par le tour en cours ne change pas en cours de route.
- [ ] Given un serveur non approuvé, when une connexion est tentée en session, then le gate de confiance existant s'applique avant tout spawn.

---

### EP-005: Extensibilité utilisateur

Porter le jeu d'événements de hooks de Codex et le modèle de skills à portées, en préservant les divergences fail-closed de Pyxis.

**Definition of Done:** les événements de cycle de vie sont couverts, les skills ont une portée et une politique d'invocation, et aucun mécanisme d'extension ne peut élargir un périmètre.

#### US-017: Couvrir les événements de cycle de vie par des hooks

**Description:** En tant qu'utilisateur, je veux déclencher des commandes au démarrage de session, à la soumission d'un prompt, avant compaction et en fin de tour, afin d'automatiser autour de l'agent et pas seulement autour d'un appel d'outil.

**Référence Codex:** `codex-rs/hooks/src/lib.rs:20-32` (`HOOK_EVENT_NAMES`, 11 événements) et `:35-46` (les 9 qui portent un matcher).
**Cible Pyxis:** `crates/agent-tools/src/hooks.rs:49-50` (2 événements), `crates/agent-cli/src/settings.rs`.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Au minimum `SessionStart`, `SessionEnd`, `UserPromptSubmit` et `Stop` s'ajoutent aux deux événements existants, avec la même charge JSON sur l'entrée standard.
- [ ] Given un hook `UserPromptSubmit` qui refuse, when l'utilisateur soumet, then le tour ne démarre pas et la raison lui est restituée.
- [ ] Given un hook qui échoue, expire ou renvoie une décision inconnue, when il s'exécute, then la décision est un refus : la règle fail-closed existante s'applique à tous les nouveaux événements.
- [ ] Given un hook déclarant `allow`, when il s'exécute, then cela se lit « pas d'objection » et ne contourne jamais une confirmation ni la défense taint : la divergence délibérée avec Codex est préservée.
- [ ] Given un hook de cycle de vie déclaré par le workspace, when il est chargé, then il est écarté : `hooks` reste une clé de sécurité globale.
- [ ] Given aucun hook déclaré, when la session s'exécute, then aucun processus n'est lancé et aucun coût n'est ajouté.

#### US-018: Donner une portée et une politique aux skills

**Description:** En tant qu'utilisateur, je veux des skills de projet en plus des skills utilisateur, et pouvoir empêcher qu'une skill soit invoquée implicitement, afin de contrôler ce que le modèle voit.

**Référence Codex:** `codex-rs/skills/src/model.rs:8-20` (`SkillMetadata { scope, policy, interface, dependencies }`), `:63-68` (`SkillPolicy { allow_implicit_invocation, products }`), `codex-rs/core-skills/src/root_loader.rs`.
**Cible Pyxis:** `crates/agent-cli/src/skills.rs:76` (`load`, racine unique), `crates/agent-cli/src/main.rs:818` (`~/.agents/skills` codé en dur), `skills.rs:241` (`catalog_block`).

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Une skill porte une portée, au minimum utilisateur et projet, et la portée la plus proche l'emporte à nom égal.
- [ ] Given une skill dont la politique interdit l'invocation implicite, when le catalogue est injecté, then elle n'y figure pas mais reste invocable explicitement.
- [ ] Given une skill de projet, when elle est chargée, then elle ne peut déclarer aucune capacité élargissant un périmètre, et son contenu reste du texte injecté.
- [ ] Given deux skills de même nom dans deux portées, when le catalogue est construit, then la résolution est déterministe et documentée.
- [ ] Given une skill invalide dans une portée, when elle est chargée, then elle est écartée seule avec une trace, les autres restant actives.
- [ ] Given le budget d'octets du catalogue, when plusieurs portées sont présentes, then il reste respecté et la troncature est déterministe.

---

### EP-006: Surface de commandes et modes non interactifs

Exposer les capacités portées et compléter le mode headless pour qu'il soit intégrable dans un pipeline.

**Definition of Done:** les commandes qui manquent le plus sont disponibles, et `-p` accepte une entrée standard et sait ne rien laisser derrière lui.

#### US-019: Ajouter les commandes de session manquantes

**Description:** En tant qu'utilisateur, je veux amorcer un dépôt, dupliquer une session et copier la dernière réponse, afin de ne pas quitter l'agent pour des gestes courants.

**Référence Codex:** `codex-rs/tui/src/slash_command.rs` (55 variantes ; `Init`, `Fork`, `Copy`, `Logout`, `Hooks`).
**Cible Pyxis:** `crates/agent-tui/src/state.rs:60-85` (`COMMANDS`, 17 entrées), `crates/agent-cli/src/interactive.rs` (dispatch), `crates/agent-session/src/lib.rs` pour la duplication.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given un dépôt sans fichier d'instructions, when l'utilisateur exécute la commande d'amorçage, then un `AGENTS.md` est écrit à partir d'une inspection réelle du dépôt et pris en compte au tour suivant sans redémarrage.
- [ ] Given un fichier d'instructions existant, when l'amorçage est demandé, then il n'est jamais écrasé sans confirmation explicite.
- [ ] Given une session en cours, when l'utilisateur la duplique, then une nouvelle session part de l'état courant et l'originale reste intacte sur disque.
- [ ] Given une réponse affichée, when l'utilisateur demande la copie, then le texte brut est mis dans le presse-papiers, et l'échec de l'accès au presse-papiers est nommé plutôt que silencieux.
- [ ] Given la déconnexion demandée, when elle s'exécute, then le crédential local est invalidé et l'absence de révocation côté serveur est explicitement signalée à l'utilisateur.
- [ ] Given les hooks déclarés, when l'utilisateur demande leur inspection, then leur liste, leur événement et leur matcher s'affichent.

#### US-020: Rendre le mode headless intégrable dans un pipeline

**Description:** En tant qu'utilisateur en script, je veux fournir le prompt sur l'entrée standard, récupérer le seul message final et ne laisser aucun fichier de session, afin d'intégrer Pyxis dans une chaîne d'outils.

**Référence Codex:** `codex-rs/exec/src/cli.rs:74` (`--output-last-message`), `:31` (`--ephemeral`), `:53` (`--output-schema`), `:35` (`--ignore-user-config`).
**Cible Pyxis:** `crates/agent-cli/src/main.rs:115`, `crates/agent-cli/src/jsonl.rs`, `crates/agent-cli/src/session.rs`.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007

**Acceptance Criteria:**
- [ ] Given un prompt fourni sur l'entrée standard sans argument, when Pyxis s'exécute en mode `-p`, then il le consomme comme prompt.
- [ ] Given un prompt fourni à la fois en argument et sur l'entrée standard, when Pyxis démarre, then la résolution est déterministe et documentée dans l'aide.
- [ ] Given le drapeau de message final, when le run se termine, then seul le dernier message assistant est écrit à la destination indiquée, sans balisage.
- [ ] Given le drapeau éphémère, when le run se termine, then aucun fichier de session n'a été écrit dans le workspace, vérifié par un test.
- [ ] Given une entrée standard vide et aucun argument, when Pyxis démarre, then il sort en erreur nommée plutôt qu'en attente indéfinie.
- [ ] Given le drapeau éphémère et une reprise demandée, when les deux sont passés, then le conflit est refusé en nommant les deux drapeaux.

## Functional Requirements

- FR-01: La politique de sandbox doit être un type à variantes porteuses de données, pas un booléen.
- FR-02: Une racine writable doit pouvoir déclarer des sous-chemins en lecture seule, évalués avant l'exécution et applicables à toute commande shell.
- FR-03: L'accès réseau doit être une donnée portée par la politique de sandbox.
- FR-04: L'autorisation réseau d'un domaine doit couvrir ses sous-domaines sur frontière d'étiquette, et l'autorisation d'un sous-domaine ne doit pas autoriser son parent.
- FR-05: Un échec d'exécution imputable au confinement doit être distingué d'un échec applicatif et permettre une escalade explicite bornée à l'appel.
- FR-06: Chaque couche de configuration doit être nommée et porter une précédence explicite, et la couche d'origine d'une valeur effective doit être inspectable.
- FR-07: Le système doit supporter des profils nommés regroupant au minimum modèle, effort, mode de permission et politique de sandbox.
- FR-08: Le système doit accepter des surcharges de configuration par argument, et refuser celles qui portent sur une clé de sécurité.
- FR-09: Le système doit exposer au modèle un outil de plan, un outil d'application de patch, un outil de lecture d'image et une exécution shell persistante avec entrée standard.
- FR-10: Tout outil ajouté, y compris MCP, doit traverser le pipeline de permissions, de taint et de hooks existant.
- FR-11: Le système doit supporter un transport MCP HTTP en plus de stdio.
- FR-12: Le système doit permettre de restreindre les outils exposés par un serveur MCP, allow-list puis deny-list dans cet ordre.
- FR-13: Un serveur MCP connecté en cours de session doit rendre ses outils appelables au tour suivant.
- FR-14: Le système doit déclencher des hooks sur les événements de cycle de vie de session, de prompt et de fin de tour, en plus des deux événements d'outil.
- FR-15: Un hook ne doit jamais pouvoir élargir un périmètre ni contourner la défense taint, quel que soit l'événement.
- FR-16: Les skills doivent porter une portée et une politique d'invocation implicite.
- FR-17: Le mode headless doit accepter un prompt sur l'entrée standard, savoir n'émettre que le message final, et savoir ne rien écrire dans le workspace.
- FR-18: Aucune configuration contrôlée par le workspace ne doit pouvoir élargir un périmètre, quel que soit le mécanisme introduit par ce PRD.
- FR-19: Toute reprise de code Codex doit être inscrite dans `docs/codex-port-inventory.md` avec sa provenance et ses modifications signalées.

## Non-Functional Requirements

- **Performance:** surcoût de l'évaluation des sous-chemins protégés sous 1 ms par appel d'outil. Démarrage à froid sous 150 ms, inchangé par les couches de configuration ajoutées. Connexion MCP HTTP bornée par un délai de 10 secondes maximum.
- **Security:** aucune clé élargissant un périmètre depuis le workspace ni depuis un argument. Correspondance d'hôte sur frontière d'étiquette de domaine, avec test négatif dédié couvrant `evil-github.com` face à `github.com`. Le taint untrusted ne peut être affaibli par aucune politique, aucun réglage MCP et aucun hook. Aucun jeton porteur MCP dans les journaux ni le transcript. Chaque nouvelle branche de décision de sécurité porte au moins un test négatif.
- **Compatibility:** aucune régression des 835 tests et 37 snapshots existants. Toute nouvelle variante d'`AgentEvent` est ignorable par un client existant sans changement de comportement. `bash` one-shot, `edit` et le mode d'écriture sur workspace conservent leur comportement actuel.
- **Reliability:** aucun processus enfant orphelin après fermeture d'une session shell persistante ou annulation d'un tour. Un échec de connexion MCP n'empêche jamais le démarrage de la session.
- **Licensing:** 100 % des reprises de code Codex inscrites dans `docs/codex-port-inventory.md`, licence et mentions conservées, fichiers modifiés signalés.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Landlock indisponible | Noyau sans support, ou plateforme non Linux | Dégradation annoncée, politique réellement appliquée nommée | "Confinement filesystem indisponible. Politique appliquée : accès complet." |
| 2 | Cible d'écriture non déterminable | Commande shell dont le chemin d'écriture est calculé | Fail-closed, refus documenté | "Écriture refusée : cible non déterminable sous un sous-chemin protégé." |
| 3 | Écriture protégée via lien symbolique | Lien pointant dans `.git/` | Refus après résolution du lien | "Écriture refusée : la cible résout sous `.git/`." |
| 4 | Faux positif de sous-chaîne réseau | `--allow github.com` puis appel vers `evil-github.com` | Connexion refusée | "Hôte bloqué : evil-github.com. Autorisés : github.com et ses sous-domaines." |
| 5 | Conflit politique réseau fermée et allow-list | Politique lecture seule sans réseau, allow-list fournie | Résolution déterministe et annoncée | "Accès réseau fermé par la politique ; allow-list ignorée." |
| 6 | Profil inexistant | Profil nommé absent de la configuration | Refus de démarrer, profils existants listés | "Profil inconnu : `x`. Disponibles : a, b." |
| 7 | Surcharge d'une clé de sécurité | `-c permission_mode=bypass` | Surcharge refusée, démarrage poursuivi | "`permission_mode` ne peut pas être surchargé en argument (clé de sécurité)." |
| 8 | Plan à deux étapes en cours | `update_plan` avec deux `in_progress` | Rejet rendu au modèle, tour préservé | — |
| 9 | Patch au contexte périmé | Fichier modifié entre la lecture et l'application | Aucune modification partielle | "Patch non appliqué : le contexte ne correspond plus à {fichier}." |
| 10 | Vision non déclarée par le provider | `view_image` sur un provider sans vision | Refus avant envoi | "Le modèle actif ne lit pas les images." |
| 11 | Session shell inactive | Délai d'inactivité dépassé | Session fermée, processus terminé | "Session shell fermée après {n}s d'inactivité." |
| 12 | Annulation avec session ouverte | Interruption pendant un processus long | Tous les enfants terminés, aucun orphelin | — |
| 13 | Serveur MCP HTTP injoignable | Hôte distant indisponible | Échec borné, session démarre quand même | "Serveur MCP {nom} injoignable : {raison}. Session démarrée sans ses outils." |
| 14 | Filtrage MCP vidant un serveur | Deny-list couvrant tous les outils | Serveur connecté, état visible | "{nom} : 0 outil exposé après filtrage." |
| 15 | Hook de cycle de vie en échec | Expiration ou sortie non nulle | Refus fail-closed, raison portée au modèle | "Hook {nom} sur {événement} : refus ({raison})." |
| 16 | Collision de noms de skills | Même nom en portée projet et utilisateur | Résolution déterministe, la plus proche gagne | "Skill `x` : la version projet masque la version utilisateur." |
| 17 | Entrée standard vide en headless | `-p` sans argument ni entrée | Erreur nommée, pas d'attente indéfinie | "Aucun prompt fourni (argument ou entrée standard)." |
| 18 | Éphémère et reprise ensemble | Deux drapeaux contradictoires | Refus nommant les deux | "`--ephemeral` et `--resume` sont incompatibles." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | La protection des sous-chemins au niveau politique laisse un contournement par une commande dont la cible n'est pas analysable statiquement | High | High | Comportement fail-closed imposé par critère d'US-002, et test négatif dédié. La limite résiduelle est documentée plutôt que masquée, comme l'est déjà la limite actuelle |
| 2 | Le portage de `SandboxPolicy` casse le comportement de confinement existant | Medium | High | US-001 exige que la variante d'écriture sur workspace soit prouvée identique par les tests de sandbox existants inchangés |
| 3 | La session shell persistante laisse des processus orphelins | Medium | High | Deux critères dédiés dans US-012 (inactivité, annulation) et une NFR de fiabilité explicite |
| 4 | `apply_patch` n'améliore rien de mesurable et ajoute 4 500 lignes | Medium | Medium | US-010 impose un comparatif chiffré sur au moins 20 éditions réelles ; l'hypothèse est déclarée comme telle et son invalidation autorise l'abandon |
| 5 | L'ajout des couches de configuration rend la résolution opaque | Medium | Medium | US-005 impose l'inspection par `/status` de la couche d'origine, et l'échec de compilation d'une couche sans précédence déclarée |
| 6 | Une reprise de code Codex crée une obligation de licence non tracée | Low | High | FR-19, critère dédié sur US-010, et protocole existant de `NOTICE-CODEX.md` |
| 7 | Le canal d'abonnement ChatGPT ferme pendant le portage | Medium | Critical | Hors périmètre. Le trait `Provider` générique reste intact, ce qui est précisément l'actif que le refus du fork préserve |

## Non-Goals

- **Forker Codex CLI.** Décision actée : `WireApi` à un seul variant, aucune tranche extractible (la TUI est un client JSON-RPC d'app-server, `codex-rs/tui/src/app_server_session.rs:17-28`), ~90 des 107 crates dans le graphe du binaire, 707 commits amont sur 30 jours. Voir `docs/strategie-parite-codex-2026-07-27.md`.
- **Vendoriser `codex-execpolicy`.** Il dépend de `starlark`, un interpréteur de langage complet. Seul son modèle `Decision` à trois états est repris, en TOML.
- **Porter la TUI de Codex** (194 354 lignes hors tests, 613 snapshots). L'esthétique de Pyxis est une décision produit distincte (ADR-2).
- **Atteindre 55 commandes.** US-019 en ajoute cinq, choisies pour ce qu'elles débloquent. Le reste attend une demande réelle.
- **Tout ce que Codex est en plus d'un agent interactif local** : app-server, tâches cloud, sous-agents, plugins, mémoires, review mode, OTLP, sandbox Windows, analytics. Environ 40 des 82 écarts, hors périmètre.
- **Ajouter des adaptateurs de providers.** Le trait `Provider` est l'actif à préserver dans ce cycle, pas à étendre.
- **Reporté à une phase 2** (repérage fait, non planifié ici) : politique d'environnement shell (`codex-rs/config/src/shell_environment_policy.rs:15`), hook de notification de fin de tour (`codex-rs/hooks/src/legacy_notify.rs:43`), recherche de fichiers classée pour les mentions (`codex-rs/file-search/`, avec `nucleo` depuis crates.io), ressources et élicitation MCP, OAuth par serveur MCP, registry de contributeurs (`ext/extension-api/src/contributors.rs`), `--output-schema`.

## Files NOT to Modify

- `docs/codex-harness-parity-audit.md` — constat fondateur du 2026-07-24, laissé intact pour rester la baseline de comparaison.
- `docs/codex-harness-parity-audit-2026-07-25.md` — passe de mesure intermédiaire, même raison.
- `tasks/prd-pyxis-status.json` — déjà listé en « Files NOT to Modify » par `tasks/prd-harness-parity.md` ; son anomalie de statut est documentée dans `docs/CURRENT_STATUS.md`.
- `LICENSE`, `NOTICE-CODEX.md` — le second s'enrichit par ajout de provenance uniquement, jamais par réécriture des obligations.
- `/home/arthur/dev/codex/**` — dépôt de référence, lecture seule.

## Technical Considerations

- **Application des variantes de sandbox (US-001):** les règles Landlock sont additives, donc la variante lecture seule ne s'obtient pas en retirant un droit d'un périmètre déjà accordé ; elle doit être posée comme un jeu de règles distinct au moment du confinement. C'est la même contrainte qui empêche aujourd'hui de soustraire `.git/`, et elle explique pourquoi US-002 travaille au niveau politique.
- **Niveau d'application des sous-chemins protégés (US-002):** Codex évalue `is_path_writable` avant l'exécution parce que le noyau ne sait pas soustraire. Pyxis doit faire porter la même évaluation à `bash`, ce qui suppose une analyse de la commande. La question ouverte est la profondeur de cette analyse : la classification par tokens existante (`crates/agent-tools/src/command.rs:59`) donne déjà l'argv, mais une redirection ou une substitution rend la cible non déterminable. Recommandation : fail-closed sur commande opaque, documenté.
- **Emplacement du type de politique (US-001):** `agent-sandbox` ou `agent-core`. `agent-core` est tentant pour que la politique soit visible du modèle dans le contexte d'environnement, mais l'invariant ADR-3 interdit toute I/O. Recommandation : le type dans `agent-core` (pure donnée), l'application dans `agent-sandbox`.
- **Format des règles d'exécution:** TOML, déjà dans le graphe. Correspondance par séquence de tokens de préfixe, jamais par sous-chaîne, cohérente avec `ApprovalKey` (`crates/agent-tools/src/permission.rs:189`) et avec le vecteur documenté de mémorisation par chaîne.
- **Session shell persistante (US-012):** Codex sépare l'ouverture (`exec_command.rs`) de l'écriture (`write_stdin.rs`), ce qui donne deux outils au modèle plutôt qu'un. Question ouverte : reprendre cette séparation, ou exposer un seul outil à mode. Recommandation : reprendre la séparation, elle rend chaque appel idempotent à décrire.
- **PTY ou tube:** un PTY change le comportement des programmes qui détectent un terminal. Trade-off : fidélité contre une dépendance supplémentaire. À trancher pendant US-012 selon que les commandes visées exigent un terminal.
- **Enregistrement dynamique d'outils (US-016):** `crates/agent-tools/src/registry.rs` est construit par un builder consommé au démarrage. Rendre le registre mutable en session touche un invariant de conception. Recommandation : registre versionné, chaque tour capturant une vue immuable, ce qui satisfait aussi le critère de stabilité en cours de tour.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Variantes de politique de sandbox exprimables | 1 (booléen) | 3 minimum, chacune testée | Fin EP-001 | Tests de `agent-sandbox` |
| Sous-chemins protégés contre `bash` | 0 (trou documenté) | `.git/` et `.pyxis/` refusés avant exécution | Fin EP-001 | Test négatif dédié |
| Outils exposés au modèle | 6 | 10 | Fin EP-003 | Registre au démarrage |
| Événements de hooks supportés | 2 sur 11 | 6 sur 11 | Fin EP-005 | Tests de `hooks.rs` |
| Transports MCP | 1 (stdio) | 2 (stdio, HTTP) | Fin EP-004 | Test d'intégration MCP |
| Couches de configuration nommées avec précédence | 0 | Toutes | Fin EP-002 | `/status` affiche la couche d'origine |
| Écarts d'audit fermés sur les 42 du harness interactif local | 0 | 20 | Fin EP-006 | Passe d'audit de contrôle |

## Open Questions

- Jusqu'où pousser l'analyse de commande d'US-002 avant de déclarer une cible non déterminable ? À trancher pendant US-002 ; la réponse fixe la taille réelle du trou résiduel et doit être documentée dans `docs/CURRENT_STATUS.md` comme l'est la limite actuelle.
- US-012 doit-elle utiliser un PTY ou des tubes ? À trancher pendant US-012 ; conditionne une dépendance supplémentaire et le comportement des programmes qui détectent un terminal.
- Le type de politique de sandbox vit-il dans `agent-core` ou `agent-sandbox` ? À trancher au démarrage d'US-001 ; conditionne la possibilité d'annoncer la politique au modèle dans le contexte d'environnement.
- Faut-il conserver `edit` après `apply_patch` ? Répondu par le comparatif chiffré d'US-010, pas avant. Le retrait d'`edit` serait une rupture de comportement et sort du périmètre de ce PRD.
[/PRD]
