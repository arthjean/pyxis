[PRD]
# PRD: Pyxis : Parité harness Codex CLI

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-24 | Arthur Jean | Initial draft, dérivé de l'audit `docs/codex-harness-parity-audit.md` (221 écarts retenus après réfutation adversariale). |

## Problem Statement

L'audit de parité harness a comparé Pyxis à Codex CLI sur douze dimensions et retenu 221 écarts après réfutation. Le résultat marquant n'est pas le volume brut : c'est que trois problèmes d'une autre nature que la parité dominent le classement.

1. **Une interruption pendant l'exécution d'un outil corrompt la session de façon irréversible.** `crates/agent-core/src/agent.rs:746` persiste le message assistant avec ses `tool_use` avant le dispatch. `crates/agent-cli/src/interactive.rs:1076` interrompt par un `JoinHandle::abort()` brutal, à un point arbitraire du future. Le snapshot capturé par `crates/agent-cli/src/session.rs:36` est relu tel quel au tour suivant (`interactive.rs:265`) et `crates/agent-provider/src/chatgpt_request.rs:186-200` réémet le `function_call` sans son `function_call_output`. La recherche confirme que les API de messages rejettent en 400 tout historique portant un appel d'outil sans résultat correspondant. Concrètement : un Échap pendant un `cargo build` rend la session inutilisable jusqu'à un `/new`, sans message compréhensible. Le cœur ne sait même pas qu'il a été interrompu : `AgentEvent::Interrupted` est fabriqué par le client.

2. **Le composer ne permet pas d'écrire plus d'une ligne.** `AppState.input` est un `String` plat (`crates/agent-tui/src/state.rs:558`), `Enter` soumet systématiquement (`state.rs:1717`), aucun binding n'insère de saut de ligne, et `render_input` dessine sur une zone de hauteur 1 sans retour à la ligne ni défilement horizontal (`crates/agent-tui/src/render.rs:1394-1413`). Au-delà de la largeur du terminal, la saisie devient aveugle : le texte disparaît et le curseur se fige en bord de zone. Un collage de 200 lignes de log est inséré brut dans ce champ, invisible et non éditable.

3. **Le système de suivi du projet déclare fait ce qui ne l'est pas.** `tasks/prd-codex-tui-parity-status.json` marque `US-017 Port du composer Codex` et `US-018 Boucle app, draw pipeline et parité snapshot` en `DONE`. Or le répertoire `crates/agent-tui/src/bottom_pane/` ne contient qu'un `tests.rs`, `insta` n'est pas une dépendance du workspace, et le dépôt compte zéro snapshot alors que le critère d'acceptation exigeait « au moins 20 snapshots » (`tasks/prd-codex-tui-parity.md:388`). Il n'existe par ailleurs aucun répertoire `.github/` : les 553 tests du dépôt ne sont exécutés par aucune automatisation.

4. **Les capacités devenues table-stakes en 2026 sont absentes ou inertes.** Les outils MCP sont listés mais jamais exposés au modèle (`docs/CURRENT_STATUS.md:19`), les skills sont détectées mais leur `SKILL.md` n'est jamais lu, aucun moteur de hooks n'existe, et la configuration se réduit à trois clés lues par un parseur maison (`crates/agent-cli/src/settings.rs:93`, fonction `parse_tomlish_string`) alors que le fichier s'appelle `settings.toml`.

5. **Deux trous de confinement concrets sont ouverts.** Sous sandbox, `/tmp` et `$TMPDIR` ne sont pas accessibles en écriture, ce qui casse tout outillage passant par `mktemp` sans autre recours que `--no-sandbox`, qui supprime le confinement entier. Et `.git/hooks` reste accessible en écriture dans le workspace : un agent détourné par injection indirecte peut y déposer du code que le prochain `git commit` de l'utilisateur exécutera hors sandbox et hors proxy.

**Why now:** l'audit vient de fournir les preuves `chemin:ligne` des deux côtés, ce qui rend le travail cadrable sans exploration supplémentaire. Le bug d'intégrité de session frappe le dogfood quotidien, donc chaque jour d'attente coûte des sessions perdues. Et la recherche 2026 montre que la sécurité des agents de code est passée sous pression réglementaire et médiatique après une série d'évasions de sandbox documentées (Cursor, Codex, Gemini CLI, Antigravity), dont deux CVE portant exactement sur le vecteur des hooks git et des configurations contrôlées par le workspace.

## Overview

Ce PRD remet le harness Pyxis au niveau attendu d'un agent de code terminal en 2026, en traitant d'abord ce qui casse et ce qui empêche de vérifier, avant ce qui manque.

La solution est ordonnée en trois releases. **R1 rétablit l'intégrité et la vérifiabilité** : un signal d'annulation coopératif dans `agent-core` qui remplace l'`abort()` brutal, des résultats d'outils synthétiques écrits pour chaque appel en vol avant persistance, un garde-fou défensif à la construction de requête qui refuse d'émettre un appel orphelin, puis la CI, le harness de snapshot et la résorption de la divergence entre les fichiers de statut et le code. **R2 traite l'ergonomie et la fidélité d'exécution** : composer multi-ligne réel, racines writables configurables, sous-chemins protégés, cohérence entre le shell annoncé au modèle et le shell réellement exécuté, sortie shell streamée. **R3 livre les contrats machine et l'extensibilité** : `config.toml` déclaratif avec un vrai parseur, sortie JSONL d'événements en headless, tracker de diff agrégé du tour, outils MCP appelables par le modèle, skills conformes à la spec ouverte agentskills, moteur de hooks avec droit de veto.

Trois décisions structurantes méritent d'être explicites. Le signal d'annulation passe par un `tokio::sync::watch` plutôt qu'un `CancellationToken` de `tokio-util`, pour ne pas ajouter de dépendance et parce que `Deps` n'accepte que des primitives synchrones (invariant ADR-3). Les sous-chemins protégés sont traités en userland et non par Landlock, parce que Landlock est additif : on ne peut pas soustraire un droit déjà accordé sous une racine writable, et prétendre le contraire donnerait une fausse assurance. Enfin, la configuration de projet a une surface volontairement inoffensive : elle ne peut ni définir de hooks, ni élargir les racines writables, ni changer le mode de permission, décision directement informée par CVE-2026-48124 où une configuration de hooks contrôlée par le workspace donnait une exécution non sandboxée.

Le cœur reste headless. Aucune des additions ne fait entrer d'ANSI, de terminal ou de dépendance d'entrée-sortie concrète dans `agent-core` : les nouvelles capacités passent par des variantes ajoutées à `AgentEvent` et par des traits injectés dans `Deps`, ce qui bénéficie identiquement à la TUI et au mode headless.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Intégrité de session sous interruption | 0 session corrompue sur 50 interruptions pendant un dispatch d'outil | 0 régression, y compris à la reprise d'une session interrompue |
| Signal de vérification automatisé | CI verte exécutant 553 tests, 20 snapshots de rendu minimum | Aucune story marquée DONE sans critère mécaniquement vérifié |
| Saisie multi-ligne | Prompt de 10 lignes rédigeable, éditable et visible intégralement | Collage de 500 lignes tenu sans dégradation de frame au-delà de 16 ms P95 |
| Confinement effectif | `$TMPDIR` writable sous sandbox, écriture dans `.git/hooks` refusée par les outils d'édition | 0 chemin d'exécution hors sandbox atteignable par les outils d'édition |
| Extensibilité utilisateur | 100 % des outils MCP listés sont appelables par le modèle | Skills et hooks tiers fonctionnels sans code Pyxis spécifique |

