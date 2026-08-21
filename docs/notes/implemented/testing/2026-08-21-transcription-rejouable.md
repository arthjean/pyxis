# Note: Transcription rejouable du chemin headless

Statut: implemented

## Problème

Le mode headless est le seul contrat machine de Pyxis : un intégrateur écrit un
parseur d'après [`docs/EVENT_SCHEMA.md`](../../../EVENT_SCHEMA.md) et branche un
pipeline dessus. Rien ne le tenait.

Aucun test ne traversait `crates/agent-cli/src/headless.rs`. Ce qui était couvert
l'était une couche plus bas, sur `agent-core` et `agent-runtime`, donc la
composition elle-même, l'ordre dans lequel le client écrit ses lignes, ce qu'il y
met et ce qu'il vide, n'était prouvée nulle part. Une modification du rédacteur
d'événements passait toutes les portes.

Le document, lui, avait dérivé deux fois sans que rien ne le dise. Six variantes
d'`AgentEvent` n'avaient aucune ligne : `hook`, `plan`, `response_metadata`,
`response_item`, `provider_extension` et `unmapped_response_item`. Et les trois
préfixes d'identifiants de ses exemples, `th_`, `tu_`, `ev_`, étaient des chaînes
que le binaire n'a jamais émises : les vraies sont `thr_`, `trn_` et `evt_`
(`crates/agent-runtime/src/id.rs:191,196,206`). Un document qui décrit une chose
non vérifiable se lit exactement comme un document juste.

Ces deux dérives ont la même cause : le contrat écrit n'était comparé à rien.

## Décision

Le chemin headless publié tourne en test, rend une transcription JSONL, et cette
transcription est gelée dans un fichier comparé octet à octet.

