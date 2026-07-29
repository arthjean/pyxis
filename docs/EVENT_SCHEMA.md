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
{"schema":1,"type":"text","data":"bonjour",
 "thread_id":"th_…","turn_id":"tu_…","event_id":"ev_…"}
```

| Champ | Type | Rôle |
|---|---|---|
| `schema` | entier | Version du schéma. `1` aujourd'hui. |
| `type` | chaîne | Discriminant en `snake_case`. |
| `data` | variable | Charge utile, absente pour les événements sans donnée. |
| `thread_id` | chaîne | Identité durable de la conversation. Ajout additif d'EP-005 de `tasks/prd-runtime-orchestration-durable.md`. |
| `turn_id` | chaîne | Tour auquel l'événement est corrélé. |
| `event_id` | chaîne | Identité de cette ligne dans le journal durable du thread. |

Les trois identifiants sont ajoutés **à côté** des champs existants, jamais à la
place de l'un d'eux : une ligne lue par un consommateur qui les ignore est
identique à ce qu'elle était avant. Ils sont omis quand le run n'a pas d'identité
runtime à rapporter, plutôt qu'émis à `null`. Ce sont les mêmes identifiants que
ceux du journal `.pyxis/sessions/<session>.jsonl` et que ceux qu'affiche
`/status` : un appelant peut donc rejouer, forker ou reprendre exactement le tour
qu'il a observé.

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
contrat que consomme aussi la TUI.

| `type` | `data` | Signification |
|---|---|---|
| `stream_reset` | — | Le flux en cours est abandonné avant validation (retry). Un consommateur doit jeter les `text` et `reasoning` non encore clos. |
| `text` | chaîne | Fragment de texte assistant. |
| `reasoning` | chaîne | Fragment de raisonnement, si le provider en émet. |
| `reasoning_replay_disabled` | `{reason}` | Le backend a refusé le replay chiffré; le sampling repart une fois sans replay dans le même budget d'attempts. |
| `retry_scheduled` | `{turn_id?, step, ordinal, max_attempts, cause, delay_ms, fallback_model?, prompt_fingerprint, model_runtime_fingerprint, tool_plan_fingerprint}` | Une nouvelle ouverture provider est planifiée. Aucun corps d'erreur, prompt, token ou account ID n'est inclus. |
| `credential_refresh` | `{turn_id?, step, attempt_ordinal, outcome}` | Cycle de récupération OAuth borné. `outcome` vaut `started`, `succeeded`, `rejected` ou `cancelled`. |
| `tool_call` | `{id, name, input}` | Un outil va s'exécuter. `input` est le JSON d'arguments réassemblé. |
| `tool_output_delta` | `{id, chunk}` | Fragment de sortie d'un outil encore en cours. Informatif : le transcript ne retient que `tool_result`. |
| `tool_result` | `{id, content, is_error, error_kind?, untrusted}` | Résultat d'outil. `untrusted` vaut `true` pour tout contenu externe. |
| `compacted` | `"micro"` \| `"auto"` \| `"reactive"` | Une compaction de contexte a eu lieu. |
| `model_turn` | `{index, input_tokens, output_tokens, context_tokens?, context_window?, estimated_context_tokens?}` | Un aller-retour modèle vient de finir. `index` est 1-based ; `input_tokens` et `output_tokens` sont **cumulés depuis le début du run**, réels quand le provider rapporte son usage, estimés localement sinon. Voir plus bas. |
| `quota` | `{primary?, secondary?}` | État de quota d'abonnement rapporté par le backend. Émis seulement quand le backend en sert un. Voir plus bas. |
| `turn_diff` | `{files:[…]}` | Diff agrégé du tour. Jamais émis quand rien n'a changé. Voir plus bas. |
| `permission_ask` | `{call_id, tool, reason, taint_forced, input_summary, input, mode}` | Demande d'autorisation. En headless, l'approbateur refuse par défaut. |
| `end_turn` | — | Le tour s'est terminé normalement. |
| `interrupted` | — | Le tour a été interrompu ; le transcript est déjà réconcilié. |
| `exhausted` | détail | Arrêt déterministe : budget, plafond de tours, boucle d'outils. |
| `error` | détail | Erreur d'agent. Émise **avant** la sortie du processus. |
| `thread_store_failed` | `{operation, detail, thread_id, event_id?}` | Le journal durable est devenu inutilisable. Signal live additif : son `event_id` corrèle la ligne mais ne prétend pas que la faute a été persistée par le writer défaillant. Il est omis si le client reconstruit la faute depuis le dernier statut après une perte d'événements live. |

`thread_store_failed` vient du runtime, pas d'`AgentEvent`. Il termine le run
avec `run_summary.data.end = "error"` et un `exit_code` non nul. `operation`
nomme la frontière (`create`, `append`, `commit_recovery`, `flush`, `read`,
`fork` ou `close`) sans inclure le contenu du prompt.

### `retry_scheduled` et `credential_refresh`

`ordinal` est 1-based et compte l'ouverture initiale, même si celle-ci ne produit
pas un événement de retry. Il reste monotone lors d'un fallback modèle et ne
redémarre qu'après une réponse provider complète, donc au sampling suivant.
`max_attempts` vient du profil résolu. `delay_ms` inclut le backoff, le jitter et
le délai serveur borné; il vaut zéro après refresh, retrait du reasoning replay ou
fallback immédiat. Si l'attempt courant est le dernier, aucun `retry_scheduled`
n'est émis et l'événement `error` terminal porte la classification exacte.

Les fingerprints relient le retry au snapshot abandonné sans exposer son contenu.
Un 401 émet au plus un couple `credential_refresh` started/résultat par sampling.
Après rejet, absence de refresh ou second 401, le terminal demande une reconnexion.

### `model_turn`

```json
{"schema":1,"type":"model_turn","data":{
  "index":2,"input_tokens":18422,"output_tokens":1290,
  "context_tokens":12040,"context_window":272000
}}
```

`context_tokens` est le remplissage **réel** de la fenêtre à cet aller-retour, tel
que le backend l'a rapporté. Le champ est **absent** quand le provider ne rapporte
aucun usage : la mesure manque, ce qui ne se confond pas avec zéro. `context_window`
est la fenêtre déclarée par le backend pour le modèle actif, **absente** tant
qu'elle est inconnue. Le cœur ne calcule aucun pourcentage : rapporter l'un à
l'autre est une décision de présentation. `estimated_context_tokens` n'apparaît
que lorsque la sonde de calibration est active (`PYXIS_DEBUG_USAGE`).

### `quota`

```json
{"schema":1,"type":"quota","data":{
  "primary":{"used_percent":42.0,"window_minutes":300,"resets_at_unix":1784989920},
  "secondary":{"used_percent":7.5,"window_minutes":10080}
}}
```

`used_percent` va de 0 à 100. `window_minutes` est la durée de la fenêtre
glissante et `resets_at_unix` l'instant de réinitialisation en secondes depuis
l'époque Unix ; les deux sont absents quand le backend ne les sert pas. Une
fenêtre entièrement vide n'est jamais émise.

### `turn_diff`

```json
{"schema":1,"type":"turn_diff","data":{"files":[
  {"path":"src/lib.rs","change":"modified","added_lines":12,"removed_lines":3,
   "unified":"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ …"},
  {"path":"assets/logo.png","change":"added","added_lines":0,"removed_lines":0}
]}}
```

`change` vaut `added`, `modified` ou `deleted`. `unified` est **absent** pour un
fichier binaire ou plus volumineux que le seuil de diff : le fichier reste listé,
son contenu n'est pas comparé.

Périmètre : les fichiers que git considère différents de `HEAD`, fichiers non
suivis compris, fichiers ignorés exclus. Les modifications faites par une commande
shell y figurent donc au même titre que celles des outils d'édition. Dans un
répertoire qui n'est pas un dépôt git, `turn_diff` n'est jamais émis.

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
  "exit_code":0,
  "thread_id":"th_…",
  "turn_id":"tu_…"
}}
```

