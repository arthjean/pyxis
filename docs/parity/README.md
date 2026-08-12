# Baseline de parité Codex

`codex-baseline-matrix.json` est la **source normative** de la parité Codex de
Pyxis. Elle est générée, pas rédigée : chaque ligne est extraite du clone Codex
en lecture seule figé au commit
`fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`, puis empreintée (SHA-256).

Les audits `docs/parity-audit-*.md` et
`docs/parity-strategy-2026-07-27.md` restent du **contexte historique** :
ils décrivent des instantanés antérieurs et n'arbitrent plus une divergence.

## Commandes

```bash
cargo run -p agent-parity -- check      # échoue avec un diff lisible si la matrice a dérivé
cargo run -p agent-parity -- generate   # régénère la matrice après une décision de baseline
cargo run -p agent-parity -- drift      # compare le HEAD amont à la baseline, sans rien modifier
```

`drift` est le guetteur amont : c'est le SEUL mode qui
accepte un clone hors du commit épinglé, parce que son travail est justement de
dire ce qui a bougé. Il n'écrit rien, ni dans Pyxis ni dans Codex, et il ignore
le numéro de commit dans sa comparaison : ce qui compte est de savoir si le
CONTRAT a bougé, pas si le clone a avancé. Il sort non nul quand une ligne de
contrat diffère, pour qu'un run planifié signale au lieu de défiler.

La recette de preuve offline complète, domaine par domaine, est dans
[`offline-suite.md`](offline-suite.md).

Le clone est résolu par `$PYXIS_CODEX_BASELINE`, sinon `/home/arthur/dev/codex`.
Un clone absent, non versionné ou sur un autre commit fait échouer le
vérificateur avec le chemin attendu et le commit attendu, avant toute
extraction. Le clone n'est jamais écrit : seuls `git rev-parse HEAD` et des
lectures de fichiers sont exécutés.

Changer de baseline est une décision explicite : mettre à jour
`BASELINE_COMMIT` dans `crates/agent-parity/src/lib.rs`, régénérer, et relire le
diff. Aucun suivi automatique du HEAD amont.

## Contenu de la matrice

| Section | Extraite de | Ce qu'elle fige |
|---|---|---|
| `models` | `codex-rs/models-manager/models.json` | slug, visibilité, `tool_mode`, Responses Lite, version multi-agent. Les capacités absentes sont explicitées avec la valeur que Codex leur donne (`direct`, `disabled`, `false`), jamais laissées vides. |
| `tool_modes` | `codex-rs/protocol/src/openai_models.rs` | `direct`, `code_mode`, `code_mode_only`. |
| `multi_agent_versions` | `codex-rs/protocol/src/protocol.rs` | `disabled`, `v1`, `v2`. |
| `multi_agent` | `handlers/multi_agents{,_v2}/`, constantes de namespace | namespace et outils réellement exposés par version. |
| `app_server_methods` | `app-server-protocol/src/protocol/common.rs` | méthode JSON-RPC, requête associée, drapeau expérimental. |

`fingerprint` couvre toutes les sections, lui-même exclu. Une extraction qui ne
retrouve plus la forme attendue d'une source échoue en nommant la source : une
section vide serait une fausse preuve de parité.