Le déterminisme vient de l'injection, pas d'un traitement après coup.
`headless::run` reçoit son horloge (`FrozenClock`), son générateur
d'identifiants (`SequentialIds::starting_at`), son puits d'écriture
(`CapturedSink`) et son fournisseur (`ScriptedProvider`, qui rejoue un flux SSE
enregistré et refuse toute requête que le script n'a pas prévue). Rien n'est
masqué après coup : la sortie est déjà stable au moment où elle est écrite.

La comparaison est brute. Pas de `trim`, pas de normalisation de fins de ligne,
pas de champs volatils remplacés : les octets gelés sont les octets rendus. Le
dernier `\n` fait partie du contrat, et `.gitattributes` épingle l'arbre en
`-text` pour qu'aucun checkout ne le réécrive.

Un seul interrupteur écrit, `PYXIS_UPDATE_TRANSCRIPTS`, et il est isolé dans la
recette `regen` comme `PYXIS_UPDATE_SCHEMAS` et `PYXIS_UPDATE_CATALOGS` : une
régénération n'est pas une comparaison qui passe, c'est une comparaison qui n'a
pas eu lieu, et confondre les deux est la façon dont une porte certifie sa propre
sortie.

Le document a ensuite été rattrapé par cette preuve, sur trois points.

Les vingt-quatre variantes sont documentées, et une porte d'`agent-doc-gates`
compte les variantes de `crates/agent-core/src/event.rs`, compte les lignes de la
table publiée, et échoue en nommant l'écart. `thread_store_failed` est la seule
ligne déclarée comme ne venant pas d'`AgentEvent`, et elle l'est par son nom dans
le code de la porte plutôt que par une tolérance de comptage.

Chaque bloc `json` du document est soit une ligne d'une transcription gelée,
comparée octet à octet à travers un marqueur `<!-- transcription: <chemin>:<rang> -->`,
soit un bloc qui déclare au-dessus de lui-même pourquoi aucun scénario ne le
produit. Dix des treize blocs sont ancrés ; les trois restants sont `quota`,
qu'aucun backend rejoué ne sert, la `truncation` d'un `tool_result`, qu'aucun
scénario ne fait déborder, et `turn_diff`, qu'aucun scénario ne peut émettre
puisque tous tournent hors dépôt git. Un bloc sans marqueur est refusé : c'est
ainsi que les mauvais préfixes ont survécu.

L'ordre terminal énoncé était faux, et c'est le code qui a tranché. Le document
disait « `run_summary`, toujours dernier ». Deux transcriptions sur quatre
finissent sur une ligne `hook`, parce que `headless.rs` déclenche le cycle de vie
`Stop` **après** le résumé, `Stop` rapportant un arrêt qui n'a lieu qu'une fois le
tour vraiment terminé. La règle publiée est désormais celle qu'on observe :
exactement un `run_summary`, rien après lui sauf au plus une ligne `hook`, cette
ligne existant si et seulement si le run s'est terminé sur `end_turn`, et
`turn_diff` avant le résumé quand il existe. `terminal_order_verdict` la rend, et
elle est appliquée deux fois, aux octets produits et aux fichiers gelés.

Le sens de la correction n'était pas neutre : l'épique demandait au document de
rattraper le code, pas l'inverse. Modifier `headless.rs` pour que `run_summary`
redevienne dernier aurait changé un comportement observable de production pour
faire tenir une phrase, ce que la NFR d'empreinte du lot interdit.

## Alternatives écartées

**`insta`.** La dépendance de snapshot évidente, et elle ne peut pas tenir ce
contrat. `Snapshot::normalize`
(`~/.cargo/registry/src/*/insta-1.48.0/src/snapshot.rs:753-765`) applique
`trim_end()` à la ligne 758 avant de remplacer `\r\n` par `\n` à la 765 : le `\n`
terminal d'une transcription JSONL est mangé par la normalisation. Or ce `\n`
final est précisément ce qu'un consommateur qui lit ligne par ligne dépend de
voir. La conclusion ne porte pas sur la version 1.48 : elle vaut pour toute
version tant que cette normalisation existe, parce que le défaut n'est pas un
bogue mais le comportement voulu d'un outil qui compare des snapshots de texte et
non des octets. Une comparaison de `Vec<u8>` contre `fs::read` fait ce qu'il faut
en dix lignes, sans dépendance et sans exception à documenter.

**Un serveur mock en loopback.** Faire répondre un vrai serveur HTTP local plutôt
qu'implémenter `Provider` aurait couvert une couche de plus, celle du transport.
Il est écarté pour un conflit précis : les tests tournent sous
`#[tokio::test(start_paused = true)]`, où l'horloge du runtime avance d'elle-même
dès que les tâches sont oisives. Une socket réelle réintroduit de l'E/S que le
runtime ne contrôle pas, donc de l'ordonnancement à l'horloge murale, donc des
durées observables dans la sortie et une flakiness qui dépend de la machine. Le
lot échange une couche de transport contre un déterminisme complet, et le
transport est déjà couvert ailleurs, dans `agent-provider`.

**Le masquage après coup.** Produire la sortie avec l'horloge et les
identifiants réels, puis remplacer les champs volatils par des jetons avant
comparaison, est la technique habituelle et elle aurait demandé moins de couture.
Elle est refusée pour ce qu'elle détruit : un horodatage figé reste un témoin, un
horodatage remis à zéro n'en est plus un. Sous masquage, une ligne
`interrupted` qui perdrait son `duration_ms`, ou un identifiant qui cesserait
d'être corrélé à son tour, passerait la comparaison sans que rien ne le voie. Le
masquage rend le test vert en aveuglant l'assertion sur exactement les champs qui
prouvent la corrélation. Deux assertions le disent : la transcription ne contient
aucun chemin absolu et aucun horodatage d'horloge murale, et elles ne sont vraies
que parce que rien n'a été substitué.

**Lancer `CARGO_BIN_EXE_pyxis`.** Traverser le binaire publié plutôt que
`headless::run` est la version forte de la preuve, et elle bute sur deux
instructions que la section suivante mesure. Elle est écartée pour ce lot, avec
sa frontière écrite plutôt que laissée implicite.

**Ajouter une couture de credential ou une variable d'environnement de
fournisseur.** C'est ce qui rendrait le binaire lançable en test, et c'est refusé
sans mesure supplémentaire : une variable qui substitue un fournisseur sur le
chemin de production est une surface d'attaque permanente payée pour un gain de
test, et un point d'injection de credential dans le binaire publié est
exactement ce qu'un lot de test n'a pas le droit d'ouvrir. Le rejeu reste confiné
à `#[cfg(test)]` et disparaît du binaire publié.

## La distance entre `headless::run` et le binaire publié

Ce que le harnais ne traverse pas, entre l'analyse d'arguments
(`crates/agent-cli/src/main.rs:901`) et l'appel à `headless::run` (`main.rs:1988`).
Les numéros sont ceux d'aujourd'hui ; la spécification du lot citait
`main.rs:1101` et `main.rs:1510`, qui ont glissé de quelques lignes depuis.

| Instruction | Ligne | Couverte ailleurs |
|---|---|---|
| `install_tls_crypto_provider()` | 900 | Oui, `main.rs:2129` : le test prouve l'idempotence de l'installation. |
| `emit_schemas(...)` | 910 | Partiellement : `main.rs:2804` couvre l'analyse du drapeau, et `agent-app-server` couvre la génération elle-même. |
| `resolve_prompt(...)` | 914 | Oui, quatre tests à partir de `main.rs:2678`, stdin compris. |
| `observability::prepare` / `install_panic_hook` / `install_tracing` | 919, 923, 924 | Oui, dix tests dans `crates/agent-cli/src/observability.rs`. |
| `settings::resolve(...)` | 937 | Oui, quarante et un tests dans `crates/agent-cli/src/settings.rs`, plus trois appels dans `main.rs`. |
| `skills::load_all(...)` | 967 | Oui, vingt et un tests dans `crates/agent-cli/src/skills.rs`. Le harnais passe un catalogue vide. |
| `read_mcp_config(...)` | 979 | Le parsing l'est, dans `crates/agent-mcp/tests/config_load.rs`. L'instruction est de toute façon sautée en headless. |
| `context::project_documents(...)` | 993 | Oui, treize tests dans `crates/agent-cli/src/context.rs`. |
| `prepare_credential_before_sandbox(...)` | 995 | **Non.** Voir plus bas. |
| `agent_sandbox::resolve_writable_roots(...)` | 1014 | Oui, `crates/agent-sandbox/tests/writable_roots.rs`. |
| `open_spill_store(...)` | 1041 | Oui, `main.rs:2837`, `2865`, `2878`. Le harnais tourne sans puits de déversement. |
| `sandbox_policy_from_args(...)` | 1045 | Oui, cinq appels de test à partir de `main.rs:2426`. |
| `enforce_sandbox(...)` | 1046 | Non : le durcissement noyau ne peut pas être appliqué au processus de test sans contraindre tous les autres tests du binaire. |
| `OpenAiChatGptProvider::new(...)` | 1518 | **Non.** Voir plus bas. |
| `chatgpt.list_models().await` | 1539 | Non pour l'appel ; le catalogue et son analyse le sont, dans `crates/agent-provider/src/openai/catalog.rs`. |
| `spawn_proxy_with_approver(...)` | 1600 | Non pour le lancement ; la politique de proxy l'est, dans `crates/agent-sandbox`. |
| `connect_mcp_at_startup(...)` | 1693 | Sauté en headless. Le registre l'est, dans `crates/agent-mcp/src/server.rs`. |
| `CommandHooks::new(...)` | 1728 | Oui, `crates/agent-tools/src/hooks.rs`. Le harnais passe `NoHooks`, ce qui est aussi ce que fait le binaire sans hook déclaré. |
| `Registry::builder(&workspace)...` et ses vingt-neuf enregistrements | 1809 à 1902 | Partiellement : chaque outil a ses tests, mais la composition complète du registre, non. Le harnais en construit un à deux outils. |
| `registry.behavioral_guidelines()` | 1906 | Oui, dans `agent-tools` et `agent-mcp`, par outil. |
| `runtime::CliStepSource::with_code_mode(...)` | 1949 | Partiellement : le harnais appelle `CliStepSource::new`, donc la même source sans code-mode. |

Deux instructions, et deux seulement, empêchent de lancer `CARGO_BIN_EXE_pyxis`
sur un runner sans trousseau.

Le **chargement du credential** : `prepare_credential_before_sandbox`
(`main.rs:1133`) appelle `load_chatgpt_credential` (`main.rs:1109`), qui lit
`store::load(KEYRING_ACCOUNT)` (`main.rs:1110`). Sans trousseau peuplé, le binaire
sort avant d'atteindre quoi que ce soit d'observable, et le résultat est une
erreur de démarrage, pas une transcription.

La **construction du fournisseur** : `OpenAiChatGptProvider::new(...)`
(`main.rs:1518`) prend cette credential et un client HTTP réel. Même avec une
credential factice, le run partirait vers un endpoint OpenAI, ce que la NFR de
sécurité du lot interdit et qu'aucune porte ne doit pouvoir déclencher.

Le coût d'une couture, en surface de production ajoutée : rendre le binaire
lançable demande soit un point d'injection de credential, soit une variable
d'environnement choisissant le fournisseur. Le premier ajoute un chemin par
lequel une credential entre dans le processus autrement que par le trousseau, sur
le binaire publié, pour la durée de vie du produit. Le second ajoute une variable
qui redirige tout le trafic modèle, donc un levier d'exfiltration lisible dans
n'importe quel environnement partagé, et un levier que le bac à sable ne peut pas
rattraper puisqu'il est en amont de lui. Les deux coûtent une surface permanente
pour couvrir vingt-deux instructions dont dix-sept sont déjà couvertes ailleurs.

Recommandation : ne pas coudre. La distance restante se réduit sans toucher au
binaire publié, en montant `headless::run` d'un cran, c'est-à-dire en faisant
entrer dans le harnais la composition complète du registre plutôt que ses deux
outils. Si le besoin d'une preuve de bout en bout revient, la forme à évaluer
n'est pas une couture mais un test d'intégration séparé, hors des portes par
défaut, sur le modèle de `PYXIS_LIVE_PARITY`, qui dépense déjà un abonnement réel
et qu'aucune recette ne peut déclencher.

## Conséquences

`cargo test -p agent-cli --bin pyxis transcript` prouve désormais quatre
comportements sur le chemin publié : le tour nu, l'appel d'outil, l'interruption
et l'erreur de flux. Une modification du rédacteur d'événements se voit dans un
diff de transcription, en octets, avant d'atteindre un intégrateur.

Une inversion de l'ordre terminal dans `headless.rs` fait tomber deux tests :
la comparaison octet à octet et l'assertion d'ordre sur les octets produits. La
seconde existe pour cette raison : une porte seule qui échoue se lit comme un
fichier périmé et se régénère. Et une régénération aveugle ne referme pas la
brèche non plus, puisque l'assertion d'ordre relit aussi les fichiers gelés.

L'arbre gelé est borné, vingt-cinq kilo-octets par scénario et cent pour l'arbre :
une transcription est une preuve qu'un humain relit dans un diff, donc sa taille
fait partie de son contrat.

Ajouter un cinquième scénario est un répertoire sous
`crates/agent-cli/tests/transcripts/`, jamais une ligne de code de porte.

Ajouter une variante à `AgentEvent` sans sa ligne dans le document fait échouer
`cargo test -p agent-doc-gates`, avec le nom de la variante et l'écart de
comptage. Le coût est réel et assumé : une variante nouvelle demande maintenant
une ligne de documentation dans le même changement.

Aucune de ces décisions n'est un ADR. La seule qu'une pull request dans
`crates/` pourrait violer est le déterminisme par injection, et elle est déjà
tenue rouge par la comparaison octet à octet : retirer une injection ne produit
pas un désaccord d'opinion, il produit une transcription différente et une porte
qui échoue. Une décision que la machine tient n'a pas besoin d'un ADR pour être
respectée, et la frontière que fixe [`AGENTS.md`](../../../../AGENTS.md) et que
détaille [`docs/notes/README.md`](../../README.md) place donc le lot dans l'arbre
des notes. L'ordre terminal, lui, appartient au document et non au registre :
c'est un fait observable de `headless.rs`, publié dans
[`docs/EVENT_SCHEMA.md`](../../../EVENT_SCHEMA.md) et tenu par
`terminal_order_verdict`, pas une décision de structure.