| Champ | Rôle |
|---|---|
| `session_id` | Nom du fichier de session sous `.pyxis/sessions/`, reprenable par `--resume`. |
| `model_turns` | Nombre d'allers-retours modèle. |
| `input_tokens`, `output_tokens` | Consommation cumulée du run. |
| `end` | `end_turn`, `interrupted`, `exhausted` ou `error`. |
| `end_detail` | Présent pour `interrupted`, `exhausted` et `error` : la cause précise. |
| `cause_category` | Catégorie lue dans la cause par le classificateur partagé. Absente sur une fin propre. |
| `cause_guidance` | Prochain pas de diagnostic pour cette catégorie, phrase identique sur toutes les surfaces. |
| `exit_code` | Code que le processus rendra. `0` en cas de succès. |
| `thread_id`, `turn_id` | Thread et tour du run, mêmes identifiants que sur les lignes d'événement. |

`cause_category` et `cause_guidance` sont additifs (EP-006/US-019 AC1) : un
consommateur qui les ignore lit la ligne qu'il lisait avant. Ils viennent
d'`agent_runtime::TurnFailure`, le même classificateur qui alimente la TUI, la
sortie stderr de `-p` et le champ `causeCategory` de `turn/completed` côté
app-server, donc les quatre surfaces ne peuvent pas nommer deux catégories
différentes pour la même cause.

