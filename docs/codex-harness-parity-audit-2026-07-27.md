# Audit de parité harness : Pyxis vs Codex CLI — passe du 2026-07-27

> **Statut : contexte historique, non normatif.** La cible de parité est
> `docs/parity/codex-baseline-matrix.json`, générée depuis le clone Codex figé
> au commit `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`
> (`cargo run -p agent-parity -- check`). Ce document décrit un instantané
> antérieur : il informe, il n'arbitre plus un écart. L'état réellement livré
> est dans `docs/CURRENT_STATUS.md`, et la preuve qui l'accompagne dans
> `docs/parity/offline-suite.md`.

Troisième passe, en lecture seule. Aucune ligne de code n'a été modifiée.

**Baseline.** [`docs/codex-harness-parity-audit.md`](./codex-harness-parity-audit.md) (2026-07-24,
221 écarts retenus après réfutation, 125 énumérés) reste le constat fondateur, laissé intact.
[`docs/codex-harness-parity-audit-2026-07-25.md`](./codex-harness-parity-audit-2026-07-25.md) mesure
le delta après les releases R1 à R3 (102 écarts restants). Ce document mesure le delta après R4,
c'est-à-dire `tasks/prd-harness-capabilities.md` (EP-001 à EP-006, tous `DONE`).

**Références de mesure.** Pyxis à `0c1cf17` : 52 093 lignes Rust, 10 crates, 835 tests, 37 snapshots
de rendu. Codex CLI à `95637f7056` : 1 222 552 lignes Rust, 107 crates. Le rapport est de **23 pour
1**. Toute lecture de ce document qui ignore ce ratio produira de mauvaises priorités.

**Méthode.** Les fermetures et les écarts structurants ci-dessous ont été revérifiés un par un sur le
code à `0c1cf17`, avec preuve `path:line`. Les décomptes résiduels par dimension sont *reportés* de
la passe du 25 moins les fermetures vérifiées : ils ne constituent pas une ré-énumération exhaustive
des 82 écarts restants, et sont donnés comme ordre de grandeur.

---

## Verdict

**La question a changé.** Les deux passes précédentes répondaient à « qu'est-ce que l'agent ne sait
pas faire ». R4 a fermé ce front : le modèle appelle des outils MCP, les skills du format ouvert sont
lues et injectées, un moteur de hooks existe, le processus est traçable, la friction d'approbation
`bash` est morte. Les deux dimensions restées `minimal` depuis le 24 juillet (MCP, extensibilité)
montent toutes les deux de deux crans.

Ce qui reste ne se lit plus comme une liste de capacités manquantes mais comme **trois blocs de
nature différente**, qu'il faut arbitrer séparément et pas au même prix :

1. **La profondeur de configuration du harness** (~25 écarts). Pyxis expose 10 clés TOML et 12
   drapeaux CLI. Codex expose des profils, des surcharges `-c key=value`, un mode de sandbox
   sélectionnable, une politique d'approbation granulaire, et une couche de configuration managée
   (`requirements.toml`). C'est le bloc le moins spectaculaire et le plus rentable en dogfood.
2. **La politique d'exécution** (~10 écarts). Le classificateur de commandes livré par EP-002 est
   une execpolicy en réduction : une liste d'allow codée en dur contre un langage de règles
   (`codex-rs/execpolicy/`, parseur + décision + amendement). Il n'existe ni mode de sandbox
   sélectionnable, ni escalade après un échec imputé au sandbox.
3. **Tout ce que Codex est en plus d'un agent interactif** (~40 écarts nominaux). App-server pour
   les IDE, tâches cloud, sous-agents, mémoires, plugins, review mode, OTLP, sandbox Windows,
   analytics. **Ce bloc ne devrait pas être compté comme un écart de parité**, et c'est la
   principale correction de cadrage que cette passe apporte.