## Target Users

### Arthur Jean, créateur et dogfooder principal

- **Role:** Solo indie maker, mainteneur de Pyxis, utilisateur quotidien de Codex CLI et de Claude Code.
- **Behaviors:** Sessions longues d'audit et de refactor, interruptions fréquentes quand l'agent part dans une mauvaise direction, prompts longs et structurés en plusieurs paragraphes.
- **Pain points:** Une interruption casse la session en cours, sans message compréhensible. Un prompt de plus d'une ligne est impossible à rédiger. Les fichiers de statut du projet ne permettent plus de savoir ce qui est réellement livré.
- **Current workaround:** Rédiger les prompts longs dans un éditeur externe puis les coller en une ligne, éviter d'interrompre l'agent, relancer `/new` après chaque interruption ratée, et basculer sur Codex CLI quand la session Pyxis est cassée.
- **Success looks like:** Interrompre est sans conséquence, un prompt long se rédige dans le composer, et l'état d'avancement du projet est déductible d'une commande plutôt que d'un fichier JSON déclaratif.

### Développeur Rust early adopter du dépôt

- **Role:** Contributeur ou évaluateur qui clone le dépôt et juge le projet sur une session réelle.
- **Behaviors:** Lance les tests, lit les PRD pour comprendre l'état d'avancement, essaie les capacités annoncées dans le README.
- **Pain points:** Aucune CI ne dit si une contribution casse quelque chose. Le README annonce la configuration MCP alors que les outils MCP ne sont pas appelables. Les statuts `DONE` ne sont pas fiables.
- **Current workaround:** Lire le code source pour vérifier chaque affirmation de la documentation.
- **Success looks like:** Une CI verte fait foi, et ce que la documentation annonce se vérifie en une commande.

### Futur client embarquant `agent-core`

- **Role:** Client riche qui embarquera `agent-core` en process, sans IPC.
- **Behaviors:** Consomme le flux d'`AgentEvent` pour rendre diffs, arbres de plan et review par hunk.
- **Pain points:** Le diff d'un tour n'existe nulle part comme donnée : chaque édition est un événement isolé et les modifications faites par une commande shell sont invisibles. L'annulation n'est pas modélisée dans le cœur, donc chaque client doit réinventer sa propre logique d'interruption.
- **Current workaround:** Aucun, l'intégration n'a pas commencé.
- **Success looks like:** Le flux d'événements suffit à rendre l'état complet d'un tour, y compris son diff agrégé et son annulation, sans dupliquer de logique d'agent.

## Research Findings

Constats issus de la recherche qui ont façonné ce PRD.

### Competitive Context

- **Codex CLI 0.145 :** couvre l'intégralité du périmètre de ce PRD. `codex exec --json` produit des événements JSONL, `--output-schema` contraint la sortie par JSON Schema, la sandbox offre trois politiques via Landlock et seccomp sur Linux, MCP supporte stdio et HTTP avec OAuth, les hooks et les skills sont livrés, la configuration est un `config.toml` à profils.
- **Claude Code :** mode headless avec `--output-format stream-json` produisant un événement JSON par ligne, incluant `session_id` et coût total dans l'événement final, hooks capables d'approuver, de refuser ou de modifier un appel d'outil, skills au format `SKILL.md`.
- **Market gap :** aucun schéma d'événements JSONL n'est standardisé entre agents, chaque CLI ayant son propre vocabulaire. Et aucun des concurrents consultés n'expose le diff agrégé d'un tour comme événement machine de première classe. Ces deux points sont les seuls du périmètre où Pyxis peut être en avance plutôt qu'en rattrapage.

### Best Practices Applied

- **Réconciliation des appels d'outils à l'annulation.** Les API de messages rejettent en 400 tout historique où un appel d'outil n'a pas de résultat correspondant. La pratique établie est d'injecter un résultat synthétique marqué « interrompu par l'utilisateur » pour chaque appel en vol avant de persister, et de ne jamais persister un tour à moitié streamé. C'est la classe de bug la plus fréquente des agents CLI, documentée à répétition sur les dépôts publics.
- **Spec ouverte Agent Skills.** Publiée sur agentskills.io sous licence ouverte et adoptée par Codex CLI, Cursor, Amp, Goose et opencode : `SKILL.md` avec frontmatter YAML portant `name` et `description`, répertoires optionnels `scripts/`, `references/` et `assets/`, découverte dans `~/.agents/skills`. Implémenter cette spec plutôt qu'un format propriétaire rend immédiatement utilisables les skills déjà installées pour les autres agents.
- **Snapshots de rendu terminal.** La recette officielle Ratatui est `TestBackend` avec une taille de terminal fixe, comparée par snapshot. La limite connue est que `TestBackend` ne reproduit pas le comportement d'un PTY réel, ce qui borne la portée des snapshots au rendu et non au comportement terminal.
- **Sandbox en allowlist.** La référence Linux est Landlock complété par seccomp, en allowlist stricte, avec la contrainte que les règles sont additives et ne permettent pas de soustraire un droit accordé.

### Security Findings

- **CVE-2026-26268 :** un hook déposé dans `.git/hooks` d'un dépôt s'exécute lorsque l'agent lance une commande git, en dehors de la sandbox. L'écriture par l'agent dans `.git/hooks` doit être refusée ou soumise à approbation explicite.
- **CVE-2026-48124 :** chez Cursor, une configuration de hooks contrôlée par le workspace donnait une exécution non sandboxée. Corollaire direct pour ce PRD : la configuration de projet ne doit jamais pouvoir déclarer de hooks ni élargir un périmètre de sécurité.
- **Configuration-Based Sandbox Escape :** motif générique documenté chez plusieurs agents, où un fichier écrit depuis la sandbox est exécuté sur l'hôte au lancement suivant.

*Sources complètes conservées dans `docs/codex-harness-parity-audit.md` et dans l'historique de recherche de la session de rédaction.*

## Assumptions & Constraints

### Assumptions (to validate)

- ~~Le backend Codex rejette un `function_call` sans `function_call_output` de la même manière que l'API Messages documentée.~~ **Validée le 2026-07-25 (US-003).** Mesure de bout en bout sur le backend réel, session corrompue reprise par `pyxis --resume <session> -p` : le binaire de `f6a8e5a` (avant garde-fou) reçoit `http 400: {"message":"No tool output found for function call call_e2e_orphan.","param":"input","type":"invalid_request_error"}` ; le binaire corrigé répare l'appel orphelin et le tour se déroule normalement.
- Les 553 tests existants passent aujourd'hui en local. Aucune CI ne l'a jamais vérifié, donc l'état vert initial est supposé et non mesuré. US-004 le confirme ou expose la dette.
- `Paragraph::line_count` avec la feature `unstable-rendered-line-info` donne un compte de lignes fidèle au rendu réel pour du texte contenant des sauts de ligne explicites. Les auteurs de ratatui déclarent cette API instable.
- Les skills présentes dans `~/.agents/skills` sur la machine de développement suivent la spec agentskills et sont donc parsables telles quelles.

### Hard Constraints

