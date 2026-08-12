# Suite de parité offline

Recette normative de la preuve de parité **sans compte OpenAI et sans réseau**.

```bash
cargo test --workspace --no-fail-fast     # la suite entière, dont tout ce qui suit
cargo run -p agent-parity -- check        # la matrice figée correspond au clone épinglé
cargo run -p agent-parity -- drift        # ce qui a bougé en amont, sans rien modifier
```

Aucune de ces commandes n'ouvre de connexion : les fixtures provider sont des
octets committés, les serveurs MCP des tests sont des processus locaux compilés
à la volée, et l'app-server est piloté en mémoire. Le seul scénario qui parle à
OpenAI est la recette live, séparée et opt-in, décrite plus bas.

## Couverture par domaine de contrat

Le tableau ci-dessous est **lu par un test** (`crates/agent-parity/tests/offline_suite.rs`) :
chaque fichier doit exister et déclarer le test nommé. Un domaine qui perd sa
preuve fait échouer la suite au lieu de disparaître en silence.

| Domaine | Fichier | Test | Ce qu'il prouve |
|---|---|---|---|
| catalogue | `crates/agent-core/src/model.rs` | `an_absent_multi_agent_version_reads_as_disabled_and_the_wire_names_round_trip` | `tool_mode`, `use_responses_lite` et `multi_agent_version` traversent descriptor -> runtime résolu avec les valeurs par défaut de la baseline. |
| function-custom-wire | `crates/agent-provider/tests/conformance.rs` | `requests_match_their_baseline_body` | Les requêtes function et freeform correspondent octet à octet aux fixtures golden dérivées du contrat baseline. |
| function-custom-wire | `crates/agent-provider/tests/conformance.rs` | `every_fixture_covers_exactly_one_contract` | Un item Responses non mappé fait échouer la suite au lieu d'être ignoré. |
| code-mode | `crates/agent-code-mode/src/session_tests.rs` | `a_first_cell_gets_a_stable_id_and_walks_its_states` | Le protocole session/cellule (`running`, `yielded`, `completed`, `failed`) sans moteur JavaScript. |
| code-mode | `crates/agent-code-mode-v8/src/engine_tests.rs` | `two_sessions_never_see_each_others_state` | Deux threads ne partagent aucun global V8. |
| code-mode | `crates/agent-code-mode-v8/tests/cell_trace.rs` | `a_failed_cell_traces_its_kind_correlated_to_its_caller` | Une cellule échouée trace son type, corrélée thread/turn/call/cell, sans son contenu. |
| code-mode | `crates/agent-tools/src/code_mode_tests.rs` | `the_registry_exposes_exec_as_a_freeform_tool` | `exec` est un outil freeform du registre, grammaire Lark comprise, sans schéma inventé. |
| terminal | `crates/agent-tools/src/exec_session.rs` | `a_finished_command_reports_the_baseline_wire` | Le wire `exec_command` de la baseline en entrée comme en sortie. |
| terminal | `crates/agent-tools/src/exec_session.rs` | `a_poll_returns_only_what_came_after_the_previous_chunk` | Sortie incrémentale bornée, corrélée, sans doublon. |
| terminal | `crates/agent-tools/src/exec_session.rs` | `the_refusals_precede_the_spawn` | cwd absent, shell refusé et cinquième session échouent avant tout processus. |
| multi-agent-v2 | `crates/agent-runtime/tests/multi_agent_v2.rs` | `a_spawn_persists_its_canonical_name_filiation_and_intersected_authority` | Nom canonique, filiation et autorité intersectée persistés au spawn. |
| multi-agent-v2 | `crates/agent-tools/tests/multi_agent_dispatch.rs` | `the_same_v2_call_answers_identically_direct_and_from_a_cell` | Les six outils v2 rendent les mêmes états et erreurs en direct et depuis Code Mode. |
| reprise | `crates/agent-runtime/tests/resume.rs` | `crash_repair_crash_is_idempotent_across_1000_deterministic_replays` | Aucune double exécution après interruption et reprise, sur 1 000 injections. |
| reprise | `crates/agent-runtime/tests/multi_agent_v2.rs` | `a_restart_rebuilds_names_states_and_undelivered_mail` | Le graphe multi-agent, les noms et le courrier non délivré survivent au redémarrage. |
| app-server | `crates/agent-app-server/tests/protocol.rs` | `a_turn_streams_ordered_items_under_stable_identifiers` | Cycle thread/turn/item avec identifiants stables et ordre causal. |
| app-server | `crates/agent-app-server/tests/protocol.rs` | `the_history_pages_without_gap_or_repeat` | Historique paginé par curseur opaque, chaque item exactement une fois. |
| app-server | `crates/agent-app-server/tests/schemas.rs` | `the_published_schemas_match_the_protocol_types` | JSON Schema et TypeScript publiés dérivent des types Rust, comparés octet à octet. |
| mcp | `crates/agent-mcp/tests/tool_call.rs` | `mcp_tools_are_registered_and_callable_by_the_model` | Un outil MCP entre dans le registre et est appelable, sur un serveur local. |
| mcp | `crates/agent-mcp/tests/tool_call.rs` | `an_mcp_result_taints_the_rest_of_the_turn` | Une sortie MCP reste untrusted pour la suite du tour. |
| erreurs | `crates/agent-runtime/tests/observability.rs` | `a_failed_turn_records_a_classifiable_cause_and_traces_it_correlated` | Une cause terminale est durable, classifiable et tracée avec ses identifiants. |
| erreurs | `crates/agent-app-server/tests/protocol.rs` | `a_failed_turn_carries_the_shared_category_and_next_step` | La même catégorie et le même prochain pas atteignent le client externe. |
| erreurs | `crates/agent-cli/src/observability.rs` | `no_observability_exporter_is_linked_into_the_build` | Aucun exportateur d'observabilité distant n'est lié : zéro connexion par défaut. |

## Recette live (opt-in, séparée)

`crates/agent-cli/tests/live_parity_sol.rs` exécute un prompt `gpt-5.6-sol` réel.
Elle est **désactivée par défaut** et ne produit jamais un succès de parité par
absence de credentials :

```bash
PYXIS_LIVE_PARITY=1 cargo test -p agent-cli --test live_parity_sol -- --nocapture
```

Le verdict est écrit dans `target/parity/live-verdict.json` et c'est LUI qu'il
faut lire, pas le résultat vert du test :

| `status` | Signification |
|---|---|
| `skipped` | `PYXIS_LIVE_PARITY` absent, ou aucun credential ChatGPT local. La parité live n'est **pas** prouvée. |
| `passed` | Le tour a produit au moins une cellule Code Mode, un résultat terminal et un transcript reprenable. |
| `external_error` | OpenAI a échoué. L'erreur exacte est reportée, le test échoue, et rien n'est converti en succès. |

Un `skipped` est un trou de preuve assumé, pas une réussite : la suite offline
ci-dessus reste la seule preuve normative.

Le verdict décrit **la dernière exécution**, pas l'historique. Un
`cargo test --workspace` ordinaire fait tourner le scénario sans opt-in et le
remet donc à `skipped` : c'est voulu, un `passed` qui survivrait à une suite
offline mentirait sur ce qui vient d'être prouvé. Le résultat durable d'un run
live va dans `docs/CURRENT_STATUS.md` et dans le tracker de la story, pas dans
ce fichier.