**Recommandation de cadrage.** « Parité harness avec Codex CLI » n'est plus un objectif utile tel
quel. À 23 pour 1, viser la parité de surface, c'est s'engager à porter un produit d'entreprise avec
les moyens d'un dépôt personnel. L'objectif qui reste défendable est : **parité sur le harness
d'agent interactif local**, c'est-à-dire les blocs 1 et 2, explicitement hors bloc 3. Formulé ainsi,
le reste à faire tient en une dizaine de lots de taille `S` à `M`, contre un chantier ouvert.

---

## Fermé depuis la passe du 25

20 écarts fermés sur les 102 restants (20 %), plus les 3 écarts « révélés par R1 à R3 » qui étaient
la recommandation n°1 de la passe précédente. Toutes les preuves ci-dessous ont été relues.

| Écart | Preuve de fermeture |
|---|---|
| `context_window` jeté au parsing | `crates/agent-provider/src/models.rs:34` (`WireModel`), `models.rs:93` (filtre `> 0`) |
| Compteurs de tokens réels jetés par la TUI | `crates/agent-core/src/event.rs:94-99` (`context_tokens`, `context_window`), `crates/agent-core/src/agent.rs:816` |
| Jauge de contexte jamais alimentée | `crates/agent-tui/src/state.rs:1227` (`context_pct` assigné hors tests) |
| Quotas d'abonnement jamais remontés | `crates/agent-core/src/quota.rs` (`QuotaWindow`, `QuotaSnapshot`), `event.rs:44` (`AgentEvent::Quota`) |
| Aucune classification de risque des commandes shell | `crates/agent-tools/src/command.rs:59` (`classify`), `CommandClass::{SideEffectFree, Argv, Opaque}` |
| Aucune mémorisation d'approbation | `crates/agent-tools/src/permission.rs:189` (`ApprovalKey`), `:222` (`ApprovalMemory`), `:269` (`clear`) |
| Approbation binaire sans portée session | dialogue à portée session + `/approvals` (`crates/agent-tui/src/state.rs:76`) |
| `/status`, `/usage`, `/diff`, `/compact` absents | `crates/agent-tui/src/state.rs:60-85` : 17 commandes contre 12 |
| Aucune compaction manuelle | `crates/agent-cli/src/interactive.rs:1219` |
| **Aucun outil MCP appelable par le modèle** | `crates/agent-mcp/src/call.rs:83` (`call`), `tool.rs:195` (`dyn_tools`), `crates/agent-cli/src/main.rs:1020` (`register_dyn`) |
| Pas de namespacing des noms d'outils MCP | `crates/agent-mcp/src/tool.rs:279` (`qualified_name`, `mcp__<srv>__<tool>`, raccourcissement déterministe sous 64 octets) |
| Pas de politique d'approbation MCP | tout appel demande par défaut, résultat systématiquement untrusted |
| Pas de trust par serveur | `crates/agent-cli/src/interactive.rs:1589` (`/mcp <server> trust`), blocage avant spawn `:1578` |
| Schémas MCP non contraints | `crates/agent-mcp/src/tool.rs:358` (`strict_input_schema`) |
| Découverte MCP derrière un drapeau expérimental | `PYXIS_EXPERIMENTAL_MCP_CONNECT` retiré du code |
| Sélection de skill insérant du texte littéral | `crates/agent-cli/src/skills.rs:288` (`instructions`), `:275` (`invocation`) |
| Aucune lecture de `SKILL.md` | `crates/agent-cli/src/skills.rs:132` (lecteur de frontmatter restreint, sans dépendance YAML) |
| Skills non décrites au modèle | `crates/agent-cli/src/skills.rs:241` (`catalog_block`, budget en octets) |
| **Aucun moteur de hooks** | `crates/agent-tools/src/hooks.rs` (contrat Claude Code, veto restrictif, échec = refus) |
| Aucun tracing structuré, aucun panic hook | `crates/agent-cli/src/observability.rs:134` (`install_tracing`), `:170` (`install_panic_hook`) |
| `eprintln!` de sonde depuis le cœur | émission par la façade `tracing` uniquement |

**Fermetures partielles**, à ne pas compter comme acquises :