- **ADR-3, cœur headless :** `agent-core` ne dépend ni de `agent-tui`, ni de `agent-provider`, et n'émet jamais d'ANSI. Toute nouvelle capacité passe par un trait injecté dans `Deps` ou par une variante ajoutée à `AgentEvent`.
- **Contrats en extension seulement :** `AgentEvent` et `StreamEvent` sont consommés par la TUI et le mode headless. Les variantes s'ajoutent, elles ne se refondent pas.
- **Landlock est additif :** aucun droit accordé sous une racine ne peut être soustrait pour un sous-chemin. Toute protection de sous-chemin est donc userland et doit être documentée comme telle.
- **`restrict_self` est irréversible et précède le runtime tokio** (`crates/agent-sandbox/src/fs.rs:130-138`). Toute racine writable configurable doit être résolue avant le démarrage du runtime.
- **ADR-11 :** Linux uniquement, provider unique `OpenAiChatGpt`. Aucune story de ce PRD n'introduit de support macOS, Windows ou multi-provider.
- **ADR-8 :** crates internes nommées `agent-*`, binaire et commande publics nommés `pyxis`.
- **Budget de complexité :** aucune abstraction spéculative. Une dépendance nouvelle doit être justifiée par un besoin présent, pas par une extension anticipée.
- **Outils MCP :** `returns_untrusted()` vaut toujours `true`, description plafonnée à 2048 caractères, taint propagé intégralement (`docs/ARCHITECTURE.md` §6).

## Quality Gates

Ces commandes doivent passer pour chaque user story :

- `cargo fmt --all -- --check` - formatage conforme
- `cargo clippy --workspace --all-targets` - lints du workspace, dont les `deny` sur `panic`, `unimplemented` et `dbg`. Volontairement sans `-D warnings` : le workspace déclare `unwrap_used = "warn"` et `expect_used = "warn"` par décision documentée (`Cargo.toml:22-24`), un durcissement global rendrait la commande rouge sur du code sain
- `cargo test --workspace` - suite complète, 553 tests au point de départ

Pour les stories touchant au rendu terminal, gate supplémentaire :

- `cargo insta test --review` puis validation visuelle des snapshots modifiés, avec justification écrite de chaque diff accepté dans le message de commit

## Epics & User Stories

Release 1 couvre EP-001 et EP-002, release 2 couvre EP-003 et EP-004, release 3 couvre EP-005 et EP-006. Le phasage est structurel et non cosmétique : EP-002 doit précéder EP-003 parce qu'un refactor du composer touche 215 tests TUI et qu'aucun filet de rendu n'existe aujourd'hui.

### EP-001: Intégrité de session et annulation coopérative

Une interruption ne doit jamais laisser la conversation dans un état que le provider refuse. Le cœur devient responsable de son propre arrêt, au lieu de subir un `abort()` décidé par le client.

**Definition of Done:** interrompre pendant un dispatch d'outil produit une session valide et reprenable, le modèle sait qu'il a été interrompu, et une session déjà corrompue sur disque redevient exploitable.

#### US-001: Signal d'annulation coopératif dans le cœur
**Description:** As a utilisateur de Pyxis, I want que l'interruption soit traitée par la boucle d'agent elle-même, so that l'arrêt se produise à une frontière connue plutôt qu'à un point arbitraire du future.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un `Deps` construit, when la boucle est lancée, then elle reçoit un canal d'annulation `tokio::sync::watch` sans qu'`agent-core` acquière de nouvelle dépendance externe
- [ ] Given une annulation signalée pendant le streaming du provider, when la boucle atteint la frontière suivante, then elle cesse de consommer le stream et n'émet plus de `Text` ni de `Reasoning`
- [ ] Given une annulation signalée pendant le dispatch d'outils, when les outils en vol se terminent ou sont abandonnés, then la boucle reprend la main au lieu d'être tuée
- [ ] Given une annulation, when la boucle s'arrête, then `AgentEvent::Interrupted` est émis par le cœur et non fabriqué par le client
- [ ] Given aucune annulation signalée, when un tour se déroule normalement, then le comportement observable et les 89 tests existants d'`agent-core` sont inchangés
- [ ] Given une annulation signalée alors que la boucle est déjà terminée, when le signal arrive, then il est ignoré sans panique ni événement supplémentaire

#### US-002: Résultats d'outils synthétiques et transcript réconcilié
**Description:** As a utilisateur qui interrompt un tour, I want que chaque appel d'outil en vol reçoive un résultat explicite, so that la conversation reste valide et le modèle comprenne ce qui s'est passé.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given une interruption pendant le dispatch, when la boucle s'arrête, then chaque `tool_use` sans résultat reçoit un `ToolResult` synthétique marqué comme interrompu, en erreur, avant toute persistance
- [ ] Given ces résultats synthétiques, when le tour suivant démarre, then le modèle lit dans l'historique que les outils ont été interrompus par l'utilisateur
- [ ] Given une interruption, when la session est persistée, then `session.sync` est appelé après la réconciliation et non avant
- [ ] Given un outil qui se termine juste avant l'arrêt, when la réconciliation s'exécute, then son résultat réel est conservé et aucun résultat synthétique ne le remplace
- [ ] Given une interruption pendant l'exécution de plusieurs outils concurrents, when la réconciliation s'exécute, then chaque appel reçoit exactement un résultat, sans doublon ni oubli
- [ ] Given une session interrompue puis reprise par `/resume`, when le transcript est rejoué, then il ne contient aucun appel d'outil orphelin

#### US-003: Garde-fou de requête et réparation des sessions corrompues
**Description:** As a utilisateur dont une session est déjà cassée, I want que la construction de requête refuse structurellement d'émettre un appel orphelin, so that les sessions existantes redeviennent exploitables sans perte.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un transcript contenant un `tool_use` sans `tool_result`, when la requête est construite, then l'appel orphelin reçoit un résultat synthétique plutôt que d'être émis seul
- [ ] Given ce même transcript, when la requête part vers le backend, then elle est acceptée et le tour se déroule normalement
- [ ] Given une session `.pyxis/sessions/*.jsonl` corrompue par une interruption antérieure, when elle est reprise, then elle est exploitable sans édition manuelle du fichier
- [ ] Given un transcript sain, when la requête est construite, then la sortie est octet pour octet identique à celle produite avant cette story
- [ ] Given un `tool_result` sans `tool_use` correspondant, when la requête est construite, then le résultat orphelin est écarté et l'anomalie est tracée
- [ ] Given un transcript réparé, when la réparation s'applique, then le fichier de session sur disque n'est jamais réécrit rétroactivement

---

### EP-002: Signal de vérification restauré

Le dépôt doit redevenir capable de prouver mécaniquement ce qu'il affirme. Cet epic est un prérequis d'EP-003, pas un parallèle.

**Definition of Done:** une commande unique fait foi sur l'état du dépôt, le rendu terminal est couvert par des snapshots, et aucun fichier de statut ne contredit le code.

#### US-004: Intégration continue GitHub Actions
**Description:** As a mainteneur, I want que chaque push exécute formatage, lints et tests, so that une régression soit détectée sans intervention humaine.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un push sur n'importe quelle branche, when la CI démarre, then elle exécute `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets` et `cargo test --workspace` sur Linux
- [ ] Given la suite complète, when elle s'exécute sur un runner standard, then le workflow se termine en moins de 15 minutes, cache de compilation compris
- [ ] Given un test qui échoue, when la CI s'exécute, then le workflow échoue et nomme le test fautif dans le résumé
- [ ] Given l'état actuel du dépôt, when la CI s'exécute pour la première fois, then tout échec préexistant est corrigé ou explicitement documenté dans cette story, jamais contourné par une exclusion silencieuse
- [ ] Given une plateforme autre que Linux, when la CI est configurée, then aucune matrice multi-OS n'est ajoutée, conformément à ADR-11

