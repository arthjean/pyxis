<!-- Généré par crates/agent-cli/src/config_catalog.rs ; ne pas éditer à la main. -->
<!-- Régénérer : PYXIS_UPDATE_CATALOGS=1 cargo test -p agent-cli --bin pyxis config_catalog -->

# Catalogue de configuration

Les 15 clés que `pyxis` accepte, avec leur type, leur défaut, la couche la plus
basse qui peut les déclarer, leur drapeau, leur variable d'environnement et leur
caractère de sécurité. Elles sont déclarées dans
[`crates/agent-cli/src/config_catalog.rs`](../crates/agent-cli/src/config_catalog.rs) et confrontées à `KNOWN_KEYS` et `SECURITY_KEYS` de
[`crates/agent-cli/src/settings.rs`](../crates/agent-cli/src/settings.rs) dans les deux sens : une clé que le loader accepte et que ce
document ignore fait échouer la suite, et l'inverse aussi.

## Couches

Une valeur effective vient d'une couche nommée, et chaque couche porte une précédence
déclarée. La résolution compare ces nombres, jamais l'ordre d'application : une couche
plus forte qui a déjà réclamé une clé la garde, quelle que soit la couche appliquée
ensuite.

| Couche | Précédence |
|---|---|
| `global settings` | 10 |
| `profile` | 15 |
| `project config` | 20 |
| `environment` | 25 |
| `command line` | 30 |

## Clés de sécurité

Sur 15 clés, **7 élargissent un périmètre de sécurité**. Une clé de
sécurité est refusée depuis un fichier que l'espace de travail contrôle,
`<workspace>/.pyxis/config.toml`, avec un avertissement nommant la couche qui a essayé,
et elle est refusée depuis `-c clé=valeur`, un argument pouvant venir d'un script du
dépôt. Les drapeaux typés restent la façon de choisir un périmètre pour une session :
l'utilisateur les a frappés. Un profil déclaré par un fichier du dépôt ne contourne pas
la règle, la portée du fichier d'origine voyageant avec lui.

## Clés

La colonne « couche la plus basse admise » nomme la couche la MOINS fiable dont une
déclaration est honorée, du fichier que le dépôt écrit au drapeau que l'utilisateur
frappe. Un défaut « aucun » veut dire que la clé n'a pas de valeur tant qu'aucune
couche ne la déclare. 9 clés ont un drapeau et 5 ont une variable
d'environnement ; les autres portent un marqueur d'absence, jamais une cellule vide.

| Clé | Type | Défaut | Couche la plus basse admise | Drapeau | Variable d'environnement | Clé de sécurité |
|---|---|---|---|---|---|---|
| `cost_budget_micro_usd` | entier positif | aucun | `project config` | `--cost-budget-micro-usd` | `PYXIS_COST_BUDGET_MICRO_USD` | non |
| `hooks` | tableau de tables | aucun | `global settings` | aucun | aucune | oui |
| `input_cost_micro_per_ktok` | entier positif | aucun | `project config` | `--input-cost-micro-per-ktok` | `PYXIS_INPUT_COST_MICRO_PER_KTOK` | non |
| `model` | chaîne | `gpt-5.5` | `project config` | `--model` | aucune | non |
| `output_cost_micro_per_ktok` | entier positif | aucun | `project config` | `--output-cost-micro-per-ktok` | `PYXIS_OUTPUT_COST_MICRO_PER_KTOK` | non |
| `overload_fallback_model` | chaîne | aucun | `project config` | `--overload-fallback-model` | `PYXIS_OVERLOAD_FALLBACK_MODEL` | non |
| `permission_mode` | chaîne | `ask` ; `accept-edits` en mode `-p` avec `--yes` | `global settings` | `--permission-mode` | aucune | oui |
| `profile` | chaîne | aucun | `global settings` | `--profile` | aucune | oui |
| `profiles` | table de tables | aucun | `project config` | aucun | aucune | non |
| `reasoning_effort` | chaîne | celui que le modèle applique sans consigne | `project config` | aucun | aucune | non |
| `safe_commands` | tableau de tables | la table intégrée seule | `global settings` | aucun | aucune | oui |
| `sandbox_mode` | chaîne | `workspace-write` | `global settings` | `--sandbox` | aucune | oui |
| `token_budget` | entier positif | aucun | `project config` | `--token-budget` | `PYXIS_TOKEN_BUDGET` | non |
| `web_search` | booléen | `false` | `global settings` | aucun | aucune | oui |
| `writable_roots` | tableau de chaînes | la racine de l'espace de travail seule | `global settings` | aucun | aucune | oui |

## Variables d'environnement hors configuration

Toute variable `PYXIS_*` lue sous `crates/*/src` se classe : soit elle porte une clé
du tableau ci-dessus, soit elle figure ici. Une variable qu'aucune des deux tables ne
nomme fait échouer la suite, et une ligne d'ici que plus aucune source ne lit aussi :
il n'y a pas de silence.

| Variable | Catégorie | Rôle |
|---|---|---|
| `PYXIS_A_VARIABLE_NOBODY_SETS` | tests | nom volontairement absent de l'environnement, lu par le test qui prouve qu'une substitution non résolue le reste |
| `PYXIS_CODEX_BASELINE` | parité | chemin du clone Codex épinglé, lu en lecture seule par `agent-parity` |
| `PYXIS_CODEX_CLIENT_VERSION` | protocole | version du client annoncée à l'endpoint ChatGPT |
| `PYXIS_DEBUG_TUI` | débogage | journal de l'interface, écrit hors du souscripteur `tracing` |
| `PYXIS_DEBUG_USAGE` | débogage | sonde de calibration de l'usage rapporté par le fournisseur |
| `PYXIS_HOME` | chemins | racine de l'état utilisateur, `~/.pyxis` par défaut |
| `PYXIS_IDLE_TIMEOUT_SECS` | transport | délai d'inactivité du flux du fournisseur |
| `PYXIS_LOG` | journalisation | filtre du souscripteur `tracing` ; sans lui aucun souscripteur n'est installé |
| `PYXIS_ORIGINATOR` | protocole | originateur annoncé à l'endpoint ChatGPT |
| `PYXIS_REDUCED_MOTION` | rendu | coupe les animations de l'interface |
| `PYXIS_TEST_ABSENT_VAR` | tests | nom volontairement absent, lu par le test de substitution d'`agent-mcp` |
| `PYXIS_UPDATE_CATALOGS` | génération | bascule les portes de catalogue en écriture |
| `PYXIS_UPDATE_SCHEMAS` | génération | bascule la porte des schémas d'app-server en écriture |
| `PYXIS_UPDATE_TRANSCRIPTS` | génération | bascule la porte des transcriptions gelées en écriture |
