# Audit de parité harness : Pyxis vs Codex CLI — passe du 2026-07-25

> **Statut : contexte historique, non normatif.** La cible de parité est
> `docs/parity/codex-baseline-matrix.json`, générée depuis le clone Codex figé
> au commit `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`
> (`cargo run -p agent-parity -- check`). Ce document décrit un instantané
> antérieur : il informe, il n'arbitre plus un écart. L'état réellement livré
> est dans `docs/CURRENT_STATUS.md`, et la preuve qui l'accompagne dans
> `docs/parity/offline-suite.md`.

Seconde passe, en lecture seule. Le constat de référence reste
[`docs/parity/audits/parity-audit-2026-07-24.md`](./parity-audit-2026-07-24.md) (2026-07-24, 221 écarts
retenus après réfutation adversariale), volontairement laissé intact : il sert de baseline de
comparaison. Ce document mesure le delta.

**Ce qui a changé entre les deux passes.** Les releases R1, R2 et R3 du chantier de parité ont
atterri en une journée ; seule l'extensibilité utilisateur restait à livrer. Chaque affirmation ci-dessous a été revérifiée sur le code à
`4bf6b85`, pas reprise de la passe précédente.

**Références.** Pyxis : ~40k lignes Rust, 10 crates, 693 tests, 33 snapshots. Codex CLI :
`/home/arthur/dev/codex` à `322d5b96cf`, ~100 crates.

---

## Verdict

**Le classement a changé de nature, pas seulement de contenu.**

La passe du 24 était dominée par trois problèmes qui n'étaient pas des écarts de parité : un bug
d'intégrité de session, un composer inutilisable, et un système de suivi qui déclarait fait ce qui ne
l'était pas. Les trois sont fermés et, plus important, **vérifiables mécaniquement** : la CI existe
et est verte (`.github/workflows/ci.yml`, runs `30170968407`, `30158706200`, `30157334639`, le plus
récent en 1 min 17 s).

