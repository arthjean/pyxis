# App-server : contrat client externe

`pyxis --app-server` sert un protocole JSON-RPC 2.0 sur l'entrée et la sortie
standard, et `--listen 127.0.0.1:<port>` sert le **même** protocole sur
WebSocket. Un client pilote ainsi Pyxis sans lier ses crates.

Les schémas publiés dans ce répertoire sont **générés**, jamais rédigés :

```bash
pyxis app-server --emit-schemas docs/app-server
```

`protocol.schema.json` (JSON Schema 2020-12) et `protocol.d.ts` (TypeScript)
sortent des types Rust de `crates/agent-app-server/src/protocol.rs`. Un test les
compare aux fichiers du dépôt ; une divergence est un échec, pas un avertissement :

```bash
cargo test -p agent-app-server --test schemas
PYXIS_UPDATE_SCHEMAS=1 cargo test -p agent-app-server --test schemas  # régénère
```

## Cycle de vie

```text
initialize                  -> capacités, version négociée
thread/start | thread/resume -> threadId, itemCount   (+ ownership)
turn/start                  -> turnId
   <- turn/started, item/started, item/*/delta, item/completed, turn/completed
   <- item/*/requestApproval, item/tool/call          (requêtes serveur)
turn/interrupt              -> arrêt coopératif
thread/items/list           -> pages d'historique par curseur
thread/unsubscribe          -> libère l'ownership
```

`initialize` précède toute mutation : toute autre méthode reçoit `-32000` tant
qu'il n'a pas réussi. Une version non supportée reçoit `-32001` avec la liste
des versions servies.

## Codes d'erreur

| Code | Sens |
|---|---|
| `-32700` `-32600` `-32601` `-32602` `-32603` | JSON-RPC standard |
| `-32000` | méthode appelée avant `initialize` |
| `-32001` | version de protocole non supportée |
| `-32002` | le thread est piloté par un autre client |
| `-32003` | thread inconnu de cette connexion |
| `-32004` | le tour nommé n'est plus celui qui tourne |
| `-32005` | le runtime a refusé (mailbox pleine, arrêt, journal) |
| `-32006` | file client saturée : la connexion se ferme |

## Causes terminales

`turn/completed` porte `status`, puis trois champs facultatifs qui décrivent
l'échec : `cause` (le texte que le journal durable a enregistré),
`causeCategory` et `causeGuidance`. Les deux derniers viennent du
classificateur partagé (`agent_runtime::TurnFailure`), celui qui alimente aussi
la TUI, la sortie stderr de `pyxis -p` et le champ `cause_category` de la ligne
`run_summary` : les quatre surfaces ne peuvent donc pas nommer deux catégories
différentes pour la même cause (EP-006/US-019 AC1).

Un client **branche sur `causeCategory`**, jamais sur le texte de `cause`. Les
valeurs sont fermées et publiées dans le schéma : `provider`, `auth`, `context`,
`invalid_request`, `model_runtime`, `guardrail`, `interrupted`, `store`,
`unknown`. Une cause que le classificateur ne reconnaît pas sort en `unknown`
avec son texte intact, plutôt que rangée dans une catégorie devinée qui
enverrait le client vers le mauvais diagnostic. Le détail de chaque catégorie
est dans `docs/EVENT_SCHEMA.md`.

## Approvals et outils dynamiques

Une approbation traverse le pipeline de permissions de Pyxis inchangé : mode de
permission, hooks, taint untrusted et mémoire de session décident AVANT que la
requête soit émise, et une absence de réponse **refuse**. Les quatre
identifiants (`threadId`, `turnId`, `itemId`, `callId`) plus l'`id` JSON-RPC
permettent une résolution unique ; une réponse tardive, dupliquée ou liée à un
tour terminé est refusée et signalée par une notification `error`.

Un client déclare ses propres outils à `thread/start` (`dynamicTools`). Ils
entrent dans le **même** registre que les outils natifs : métadonnées
fail-closed (sensible, non concurrent, sortie untrusted), confirmation par
défaut, jamais mémorisable, et exécution renvoyée au client par
`item/tool/call`. Un nom invalide, dupliqué, ou un schéma non strict est refusé
et tracé ; le modèle ne voit alors pas l'outil.

## Backpressure

Une connexion a au plus 1 024 messages ou 16 MiB en vol. Au-delà, le serveur
émet une notification `error` nommant la cause et ferme la connexion. Rien
n'est perdu : ce qui est commité est dans le journal durable et se relit avec
`thread/resume` puis `thread/items/list`.

## Divergences assumées avec Codex

| Divergence | Raison |
|---|---|
| 8 méthodes sur les 131 de la baseline | Le reste (comptes, cloud, `fs/*`, marketplace, realtime) relève des non-goals du PRD. |
| **Un seul thread ouvert** par processus | Le registre d'outils, la session Code Mode et le handle multi-agent appartiennent au thread ouvert. Un deuxième thread vivant les rebinderait sous le premier ; le client reçoit `-32002` plutôt qu'un thread dont les outils pointent ailleurs. |
| `turn/reasoning/delta` au lieu d'un delta de raisonnement porté par un item | Pyxis ne persiste pas le raisonnement. L'attacher à un item ferait diverger la numérotation d'un thread repris de celle de son journal. |
| `item/tool/requestApproval` en plus des deux familles Codex | Pyxis dispatche des outils génériques (MCP, multi-agent, Code Mode) par le même pipeline ; les forcer en « commande » ou « changement de fichier » serait faux. |
| Identifiants d'items `item_<n>` dérivés du rang durable | Un identifiant live est celui que le même item portera une fois relu du disque. Un artefact que le journal ne tiendra jamais (rapport d'erreur moteur) est nommé dans l'espace `error_<n>` et ne consomme pas de rang. |
| WebSocket en boucle locale avec jeton porteur obligatoire | Une socket locale est joignable par tout processus de la machine : « local » n'est pas une autorisation. stdio n'en demande pas, puisque écrire sur l'entrée standard de ce processus suppose déjà les droits de l'utilisateur. |
| `thread/items/list` sert l'état durable | L'historique est ce que le journal tient. Ce qu'un tour en cours n'a pas encore commité arrive par le flux live, pas par la pagination. |

Source normative des noms : `docs/parity/codex-baseline-matrix.json`
(section `app_server_methods`). Provenance : `NOTICE-CODEX.md` et
`docs/codex-port-inventory.md`.