| `cause_category` | Sens | Prochain pas |
|---|---|---|
| `provider` | Le fournisseur n'a pas rendu de tour utilisable. | Relancer ; vérifier la connectivité si cela se répète. |
| `auth` | Credential refusée ou non renouvelable. | Reconnecter l'abonnement ChatGPT. |
| `context` | Le transcript ne rentre plus dans le budget de contexte. | Nouveau thread, ou `/rewind`. |
| `invalid_request` | La requête ne pouvait pas porter ce que le modèle a demandé. | Rapporter ; le tour n'est pas rejouable tel quel. |
| `model_runtime` | Capacité refusée localement, avant tout appel réseau. | Choisir un modèle supporté, ou installer le composant manquant. |
| `guardrail` | Un garde-fou a arrêté la boucle ; le travail n'est pas fini. | Relever la limite nommée, ou découper la tâche. |
| `interrupted` | Annulation, arrêt du processus ou réparation à la reprise. | Reprendre le thread et resoumettre. |
| `store` | Le journal durable est illisible ou non écrivable. | Vérifier le fichier de session et l'espace disque. |
| `unknown` | Cause non reconnue, reportée comme telle plutôt que devinée. | Lire `end_detail` et la trace sous `PYXIS_LOG=debug`. |

`end: "interrupted"` est apparu avec EP-005 : avant lui, rien ne pouvait
interrompre un run `-p`, donc le cas n'était pas observable. Ctrl+C passe
désormais par le runtime, le tour s'arrête coopérativement, réconcilie son
transcript et écrit son propre terminal. Un consommateur strict qui n'aurait
connu que trois valeurs de `end` doit traiter une valeur inconnue comme un échec,
ce que le `exit_code` dit déjà.

## Codes de sortie

| Code | Sens |
|---|---|
| `0` | Le tour s'est terminé normalement. |
| `1` | Échec : erreur d'agent, interruption, arrêt par budget, ou erreur de démarrage (arguments, credential absente, journal de session corrompu). |

Un appelant qui distingue les causes lit `end` et `end_detail` du `run_summary`
plutôt que d'interpréter le code de sortie.

## Ordre garanti

1. Les événements du tour, dans l'ordre d'émission par la boucle d'agent.
2. `turn_diff`, s'il y a quelque chose à montrer.
3. `run_summary`, toujours dernier.

En mode interactif, `turn_diff` est émis **avant** l'événement terminal du tour,
parce que l'interface clôt le tour dès qu'elle le voit. Un consommateur du flux
JSONL n'a pas à s'en soucier : la seule garantie dont il dépend est que
`turn_diff` précède `run_summary`.