- *Surface de commandes* : 17 contre 55 côté Codex (`codex-rs/tui/src/slash_command.rs`,
  décompte exact des variantes de l'énumération). Manquent notamment `/init`, `/fork`, `/review`,
  `/copy`, `/plan`, `/hooks`, `/logout`, `/theme`, `/title`.
- *Execpolicy* : `classify` est une liste d'allow codée en dur, sans fichier de règles, sans
  `forbidden`, sans amendement. Codex a un langage dédié et un `codex execpolicy check`.
- *Hooks* : 2 événements (`hooks.rs:49-50`) contre 11 (`codex-rs/hooks/src/lib.rs:20`). Manquent
  `PermissionRequest`, `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `Stop`, `PreCompact`,
  `PostCompact`, `SubagentStart`, `SubagentStop`. Déclaration globale uniquement, ce qui est un
  choix de sécurité assumé mais empêche tout hook de projet, même en dépôt de confiance.
- *Skills* : une seule racine (`~/.agents/skills`, `crates/agent-cli/src/main.rs:818`), aucun scope
  projet ni système, `scripts/`, `references/` et `assets/` de la spécification non supportés. Codex
  a un chargeur multi-racines, des skills système embarquées et un chargeur distant
  (`codex-rs/core-skills/src/{root_loader,system,remote}.rs`).

---

## Re-scoring par dimension

Échelle : `none` < `minimal` < `partial` < `substantial` < `full`.

| Dimension | 07-24 | 07-25 | 07-27 | Restants (report) |
|---|---|---|---|---|
| MCP | minimal | minimal | **substantial** | 9 |
| Extensibilité (skills, hooks, plugins) | minimal | minimal | **substantial** | 4 |
| Sandbox et approbations | partial | partial | **substantial** | 10 |
| Observabilité et assurance qualité | minimal | partial | **substantial** | 3 |
| Boucle agentique et protocole d'événements | partial | substantial | substantial | 7 |
| Angles morts (critique de complétude) | partial | substantial | substantial | 5 |
| Modèles, providers, authentification | partial | partial | partial | 3 |
| Contexte projet injecté et prompt système | partial | partial | partial | 5 |
| Persistance et gestion du contexte | partial | partial | partial | 4 |
| Suite d'outils exposée au modèle | partial | partial | partial | 6 |
| Interface terminal | partial | partial | partial | 14 |
| Configuration, profils, précédence | minimal | partial | partial | 9 |
| Modes non interactifs et intégration | minimal | partial | partial | 3 |

Quatre dimensions montent, aucune ne descend, aucune n'est plus `minimal`. **Le plancher du dépôt
est désormais `partial` partout.** Total résiduel ≈ 82 écarts, dont ~40 relèvent du bloc 3
ci-dessus, donc hors périmètre défendable.

---

## Écarts structurants restants, par ordre de valeur

### 1. [Configuration] Aucune sélection de mode de sandbox ni de politique d'approbation

`majeur` · `absent` · effort `M`

**Impact.** C'est le plus gros écart restant qui se paie à chaque lancement. Pyxis a un seul
comportement de confinement, réglable seulement par `--no-sandbox` (tout ou rien) et par
`writable_roots` en configuration globale. Il n'y a pas d'équivalent de `read-only` pour une phase
de lecture, ni de `danger-full-access` explicite et journalisé.

**Pyxis.** `crates/agent-cli/src/main.rs:115` (`parse_args_from`) : 12 drapeaux, aucun
`--sandbox-mode`, aucun `--permission-mode`, aucun `-c key=value`. Le mode de permission n'est
réglable qu'en configuration globale ou par `/permissions` en session.

**Codex.** `codex-rs/protocol/src/config_types.rs:86` (`SandboxMode` : `read-only`,
`workspace-write`, `danger-full-access`) et `codex-rs/protocol/src/protocol.rs:908`
(`AskForApproval` : `untrusted`, `on-request`, `Granular(..)` par catégorie).

### 2. [Configuration] Aucun profil, aucune surcharge ponctuelle, aucune référence documentée

`majeur` · `absent` · effort `M`

**Impact.** Changer de modèle ou d'effort pour un run demande d'éditer un fichier global. Les 10
clés connues (`crates/agent-cli/src/settings.rs:36`) ne sont décrites que dans
`docs/CURRENT_STATUS.md`, en prose : il n'existe aucune page de référence de la configuration, ce
qui est le premier document que cherche un utilisateur nouveau.

**Codex.** Profils nommés (`<name>.config.toml`, `config_types.rs:98`), surcharges `-c`, couche
managée `requirements.toml`, et une référence de configuration publiée.

### 3. [Sandbox] Pas d'escalade après un échec imputé au sandbox

`majeur` · `absent` · effort `M`

**Impact.** Quand une commande échoue parce que le confinement l'a bloquée, l'agent voit une erreur
générique et boucle souvent sur des variantes de la même commande. Le signal existe côté noyau, il
n'est pas remonté comme une cause distincte, et aucune surface ne propose de ré-exécuter avec un
périmètre élargi pour ce seul appel.

**Codex.** `codex-rs/shell-escalation/`.

### 4. [Outils] Trois familles d'outils que les modèles `*-codex` attendent

`majeur` · `absent` · effort `L`

**Impact.** Les modèles fine-tunés Codex sont entraînés sur des noms et des schémas précis. Pyxis
expose 6 outils intégrés (`crates/agent-cli/src/main.rs:1013-1018`) contre une vingtaine de familles
côté Codex (`codex-rs/core/src/tools/handlers/`). Trois manques ont un effet mesurable sur le
comportement, pas seulement sur la couverture :

- `update_plan` (`handlers/plan.rs`) : sans lui, aucune structuration de tâche longue n'est visible.
- `apply_patch` (`handlers/apply_patch.rs` + grammaire `apply_patch.lark`) : format d'édition natif
  des modèles Codex ; Pyxis passe par `edit` avec appariement flou en quatre passes.
- `view_image` (`handlers/view_image.rs`) : `ContentBlock::Image` existe dans le cœur
  (`crates/agent-core/src/message.rs:109`) et est correctement retiré à la compaction, mais aucun
  outil ni aucune entrée utilisateur ne produit jamais d'image. La capacité est câblée à vide.

Le reste (`unified_exec` avec PTY et stdin, `web_search`, `request_user_input`, sous-agents) relève
du bloc 3 ou d'un choix produit à part entière.

### 5. [Non interactif] Le mode `-p` n'est pas scriptable de bout en bout

`moyen` · `absent` · effort `S`

**Impact.** Pas de lecture du prompt sur stdin (donc pas de `cat prompt.md | pyxis -p`), pas de
`--output-schema` pour contraindre la sortie à un JSON Schema, pas de `--output-last-message` pour
récupérer le seul message final. Le mode headless écrit en outre toujours un fichier de session dans
le workspace, sans équivalent de `--ephemeral`.

**Codex.** `codex-rs/exec/src/cli.rs:53` (`--output-schema`), `:74` (`--output-last-message`),
`:31` (`--ephemeral`), `:35` (`--ignore-user-config`).

### 6. [MCP] Transport stdio uniquement, pas de gestion hors session

`moyen` · `absent` · effort `L`

**Pyxis.** `crates/agent-mcp/src/lib.rs:8` documente explicitement le report des transports HTTP,
des ressources, de l'élicitation et des notifications de progression. Les outils sont enregistrés au
démarrage seulement : une connexion en cours de session change le cycle de vie, pas ce que le modèle
peut appeler. Aucune sous-commande CLI de gestion des serveurs.

**Codex.** `codex-rs/rmcp-client/src/http_client_adapter.rs` (Streamable HTTP + SSE), OAuth par
serveur, `codex mcp` en sous-commande (`codex-rs/cli/src/main.rs:138`).

### 7. [Providers] Aucune révocation, deux paramètres de génération figés

`moyen` · `absent` · effort `S`

**Pyxis.** L'invalidation de crédential est purement locale
(`crates/agent-provider/src/credential.rs:88`) : aucun appel de révocation côté serveur, donc un
`logout` laisse le refresh token vivant. `reasoning.summary` est figé à `"auto"` et `text.verbosity`
à `"low"` (`crates/agent-provider/src/chatgpt_request.rs`), sans clé de configuration.

### 8. [Interface terminal] Le confort d'édition et la restitution restent en retrait

`moyen` · `absent` · effort `M`

Raccourcis limités à Ctrl+A/E/U/W, pas de recherche inverse d'historique, pas d'ouverture dans
`$EDITOR`, pas de mode transcript, pas de reflow du scrollback au redimensionnement, aucune
notification de fin de tour, titre de fenêtre jamais mis à jour (aucune séquence OSC dans le dépôt),
pas de retour arrière sur un message envoyé.

### 9. [Réseau] Allow-list en égalité stricte

`moyen` · `divergent` · effort `S`

`crates/agent-sandbox/src/proxy.rs:30` compare l'hôte par égalité exacte : `--allow github.com`
n'autorise pas `api.github.com`. Aucun sous-domaine, aucun motif. Sur un usage réel, cela pousse
l'utilisateur vers `--no-sandbox`, ce qui est le pire des résultats pour une défense.

---

## Ce que Pyxis fait mieux que Codex

Inchangé et confirmé à `0c1cf17` :

- Taint untrusted propagé au sens OWASP LLM01 (`crates/agent-tools/src/taint.rs`), forçant une
  approbation sur toute action destructrice ou réseau qui suit une lecture de contenu non fiable,
  quel que soit le mode. R4 étend cette propriété aux résultats MCP : tout retour de serveur est
  untrusted par construction.
- Hooks fail-closed par conception : un hook ne peut que **resserrer**, `allow` se lit « pas
  d'objection » et jamais comme un contournement, tout échec refuse, un `deny` prime sur
  `BypassPermissions`. Codex laisse un hook `allow` court-circuiter une confirmation.
- Machine à transitions pure validée par un `Accumulator` qui fait échouer fail-closed tout provider
  hors contrat (`crates/agent-core/src/transition.rs`).
- Annulation coopérative avec réconciliation du transcript (`crates/agent-core/src/cancel.rs:66`,
  `biased`) : un outil terminé au moment du signal conserve son résultat réel.
- Diff agrégé du tour comme événement machine de première classe (`AgentEvent::TurnDiff`).
- Cœur headless strict : aucune bibliothèque n'écrit sur une sortie de processus, un seul
  installateur de subscriber, dans le binaire.

---

## Conséquences pour la suite

1. **Reformuler l'objectif.** Remplacer « parité harness Codex CLI » par « parité sur le harness
   d'agent interactif local », avec le bloc 3 (app-server, cloud, sous-agents, plugins, mémoires,
   review, OTLP, Windows) déclaré hors périmètre par écrit. Sans cette décision, tout PRD de suite
   hérite d'un dénominateur de 23 pour 1.
2. **Lot `M` le plus rentable : la surface de configuration.** `--sandbox-mode`, `--permission-mode`,
   `-c key=value`, plus une page de référence des clés. Ferme la moitié du bloc 1 et supprime la
   principale friction de lancement.
3. **Lot `S` : rendre `-p` scriptable.** stdin, `--output-last-message`, `--ephemeral`. Trois
   drapeaux, valeur immédiate pour l'intégration dans un pipeline.
4. **Lot `S` : l'allow-list réseau par suffixe de domaine.** Un écart d'une ligne de logique qui
   décide aujourd'hui si l'utilisateur garde le sandbox ou le désactive.
5. **Lot `L` à arbitrer : `apply_patch` et `update_plan`.** Ce sont les deux seuls outils manquants
   dont l'absence change le comportement du modèle plutôt que la couverture fonctionnelle. À
   trancher explicitement, pas à laisser dans un backlog.
6. **EP-006 de `tasks/prd-harness-parity.md` est à réviser.** Son périmètre (extensibilité) a été
   livré par `tasks/prd-harness-capabilities.md` pendant qu'il restait `TODO`. Le fichier de statut
   décrit un état faux.