#### US-005: Harness de snapshot du rendu terminal
**Description:** As a mainteneur du TUI, I want pouvoir capturer un rendu terminal complet en snapshot, so that une régression d'espacement, de préfixe ou de troncature soit détectée automatiquement.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Given un `AppState` donné, when le harness rend une frame, then il produit un snapshot texte déterministe via `TestBackend` à taille de terminal fixe
- [ ] Given deux exécutions successives sur le même état, when les snapshots sont comparés, then ils sont identiques, sans dépendance à l'horloge, à l'aléa ou à l'environnement
- [ ] Given un changement de rendu, when les tests s'exécutent, then le diff de snapshot est lisible ligne par ligne et révisable par `cargo insta review`
- [ ] Given le harness, when il est ajouté, then `insta` figure uniquement en `dev-dependencies` et n'entre pas dans le binaire distribué
- [ ] Given un état qui provoque une panique de rendu, when le snapshot est pris, then le test échoue avec l'état fautif dans le message plutôt que par une panique nue

#### US-006: Couverture snapshot des flux critiques
**Description:** As a mainteneur, I want au moins vingt snapshots couvrant les flux critiques du TUI, so that le critère d'acceptation posé par le PRD de parité TUI soit réellement satisfait.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given les flux critiques, when les snapshots sont écrits, then au moins vingt couvrent : écran d'accueil, message utilisateur, deltas de streaming, bloc de code markdown, tableau markdown, exécution shell en cours, exécution réussie, exécution en erreur, diff d'édition, dialogue d'approbation, saisie en attente, redimensionnement, session reprise, indicateur de contexte, menu de commandes, état interrompu
- [ ] Given un rendu inchangé, when les snapshots s'exécutent, then aucun diff n'est produit
- [ ] Given une divergence intentionnelle par rapport au rendu Codex, when le snapshot la capture, then elle est enregistrée comme divergence Pyxis assumée dans `docs/codex-port-inventory.md`
- [ ] Given un terminal de largeur inhabituelle, when le rendu est capturé à 40 et à 200 colonnes, then les deux snapshots sont stables et exempts de débordement horizontal

#### US-007: Harness d'intégration bout en bout
**Description:** As a mainteneur, I want un test qui démarre l'agent complet avec un provider simulé, so that le câblage entre CLI, session, outils et sandbox soit vérifié et pas seulement chaque pièce isolément.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Given un provider simulé rejouant un flux SSE enregistré, when le mode headless s'exécute sur un répertoire temporaire, then un tour complet incluant un appel d'outil et une réponse finale se déroule sans réseau
- [ ] Given ce harness, when un tour est interrompu à mi-parcours, then le test vérifie que la session résultante est valide et reprenable
- [ ] Given ce harness, when il s'exécute en CI, then il ne dépend ni d'un keyring, ni d'un terminal, ni d'identifiants réels
- [ ] Given un flux SSE malformé, when le harness le rejoue, then le test vérifie que l'erreur remonte comme échec de contrat provider et non comme panique

#### US-008: Résorption de la divergence entre statuts et code
**Description:** As a mainteneur, I want que les fichiers de statut reflètent la réalité du code, so that l'avancement du projet redevienne déductible sans lire les sources.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given `tasks/prd-codex-tui-parity-status.json`, when chaque story marquée `DONE` est confrontée à ses critères d'acceptation, then celles dont les critères ne sont pas satisfaits repassent en `IN_REVIEW` avec la preuve `chemin:ligne` du manquement
- [ ] Given `US-017` et `US-018` de ce PRD antérieur, when leur statut est revu, then il reflète que le composer n'a pas été porté et que la couverture snapshot était nulle au moment de la revue
- [ ] Given les autres PRD du dépôt, when leurs statuts `DONE` sont échantillonnés, then tout écart constaté est corrigé ou documenté
- [ ] Given une story dont le critère est ambigu, when la revue ne peut pas trancher, then elle est marquée `IN_REVIEW` plutôt que laissée `DONE` par défaut
- [ ] Given un fichier de statut invalide ou désynchronisé de son PRD, when la revue le lit, then l'anomalie est corrigée et non contournée par une réécriture du critère d'acceptation d'origine

---

### EP-003: Composer multi-ligne

Rendre le composer capable de ce qu'un utilisateur attend d'un champ de saisie : plusieurs lignes, visibles, éditables.

**Definition of Done:** un prompt de dix lignes se rédige, s'édite et se relit intégralement, et un collage volumineux reste manipulable.

#### US-009: Modèle de saisie multi-ligne
**Description:** As a utilisateur, I want insérer un saut de ligne dans mon prompt sans le soumettre, so that je puisse rédiger une instruction structurée en plusieurs paragraphes.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given le composer, when l'utilisateur presse Alt+Entrée ou Ctrl+J, then un saut de ligne est inséré à la position du curseur et le message n'est pas soumis
- [ ] Given un terminal qui rapporte les modificateurs sur Entrée, when l'utilisateur presse Maj+Entrée, then le comportement est identique à Alt+Entrée
- [ ] Given un terminal qui ne distingue pas Maj+Entrée, when l'utilisateur presse Entrée, then le message est soumis, et l'aide de pied de page indique le raccourci d'insertion réellement disponible
- [ ] Given une saisie multi-ligne, when l'utilisateur navigue avec les flèches haut et bas, then le curseur se déplace entre les lignes de la saisie, et l'historique des prompts n'est rappelé que lorsque le curseur est déjà sur la première ou la dernière ligne
- [ ] Given une saisie contenant des caractères multi-octets et des graphèmes composés, when le curseur se déplace ou qu'un caractère est effacé, then aucune position ne tombe au milieu d'un caractère et aucune panique n'est levée
- [ ] Given une saisie vide, when l'utilisateur presse Entrée, then rien n'est soumis, comportement inchangé

#### US-010: Rendu wrappé, hauteur dynamique et défilement
**Description:** As a utilisateur, I want voir l'intégralité de ce que je tape, so that je puisse relire et corriger avant d'envoyer.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given une ligne plus large que le terminal, when elle est rendue, then elle est repliée sur plusieurs lignes visuelles et aucun caractère n'est perdu
- [ ] Given une saisie de N lignes, when elle est rendue, then la hauteur du composer s'adapte entre une ligne et un plafond configuré, calculée à partir du nombre de lignes réellement rendues après repli
- [ ] Given une saisie dépassant le plafond de hauteur, when le curseur se déplace, then la zone défile verticalement pour que la ligne du curseur reste visible
- [ ] Given le curseur sur une ligne repliée, when la frame est dessinée, then la position du curseur à l'écran correspond exactement à sa position logique dans le texte
- [ ] Given un composer de dix lignes, when une frame est rendue, then le temps de rendu P95 reste sous 16 ms
- [ ] Given un terminal réduit à moins de lignes que le composer n'en demande, when la frame est dessinée, then le transcript et le composer restent tous deux visibles sans débordement ni panique

#### US-011: Collage multi-ligne et collage volumineux
**Description:** As a utilisateur, I want coller un extrait de code ou un log sans que le composer devienne inutilisable, so that je puisse envoyer du contexte réel à l'agent.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**
- [ ] Given un collage contenant des sauts de ligne, when il est inséré, then les lignes sont préservées et le message n'est pas soumis
- [ ] Given un collage de plus de 500 lignes, when il est inséré, then le composer affiche un résumé compact indiquant le volume collé plutôt que d'occuper tout l'écran
- [ ] Given un collage résumé, when le message est soumis, then le contenu intégral est transmis au modèle, jamais la représentation résumée
- [ ] Given un collage contenant des séquences d'échappement ANSI, when il est inséré, then elles sont neutralisées à l'affichage et ne peuvent pas altérer le rendu du terminal
- [ ] Given un collage pendant qu'un dialogue d'approbation est ouvert, when il arrive, then il est ignoré et le dialogue reste intact

