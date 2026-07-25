# Schéma d'événements JSONL

Contrat machine du mode headless (`US-017`, `tasks/prd-harness-parity.md`).

```sh
pyxis -p "resume ce dépôt" --output-format json
```

Chaque ligne de la sortie standard est **un objet JSON complet**, vidé dès son
écriture. Aucune ligne n'est reformatée, aucune n'est destinée à un humain. Le
format textuel par défaut (`--output-format text`, ou l'absence du drapeau) reste
strictement ce qu'il était : la réponse finale, rien d'autre.

Les diagnostics (`[config]`, `[sandbox]`, `[diff]`, erreurs de démarrage) partent
sur **stderr** et ne polluent jamais le flux JSONL.

## Forme d'une ligne

```json
{"schema":1,"type":"text","data":"bonjour"}
```

| Champ | Type | Rôle |
|---|---|---|
| `schema` | entier | Version du schéma. `1` aujourd'hui. |
| `type` | chaîne | Discriminant en `snake_case`. |
| `data` | variable | Charge utile, absente pour les événements sans donnée. |

`schema` est incrémenté seulement si une ligne **déjà émise change de forme**.
Ajouter un type d'événement ou un champ optionnel ne casse pas un consommateur qui
ignore ce qu'il ne connaît pas, et n'incrémente donc pas la version. Un
consommateur strict doit refuser une version majeure inconnue et ignorer les
`type` inconnus.

Le vocabulaire est celui de Pyxis, pas celui d'un concurrent : la recherche menée
pour ce PRD n'a trouvé aucun schéma JSONL standardisé entre agents de code, donc
s'aligner sur l'un d'eux aurait imité un choix arbitraire au lieu de documenter le
sien.

## Types d'événements

Les types suivants sont la sérialisation directe d'`agent_core::AgentEvent`, le
contrat que consomment aussi la TUI et le futur client Paneflow.

| `type` | `data` | Signification |
|---|---|---|
| `stream_reset` | — | Le flux en cours est abandonné avant validation (retry). Un consommateur doit jeter les `text` et `reasoning` non encore clos. |
| `text` | chaîne | Fragment de texte assistant. |
| `reasoning` | chaîne | Fragment de raisonnement, si le provider en émet. |
| `tool_call` | `{id, name, input}` | Un outil va s'exécuter. `input` est le JSON d'arguments réassemblé. |
| `tool_output_delta` | `{id, chunk}` | Fragment de sortie d'un outil encore en cours. Informatif : le transcript ne retient que `tool_result`. |
| `tool_result` | `{id, content, is_error, error_kind?, untrusted}` | Résultat d'outil. `untrusted` vaut `true` pour tout contenu externe. |
| `compacted` | `"micro"` \| `"auto"` \| `"reactive"` | Une compaction de contexte a eu lieu. |
| `model_turn` | `{index, input_tokens, output_tokens}` | Un aller-retour modèle vient de finir. `index` est 1-based ; les compteurs sont **cumulés depuis le début du run**, réels quand le provider rapporte son usage, estimés localement sinon. |
| `permission_ask` | `{call_id, tool, reason, taint_forced, input_summary, input, mode}` | Demande d'autorisation. En headless, l'approbateur refuse par défaut. |
| `end_turn` | — | Le tour s'est terminé normalement. |
| `interrupted` | — | Le tour a été interrompu ; le transcript est déjà réconcilié. |
| `exhausted` | détail | Arrêt déterministe : budget, plafond de tours, boucle d'outils. |
| `error` | détail | Erreur d'agent. Émise **avant** la sortie du processus. |


### `run_summary`

Dernière ligne du run. Ce n'est pas un `AgentEvent` : l'identifiant de session et
le code de sortie sont des faits de processus.

```json
{"schema":1,"type":"run_summary","data":{
  "session_id":"20260725-101500-abcd.jsonl",
  "model_turns":3,
  "input_tokens":18422,
  "output_tokens":1290,
  "end":"end_turn",
  "exit_code":0
}}
```

| Champ | Rôle |
|---|---|
| `session_id` | Nom du fichier de session sous `.pyxis/sessions/`, reprenable par `--resume`. |
| `model_turns` | Nombre d'allers-retours modèle. |
| `input_tokens`, `output_tokens` | Consommation cumulée du run. |
| `end` | `end_turn`, `exhausted` ou `error`. |
| `end_detail` | Présent pour `exhausted` et `error` : la cause précise. |
| `exit_code` | Code que le processus rendra. `0` en cas de succès. |

## Codes de sortie

| Code | Sens |
|---|---|
| `0` | Le tour s'est terminé normalement. |
| `1` | Échec : erreur d'agent, arrêt par budget, ou erreur de démarrage (arguments, credential absente). |

Un appelant qui distingue les causes lit `end` et `end_detail` du `run_summary`
plutôt que d'interpréter le code de sortie.

## Ordre garanti

1. Les événements du tour, dans l'ordre d'émission par la boucle d'agent.
2. `run_summary`, toujours dernier.
