# Schéma d'événements JSONL

Contrat machine du mode headless.

```sh
pyxis -p "resume ce dépôt" --output-format json
```

Chaque ligne de la sortie standard est **un objet JSON complet**, vidé dès son
écriture. Aucune ligne n'est reformatée, aucune n'est destinée à un humain. Le
format textuel par défaut (`--output-format text`, ou l'absence du drapeau) reste
strictement ce qu'il était : la réponse finale, rien d'autre.

Les diagnostics (`[config]`, `[sandbox]`, `[diff]`, erreurs de démarrage) partent
sur **stderr** et ne polluent jamais le flux JSONL.

## D'où viennent les exemples

Chaque bloc `json` de ce document est dans l'un de deux états, et le commentaire
qui le précède dit lequel. Un exemple **ancré** porte
`<!-- transcription: <chemin>:<rang> -->` : c'est alors la ligne de ce rang, telle
quelle, copiée d'une transcription gelée sous
`crates/agent-cli/tests/transcripts/`, et une porte d'`agent-doc-gates` la compare
octet à octet. Un exemple **hors transcription** porte
`<!-- hors transcription: <raison> -->` : aucun scénario gelé n'émet ce type, donc
la forme est rédigée et ne vaut que comme illustration. La même porte refuse un
bloc `json` qui ne porte ni l'un ni l'autre, pour qu'un exemple ne puisse pas
redevenir une invention silencieuse.

## Forme d'une ligne

<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:3 -->
```json
{"schema":1,"type":"text","data":"Le fichier contient ","thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_00000000000000000000000000000009"}
```

| Champ | Type | Rôle |
|---|---|---|
| `schema` | entier | Version du schéma. `1` aujourd'hui. |
| `type` | chaîne | Discriminant en `snake_case`. |
| `data` | variable | Charge utile, absente pour les événements sans donnée. |
| `thread_id` | chaîne | Identité durable de la conversation. Ajout additif du runtime d'orchestration durable. |
| `turn_id` | chaîne | Tour auquel l'événement est corrélé. |
| `event_id` | chaîne | Identité de cette ligne dans le journal durable du thread. |

Les trois identifiants sont ajoutés **à côté** des champs existants, jamais à la
place de l'un d'eux : une ligne lue par un consommateur qui les ignore est
identique à ce qu'elle était avant. Ils sont omis quand le run n'a pas d'identité
runtime à rapporter, plutôt qu'émis à `null`. Ce sont les mêmes identifiants que
ceux du journal `.pyxis/sessions/<session>.jsonl` et que ceux qu'affiche
`/status` : un appelant peut donc rejouer, forker ou reprendre exactement le tour
qu'il a observé.

Chacun est un préfixe suivi de **trente-deux caractères hexadécimaux
minuscules**, seize octets rendus en base seize. Les préfixes sont `thr_` pour un
thread, `trn_` pour un tour et `evt_` pour un événement
(`crates/agent-runtime/src/id.rs:191,196,206`). L'hexadécimal majuscule est
refusé à l'analyse : il n'existe qu'une écriture canonique par identifiant, donc
deux chaînes différentes ne peuvent pas désigner le même tour.

`event_id` est présent sur les lignes que la boucle d'agent publie, et **absent**
sur les deux lignes que le client écrit lui-même : `run_summary`, qui est un fait
de processus, et le `hook` terminal, qui rapporte un lancement effectué après la
fin du tour. Aucune des deux n'a été écrite dans le journal durable du thread,
donc aucune ne peut prétendre à une identité qui y renvoie. Un consommateur
corrèle ces deux lignes par `thread_id` et `turn_id`, qu'elles portent toutes les
deux.

`schema` est incrémenté seulement si une ligne **déjà émise change de forme**.
Ajouter un type d'événement ou un champ optionnel ne casse pas un consommateur qui
ignore ce qu'il ne connaît pas, et n'incrémente donc pas la version. Un
consommateur strict doit refuser une version majeure inconnue et ignorer les
`type` inconnus.

Le vocabulaire est celui de Pyxis, pas celui d'un concurrent : la recherche menée
pour ce contrat n'a trouvé aucun schéma JSONL standardisé entre agents de code, donc
s'aligner sur l'un d'eux aurait imité un choix arbitraire au lieu de documenter le
sien.

## Types d'événements

Les types suivants sont la sérialisation directe d'`agent_core::AgentEvent`, le
contrat que consomme aussi la TUI. La table les donne dans l'ordre de l'énumération
et les donne **tous** : une porte d'`agent-doc-gates` compte les variantes de
`crates/agent-core/src/event.rs`, compte les lignes de cette table, et échoue en
nommant l'écart quand les deux nombres diffèrent.

| `type` | `data` | Signification |
|---|---|---|
| `stream_reset` | — | Le flux en cours est abandonné avant validation (retry). Un consommateur doit jeter les `text` et `reasoning` non encore clos. |
| `text` | chaîne | Fragment de texte assistant. |
| `reasoning` | chaîne | Fragment de raisonnement, si le provider en émet. |
| `reasoning_replay_disabled` | `{reason}` | Le backend a refusé le replay chiffré; le sampling repart une fois sans replay dans le même budget d'attempts. |
| `response_metadata` | `{response_id?, model?, service_tier?, request_id?, turn_state?, models_etag?, end_turn?, safety?, verifications?, moderation?, reasoning?}` | En-têtes et métadonnées de la réponse provider. Tous les champs sont omis quand le backend ne les sert pas : la ligne existe même quand elle ne porte qu'un `response_id`. Voir plus bas. |
| `response_item` | `{phase, output_index?, item}` | Cycle de vie d'un élément de réponse, indépendant du provider. `phase` vaut `added` ou `done`. Voir plus bas. |
| `provider_extension` | `{event_type, payload, original_bytes, truncated?, redacted?}` | Événement provider additif, borné et expurgé. Voir plus bas. |
| `unmapped_response_item` | `{item_type, extension?}` | Le backend a servi un élément que l'adaptateur ne sait pas traduire : son contenu n'a donc jamais atteint le transcript. Rapporté plutôt que jeté, parce que le silence se lirait comme « le modèle n'a rien produit ». |
| `retry_scheduled` | `{turn_id?, step, ordinal, max_attempts, cause, delay_ms, fallback_model?, prompt_fingerprint, model_runtime_fingerprint, tool_plan_fingerprint}` | Une nouvelle ouverture provider est planifiée. Aucun corps d'erreur, prompt, token ou account ID n'est inclus. |
| `credential_refresh` | `{turn_id?, step, attempt_ordinal, outcome}` | Cycle de récupération OAuth borné. `outcome` vaut `started`, `succeeded`, `rejected` ou `cancelled`. |
| `tool_call` | `{id, name, input, kind}` | Un outil va s'exécuter. `input` est le JSON d'arguments réassemblé, `kind` la nature du call, `{"kind":"other"}` par défaut. |
| `tool_output_delta` | `{id, chunk}` | Fragment de sortie d'un outil encore en cours. Informatif : le transcript ne retient que `tool_result`. |
| `tool_result` | `{id, content, status?, structured_content?, is_error, error_kind?, untrusted, duration_ms?, truncation?, execution?}` | Résultat d'outil. `untrusted` vaut `true` pour tout contenu externe. `truncation` n'apparaît que si le contenu servi au modèle n'est pas la sortie entière. Voir plus bas. |
| `compacted` | `"micro"` \| `"auto"` \| `"reactive"` | Une compaction de contexte a eu lieu. |
| `model_turn` | `{index, input_tokens, output_tokens, last_usage?, total_usage, context_tokens?, context_window?, auto_compact_token_limit?, estimated_context_tokens?}` | Un aller-retour modèle vient de finir. `index` est 1-based ; `input_tokens` et `output_tokens` sont **cumulés depuis le début du run**, réels quand le provider rapporte son usage, estimés localement sinon. Voir plus bas. |
| `turn_diff` | `{files:[…]}` | Diff agrégé du tour. Jamais émis quand rien n'a changé. Voir plus bas. |
| `quota` | `{primary?, secondary?}` | État de quota d'abonnement rapporté par le backend. Émis seulement quand le backend en sert un. Voir plus bas. |
| `plan` | `{explanation?, steps:[{step, status}]}` | Plan de tâches publié par le modèle. `status` vaut `pending`, `in_progress` ou `completed`. Purement informatif : le plan ne pilote jamais la boucle. |
| `permission_ask` | `{call_id, tool, reason, taint_forced, input_summary, input, mode}` | Demande d'autorisation. En headless, l'approbateur refuse par défaut. |
| `hook` | `{event, tool?, status, message?}` | Un hook a tourné. `event` nomme l'événement du contrat de référence (`PreToolUse`, `SessionStart`, `Stop`, …), `tool` est absent sur un événement de cycle de vie, et `status` vaut `completed`, `blocked` ou `failed`. Voir plus bas. |
| `end_turn` | — | Le tour s'est terminé normalement. |
| `interrupted` | `{reason, started_at_ms?, completed_at_ms?, duration_ms?, reconciled_tool_calls}` | Le tour a été interrompu ; le transcript est déjà réconcilié. `reconciled_tool_calls` non nul signale des appels d'outils dont le modèle n'a jamais vu le résultat. |
| `exhausted` | détail | Arrêt déterministe : budget, plafond de tours, boucle d'outils. |
| `error` | détail | Erreur d'agent. Émise **avant** la sortie du processus. |
| `thread_store_failed` | `{operation, detail, thread_id, event_id?}` | Le journal durable est devenu inutilisable. Signal live additif : son `event_id` corrèle la ligne mais ne prétend pas que la faute a été persistée par le writer défaillant. Il est omis si le client reconstruit la faute depuis le dernier statut après une perte d'événements live. |

`thread_store_failed` vient du runtime, pas d'`AgentEvent` : c'est la seule ligne
de cette table qui ne corresponde à aucune variante, et la porte de comptage la
connaît par son nom. Il termine le run avec `run_summary.data.end = "error"` et
un `exit_code` non nul. `operation` nomme la frontière (`create`, `append`,
`commit_recovery`, `flush`, `read`, `fork` ou `close`) sans inclure le contenu du
prompt.

### `response_metadata`, `response_item` et `provider_extension`

Ces trois types portent ce que le backend a envoyé au-delà du texte et des
outils. Ils sont **informatifs** : un consommateur qui ne les connaît pas lit le
flux qu'il lisait avant, et rien de ce qu'ils portent n'est nécessaire pour
reconstruire la réponse.

<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:1 -->
```json
{"schema":1,"type":"response_metadata","data":{"response_id":"resp_e2e_2"},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_00000000000000000000000000000007"}
```

<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:2 -->
```json
{"schema":1,"type":"provider_extension","data":{"event_type":"response.created","payload":{"response":{"id":"resp_e2e_2"},"type":"response.created"},"original_bytes":58},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_00000000000000000000000000000008"}
```

<!-- transcription: crates/agent-cli/tests/transcripts/tool-call/expected.jsonl:4 -->
```json
{"schema":1,"type":"response_item","data":{"phase":"added","output_index":0,"item":{"id":"item_1","kind":"function_call","payload":{"event_type":"response.item.function_call","payload":{"arguments":"","call_id":"call_read_1","id":"item_1","name":"read","type":"function_call"},"original_bytes":91}}},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_0000000000000000000000000000000a"}
```

`payload` est toujours une `ProviderExtension`, donc toujours borné et expurgé :
`original_bytes` est la taille de la charge utile telle que le backend l'a
envoyée, `truncated` dit qu'elle dépassait les 64 Ko admis à cette couture, et
`redacted` qu'un champ sensible en a été retiré. Les deux drapeaux sont omis
quand ils valent `false`. `kind` d'un `item` est un vocabulaire fermé
(`message`, `reasoning`, `function_call`, `web_search_call`, …) plus une forme
ouverte pour ce que l'adaptateur voit sans le connaître ; un élément dont le type
de fil n'est traduit par aucune variante ne devient pas un `response_item` mais
un `unmapped_response_item`.

Un même élément apparaît deux fois quand il est diffusé : une fois en `added`,
avec des arguments encore incomplets, une fois en `done`. Un consommateur qui
agrège doit donc traiter `phase` et pas seulement l'identifiant.

### `tool_result` et `truncation`

<!-- transcription: crates/agent-cli/tests/transcripts/tool-call/expected.jsonl:10 -->
```json
{"schema":1,"type":"tool_result","data":{"id":"call_read_1","content":"     1\tla phrase attendue\n","status":"success","is_error":false,"untrusted":true,"duration_ms":0},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_00000000000000000000000000000011"}
```

<!-- hors transcription: aucun scénario gelé ne déborde la limite du modèle, donc aucun n'émet `truncation` -->
```json
{"schema":1,"type":"tool_result","data":{"id":"call_1","content":"…","is_error":false,"untrusted":true,"truncation":{"original_bytes":10485760,"kept_bytes":29873,"strategy":"head","continuation_hint":".pyxis/spill/0123456789ab/fedcba987654-grep-call_1.txt"}}}
```

`status` vaut `success`, `error` ou `rejected` et double `is_error` d'une nuance
que le booléen ne porte pas : un refus d'autorisation n'est pas une panne
d'outil. `duration_ms` est l'exécution mesurée, `execution` la manière dont
l'outil a tourné quand elle n'allait pas de soi, et `structured_content` la forme
typée d'un résultat quand l'outil en produit une en plus du texte. Les quatre
sont omis plutôt qu'émis vides.

`truncation` est omis quand le modèle reçoit la sortie complète. Quand il est
présent, `original_bytes` est la taille de la sortie produite par l'outil,
`kept_bytes` celle du contenu réellement servi, et `strategy` (`head` ou `tail`)
dit quelle extrémité a été conservée en priorité. Un résultat déversé vaut
`head` et montre pourtant ses deux extrémités : la notice qui suit l'aperçu dit
la forme exacte, à l'endroit où le modèle la lit.

`continuation_hint` dit quoi faire des octets manquants. Deux formes existent et
un consommateur ne doit pas les distinguer par analyse :

- une consigne en clair quand rien n'a été mis de côté, par exemple relancer
  l'outil avec une requête plus étroite ;
- un **chemin relatif au workspace** quand la sortie a été déversée sur disque,
  sous `.pyxis/spill/`. Le fichier contient la sortie entière, telle que l'outil
  l'a produite ; le modèle la relit avec `read` (`offset`, `limit`) ou la cherche
  avec `grep`.

`bash` déverse chaque flux dans son propre fichier, parce que ses deux lecteurs
tournent en parallèle et qu'un entrelacement des deux serait un ordre qu'aucun
d'eux ne pourrait énoncer. Quand les deux flux débordent, `original_bytes` compte
les octets des deux et `continuation_hint` n'en nomme qu'un, la sortie standard
d'abord. La notice qui suit l'aperçu, elle, nomme chaque fichier : c'est elle qui
fait autorité sur ce qui a été déversé, et un consommateur qui veut tous les
chemins la lit plutôt que de déduire du seul localisateur.

Aucun chemin absolu n'est jamais sérialisé : le localisateur reste relatif à la
racine du workspace, donc partageable et stable d'une machine à l'autre. Un
client transporte et affiche cette chaîne, il ne la découpe ni ne la reconstruit :
la disposition du stockage appartient au binaire et peut changer sans préavis.
Le déversement est best-effort. Quand il échoue, ou quand aucun stockage
n'existe, le résultat servi reste celui que l'outil a produit et `is_error`
n'est jamais levé pour autant : la sortie est alors simplement bornée comme
avant, donc `truncation` est absent si elle tient sous la limite du modèle, et
présent avec une consigne en clair si elle ne tient pas. Un chemin dans
`continuation_hint` prouve donc qu'un fichier existe ; son absence ne prouve
rien sur la cause.

### `retry_scheduled` et `credential_refresh`

<!-- transcription: crates/agent-cli/tests/transcripts/stream-error/expected.jsonl:5 -->
```json
{"schema":1,"type":"retry_scheduled","data":{"turn_id":"trn_00000000000000000000000000000003","step":1,"ordinal":2,"max_attempts":2,"cause":"retryable","delay_ms":0,"prompt_fingerprint":"b00b0f4096d711a45e9e7e0b8020c62769feb232e1716754f4abd7f294b06e28","model_runtime_fingerprint":"5b71110c0b8ccc36f63528d36b266a3ab004ec7821360a1273da39e05ead90e7","tool_plan_fingerprint":"08ab4cf9638a1a5b4c80affbe15eec8212622d7a829306e54e1c67c588593d3c"},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_0000000000000000000000000000000c"}
```

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

<!-- transcription: crates/agent-cli/tests/transcripts/tool-call/expected.jsonl:16 -->
```json
{"schema":1,"type":"model_turn","data":{"index":2,"input_tokens":300,"output_tokens":27,"last_usage":{"input":180,"cached_input":0,"cache_write_input":0,"output":9,"reasoning_output":0,"total":189},"total_usage":{"input":300,"cached_input":0,"cache_write_input":0,"output":27,"reasoning_output":0,"total":465},"context_tokens":180},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003","event_id":"evt_00000000000000000000000000000017"}
```

`last_usage` est le compteur de **cet** aller-retour tel que le backend l'a
rapporté, `total_usage` la somme élément par élément de tous les allers-retours
rapportés depuis le début du run ; c'est ce couple qui rend l'efficacité de cache
et la part de raisonnement calculables, ce que les deux totaux plats
`input_tokens` et `output_tokens` ne peuvent pas exprimer. `last_usage` est
**absent** quand le provider n'a rapporté aucun usage : les deux totaux plats
portent alors une estimation locale, et les confondre ferait passer une
estimation pour une mesure.

`context_tokens` est le remplissage **réel** de la fenêtre à cet aller-retour, tel
que le backend l'a rapporté. Le champ est **absent** quand le provider ne rapporte
aucun usage : la mesure manque, ce qui ne se confond pas avec zéro. `context_window`
est la fenêtre déclarée par le backend pour le modèle actif, **absente** tant
qu'elle est inconnue. Le cœur ne calcule aucun pourcentage : rapporter l'un à
l'autre est une décision de présentation. `auto_compact_token_limit` est le seuil
que le descripteur du modèle fixe à la compaction proactive, absent tant qu'aucun
n'est déclaré. `estimated_context_tokens` n'apparaît que lorsque la sonde de
calibration est active (`PYXIS_DEBUG_USAGE`).

### `quota`

<!-- hors transcription: aucun scénario gelé ne rejoue un backend qui sert un quota -->
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

<!-- hors transcription: les scénarios gelés tournent hors dépôt git, donc aucun n'émet `turn_diff` -->
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

### `hook`

<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:9 -->
```json
{"schema":1,"type":"hook","data":{"event":"Stop","status":"completed"},"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003"}
```

Un `hook` est émis chaque fois qu'un hook tourne, donc autour des appels d'outils
quand l'utilisateur en a déclaré, et **à la fin de chaque exécution réussie**
pour l'événement `Stop`. Cette dernière ligne est émise après `run_summary`,
parce que `Stop` rapporte un lancement qui n'a lieu qu'une fois le tour vraiment
terminé ; elle est absente d'un run interrompu, épuisé ou en erreur, où l'agent
ne s'est pas arrêté de lui-même. Elle ne porte pas d'`event_id` : rien n'en a été
écrit dans le journal durable du thread.

Un hook déclaré `Stop` qui refuse ne rouvre pas le tour : `status` vaut alors
`blocked` et le run est déjà résumé. Un consommateur qui traite cette ligne la
lit comme un fait rapporté, pas comme une transition.

### `run_summary`

Ce n'est pas un `AgentEvent` : l'identifiant de session et le code de sortie sont
des faits de processus.

<!-- transcription: crates/agent-cli/tests/transcripts/bare-turn/expected.jsonl:8 -->
```json
{"schema":1,"type":"run_summary","data":{"session_id":"bare-turn.jsonl","model_turns":1,"input_tokens":180,"output_tokens":9,"end":"end_turn","exit_code":0,"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003"}}
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

`cause_category` et `cause_guidance` sont additifs : un
consommateur qui les ignore lit la ligne qu'il lisait avant. Ils viennent
d'`agent_runtime::TurnFailure`, le même classificateur qui alimente la TUI, la
sortie stderr de `-p` et le champ `causeCategory` de `turn/completed` côté
app-server, donc les quatre surfaces ne peuvent pas nommer deux catégories
différentes pour la même cause.

<!-- transcription: crates/agent-cli/tests/transcripts/stream-error/expected.jsonl:11 -->
```json
{"schema":1,"type":"run_summary","data":{"session_id":"stream-error.jsonl","model_turns":0,"input_tokens":0,"output_tokens":0,"end":"error","end_detail":"provider: stream: provider stream failed","cause_category":"provider","cause_guidance":"retry the turn; check connectivity if it repeats","exit_code":1,"thread_id":"thr_00000000000000000000000000000001","turn_id":"trn_00000000000000000000000000000003"}}
```

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

`end: "interrupted"` est apparu avec le runtime d'orchestration durable : avant lui, rien ne pouvait
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
3. `run_summary`, qui clôt le compte du run.
4. Le `hook` de l'événement `Stop`, et lui seul, quand le run s'est terminé
   normalement. Rien d'autre ne suit `run_summary`.

Un consommateur qui s'arrête sur `run_summary` lit un run complet : tout ce qui
décide de son résultat est déjà passé, et le `exit_code` y est. Un consommateur
qui veut aussi le verdict du hook terminal lit une ligne de plus, et sait qu'elle
n'existe que si `end` vaut `end_turn`.

Cette règle n'est pas une prose : `crates/agent-cli/src/transcript/scenario.rs`
la rend en verdict et l'applique deux fois, aux octets que le harnais produit et
aux fichiers gelés eux-mêmes, de sorte qu'une inversion des terminaux dans
`crates/agent-cli/src/headless.rs` échoue même si les fichiers ont été
régénérés sans être relus.

En mode interactif, `turn_diff` est émis **avant** l'événement terminal du tour,
parce que l'interface clôt le tour dès qu'elle le voit. Un consommateur du flux
JSONL n'a pas à s'en soucier : la seule garantie dont il dépend est que
`turn_diff` précède `run_summary`.