---

### EP-004: Fidélité et confinement de l'exécution

Ce que Pyxis annonce au modèle doit correspondre à ce qu'il exécute, et le confinement doit couvrir les usages réels sans forcer à le désactiver entièrement.

**Definition of Done:** la sandbox n'oblige plus à `--no-sandbox` pour un usage normal, les chemins d'exécution hors sandbox sont fermés côté outils, et le shell annoncé est le shell exécuté.

#### US-012: Racines writables configurables incluant le répertoire temporaire
**Description:** As a utilisateur sous sandbox, I want que les outils qui écrivent dans un répertoire temporaire fonctionnent, so that je n'aie pas à désactiver tout le confinement pour compiler.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given la sandbox active, when une commande écrit dans `$TMPDIR` ou dans `/tmp`, then l'écriture réussit
- [ ] Given une liste de racines writables supplémentaires fournie par configuration, when la sandbox est établie, then chaque racine existante est accordée en écriture et chaque racine absente est ignorée avec une trace
- [ ] Given ces racines, when elles sont résolues, then la résolution a lieu avant le démarrage du runtime tokio, contrainte imposée par le caractère irréversible de `restrict_self`
- [ ] Given une racine writable pointant vers la racine du système ou vers le répertoire personnel entier, when la configuration est chargée, then elle est refusée avec un message expliquant que le confinement deviendrait vide de sens
- [ ] Given aucune configuration fournie, when la sandbox est établie, then le comportement par défaut inclut le répertoire temporaire et reste inchangé par ailleurs

#### US-013: Sous-chemins protégés contre l'exécution différée
**Description:** As a utilisateur, I want que l'agent ne puisse pas écrire dans les emplacements dont le contenu s'exécute plus tard hors sandbox, so that une injection indirecte ne se transforme pas en exécution sur ma machine.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un outil d'écriture ou d'édition ciblant `.git/hooks`, `.git/config`, `.pyxis/` ou le fichier de configuration du projet, when la validation d'entrée s'exécute, then l'appel est refusé avant exécution avec un message nommant la raison
- [ ] Given un chemin qui atteint une zone protégée par lien symbolique ou par remontée relative, when il est validé, then il est refusé de la même manière que le chemin direct
- [ ] Given ce refus, when il se produit, then il ne dépend pas du mode de permission actif et ne peut pas être contourné par `DontAsk` ou `BypassPermissions`
- [ ] Given la commande `bash`, when elle écrit dans une zone protégée, then la limitation est documentée explicitement dans le message d'aide de la sandbox et dans `docs/CURRENT_STATUS.md`, car Landlock ne permet pas de soustraire ce droit
- [ ] Given un projet sans dépôt git, when la protection s'applique, then l'absence de `.git` n'est pas une erreur

#### US-014: Cohérence entre le shell annoncé et le shell exécuté
**Description:** As a utilisateur, I want que l'agent exécute les commandes dans le shell qu'il croit utiliser, so that les constructions qu'il produit fonctionnent.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un shell de connexion défini dans l'environnement, when une commande est exécutée, then elle l'est par ce shell en mode non interactif
- [ ] Given l'absence de shell de connexion ou un shell introuvable, when une commande est exécutée, then le repli sur `sh` est appliqué et le contexte annoncé au modèle indique `sh`
- [ ] Given le contexte injecté au modèle, when il est composé, then le shell qu'il annonce est exactement celui qui sera utilisé pour exécuter
- [ ] Given un shell de connexion qui n'est pas compatible POSIX ou qui échoue à démarrer, when la première commande est lancée, then l'échec est détecté et le repli sur `sh` s'applique sans faire échouer le tour
- [ ] Given ce changement, when les 84 tests d'`agent-tools` s'exécutent, then aucun ne régresse

#### US-015: Sortie shell streamée
**Description:** As a utilisateur, I want voir la sortie d'une commande longue pendant qu'elle s'exécute, so that je puisse distinguer une compilation en cours d'un blocage et décider d'interrompre.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given une commande produisant de la sortie progressivement, when elle s'exécute, then des fragments de sortie sont émis comme événements structurés avant la fin de la commande
- [ ] Given ces fragments, when le TUI les reçoit, then ils s'affichent dans la cellule d'exécution en cours, avec une latence inférieure à 500 ms après leur production
- [ ] Given une commande produisant un volume de sortie très supérieur au plafond de rétention, when elle s'exécute, then l'affichage reste borné et la sortie finale conserve la politique de troncature existante
- [ ] Given une commande interrompue en cours de sortie, when l'interruption survient, then les fragments déjà émis restent affichés et le résultat d'outil reflète l'interruption
- [ ] Given le mode headless, when des fragments sont émis, then ils n'altèrent pas la sortie textuelle finale existante

---

### EP-005: Contrats machine

Rendre Pyxis configurable et observable par une machine, ce que l'architecture promet depuis ADR-3 sans l'avoir jamais exposé.

**Definition of Done:** un projet transporte sa configuration, un appelant automatisé observe le déroulement d'un run, et le diff d'un tour existe comme donnée.

#### US-016: Configuration déclarative avec un vrai parseur TOML
**Description:** As a utilisateur, I want préconfigurer Pyxis par fichier, so that mes réglages survivent au lancement et voyagent avec le projet.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un fichier de configuration global et un fichier de projet, when Pyxis démarre, then la précédence appliquée est défauts, puis global, puis projet, puis variables d'environnement, puis arguments de ligne de commande
- [ ] Given un fichier TOML, when il est lu, then il est analysé par la bibliothèque TOML de référence et non par un découpage textuel, et le parseur maison `parse_tomlish_string` est retiré
- [ ] Given un fichier TOML invalide, when il est lu, then l'erreur nomme le fichier, la ligne et la clé fautive, et Pyxis démarre avec les défauts plutôt que d'échouer
- [ ] Given un fichier de configuration de projet, when il déclare des hooks, des racines writables ou un mode de permission, then ces clés sont ignorées avec un avertissement, car un fichier contrôlé par le workspace ne doit jamais élargir un périmètre de sécurité
- [ ] Given une clé inconnue, when la configuration est lue, then elle est signalée sans faire échouer le démarrage
- [ ] Given le mode headless, when il démarre, then il lit la configuration au même titre que le mode interactif

#### US-017: Sortie JSONL d'événements en mode headless
**Description:** As a appelant automatisé, I want observer le déroulement d'un run sous forme d'événements machine, so that je puisse intégrer Pyxis en CI ou dans un orchestrateur sans analyser du texte destiné à un humain.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given le mode headless avec le format de sortie machine demandé, when un tour s'exécute, then chaque événement est écrit sur la sortie standard comme un objet JSON par ligne, vidé immédiatement
- [ ] Given ce flux, when il est produit, then chaque ligne porte un numéro de version de schéma, et le schéma est documenté dans `docs/`
- [ ] Given la fin du run, when le dernier événement est émis, then il récapitule l'identifiant de session, le nombre de tours, la consommation de jetons et la cause de fin
- [ ] Given le format textuel par défaut, when il est utilisé, then la sortie est identique à celle produite avant cette story
- [ ] Given un événement contenant du contenu non textuel ou des caractères de contrôle, when il est sérialisé, then la ligne reste un JSON valide analysable
- [ ] Given une erreur fatale en cours de run, when elle survient, then elle est émise comme événement structuré avant la sortie du processus, et le code de sortie la distingue d'un succès