Ce qui reste tient sur un seul axe : **ce que l'agent sait faire**. Les quatre dimensions à zéro
fermeture (MCP 14 écarts, extensibilité 8, suite d'outils 7, contexte projet 6) décrivent toutes la
même chose sous quatre angles. Pyxis expose six outils au modèle contre une vingtaine de familles
côté Codex, ne lit aucun `SKILL.md`, n'a aucun moteur de hooks, et ne peut appeler aucun outil MCP.

Trois écarts d'un genre nouveau apparaissent, créés ou révélés par R3 : la donnée existe désormais
dans le contrat mais personne ne la consomme. Ils sont détaillés en section « Écarts révélés par R1
à R3 » et représentent le meilleur rapport valeur/effort du dépôt aujourd'hui.

---

## Fermé depuis la passe du 24

23 écarts pertinents fermés sur les 125 énumérés (18 %). Vérifications :

| Écart (passe du 24) | Preuve de fermeture |
|---|---|
| `function_call` orphelin persisté et réémis | `crates/agent-core/src/agent.rs:302` (`reconcile_interrupted_calls`), `crates/agent-provider/src/chatgpt_request.rs:236` (résultat synthétique à la construction) |
| Aucun mécanisme d'annulation dans le cœur | `crates/agent-core/src/cancel.rs` (`CancelToken` sur `tokio::sync::watch`), frontière d'arrêt unique en `agent.rs:431` |
| `AgentEvent::Interrupted` fabriqué par le client | `crates/agent-core/src/agent.rs:1057` : émis par la boucle |
| Mode headless sans flux d'événements | `crates/agent-cli/src/jsonl.rs`, schéma versionné dans `docs/EVENT_SCHEMA.md` |
| Identifiant de session jamais restitué en headless | ligne `run_summary` du flux JSONL |
| Mode headless muet pendant le run | idem : un événement par ligne, vidé à l'écriture |
| Racines writables non configurables, `/tmp` non writable | `crates/agent-sandbox/src/fs.rs:102` (`resolve_writable_roots`), `fs.rs:206` |
| Pas de sous-chemins protégés | `crates/agent-tools/src/path.rs:29` (`PROTECTED_SUBPATHS`), `path.rs:84` (résolution des liens symboliques) |
| Aucun fichier de configuration déclaratif | `crates/agent-cli/src/settings.rs:20` (`config.toml` projet) |
| Parseur `settings.toml` maison | `toml` en dépendance (`Cargo.toml:77`), `parse_tomlish_string` retiré |
| Aucune précédence multi-couches | défauts < `~/.pyxis/settings.toml` < `<ws>/.pyxis/config.toml` < env < CLI (`settings.rs:95`) |
| Aucune validation ni diagnostic de configuration | `settings.rs:35` (`KNOWN_KEYS`), clé inconnue signalée sans échec |
| Préférences ignorées en headless | les deux modes lisent la configuration |
| Composer mono-ligne | `crates/agent-tui/src/state.rs:706` (`NEWLINE_MODIFIERS`), `state.rs:974` (`insert_newline`) |
| Saisie longue tronquée | `crates/agent-tui/src/composer.rs` (repli, hauteur dynamique, défilement) |
| Gros collage inséré brut | résumé `[collage : N lignes]`, snapshot `composer_large_paste.snap` |
| Zéro snapshot de rendu | 33 fichiers sous `crates/agent-tui/tests/snapshots/` |
| Aucune CI | `.github/workflows/ci.yml`, trois runs verts consécutifs |
| Aucun harness d'intégration | `crates/agent-cli/tests/e2e_headless.rs` (29 fonctions, rejeu SSE réel sur `tests/fixtures/*.sse`) |
| Aucun suivi du diff agrégé du tour | `crates/agent-tools/src/turn_diff.rs`, `AgentEvent::TurnDiff` |
| Sortie shell rendue à la complétion seulement | `AgentEvent::ToolOutputDelta`, `crates/agent-tools/src/bash.rs:19-21` (seuils de flush) |
| `bash` en `sh -c` alors que le contexte annonce `$SHELL` | `crates/agent-tools/src/shell.rs` |
| Divergence entre fichiers de statut et code | section « Status Reconciliation » de `docs/CURRENT_STATUS.md` |

**Fermetures partielles**, à ne pas compter comme acquises :

- *Mode de permission non sélectionnable* : réglable par `permission_mode` en configuration globale
  (`settings.rs:21`), toujours pas par drapeau CLI. `parse_args_from` (`crates/agent-cli/src/main.rs:112`)
  ne connaît que `--yes` et `--no-sandbox`.
- *Usage tokens non exposé aux clients* : `AgentEvent::ModelTurn` porte les compteurs réels, mais
  aucun client ne les rend. Voir ci-dessous.
- *Impossible de dumper ce que le modèle voit* : `PYXIS_DEBUG_TRANSCRIPT` existe
  (`chatgpt_request.rs:105`) mais n'écrit que les anomalies réparées, pas la requête complète.
- *Politique de sandbox non configurable* : `writable_roots` seulement. Aucun mode
  read-only / workspace-write / danger-full-access.

---

## Re-scoring par dimension

Échelle : `none` < `minimal` < `partial` < `substantial` < `full`.

| Dimension | 2026-07-24 | 2026-07-25 | Fermés | Restants |
|---|---|---|---|---|
| Boucle agentique et protocole d'événements | partial | **substantial** | 4/12 | 8 |
| Angles morts (critique de complétude) | partial | **substantial** | 3/8 | 5 |
| Configuration, profils, précédence | minimal | **partial** | 5/14 | 9 |
| Modes non interactifs et intégration | minimal | **partial** | 3/6 | 3 |
| Observabilité et assurance qualité | minimal | **partial** | 3/9 | 6 |
| Interface terminal | partial | partial | 3/18 | 15 |
| Sandbox et approbations | partial | partial | 2/14 | 12 |
| Suite d'outils exposée au modèle | partial | partial | 0/7 | 7 |
| Contexte projet injecté et prompt système | partial | partial | 0/6 | 6 |
| Persistance et gestion du contexte | partial | partial | 0/5 | 5 |
| Modèles, providers, authentification | partial | partial | 0/4 | 4 |
| Extensibilité (skills, hooks, plugins, commandes) | minimal | minimal | 0/8 | 8 |
| MCP | minimal | minimal | 0/14 | 14 |

Cinq dimensions montent d'un cran. Aucune ne descend. Les deux dimensions restées `minimal` sont
exactement le périmètre de l'extensibilité utilisateur.

---

## Écarts révélés par R1 à R3

Ces trois écarts n'existaient pas ou n'étaient pas visibles dans la passe du 24. Ils partagent la
même forme : **la donnée est produite, le consommateur manque**. C'est le meilleur rapport
valeur/effort du dépôt.

### La TUI jette les compteurs de tokens réels qu'elle reçoit

`majeur` · `absent` · effort `S`

**Impact.** Le contrat client fait entrer l'usage réel du backend, mais l'utilisateur
voit toujours une estimation `caractères / 4` connue pour dériver d'un facteur 3 à 24. La donnée
fiable transite à chaque tour et est jetée.

**Pyxis.** `crates/agent-tui/src/app_event.rs:515` : `AgentEvent::ModelTurn(_) => Vec::new()`.
`crates/agent-tui/src/state.rs:1121` : `AgentEvent::ModelTurn(_) => {}`. L'affichage repose sur
`turn_chars` (`state.rs:638`, incrémenté en `state.rs:1060` et `1075`).

**Codex.** `codex-rs/protocol/src/protocol.rs:2138` (`TokenCountEvent`) est émis à chaque `Completed`
et alimente directement le statut de session.

### La jauge de contexte est dessinée mais n'est jamais alimentée

`majeur` · `absent` · effort `S`

**Impact.** `render.rs:1581` dessine un indicateur de remplissage du contexte conditionné à
`state.context_pct`. Ce champ n'est assigné nulle part hors `examples/` et `tests/` : en usage réel
il vaut toujours `None` et la jauge n'apparaît jamais. Le snapshot `context_indicator.snap` la
capture parce que le test la force à `Some(38)`.

**Pyxis.** `crates/agent-tui/src/state.rs:604` (déclaration), `state.rs:741` (initialisé à `None`).
Grep `context_pct` sur `crates/` : seules occurrences productrices dans
`crates/agent-tui/examples/*.rs` et `crates/agent-tui/tests/render_snapshots.rs:423,433`.

### `context_window` est servi par le backend et jeté au parsing

`majeur` · `absent` · effort `S`

**Impact.** C'est la pièce manquante des deux écarts précédents : sans fenêtre de contexte du modèle
actif, aucun pourcentage n'est calculable. Le backend la renvoie déjà dans le catalogue `/models`,
et le désérialiseur ne la déclare pas.

**Pyxis.** `crates/agent-provider/src/models.rs:84` : l'échantillon de test du catalogue contient
`"context_window":272000`. `models.rs:28` (`struct WireModel`) et `models.rs:13`
(`struct CatalogModel`) ne portent aucun champ correspondant. La valeur est silencieusement
abandonnée par la tolérance aux champs inconnus.

**Codex.** `codex-rs/protocol/src/protocol.rs:2090` (`TokenUsageInfo { model_context_window, .. }`)
et `protocol.rs:2244` (`percent_of_context_window_remaining`).

> Ces trois écarts forment une seule chaîne : ajouter un champ au désérialiseur, propager la fenêtre
> jusqu'au client, consommer `ModelTurn` dans la TUI. Un seul lot, effort `S`, ferme trois écarts
> pertinents et supprime la seule métrique du produit connue pour être fausse.

---

## Écarts structurants restants

### [MCP] Aucun outil MCP n'est appelable par le modèle

`bloquant` · `absent` · effort `XL`

Inchangé depuis la passe du 24, et c'est désormais le plus gros bloc isolé du dépôt (14 écarts).

**Pyxis.** `grep -rn 'call_tool\|CallTool\|tools/call' crates/ --include='*.rs'` : aucun résultat.
`crates/agent-mcp/src/client.rs` n'expose que `connect` (l. 50), `connect_hardened` (l. 57),
`list_tools` (l. 95) et `cancel` (l. 130). Toute connexion est en outre verrouillée derrière
`PYXIS_EXPERIMENTAL_MCP_CONNECT` (`crates/agent-cli/src/interactive.rs:1439`), donc même la
découverte est inaccessible par défaut.

**Codex.** `codex-rs/core/src/tools/handlers/mcp.rs` implémente le trait outil, `codex-rs/codex-mcp/`
porte le gestionnaire de connexions et l'appel réel.

### [Extensibilité] Sélectionner une skill insère du texte littéral

`majeur` · `divergent` · effort `M`

**Impact.** `/skills` liste les répertoires de `~/.agents/skills` et la sélection écrit `/<nom> `
dans le composer, envoyé tel quel au modèle. Aucune instruction n'est injectée. Une skill installée
pour Codex ou Claude Code ne produit donc rien d'autre qu'un mot dans le prompt.

**Pyxis.** `crates/agent-cli/src/main.rs:623` (`read_skills`) ne lit que des noms de répertoires.
`crates/agent-tui/src/state.rs:1796-1800` : `self.set_input(format!("/{} ", item.id))`, commentaire
« INSERTION (no execution) ». Grep `SKILL.md` sur `crates/` : aucun résultat.

**Codex.** `codex-rs/skills/` et `codex-rs/core-skills/`, plus la commande `/skills` documentée dans
`docs/skills.md`.

### [Sandbox] Aucune classification de risque des commandes shell, aucune mémorisation d'approbation

`majeur` · `absent` · effort `L`

**Impact.** C'est l'écart restant qui se paie le plus souvent : en mode `Default`, **chaque** appel
`bash` déclenche une confirmation, y compris `ls` ou `git status`. Aucune réponse n'est mémorisée,
ni pour la session ni pour un préfixe de commande.

**Pyxis.** `crates/agent-tools/src/bash.rs:94` : `fn permission(&self, _input: &Self::Input, ...)`
ignore son entrée et renvoie une décision fixe. `crates/agent-tools/src/permission.rs:103-160`
(`resolve_permission`) est une fonction pure sans état de session : grep `always|remember|session_approv`
n'y trouve rien.

**Codex.** `codex-rs/execpolicy/` (règles allow/prompt/forbidden par commande) et
`codex-rs/shell-escalation/`.

### [Persistance] La compaction pleine ne conserve que le dernier message utilisateur

`majeur` · `divergent` · effort `M`

**Pyxis.** `crates/agent-core/src/compaction.rs:104-113` : le transcript est remplacé par
`[summary] + dernier message utilisateur`, assertion explicite dans le test l. 426. Aucune
compaction manuelle n'est exposée : grep `"/compact"` sur `crates/` ne retourne rien.

### [Interface terminal] Surface de commandes étroite

`moyen` · `absent` · effort `M`

12 commandes (`crates/agent-tui/src/state.rs:61-76`) contre 57 dans
`codex-rs/tui/src/slash_command.rs`. Manquent notamment `/status`, `/usage`, `/diff`, `/compact`,
`/fork`, `/review`, `/init`, `/copy`. Les données de `/status`, `/usage` et `/diff` existent déjà
côté Pyxis (`ModelTurn`, `TurnDiff`, configuration résolue) : seule la surface manque.

### [Observabilité] Aucun tracing structuré, aucun panic hook

`moyen` · `absent` · effort `M`

**Pyxis.** Aucune dépendance `tracing` dans le workspace (grep sur `Cargo.toml` et
`crates/*/Cargo.toml`). Aucun `std::panic::set_hook` en dehors du harness de test
(`crates/agent-tui/tests/harness/mod.rs:23`) : un panic laisse le terminal en mode raw sans trace
exploitable. La sonde `PYXIS_DEBUG_USAGE` écrit toujours par `eprintln!` depuis le cœur
(`crates/agent-core/src/agent.rs:620`), en tension avec la règle du cœur headless, même si elle est
désactivée par défaut.

### [Providers] Le catalogue et la déconnexion restent incomplets

`moyen` · `absent` · effort `S` à `M`

**Pyxis.** `crates/agent-provider/src/models.rs:13` ignore `context_window` (voir plus haut) et les
instructions de base servies par le catalogue. `reasoning.summary` est figé à `"auto"`
(`chatgpt_request.rs:78`) et `text.verbosity` à `"low"` (`chatgpt_request.rs:50`), sans clé de
configuration. Grep `logout|revoke` sur `crates/agent-auth` et `crates/agent-cli` : aucun résultat,
donc aucune révocation du refresh token côté serveur.

---

## Écarts inchangés, résumé par dimension

Restants non détaillés ci-dessus, tous vérifiés inchangés à `4bf6b85` :

- **Boucle (8)** : rate limits et quotas d'abonnement jamais remontés ; aucun événement pendant le
  backoff de retry ; pas de steering en cours de tour (`interactive.rs:742`, file d'attente vidée
  seulement après un événement terminal) ; reasoning aplati sans distinction summary/raw ; signal
  `end_turn` du backend ignoré ; quota épuisé rendu comme erreur brute.
- **Outils (7)** : aucun accès web, outils MCP non exposés, canal provider limité aux tools
  `function`, pas d'`update_plan`, `bash` one-shot sans PTY ni stdin, noms et schémas divergents de
  ceux sur lesquels les modèles `*-codex` sont entraînés, aucune auto-approbation.
- **Sandbox (12)** : pas de modes de sandbox sélectionnables, pas d'execpolicy, pas d'escalade après
  échec imputé au sandbox, allow-list réseau en égalité stricte, journal des hôtes bloqués écrit
  mais jamais lu, pas de trust de répertoire au premier lancement.
- **Configuration (9)** : découverte du contexte projet codée en dur (`crates/agent-cli/src/context.rs:13,17,20`),
  prompt système non surchargeable, seuils de compaction non configurables, aucune famille de
  configuration TUI, aucune documentation de référence de la configuration (les clés ne sont
  décrites que dans `docs/CURRENT_STATUS.md`).
- **Contexte projet (6)** : sélection du prompt par sous-chaîne de slug (`crates/agent-cli/src/prompt.rs:20`),
  pas de fichier d'instructions utilisateur global, le modèle ignore le mode de permissions actif et
  la portée du sandbox, skills non décrites au modèle, contexte figé au démarrage, aucune surface
  d'inspection.
- **Extensibilité (8)** : aucun moteur de hooks (Codex en déclare 11 événements,
  `codex-rs/hooks/src/lib.rs:20`), une seule racine de skills sans scopes, aucun format de plugin.
- **MCP (14)** : transport stdio uniquement, pas d'OAuth par serveur, pas de namespacing des noms
  d'outils, pas de politique d'approbation, pas de filtrage par serveur, ressources et élicitation
  non supportées, pas de gestion CLI des serveurs.
- **Interface terminal (15)** : raccourcis d'édition limités à Ctrl+A/E/U/W (`state.rs:1939-1952`),
  pas de reflow du scrollback au redimensionnement, mentions `@` sur instantané figé, pas de
  recherche inverse d'historique, approbation binaire, pas de retour arrière sur un message, pas
  d'ouverture dans `$EDITOR`, aucune notification de fin de tour, titre de fenêtre jamais mis à jour.
- **Non interactif (3)** : pas de lecture du prompt sur stdin, pas de sortie contrainte par un JSON
  Schema, le mode headless écrit toujours un fichier de session dans le workspace.

---

## Ce que Pyxis fait mieux que Codex

Inchangé depuis la passe du 24, et renforcé par R1 :

- Garde-fous déterministes explicites : `ExhaustReason` typé (`crates/agent-core/src/transition.rs:35`),
  loop-guard signal-puis-abort (`guardrail.rs:27`).
- Taint untrusted propagé au sens OWASP LLM01 (`crates/agent-tools/src/taint.rs`), forçant une
  approbation sur toute action destructrice ou réseau suivant une lecture de contenu non fiable,
  quel que soit le mode (`permission.rs:158`).
- Machine à transitions pure validée par un `Accumulator` qui fait échouer fail-closed tout provider
  hors contrat (`transition.rs:167-220`).
- Annulation coopérative avec réconciliation du transcript : `CancelToken::guard` poll le futur en
  premier (`cancel.rs:66`, `biased`), donc un outil terminé au moment du signal conserve son résultat
  réel. Codex fait un `handle.abort()` après délai de grâce (`codex-rs/core/src/tasks/mod.rs:867-897`).
- Diff agrégé du tour comme événement machine de première classe : aucun des concurrents consultés
  ne l'expose.

---

## Conséquences pour la suite

1. **La chaîne fenêtre de contexte** (`context_window` → `ModelTurn` → `context_pct`) est un lot
   `S` qui ferme trois écarts et supprime la seule métrique produit connue pour être fausse. À faire
   avant tout le reste.
2. **La friction d'approbation `bash`** est le seul écart restant qui se paie à chaque tour de
   dogfood. Elle n'est couverte par aucun chantier en cours.
3. **L'extensibilité utilisateur reste correctement cadrée** mais sous-dimensionnée pour MCP :
   14 écarts derrière quatre chantiers. Le découpage mérite d'être repris.
4. **Les commandes `/status`, `/usage`, `/diff`, `/compact`** sont des surfaces sur des données déjà
   produites. Effort `S` chacune, valeur immédiate en dogfood.
