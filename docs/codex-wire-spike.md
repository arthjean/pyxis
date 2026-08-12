# Spike wire Codex — protocole & verdict

Statut : **verdict live historique consigné (2026-06-17), AC2 à revalider après
rename Pyxis.** Le câblage qui rend le spike exécutable et réversible est en place.
Le run réseau réel ci-dessous prouvait le cycle multi-tour et l'ancien originator
du projet. Depuis le rename, `originator=pyxis` doit être retesté. AC3 a révélé
une sous-estimation structurelle du `HeuristicCounter` (omission system prompt +
tools), désormais corrigée par l'estimation statique `system + tools`.

## Ce que le code livre déjà

- **`originator` basculable sans recompiler.** L'en-tête d'inférence est résolu par
  `agent_auth::oauth::openai_chatgpt::originator()` qui lit `PYXIS_ORIGINATOR`
  (défaut `pyxis`). Le flow OAuth (`build_authorize_url`) garde `pyxis` : on ne touche
  pas au chemin auth validé en live.
- **Fallback documenté et testé.** `originator_for(pyxis_accepted: bool)` renvoie
  `pyxis` si accepté, sinon `codex_cli_rs` (l'identité du Codex CLI officiel OSS, déjà
  whitelistée). Couvert par `originator_fallback_selection`.
- **Point post-rename.** Le verdict live `originator` doit être refait pour Pyxis.
  Tant que ce n'est pas fait, garder le fallback `codex_cli_rs` comme plan de sortie.
- **AC3 corrigé côté code.** `estimate_static_input(...)` compte le system prompt,
  le contexte statique et les schémas d'outils ; `agent.rs` ajoute
  `static_input_tokens` aux projections pré-tour et mid-turn.
- **Wire durci autour du spike** : connect timeout 20 s, idle timeout
  60 s, 429 terminaux non retryés, `Retry-After` honoré, dernier tour assistant persisté.

## Protocole du run live (à exécuter par l'utilisateur)

Le run consomme du quota d'abonnement réel : action outward-facing, déclenchée
manuellement, jamais par l'agent.

```bash
# 1. happy path : forcer ≥ 2 tours modèle avec un outil entre les deux.
pyxis -p "lis le fichier Cargo.toml puis résume en une phrase ce qu'est ce workspace" --yes

# 2. inspecter le transcript JSONL produit (dernier fichier de session) :
ls -t .pyxis/sessions/*.jsonl | head -1
```

### AC1 — cycle multi-tour avec outil
Vérifier dans le JSONL la séquence `user → assistant(tool_use) → tool(tool_result) →
assistant(text)` et l'absence de `400`. **À consigner :** OK / KO (+ corps d'erreur si KO).

### AC2 — originator
- Run par défaut (`originator=pyxis`). Si une requête revient en `400`/`403` évoquant
  l'`originator`, relancer avec le fallback :
  ```bash
  PYXIS_ORIGINATOR=codex_cli_rs pyxis -p "…" --yes
  ```
- **À consigner :** `pyxis` accepté (oui/non). Si non, garder `codex_cli_rs` comme défaut
  (ajuster le défaut de `originator()` ou exporter la variable en session).

### AC3 — écart tokenizer
Relever l'`usage.input_tokens` réel renvoyé par le backend et le comparer à l'estimation
locale `HeuristicCounter` sur le même contexte. **À consigner :** ratio réel/estimé, pour
calibrer la marge de compaction (un `HeuristicCounter` qui sous-estime de X % impose une
marge de sécurité ≥ X % sur le seuil d'auto-compaction).

## Verdict du run live historique (2026-06-17, ancien nom)

Run exécuté avec `--no-sandbox` : le Landlock de Pyxis échoue en `EACCES` dans
l'environnement d'automatisation imbriqué (sandbox dans sandbox). Orthogonal au
spike wire (il valide le canal Responses, pas le confinement FS) ; prompt en
lecture seule. Le sandbox reste actif en usage normal (`pyxis` sans `--no-sandbox`).

```
Date du run : 2026-06-17  (binaire durci, modèle gpt-5.5 par défaut)
AC1 cycle multi-tour : OK
  transcript .pyxis/sessions/1781703238804.jsonl :
  user → assistant(text + tool_use:read) → tool(tool_result) → assistant(text), aucun 400.
AC2 ancien originator : ACCEPTÉ
  run par défaut sous l'ancien nom du projet, aucun 400/403 sur 2 tours + 1 outil.
  Après rename, originator=pyxis reste À REVALIDER.
  Fallback codex_cli_rs disponible (PYXIS_ORIGINATOR + originator_for).
AC3 input_tokens réel vs estimé (sonde PYXIS_DEBUG_USAGE) :
  tour 1 : réel=1389  estimé_local=58   ratio=23.9×
  tour 2 : réel=2475  estimé_local=827  ratio=3.0×
Décision marge compaction :
  Cause de l'écart : `estimate_input(messages, counter)` ne compte QUE les messages
  (+ contexte éphémère AGENTS.md/env). Il OMET le system prompt long
  (~300 lignes) ET les schémas des 6 outils, qui dominent l'input des premiers tours.
  L'écart n'est donc PAS un drift par-token (quelques %) mais une omission structurelle
  (3× à 24× selon le ratio messages/scaffold).
  → Le seuil de compaction RÉACTIF reste SÛR : il s'ancre sur l'`usage` backend réel
    (`budget.observe_usage`), pas sur l'estimation.
  → DANGER ciblé : les projections PRÉ-tour (budget kill-switch) et MidTurn
    (`force_compact`, agent.rs:462) reposent sur `estimate_input` → trop
    optimistes de 1300+ tokens sur les tours froids → compaction/arrêt déclenchés
    trop tard.
  Suite livrée :
    `estimate_static_input(...)` compte le system prompt, le contexte statique et les
    schémas d'outils. `agent.rs` ajoute `static_input_tokens` aux projections pré-tour
    et mid-turn. Le ratio historique ci-dessus reste une preuve du bug initial, pas une
    action ouverte.
```

## Reasoning replay (P2) — validation live requise

Le replay des reasoning items chiffrés est **livré mais désactivé par défaut**
(`OpenAiChatGptProvider::with_reasoning_replay(false)`, jamais activé dans la CLI).
Chemin plat inchangé : OFF → le mapper n'émet pas de `EncryptedReasoning`, donc les
messages n'en contiennent pas et `build_input` reste identique au MVP. Couvert par
tests unitaires : capture (ON/OFF), réémission `rs` avant `fc`, orphelin sauté, drop
à la compaction, round-trip serde.

**À valider en live AVANT d'activer** (risque 400 « orphaned/duplicate reasoning
item ») : activer via `.with_reasoning_replay(true)`, forcer un tour reasoning+outil,
inspecter qu'aucun `400` ne survient et que la paire `rs`/`fc` est acceptée.

**Prérequis à câbler AVANT d'activer le replay par défaut** (latents tant que OFF,
relevés en audit) :
- **AC4 — drop au changement de modèle** : vider les `EncryptedReasoning` du
  transcript au switch de slug (`/models`), sinon le reasoning d'un modèle précédent
  est réinjecté (risque 400).
- **Données au repos (OWASP LLM06)** : `agent-session` filtre les
  `EncryptedReasoning` avant écriture JSONL (`sync` et checkpoint). Le replay reste
  disponible en mémoire pendant la session active, mais ne survit pas au `/resume`.
- **Borne mémoire** : poser un cap par item (ex. 64 Ko) et par tour (ex. 16 items) sur
  l'accumulation des reasoning items (`Accumulator.reasonings`).