#### US-018: Tracker de diff agrégé du tour
**Description:** As a utilisateur et as a client du cœur, I want connaître l'ensemble des modifications d'un tour, so that je puisse répondre à la question la plus fréquente en fin de tour sans reconstituer les éditions une par une.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-017

**Acceptance Criteria:**
- [ ] Given un tour qui modifie des fichiers, when il se termine, then un diff agrégé couvrant l'ensemble des fichiers touchés est disponible comme donnée structurée
- [ ] Given des modifications produites par une commande shell et non par un outil d'édition, when le tour se termine, then elles apparaissent dans le diff agrégé
- [ ] Given un fichier créé puis supprimé dans le même tour, when le diff est calculé, then il n'apparaît pas comme modification nette
- [ ] Given un fichier binaire ou un fichier dépassant un seuil de taille, when il est touché, then il est listé comme modifié sans que son contenu soit diffé
- [ ] Given un tour sans modification, when il se termine, then le diff agrégé est vide et aucun événement parasite n'est émis
- [ ] Given un tour interrompu, when l'interruption survient, then le diff agrégé reflète les modifications déjà appliquées

---

### EP-006: Extensibilité utilisateur

Ouvrir les trois canaux d'extension attendus en 2026, sans inventer de format propriétaire là où une spec ouverte existe.

**Definition of Done:** un serveur MCP configuré rend ses outils appelables, une skill installée pour un autre agent fonctionne sans adaptation, et un hook peut refuser un appel d'outil.

#### US-019: Appel d'outil MCP et adaptation au registre
**Description:** As a mainteneur, I want pouvoir invoquer un outil d'un serveur MCP depuis le registre d'outils, so que la découverte existante devienne une capacité réelle.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given une connexion MCP établie, when un outil est invoqué avec ses arguments, then son résultat est retourné dans la forme attendue par le registre
- [ ] Given un outil qui échoue fonctionnellement, when il est invoqué, then l'échec est rendu comme résultat d'outil en erreur et non comme erreur de protocole, conformément à la distinction imposée par le SDK
- [ ] Given un serveur qui ne répond pas, when un appel est émis, then il expire après un délai borné et l'échec est attribué au serveur nommé
- [ ] Given un serveur qui se déconnecte pendant un appel, when la déconnexion survient, then l'appel retourne une erreur nommant le serveur et la connexion repasse dans un état non connecté sans panique
- [ ] Given un outil MCP adapté au registre, when ses métadonnées sont lues, then il déclare toujours retourner du contenu non fiable

#### US-020: Exposition des outils MCP à la boucle du modèle
**Description:** As a utilisateur, I want que les outils de mes serveurs MCP soient appelables par le modèle, so que configurer un serveur change réellement ce que l'agent sait faire.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-019

**Acceptance Criteria:**
- [ ] Given des serveurs MCP connectés, when la liste des outils est envoyée au modèle, then elle inclut leurs outils sous un nom préfixé par le serveur d'origine
- [ ] Given deux serveurs exposant un outil de même nom, when les outils sont enregistrés, then le préfixe garantit l'unicité et aucun outil n'en masque un autre
- [ ] Given un outil MCP appelé par le modèle, when son résultat revient, then le contenu est marqué non fiable et la propagation de taint force une demande d'approbation avant toute action destructrice ou réseau dans le même tour
- [ ] Given une description d'outil dépassant la limite, when elle est exposée, then elle est tronquée à 2048 caractères
- [ ] Given un serveur MCP indisponible au démarrage, when la liste d'outils est composée, then les outils des serveurs sains restent disponibles et l'indisponibilité est signalée sans bloquer la session
- [ ] Given le mode headless, when aucun serveur MCP n'est chargé, then le comportement reste inchangé

#### US-021: Skills conformes à la spécification ouverte
**Description:** As a utilisateur, I want que mes skills existantes fonctionnent dans Pyxis, so que je n'aie pas à maintenir un format spécifique.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given un répertoire de skill contenant un `SKILL.md` avec frontmatter, when Pyxis démarre, then le nom et la description sont lus et la skill est enregistrée
- [ ] Given le catalogue de skills, when la liste d'outils est composée, then le modèle reçoit le nom et la description de chaque skill disponible
- [ ] Given une skill invoquée par son nom, when elle est sélectionnée, then ses instructions sont injectées dans le contexte, au lieu que le texte de la commande soit envoyé littéralement au modèle
- [ ] Given un `SKILL.md` absent, mal formé ou sans frontmatter exploitable, when le répertoire est lu, then la skill est ignorée avec une trace et le démarrage n'échoue pas
- [ ] Given une skill dont la description est volumineuse, when elle est exposée, then elle est tronquée à la même limite que les autres descriptions d'outils
- [ ] Given l'absence totale de répertoire de skills, when Pyxis démarre, then aucun avertissement n'est émis

#### US-022: Moteur de hooks avec droit de veto
**Description:** As a utilisateur, I want exécuter mes propres commandes autour des appels d'outils, so que je puisse automatiser un formatage ou bloquer une action dangereuse selon mes règles.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-016

**Acceptance Criteria:**
- [ ] Given un hook déclaré pour l'événement précédant un appel d'outil, when un outil est sur le point de s'exécuter, then le hook reçoit le nom de l'outil et ses arguments sur son entrée standard
- [ ] Given un hook qui retourne un refus, when il se termine, then l'outil n'est pas exécuté et la raison du refus est transmise au modèle
- [ ] Given un hook déclaré pour l'événement suivant un appel d'outil, when l'outil s'est exécuté, then le hook reçoit le résultat
- [ ] Given un hook qui dépasse son délai d'exécution ou qui échoue, when l'événement précédant un appel est traité, then l'appel est refusé, conformément au principe fail-closed du projet
- [ ] Given des hooks déclarés dans un fichier de configuration de projet, when la configuration est chargée, then ils sont ignorés, seule la configuration globale pouvant déclarer des hooks
- [ ] Given un hook qui écrit un volume important sur sa sortie, when il se termine, then la sortie est bornée avant d'être transmise
- [ ] Given aucune déclaration de hook, when un outil s'exécute, then le surcoût est nul et le comportement est inchangé

## Functional Requirements

- FR-01: La boucle d'agent doit s'arrêter sur signal coopératif à une frontière connue, et non par annulation externe de sa tâche.
- FR-02: Le système doit garantir qu'aucune requête ne contient un appel d'outil sans résultat correspondant, quelle que soit l'origine du transcript.
- FR-03: Le système doit informer le modèle, dans l'historique, qu'un outil a été interrompu par l'utilisateur.
- FR-04: Le composer doit permettre l'insertion d'un saut de ligne sans soumission, et afficher intégralement une saisie de plusieurs lignes.
- FR-05: Le système doit exécuter les commandes shell dans le shell qu'il annonce au modèle.
- FR-06: Le système doit refuser toute écriture d'un outil d'édition vers un emplacement dont le contenu s'exécute ultérieurement hors sandbox.
- FR-07: Le système ne doit PAS permettre à un fichier de configuration contrôlé par le workspace d'élargir un périmètre de sécurité.
- FR-08: Le système doit exposer, en mode non interactif, un flux d'événements machine versionné et documenté.
- FR-09: Le système doit exposer les outils des serveurs MCP connectés à la boucle du modèle, avec propagation de taint.
- FR-10: Le système doit permettre à un hook utilisateur de refuser un appel d'outil avant son exécution.
- FR-11: Le système doit produire, pour chaque tour, un diff agrégé des fichiers modifiés, y compris par des commandes shell.
- FR-12: Le système ne doit PAS faire échouer le démarrage à cause d'un fichier de configuration, d'une skill ou d'un serveur MCP invalide.

## Non-Functional Requirements

- **Performance :** rendu de frame P95 sous 16 ms avec un composer de 10 lignes et un transcript de 500 cellules. Latence entre la frappe d'interruption et l'émission de l'événement correspondant sous 200 ms P95. Latence d'affichage d'un fragment de sortie shell sous 500 ms après sa production. Surcoût de sérialisation d'un événement JSONL sous 1 ms.
- **Sécurité :** aucun outil d'édition ne peut écrire dans `.git/hooks`, `.git/config`, `.pyxis/` ni le fichier de configuration de projet, y compris par lien symbolique. Tout contenu issu d'un outil MCP est marqué non fiable à 100 %. Une configuration de projet ne peut modifier aucune clé de sécurité. Un hook qui expire ou échoue avant un appel d'outil provoque un refus, jamais une autorisation par défaut.
- **Fiabilité :** 0 session corrompue sur 50 interruptions pendant un dispatch d'outil. Un serveur MCP indisponible n'empêche jamais le démarrage d'une session. Un délai d'appel MCP borné à 60 secondes par défaut. Un hook borné à 5 secondes.
- **Observabilité :** 100 % des variantes d'`AgentEvent` sérialisables en JSON. Schéma d'événements versionné et documenté dans `docs/`. Cause de fin de run explicite dans l'événement final.
- **Maintenabilité :** CI complète sous 15 minutes, cache compris. Au moins 20 snapshots de rendu. Aucune nouvelle dépendance de production sans justification écrite dans la story qui l'introduit.
- **Compatibilité :** aucune régression sur les 553 tests existants. La sortie textuelle du mode headless par défaut reste identique octet pour octet.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Interruption pendant plusieurs outils concurrents | Échap pendant un dispatch parallèle de lectures | Chaque appel reçoit exactement un résultat synthétique, aucun doublon | "Interrompu. Les outils en cours ont été arrêtés." |
| 2 | Interruption juste après la fin d'un outil | Échap dans la fenêtre entre fin d'exécution et persistance | Le résultat réel est conservé, aucun résultat synthétique ne l'écrase | aucun |
| 3 | Session déjà corrompue sur disque | Reprise d'une session interrompue avant ce PRD | Réparation à la construction de requête, session exploitable | "Session réparée : 1 appel d'outil sans résultat." |
| 4 | Collage volumineux | Collage de plus de 500 lignes dans le composer | Résumé compact affiché, contenu intégral transmis à l'envoi | "[collage : 847 lignes]" |
| 5 | Terminal plus petit que le composer | Redimensionnement sous la hauteur demandée | Transcript et composer restent visibles, aucun débordement | aucun |
| 6 | Répertoire temporaire indisponible | `$TMPDIR` pointe vers un chemin inexistant | Racine ignorée avec trace, sandbox établie sans elle | "Racine writable ignorée : chemin introuvable." |
| 7 | Écriture vers une zone protégée par lien symbolique | Outil d'édition ciblant un lien vers `.git/hooks` | Refus avant exécution, identique au chemin direct | "Écriture refusée : cible protégée (.git/hooks)." |
| 8 | Shell de connexion cassé | Shell configuré introuvable ou refusant de démarrer | Repli sur `sh`, tour poursuivi, contexte modèle corrigé | "Shell indisponible, repli sur sh." |
| 9 | Configuration TOML invalide | Erreur de syntaxe dans le fichier de projet | Démarrage avec les défauts, erreur localisée | "config.toml ligne 12 : clé inattendue." |
| 10 | Configuration de projet déclarant des hooks | Dépôt cloné contenant une configuration hostile | Clés de sécurité ignorées avec avertissement | "Clés de sécurité ignorées dans la configuration de projet." |
| 11 | Serveur MCP indisponible au démarrage | Serveur configuré qui ne répond pas | Outils des serveurs sains disponibles, indisponibilité signalée | "Serveur MCP 'x' indisponible, ses outils sont absents." |
| 12 | Hook qui expire | Hook utilisateur bloqué au-delà de son délai | L'appel d'outil est refusé, fail-closed | "Hook expiré, appel d'outil refusé." |
| 13 | Sortie shell très volumineuse | Commande produisant des centaines de milliers de lignes | Affichage borné, troncature finale préservée | "[sortie tronquée]" |
| 14 | Fichier binaire modifié dans un tour | Commande shell écrasant une image | Listé comme modifié, contenu non diffé | "fichier binaire modifié" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Le refactor du composer casse une partie des 215 tests TUI existants | High | Med | EP-002 précède EP-003 : les snapshots et la CI existent avant que le modèle de saisie ne change, ce qui transforme une régression silencieuse en échec visible |
| 2 | L'API de repli de texte de ratatui est déclarée instable par ses auteurs | Med | Med | Les snapshots capturent le rendu réel, donc une évolution de l'API produit un diff explicite plutôt qu'une dérive silencieuse. La montée de version de ratatui devient une décision consciente |
| 3 | La CI révèle des tests déjà cassés, non détectés faute d'automatisation | Med | Med | US-004 traite explicitement ce cas : tout échec préexistant est corrigé ou documenté dans la story, jamais contourné par une exclusion |
| 4 | La protection des sous-chemins ne couvre pas `bash`, ce qui donne une fausse impression de sécurité | High | High | La limitation est documentée dans le message d'aide de la sandbox et dans `docs/CURRENT_STATUS.md`, au lieu d'être passée sous silence. Landlock étant additif, aucune correction kernel n'est possible sans changer la stratégie de sandbox entière |
| 5 | L'annulation coopérative ne peut pas interrompre un outil bloqué dans un appel système | Med | Med | Le délai d'exécution par outil existe déjà et reste la borne supérieure. L'annulation reprend la main à la frontière suivante, et le résultat synthétique garantit la validité du transcript même si l'outil ne s'arrête pas immédiatement |
| 6 | Le périmètre de 22 stories dépasse le seuil au-delà duquel les PRD échouent habituellement | High | Med | Phasage explicite en trois releases, chacune livrable et vérifiable indépendamment. R1 seule résout les deux problèmes qui bloquent l'usage quotidien |
| 7 | La spec agentskills évolue et le format lu devient obsolète | Low | Low | Seuls le nom et la description sont exploités en v1, ce qui est le socle stable de la spec. Les répertoires optionnels sont hors périmètre |
| 8 | L'ajout de variantes à `AgentEvent` casse un consommateur | Low | Med | Les variantes sont ajoutées, jamais modifiées, et le contrat est couvert par les tests des trois consommateurs actuels |

## Non-Goals

- **Transport MCP distant et OAuth par serveur.** Seul stdio reste supporté. La roadmap place ce sujet en Phase 2 et l'ajouter ici doublerait le périmètre d'EP-006 pour un besoin non présent en dogfood.
- **Protocole de type serveur d'application ou JSON-RPC pour intégration IDE.** Pyxis vise l'embarquement in-process, pas l'interopérabilité IDE. La sortie JSONL couvre le besoin d'observabilité machine sans introduire de protocole bidirectionnel.
- **Profils de configuration.** Codex en propose, aucun besoin actuel ne les justifie. La précédence à quatre niveaux couvre les cas réels.
- **Filtrage réseau au niveau kernel.** Le proxy coopératif reste la solution, avec sa limite déjà documentée par ADR-7. Y toucher relèverait d'un changement de stratégie de sandbox, pas d'une correction.
- **Modes de collaboration de première classe, retour arrière sur un message précédent, steering en cours de tour.** Ces trois manques sont réels et documentés dans l'audit, mais aucun ne bloque l'usage quotidien. Ils relèvent d'un PRD ultérieur une fois le harness assaini.
- **Support macOS et multi-provider.** Exclus par ADR-11 et par la roadmap Phase 2 et Phase 3.
- **Reflow du scrollback déjà émis au redimensionnement.** Écart majeur identifié par l'audit, mais il touche le moteur d'insertion inline et non le composer. À traiter séparément pour ne pas coupler deux refontes du TUI dans la même release.

## Files NOT to Modify

- `crates/agent-core/src/event.rs` et `crates/agent-core/src/provider.rs` : contrats consommés par la TUI et le mode headless. Extension par ajout de variantes uniquement, jamais de refonte.
- `crates/agent-sandbox/src/fs.rs:130-138` (`restrict_self`) : séquence irréversible exécutée avant le runtime tokio. Tout changement d'ordre casse l'héritage du confinement par les sous-processus.
- `crates/agent-tools/src/permission.rs` : logique fail-closed et propagation de taint, couverte par neuf tests de sécurité. Les défauts ne doivent pas être affaiblis, seulement étendus.
- `docs/ARCHITECTURE.md` invariants 1 à 9 : amendables uniquement par un ADR, pas par une story.
- `tasks/prd-pyxis.md`, `tasks/prd-codex-orchestration.md`, `tasks/prd-response-rendering.md` et leurs fichiers de statut : archives historiques. Seul `tasks/prd-codex-tui-parity-status.json` est modifié, et uniquement par US-008.
- `docs/codex-harness-parity-audit.md` : constat daté servant de référence à ce PRD.

## Technical Considerations

- **Signal d'annulation :** recommandation `tokio::sync::watch`, déjà disponible via la feature `sync` du workspace, plutôt qu'un `CancellationToken` de `tokio-util` qui ajouterait une dépendance. L'ingénierie doit confirmer que la propagation aux sous-tâches d'outils concurrents ne demande pas la hiérarchie de tokens offerte par `tokio-util`. Le champ est-il porté par `Deps` ou par `RunConfig` ?
- **Modèle de saisie :** recommandation d'un composer maison sur `Paragraph` et `line_count`, la feature `unstable-rendered-line-info` étant déjà activée pour ce calcul. Alternative évaluée : `tui-textarea`, écartée parce que son moteur de repli diverge du pipeline de rendu inline. L'ingénierie doit trancher la représentation interne : `Vec<String>` de lignes, ou `String` unique avec index de curseur ? Le second minimise le diff sur les 215 tests existants, le premier simplifie la navigation verticale.
- **Positionnement du curseur sur ligne repliée :** aucun helper n'existe dans ratatui. Le calcul devra être dérivé du comptage de lignes sur le préfixe jusqu'au curseur. Le coût linéaire est-il acceptable à chaque frame, ou faut-il le mémoriser dans le cache existant en mutabilité intérieure ?
- **Frontmatter des skills :** la spec impose du YAML. Le workspace n'a aucune dépendance YAML. L'ingénierie doit choisir entre une bibliothèque YAML maintenue et un analyseur de frontmatter restreint aux deux clés utilisées. Le précédent de `parse_tomlish_string` invite à la prudence sur l'analyse maison, mais deux clés scalaires ne sont pas un langage.
- **Détection de Maj+Entrée :** dépend du protocole de clavier du terminal. Faut-il activer le protocole étendu de crossterm quand il est disponible, avec le risque de changer le comportement d'autres touches, ou se limiter aux raccourcis universels ?
- **Diff agrégé :** la capture des modifications faites par une commande shell suppose une comparaison avant et après. Faut-il une empreinte des fichiers du workspace, coûteuse sur un gros dépôt, ou une surveillance du système de fichiers, qui ajoute une dépendance ? Un périmètre restreint aux fichiers suivis par git est-il suffisant ?
- **Sérialisation d'`AgentEvent` :** les variantes portent-elles déjà `Serialize`, ou faut-il une représentation de transport distincte pour ne pas figer la structure interne dans un contrat public ?

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Sessions corrompues après interruption pendant un dispatch d'outil | 100 % | 0 % | Month-1 | Test d'intégration US-007 répété 50 fois |
| Tests exécutés automatiquement à chaque push | 0 | 553 et plus | Month-1 | Statut du workflow CI |
| Snapshots de rendu terminal | 0 | 20 minimum | Month-1 | Décompte des fichiers de snapshot |
| Lignes maximales rédigeables dans le composer | 1 | Illimitée, 10 visibles sans défilement | Month-1 | Snapshot de composer multi-ligne |
| Outils MCP appelables par le modèle | 0 sur N listés | 100 % des outils listés | Month-6 | Test d'intégration avec serveur MCP simulé |
| Recours à `--no-sandbox` en dogfood quotidien | Nécessaire dès qu'un outil écrit dans un répertoire temporaire | 0 recours sur une semaine de dogfood | Month-1 | Journal d'usage personnel |
| Stories marquées DONE sans critère vérifiable | 2 identifiées, périmètre non exhaustif | 0 | Month-1 | Revue US-008 |
| Écarts pertinents ouverts de l'audit de parité | 117 | 117 moins ceux couverts par ce PRD, recomptés | Month-6 | Nouvelle passe d'audit sur les dimensions traitées |

## Open Questions

- La réparation d'une session corrompue doit-elle réécrire le fichier `.jsonl` sur disque, ou rester une correction en mémoire à chaque chargement ? La story tranche pour la seconde option par prudence, mais un utilisateur reprenant souvent d'anciennes sessions paierait le coût à chaque fois. À trancher par Arthur avant EP-001, car cela change le contrat de `agent-session`.
- ~~Le diff agrégé doit-il se limiter aux fichiers suivis par git ?~~ **Tranchée le 2026-07-25 (US-018) : oui, périmètre git.** La découverte est déléguée à `git status --porcelain -uall`, donc fichiers non suivis inclus et fichiers ignorés exclus. Aucune dépendance de surveillance du système de fichiers n'a été ajoutée. Contrepartie assumée et documentée dans `docs/CURRENT_STATUS.md` : hors dépôt git, le diff agrégé est toujours vide.
- Faut-il exposer un raccourci pour ouvrir le prompt dans l'éditeur externe défini par l'environnement, comme le font plusieurs concurrents ? Non inclus dans ce PRD, mais cela réduirait la pression sur EP-003. À trancher après le premier usage réel du composer multi-ligne.
- ~~Le schéma d'événements JSONL doit-il viser une convergence avec le vocabulaire d'un concurrent, ou assumer son propre vocabulaire documenté ?~~ **Tranchée le 2026-07-25 (US-017) : vocabulaire propre, documenté et versionné** dans `docs/EVENT_SCHEMA.md`. Aucun standard n'existant, s'aligner sur un concurrent aurait imité un choix arbitraire au lieu d'assumer le sien. Chaque ligne porte `schema`, incrémenté seulement si une ligne déjà émise change de forme.
[/PRD]
