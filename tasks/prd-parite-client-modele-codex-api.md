[PRD]
# PRD: Parité du client modèle et des API provider Codex

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-01 | Arthur Jean | PRD résiduel fondé sur l'audit comportemental de Codex `ee0247f95a6fe2b094ba2253d82cae2a2b4c2dff` |

## Problem Statement

1. Pyxis implémente le chemin ChatGPT Responses en HTTP/SSE, mais ne reproduit pas le transport WebSocket, l'état de tour, la continuation incrémentale ni le fallback de session du client Codex courant.
2. Le contrat canonique Pyxis ne peut pas représenter plusieurs entrées et sorties observables de la référence: service tier, structured output, métadonnées de réponse, items complets, erreurs typées, quotas multi-limites et identifiants de diagnostic.
3. Le catalogue distant est partiel, non scopé par provider ou identité, sans ETag et jamais rafraîchi en mode headless. Un modèle accessible au compte peut donc être refusé ou exécuté avec des capacités obsolètes.
4. Pyxis ne fournit ni provider OpenAI configurable, ni les modes d'authentification et Bedrock de `model-provider`, ni les API auxiliaires compact, memories, images, search, files et Realtime de `codex-api`.
5. Les PRD de parité précédents sont terminés sur des baselines plus anciennes. Les rouvrir rendrait leur statut faux et mélangerait les contrats déjà prouvés avec les écarts résiduels découverts le 1er août 2026.

**Why now:** le client Codex courant utilise Responses WebSocket, des métadonnées et des surfaces API qui étaient explicitement hors scope des anciennes baselines Pyxis. Sans nouveau contrat figé et fixtures de conformité, chaque évolution du backend peut produire une divergence silencieuse de sortie, d'état, d'erreur ou de modèle disponible.

## Overview

Ce PRD ferme les écarts observables entre Pyxis et le client modèle de Codex au commit `ee0247f95a6fe2b094ba2253d82cae2a2b4c2dff`. Il couvre les entrées, sorties, états, erreurs et cas limites de `core/src/client.rs`, `model-provider` et `codex-api`, sans imposer leur structure interne à Pyxis.

L'implémentation conserve le sens des dépendances existant: les contrats provider-neutral vivent dans `agent-core`, les projections de wire et transports dans `agent-provider`, l'authentification dans `agent-auth`, le contexte immuable du tour dans `agent-runtime`, et les événements externes additifs dans `agent-app-server`. SSE et WebSocket doivent alimenter le même mapper canonique et produire les mêmes événements normalisés.

Le programme est ordonné en deux gates. Le gate Client couvre EP-001 à EP-004 et ferme la parité du chemin Responses ChatGPT. Le gate Provider API couvre EP-005 et EP-006, ajoute les providers/authentifications et surfaces API manquantes. Les anciens PRD restent des preuves historiques et ne sont jamais modifiés.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Familles d'écarts de l'audit encore ouvertes | au plus 4 sur 16 | 0 sur 16 |
| Fixtures Responses normalisées identiques entre SSE et WebSocket | 100 % des fixtures P0 | 100 % de la matrice baseline |
| Familles de providers exécutables | 2: ChatGPT et OpenAI configurable | 3: ajout de Bedrock |
| Familles d'API auxiliaires disponibles | au moins 3 sur 6 | 6 sur 6 |
| Résolution distante des modèles en mode headless | 100 % des scénarios de catalogue valides | 100 % avec isolation multi-compte et stale-on-error |

## Target Users

### Mainteneur et dogfooder Pyxis

- **Role:** Arthur ou un mainteneur qui compare Pyxis au client Codex courant et utilise Pyxis comme agent natif de Paneflow.
- **Behaviors:** lance des tours interactifs et headless, change de modèle, inspecte les traces, reproduit les échecs provider et met à jour la baseline Codex.
- **Pain points:** une divergence de terminal, de quota, de modèle ou d'item peut rester invisible; WebSocket et les endpoints annexes nécessitent aujourd'hui de repasser par Codex CLI.
- **Current workaround:** lancer Codex CLI, lire manuellement les événements bruts ou maintenir des hypothèses dans plusieurs PRD historiques.
- **Success looks like:** la matrice de conformité localise chaque divergence, les deux transports produisent le même contrat, et toute erreur contient une cause et les identifiants de diagnostic disponibles.

### Intégrateur du runtime ou de l'app-server

- **Role:** développeur d'une TUI, d'un IDE, d'un script ou d'un orchestrateur qui pilote Pyxis via ses crates ou son app-server.
- **Behaviors:** crée ou reprend un thread, soumet un tour, observe les items, suit l'usage et sélectionne un provider ou une API auxiliaire.
- **Pain points:** les événements actuels perdent des métadonnées et certains payloads; la disponibilité d'une capacité dépend d'un adapter ChatGPT unique et de valeurs embarquées.
- **Current workaround:** analyser le JSON provider, ignorer les items inconnus ou construire une intégration spécifique hors du contrat Pyxis.
- **Success looks like:** un contrat additif et versionné expose les mêmes résultats, erreurs et états quel que soit le transport ou le provider, sans accès aux secrets.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- [OpenAI Responses WebSocket](https://developers.openai.com/api/docs/guides/websocket-mode): la même suite d'événements que HTTP est transportée sur une connexion persistante, avec un seul response in-flight, une limite de connexion de 60 minutes et la continuation par `previous_response_id`.
- [OpenAI Responses streaming](https://platform.openai.com/docs/api-reference/responses-streaming): le protocole sépare création, items, deltas et terminaux `completed`, `failed` et `incomplete`.
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming): un concurrent direct conserve l'ordre start/delta/stop, les deltas JSON partiels, l'usage cumulatif et les erreurs pouvant arriver après HTTP 200.
- [Amazon Bedrock ConverseStream](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html): la couche provider-neutral expose capability probing et exceptions de stream explicites plutôt que de réduire tous les providers à du texte.
- **Market gap:** Pyxis peut offrir cette fidélité tout en gardant un cœur inspectable, durable et provider-neutral, sans devenir un fork de `codex-core`.

### Best Practices Applied

- [OpenAI backwards compatibility](https://developers.openai.com/api/reference/overview#backwards-compatibility): tolérer les nouveaux types/propriétés, préserver les champs connus et ne pas faire échouer un stream uniquement à cause d'une extension additive.
- [OpenAI authentication](https://developers.openai.com/api/reference/overview#authentication): conserver clés et workload identity côté client, transmettre des request IDs non sensibles et ne jamais journaliser les credentials.
- [Anthropic rate limits](https://platform.claude.com/docs/en/api/rate-limits): préserver limites, restants, resets et `retry-after` au lieu de les aplatir dans un message générique.
- `reqwest 0.12`, `eventsource-stream 0.2` et `tokio-tungstenite 0.28` couvrent déjà le transport nécessaire. Pyxis doit fournir lui-même compression de requête, reconnexion, terminalité, backpressure, close handshake et timeouts.

## Assumptions & Constraints

### Assumptions (to validate)

- Le backend ChatGPT subscription accepte le WebSocket Responses et les headers du client Codex courant. Cette hypothèse est à risque élevé et doit être tranchée par US-007 avant toute implémentation WebSocket.
- Les événements SSE et WebSocket peuvent être normalisés par un mapper unique sans perdre les métadonnées propres au transport.
- Les API compact, memories, images, search, files et Realtime doivent être capability-gated parce que leur disponibilité peut varier selon le provider et l'identité.
- Les types sérialisés actuels peuvent être étendus additivement sans réécrire les sessions historiques.
- Les dépendances réseau existantes suffisent pour Responses; Bedrock peut exiger l'ajout ciblé du SDK AWS officiel si l'implémentation de référence ne peut pas être reproduite proprement avec le graphe actuel.

### Hard Constraints

- La baseline normative est `/home/arthur/dev/codex` au commit `ee0247f95a6fe2b094ba2253d82cae2a2b4c2dff`.
- `/home/arthur/dev/codex` est read-only, y compris ses fichiers `.codex` non suivis.
- `agent-core` ne dépend pas de `agent-provider`, `agent-auth`, `agent-runtime`, `agent-app-server` ou d'un type OpenAI/Bedrock.
- Les sessions JSONL existantes restent lisibles sans migration destructive ni réécriture.
- Aucun token, credential, URL signée, payload sensible ou identifiant de compte brut n'est écrit dans les logs, fixtures, caches ou événements externes.
- Les PRD et trackers marqués `DONE` restent immuables; ce PRD est la seule source de suivi pour ces écarts résiduels.
- Le scope est plafonné à 20 stories. Toute capacité supplémentaire exige un PRD séparé.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - format Rust déterministe.
- `cargo clippy --workspace --all-targets` - lints du workspace sur toutes les cibles.
- `cargo test --workspace --no-fail-fast` - suite complète sans masquer les échecs suivants.
- `cargo test -p agent-provider --test conformance --no-fail-fast` - fixtures exactes des requêtes, événements et erreurs provider.
- `cargo test -p agent-app-server --test schemas --no-fail-fast` - schémas externes générés identiques aux types sérialisés.

Additional gates:

- Tout changement de wire ajoute une fixture golden sans token, compte, URL signée ou contenu de session réel.
- Toute nouvelle variante sérialisée prouve le décodage des fixtures de sessions et clients antérieurs.
- Toute reprise substantielle de code Apache-2.0 met à jour l'inventaire de provenance applicable sans modifier les textes de licence existants.

## Epics & User Stories

### EP-001: Contrat résiduel et preuve de parité

Figer la cible actuelle et étendre verticalement le contrat canonique jusqu'à l'app-server avant de multiplier les transports et providers.

**Definition of Done:** chaque écart audité est lié à une fixture ou une story, les nouveaux champs sont provider-neutral et additifs, et aucun événement observable n'est silencieusement abandonné par l'app-server.

#### US-001: Figer la matrice résiduelle au commit courant

**Description:** As a mainteneur Pyxis, I want une matrice de parité liée au commit audité so that chaque implémentation et chaque revue utilisent la même définition observable.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**

- [ ] Given la référence au commit `ee0247f95a6fe2b094ba2253d82cae2a2b4c2dff`, when le générateur inventorie les contrats, then les 16 familles d'écarts de l'audit sont présentes avec entrée, sortie, état, erreur, cas limite et preuve attendue.
- [ ] Given un écart déjà couvert par un PRD `DONE`, when la matrice est produite, then elle référence sa preuve existante et n'ouvre une nouvelle ligne que pour le résiduel courant.
- [ ] Given la référence absente, dirty sur un fichier suivi ou positionnée sur un autre commit, when la vérification démarre, then elle échoue avec le chemin et le commit attendus sans modifier la référence.
- [ ] Les fichiers non suivis sous `/home/arthur/dev/codex/.codex` ne sont ni lus comme source normative, ni modifiés, ni inclus dans une fixture.

#### US-002: Étendre le canonique et sa projection externe

**Description:** As an intégrateur, I want un contrat provider-neutral capable de représenter les requêtes, réponses, métadonnées et erreurs de la baseline so that SSE, WebSocket et l'app-server exposent la même information.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given une requête enrichie, when elle traverse le canonique, then instructions explicites, service tier, output schema strict, stream options, client metadata et cache key restent distincts et sérialisables.
- [ ] Given une réponse baseline, when elle traverse le canonique puis l'app-server, then response ID, modèle effectif, service tier effectif, request ID, turn state, ETag, safety, vérifications, modération et état de reasoning restent observables.
- [ ] Given une ancienne session ou un ancien message app-server sans les nouveaux champs, when il est décodé, then les valeurs absentes prennent un défaut documenté et aucun historique n'est réécrit.
- [ ] Given un événement ou item inconnu, vide ou supérieur à la borne configurée, when il est projeté, then il est conservé sous une forme bornée et redacted ou refusé avec une erreur typée; il n'est jamais silencieusement abandonné par un wildcard.

### EP-002: Requêtes HTTP et streaming SSE fidèles

Aligner le chemin Responses HTTP/SSE avant de le réutiliser comme contrat de référence pour WebSocket.

**Definition of Done:** les corps, headers, items, deltas, terminaux, erreurs, usages et quotas correspondent aux fixtures Codex et toute fermeture sans terminal produit une erreur.

#### US-003: Compléter le corps de requête Responses

**Description:** As a caller modèle, I want exprimer tous les contrôles de requête de la baseline so that le backend reçoit exactement l'intention du tour.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given un modèle compatible, when le corps est construit, then `service_tier`, structured output strict, `stream_options`, `client_metadata`, verbosity, cache key et `reasoning.encrypted_content` correspondent à la fixture de référence.
- [ ] Given Responses Lite, when le corps est construit, then additional tools, instructions développeur, reasoning context et parallel tool calls suivent le dialecte Lite sans dupliquer les instructions.
- [ ] Given une instruction absente ou explicitement vide, when la requête est encodée, then Pyxis n'injecte pas `You are a helpful assistant.` et reproduit la présence ou omission de la référence.
- [ ] Given un output schema invalide, un tier non supporté ou un contrôle incompatible avec le modèle, when la requête est validée, then une erreur locale typée est retournée avant credential et réseau.

#### US-004: Aligner configuration HTTP, headers et compression

**Description:** As an opérateur provider, I want configurer le transport et conserver ses métadonnées so that les déploiements compatibles et les diagnostics se comportent comme Codex.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given une configuration provider, when une URL est construite, then base URL, path, query params, headers par défaut, retry policy et idle timeout composent la même requête que la référence.
- [ ] Given un tour ChatGPT, when la requête part, then client request ID, session/thread IDs, originator, beta features, subagent, compatibility, attestation, Responses Lite et turn state applicables sont envoyés.
- [ ] Given la compression activée pour un provider compatible, when le JSON est envoyé, then le body est pré-encodé en zstd, `Content-Encoding` est correct et le contenu décompressé est byte-equivalent au JSON non compressé.
- [ ] Given une URL, un header, une compression ou une policy invalide, when le request builder est créé, then il retourne une erreur typée sans socket ouverte et sans inclure de credential dans le message.

#### US-005: Préserver items, deltas et métadonnées SSE

**Description:** As a consommateur de stream, I want recevoir chaque item et delta au moment où le backend l'émet so that l'ordre, la latence et le contenu restent fidèles.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-003

**Acceptance Criteria:**

- [ ] Given des arguments function ou custom fragmentés, when chaque delta SSE arrive, then un delta canonique correspondant est émis immédiatement et le payload terminal autoritaire ne produit aucun doublon.
- [ ] Given des reasoning summary/content events, when ils sont mappés, then item ID, summary/content index, part added, text done et deltas restent distincts sans injection artificielle de saut de ligne.
- [ ] Given created, output item added/done ou response metadata, when l'événement arrive, then le payload complet connu et les métadonnées de header ou d'événement sont projetés dans l'ordre.
- [ ] Given un type additif inconnu ou une frame JSON malformée isolée, when le stream continue ensuite avec un terminal valide, then l'extension est bornée et observable ou la frame est diagnostiquée puis ignorée; le stream ne redémarre pas et les deltas déjà livrés ne sont pas dupliqués.

#### US-006: Aligner terminaux, erreurs, quotas et usage

**Description:** As a runtime agentique, I want distinguer succès, réponse incomplète et erreurs provider so that retry, compaction et message utilisateur prennent la bonne branche.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004, US-005

**Acceptance Criteria:**

- [ ] Given `response.completed`, when il est reçu, then le response ID, usage et `end_turn` sont émis avant un unique terminal de succès; toute fermeture antérieure est une erreur.
- [ ] Given `response.incomplete`, `response.failed` ou un `response.done` non reconnu par la baseline, when il est traité, then Pyxis produit la même erreur typée et la même classification de retry que la référence, sans terminal de succès synthétique.
- [ ] Given context overflow, quota, usage not included, cyber policy, invalid prompt/image, surcharge, retry delay ou 401/403, when l'erreur arrive par HTTP ou après HTTP 200, then sa catégorie, son délai et ses request/auth diagnostic IDs disponibles sont conservés.
- [ ] Given plusieurs familles de limites ou un compteur de tokens supérieur à `u32::MAX`, when les données sont parsées, then limit ID/name, primary/secondary, credits, plan, promo, reached type et compteurs `i64` restent exacts sans wrap ni fusion de pools.

### EP-003: Session Responses WebSocket

Ajouter un transport persistant borné qui réutilise le même contrat que SSE et revient vers HTTP de façon déterministe.

**Definition of Done:** un tour peut préconnecter, streamer, continuer incrémentalement, renouveler ou fermer une connexion, et basculer vers SSE sans divergence d'événement ni double exécution.

#### US-007: Valider le contrat WebSocket ChatGPT en live

**Description:** As a mainteneur, I want une preuve opt-in du handshake WebSocket subscription so that l'implémentation ne repose pas sur l'API publique ou des headers supposés.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given une autorisation opérateur explicite et un credential ChatGPT valide, when le probe se connecte, then il consigne URL, upgrade, headers non sensibles, metadata initiale, terminal et close code nécessaires sans enregistrer token ou account ID.
- [ ] Given un response `generate=false`, when le probe préchauffe puis envoie un tour réel, then il vérifie si le response ID peut être réutilisé et si le turn state est stable dans le tour.
- [ ] Given 401, 403, 426 ou endpoint absent, when le probe échoue, then le verdict distingue auth, incompatibilité et absence de capability, bloque US-008/US-009 si nécessaire et n'essaie pas silencieusement l'URL publique.
- [ ] Le probe est désactivé par défaut, possède un timeout total de 60 secondes et ne modifie ni session utilisateur ni cache persistant.

#### US-008: Implémenter le cycle de vie WebSocket borné

**Description:** As a caller modèle, I want une connexion Responses persistante avec backpressure et close handshake so that les tours à faible latence ne compromettent pas la fiabilité du runtime.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-007

**Acceptance Criteria:**

- [ ] Given un provider WebSocket-capable, when une session de tour démarre, then la connexion est préconnectable, authentifiée avec les mêmes scopes que HTTP et limitée à un response in-flight.
- [ ] Given une pression d'écriture, when le sink n'est pas ready, then le producteur attend ou échoue selon la borne configurée; aucun message n'est perdu et le write buffer n'est jamais illimité.
- [ ] Given cancellation ou fin de session, when la connexion ferme, then la close frame est envoyée, le peer est pollé et toutes les tâches atteignent un état terminal en moins de 5 secondes.
- [ ] Given frame binaire inattendue, idle timeout, message au-dessus de 64 MiB ou close avant `response.completed`, when le reader traite le flux, then il retourne une erreur typée et ne publie aucun succès.

#### US-009: Réutiliser état de tour et réponses incrémentales

**Description:** As a runtime de tour, I want envoyer uniquement l'extension d'une requête compatible so that le backend réutilise la réponse précédente sans altérer le transcript logique.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005, US-008

**Acceptance Criteria:**

- [ ] Given une requête qui étend strictement la précédente avec les mêmes propriétés non-input, when elle est envoyée, then seul le suffixe est transmis avec le bon `previous_response_id`.
- [ ] Given les output items du serveur précédent, when la compatibilité est évaluée, then ils font partie de la baseline et ne sont pas renvoyés comme nouvel input.
- [ ] Given un changement de modèle, instructions, tools, service tier, schema, metadata significative ou préfixe input, when la requête est préparée, then Pyxis envoie un corps complet sans `previous_response_id`.
- [ ] Given `previous_response_not_found` ou une réponse sans ID, when la continuation échoue, then l'état incrémental est invalidé et un unique retry complet est tenté sans dupliquer les événements déjà acceptés.

#### US-010: Garantir fallback et équivalence SSE/WebSocket

**Description:** As an utilisateur, I want que le runtime sélectionne ou abandonne WebSocket sans changer le résultat observable so that le transport reste une optimisation de session.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006, US-009

**Acceptance Criteria:**

- [ ] Given les mêmes fixtures de réponse, when elles passent par SSE et WebSocket, then les séquences canoniques, items assemblés, usage, terminaux et erreurs sont identiques.
- [ ] Given HTTP 426 pendant l'upgrade ou retry budget WebSocket épuisé, when le fallback s'active, then WebSocket reste désactivé pour cette session et le tour repart une seule fois sur HTTP.
- [ ] Given un 401 récupérable pendant handshake ou stream create, when la récupération réussit, then une seule nouvelle tentative utilise le credential actualisé; un échec permanent reste terminal.
- [ ] Given une nouvelle session après un fallback antérieur, when elle démarre, then elle réévalue la capability WebSocket et ne réutilise ni connexion, turn state ni response ID de l'ancienne session.

### EP-004: Outils, items et catalogue de modèles

Fermer les pertes de vocabulaire et faire du catalogue distant une source scoped et exploitable dans tous les modes.

**Definition of Done:** tous les outils et items de la baseline ont un round-trip, les extensions restent visibles, et interactive/headless résolvent le même catalogue pour une identité donnée.

#### US-011: Compléter l'algèbre des outils Responses

**Description:** As a planificateur d'outils, I want représenter toutes les formes de tools de la baseline so that le provider n'invente ni ne supprime de capacité.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given function, freeform, namespace, tool search ou web search, when le plan traverse le canonique puis Responses, then type, nom, exécution, filters, location, context size et grammaire applicables restent exacts.
- [ ] Given une function, when elle est encodée, then `strict`, `defer_loading`, input schema et output schema conservent les valeurs du tool au lieu d'être forcés globalement.
- [ ] Given un provider ou modèle qui ne supporte pas un tool kind, when le plan est validé, then une incompatibilité typée est retournée avant credential et réseau.
- [ ] Given un schéma strict invalide, un namespace dupliqué ou une référence différée introuvable, when le catalogue est construit, then aucun outil partiel n'est exposé ni dispatchable.

#### US-012: Préserver le vocabulaire complet des response items

**Description:** As a consommateur de transcript, I want conserver chaque output item supporté so that web, image, search, shell, agent et compaction restent inspectables et rejouables.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-011

**Acceptance Criteria:**

- [ ] Given message, agent message, reasoning, local shell, function/custom call/output, tool search call/output, web search, image generation, compaction ou context compaction, when l'item arrive, then son payload baseline effectue un round-trip sans perte.
- [ ] Given `output_item.added` puis `output_item.done`, when les deux arrivent, then l'identité et l'état de l'item restent corrélés et le terminal autoritaire ne crée aucun doublon.
- [ ] Given un item connu malformé ou un type inconnu, when il est traité, then il devient une représentation Other bornée avec diagnostic, ou une erreur de contrat avant publication; son contenu n'est pas réduit au seul type.
- [ ] Given un payload contenant credential, URL signée ou champ marqué sensible, when il est persisté ou projeté, then la valeur est redacted tout en conservant le type et la cause de perte.

#### US-013: Rendre le catalogue fidèle, scoped et disponible en headless

**Description:** As a utilisateur multi-provider, I want résoudre les modèles accessibles à mon identité avec leurs capacités complètes so that interactive et headless exécutent le même contrat.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**

- [ ] Given `/models`, when le catalogue est décodé, then service tiers, reasoning controls, outils, context windows/max/effective percent, modalities, upgrade, visibility et autres champs baseline sont conservés ou diagnostiqués explicitement.
- [ ] Given un ETag de catalogue ou `X-Models-Etag` sur une response, when il change, then le refresh scoped est déclenché et l'installation du nouveau snapshot reste atomique.
- [ ] Given interactive ou headless avec le même provider, endpoint et identité, when la résolution démarre, then les deux modes utilisent le même chemin de refresh et le même descriptor; la version client par défaut vient du build courant, pas d'une constante manuelle périmée.
- [ ] Given un compte ou endpoint différent, un catalogue malformé, vide, trop grand ou un échec réseau transitoire, when le refresh tourne, then aucune entrée ne fuit entre scopes, le dernier snapshot valide peut rester stale et le cache-disabled path reste disponible.

### EP-005: Providers configurables et authentification

Étendre la largeur provider après stabilisation du contrat et des transports, sans affaiblir les frontières de credentials.

**Definition of Done:** ChatGPT, OpenAI-compatible configuré et Bedrock construisent un compte, un catalogue, des capabilities et un stream canonique avec erreurs d'authentification explicites.

#### US-014: Ajouter un provider OpenAI-compatible configuré

**Description:** As an opérateur, I want cibler un endpoint Responses compatible avec sa configuration so that Pyxis n'est pas lié au backend ChatGPT.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004, US-013

**Acceptance Criteria:**

- [ ] Given un provider configuré, when il est créé, then name, base URL, query params, headers, retry policy, idle timeout, WebSocket capability et wire API pilotent chaque endpoint.
- [ ] Given un endpoint Azure Responses, when une requête est construite, then `store` et les conventions propres à Azure correspondent à la baseline sans affecter ChatGPT ou OpenAI standard.
- [ ] Given une configuration statique de modèles, when elle est fournie, then le provider utilise un manager sans fetch; sans catalogue statique, il utilise le manager distant scoped.
- [ ] Given URL non HTTP(S), header interdit, combinaison auth/provider incohérente ou capability absente, when le provider démarre, then il échoue avant réseau avec le champ fautif et sans fallback vers ChatGPT.

#### US-015: Aligner modes d'auth et récupération 401

**Description:** As an utilisateur provider, I want utiliser l'identité autorisée et comprendre son échec so that Pyxis récupère les expirations sans masquer les erreurs permanentes.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**

- [ ] Given API key, bearer expérimental, ChatGPT OAuth, PAT, headers préconstruits ou agent identity, when l'auth est résolue, then seuls les headers prévus pour ce provider et ce scope sont attachés.
- [ ] Given un compte FedRAMP ou ChatGPT, when les headers sont construits, then account ID et indicateur FedRAMP applicables sont présents sans être exposés dans logs, cache keys ou erreurs.
- [ ] Given un 401, when une récupération est disponible, then une seule étape scoped est tentée; succès, échec permanent, échec transitoire et récupération indisponible restent des états distincts.
- [ ] Given révocation, changement de compte ou bootstrap agent identity indisponible, when l'état change en session, then les credentials et catalogues de l'ancien scope ne sont pas réutilisés et le fallback autorisé est explicite.

#### US-016: Ajouter le provider Amazon Bedrock

**Description:** As an utilisateur Bedrock, I want exécuter un modèle via son auth et ConverseStream so that le contrat multi-provider de Pyxis possède la même largeur que la référence.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-015

**Acceptance Criteria:**

- [ ] Given une configuration Bedrock valide, when le provider est créé, then région, modèle, auth AWS autorisée, account state et modèles préférés sont résolus sans header OpenAI.
- [ ] Given un ConverseStream texte, tool use, usage, stop ou stream exception, when il est normalisé, then il produit les mêmes catégories canoniques que Responses pour le comportement équivalent.
- [ ] Given structured output ou tool schema supporté, when la requête part, then le capability probing autorise le contrat; un schéma non supporté retourne une erreur locale ou Bedrock 400 typée sans retry.
- [ ] Given credential AWS absent, API key Bedrock utilisée sur un autre provider ou feature WebSocket non supportée, when l'opération est demandée, then une erreur d'account/capability explicite est retournée sans fallback de provider.

### EP-006: API auxiliaires de codex-api

Porter les clients auxiliaires comme capabilities provider-scoped, avec types et erreurs propres, sans les simuler par une génération textuelle ordinaire.

**Definition of Done:** les six familles compact, memories, images, search, files et Realtime sont appelables, capability-gated et couvertes par fixtures de succès, erreur et timeout.

#### US-017: Porter remote compact et memories

**Description:** As a runtime, I want utiliser les primitives de compaction et mémoire du provider so that leur sortie structurée ne soit pas remplacée par une réponse texte ordinaire.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006, US-014

**Acceptance Criteria:**

- [ ] Given un provider compatible, when `/responses/compact` est appelé, then il retourne la fenêtre de `ResponseItem` structurés, conserve le turn state et n'invente aucun response ID.
- [ ] Given un `MemorySummarizeInput`, when `memories/trace_summarize` répond, then chaque output typé est décodé et corrélé à l'appel.
- [ ] Given succès distant, when le runtime applique la compaction, then la mutation locale intervient seulement après validation complète du payload et commit durable.
- [ ] Given capability absente, timeout, JSON malformé ou erreur provider, when l'appel échoue, then le transcript local reste byte-identique et l'erreur identifie l'opération.

#### US-018: Porter génération et édition d'images

**Description:** As a caller provider, I want générer ou éditer une image via le client typé so that la sortie image et ses métadonnées restent distinctes d'un message texte.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**

- [ ] Given une `ImageGenerationRequest` valide, when `images/generations` répond, then URLs ou données, format, taille, qualité et métadonnées baseline sont décodés sans perte.
- [ ] Given une `ImageEditRequest` valide, when `images/edits` répond, then image source, masque et prompt utilisent le même auth/provider scope que la génération.
- [ ] Given une réponse image contenant un résultat sensible ou volumineux, when elle est tracée, then le contenu binaire/URL signée est omis des logs et seule la métadonnée non sensible est observable.
- [ ] Given capability absente, entrée invalide, HTTP non-2xx ou JSON malformé, when l'appel échoue, then une erreur d'image typée est retournée sans bloc texte synthétique.

#### US-019: Porter search et le cycle de fichiers

**Description:** As an intégrateur, I want rechercher et téléverser des fichiers via des clients dédiés so that les résultats et ressources sont adressables sans contourner le provider.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**

- [ ] Given une `SearchRequest`, when `alpha/search` répond, then commandes, résultats, items et pagination baseline sont décodés dans un `SearchResponse` typé.
- [ ] Given un fichier au plus égal à 512 MiB, when create, blob upload, finalize et download link réussissent, then l'ID, URI `sediment://`, nom, taille, MIME et URL finale sont corrélés.
- [ ] Given une URL d'upload sur un autre host, when le blob est envoyé, then les credentials provider ne sont jamais forwardés et les request IDs Azure nécessaires restent diagnostiquables sans URL signée dans les logs.
- [ ] Given fichier supérieur à 512 MiB, taille incohérente, upload non ready/failed ou search non autorisé, when l'opération démarre, then elle échoue à la première frontière applicable sans ressource partielle présentée comme disponible.

#### US-020: Porter Realtime call et Realtime WebSocket

**Description:** As an intégrateur temps réel, I want créer et piloter un appel Realtime typé so that SDP, call ID et événements audio/texte suivent le provider configuré.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-008, US-014

**Acceptance Criteria:**

- [ ] Given un SDP et une session valides, when `realtime/calls` répond, then le SDP réponse et le call ID du header Location sont retournés comme une unité typée.
- [ ] Given un parser FramelessBidi, V1 ou RealtimeV2 supporté, when la sideband WebSocket se connecte, then session update, context append et événements utilisent le framing attendu du provider.
- [ ] Given cancellation ou close normal, when l'appel s'arrête, then les writers/readers terminent en moins de 5 secondes et aucun événement du call ne fuit vers une autre session.
- [ ] Given Location absent, SDP invalide, frame inconnue, timeout ou capability Realtime absente, when l'opération échoue, then l'erreur nomme la phase et aucune connexion orpheline ne reste active.

## Functional Requirements

- FR-01: Le système doit figer et vérifier la baseline Codex par commit avant d'exécuter les fixtures de parité.
- FR-02: Le système doit représenter additivement tous les champs de requête et de réponse observables de la baseline.
- FR-03: SSE et WebSocket doivent produire un même flux canonique pour un même flux logique provider.
- FR-04: Une fermeture HTTP/SSE/WebSocket sans terminal baseline valide doit être une erreur.
- FR-05: `response.incomplete` et `response.failed` ne doivent pas devenir un succès synthétique.
- FR-06: Les deltas tool/reasoning doivent être émis dans leur ordre provider sans buffering jusqu'à l'item terminal.
- FR-07: Les erreurs doivent conserver catégorie, retry delay, request IDs et diagnostics d'identité disponibles.
- FR-08: Les quotas doivent préserver chaque limit ID/name, fenêtre, crédit, plan, promo et cause atteinte.
- FR-09: Les compteurs de tokens doivent accepter la plage `i64` positive sans cast tronquant.
- FR-10: Les événements et items additifs inconnus doivent rester bornés et observables sans interrompre un stream valide.
- FR-11: Le WebSocket doit accepter un seul response in-flight et réutiliser `previous_response_id` uniquement pour une extension compatible.
- FR-12: Le fallback WebSocket vers HTTP doit être session-scoped, borné et sans double publication.
- FR-13: Les outils namespace, tool search, web search, function et custom doivent effectuer un round-trip sans forcer `strict`.
- FR-14: Les response items connus doivent conserver leur payload complet et leur identité added/done.
- FR-15: Les catalogues doivent être scopés par provider, endpoint et identité non secrète, avec ETag, stale-on-transient-error et chemin sans cache.
- FR-16: Le mode headless doit utiliser le même chemin de résolution distante que le mode interactif.
- FR-17: Le système doit supporter ChatGPT OAuth, OpenAI-compatible configuré et Amazon Bedrock.
- FR-18: Les échecs de refresh permanent et transitoire doivent rester distincts.
- FR-19: Chaque API auxiliaire doit être capability-gated et retourner un type propre à l'opération.
- FR-20: Le système ne doit PAS forwarder un credential provider à un host d'upload tiers.

## Non-Functional Requirements

- **Conformité:** 100 % des fixtures P0 doivent produire les mêmes événements normalisés, erreurs et payloads que la baseline figée.
- **Performance stream:** aucun transport ne bufferise la réponse complète; chaque delta provider inférieur ou égal à 64 KiB est projeté avant la lecture de l'événement suivant.
- **WebSocket:** un maximum de 1 response in-flight par connexion, message maximal 64 MiB, frame maximale 16 MiB et write buffer maximal explicitement borné.
- **Timeouts:** connect, headers, read-idle, write, close et endpoint unary possèdent tous une durée configurable; les tests de timeout doivent terminer dans les 100 ms de temps Tokio simulé après l'échéance.
- **Shutdown:** toute tâche réseau créée par un client atteint un état terminal ou est abortée avec cause en moins de 5 secondes après cancellation.
- **Catalogue:** `/models` conserve la limite de réponse de 4 MiB et le timeout total de 5 secondes par défaut.
- **Files:** toute taille supérieure à 536 870 912 octets est refusée avant création de ressource distante.
- **Sécurité:** 0 secret, token, account ID brut, payload binaire ou URL signée dans 100 % des fixtures de logs/redaction.
- **Compatibilité:** 100 % des fixtures de sessions et schémas app-server antérieures au PRD restent décodables.
- **Fiabilité:** au plus 1 récupération 401 et 1 retry complet après `previous_response_not_found` par sampling.

## Edge Cases & Error States

Systematic coverage of unhappy paths. Evidence shows earlier defect discovery significantly reduces cost.

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Catalogue vide | `/models` retourne zéro modèle visible | Conserver le dernier snapshot valide ou l'embarqué, diagnostic scoped | `model_catalog_empty` |
| 2 | Headers retardés | TCP/TLS établi, aucun header | Header timeout, erreur retryable bornée | `provider_header_timeout` |
| 3 | Frame SSE malformée | JSON invalide entre deux événements valides | Diagnostiquer, ignorer la frame et poursuivre | `malformed_sse_event_ignored` |
| 4 | Réponse incomplète | `response.incomplete` avec raison | Erreur typée avec raison, aucun succès | `incomplete_response` |
| 5 | Close WebSocket prématuré | Close avant completed | Erreur stream et invalidation de la connexion | `websocket_closed_before_terminal` |
| 6 | Upgrade refusé | HTTP 426 | Fallback HTTP pour la session | `websocket_fallback_http` |
| 7 | Réponse précédente évincée | `previous_response_not_found` | Invalider l'incrémental, retry complet unique | `previous_response_not_found` |
| 8 | Compte changé en session | Nouvelle identité/account ID | Nouveau scope auth/catalogue, aucune réutilisation | `provider_scope_changed` |
| 9 | Payload inconnu volumineux | Item/event au-dessus de la borne | Tronquer/redacter avec taille originale ou refuser typé | `provider_payload_too_large` |
| 10 | Compteur élevé | Tokens supérieurs à `u32::MAX` | Conserver valeur `i64`, aucun wrap | `token_counter_out_of_range` seulement au-delà de `i64::MAX` |
| 11 | Deux pools quota | Headers `codex` et limite secondaire nommée | Émettre deux snapshots identifiés | aucune fusion silencieuse |
| 12 | Refresh transitoire | Réseau indisponible pendant refresh | Erreur transitoire distincte de reconnect permanent | `auth_refresh_transient` |
| 13 | Fichier trop grand | Taille supérieure à 512 MiB | Refus local avant réseau | `file_too_large` |
| 14 | API non supportée | Capability absente sur provider | Refus local, aucun fallback vers Responses textuel | `provider_capability_unsupported` |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Le backend ChatGPT subscription n'accepte pas le contrat WebSocket attendu | High | High | US-007 est un gate opt-in; no-go explicite et aucun faux fallback vers l'API publique |
| 2 | Les événements inconnus transportent des données sensibles ou non bornées | Med | High | Borne 64 KiB, redaction avant persistance/projection, fixtures adversariales |
| 3 | Un cache catalogue fuit entre comptes ou endpoints | Med | High | Scope provider + endpoint + identité non secrète, invalidation sur account switch, tests concurrents |
| 4 | Une configuration custom forwarde un credential au mauvais host | Med | High | Auth liée au provider et à l'origin, upload tiers sans headers provider, tests de capture |
| 5 | Le scope de 20 stories dérive pendant l'implémentation | High | Med | Aucun ajout de story; nouvelle capacité déplacée dans un PRD séparé, gates Client puis Provider API |
| 6 | Le SDK Bedrock augmente fortement le graphe ou impose une auth incompatible | Med | Med | Dépendance officielle ciblée seulement si exigée, mesure `cargo tree`, provider isolé et capability-gated |
| 7 | La baseline Codex continue d'évoluer pendant l'implémentation | High | Med | Commit figé, matrice générée, mise à jour de baseline traitée comme changelog explicite et non comme drift silencieux |

## Non-Goals

Explicit boundaries for this version:

- Refaire le TUI, la boucle durable, Code Mode, multi-agent, MCP, sandbox ou les outils terminal déjà livrés par les PRD `DONE`.
- Copier l'architecture interne de `codex-core`, créer un fork ou introduire ses types provider-specific dans `agent-core`.
- Ajouter des adapters Anthropic, Gemini, OpenRouter ou Ollama spécifiques; seuls le provider OpenAI-compatible configuré et Bedrock présents dans la référence sont inclus.
- Ajouter une UI de quota, modèle, image, search ou Realtime; ce PRD livre les contrats et clients, pas leur présentation.
- Réécrire les sessions historiques, les caches app-managed, les PRD/status historiques ou les données utilisateur existantes.
- Garantir une API auxiliaire sur un provider qui ne l'annonce pas; l'absence reste une capability typée.

## Files NOT to Modify

- `/home/arthur/dev/codex/**` - référence read-only, y compris `.codex` et ses fichiers non suivis.
- `tasks/prd-*-status.json` existants - états historiques; seul le tracker de ce PRD est modifiable.
- `tasks/prd-*.md` existants - décisions et critères historiques immuables.
- `.pyxis/sessions/**` et toute base/session/cache app-managed - aucune migration destructive ou édition manuelle.
- `spikes/**` - artefacts jetables des premières phases, hors workspace actuel.
- `crates/agent-sandbox/**` - politique de sécurité orthogonale; le transport doit réutiliser son routage sans modifier ses invariants.

## Technical Considerations

- **Architecture:** faut-il étendre `StreamEvent` par variantes explicites ou introduire une enveloppe metadata provider-neutral? Recommandation: variantes explicites pour les comportements consommés, enveloppe bornée uniquement pour l'additif inconnu. Engineering to confirm exhaustiveness cost.
- **Transport:** faut-il une trait object interne ou un enum SSE/WebSocket dans `agent-provider`? Recommandation: un seam interne partagé qui rend impossible l'usage de deux mappers divergents. Engineering to confirm allocation and test ergonomics.
- **Persistence:** quelles métadonnées doivent entrer dans le journal durable plutôt que rester des événements éphémères? Recommandation: persister response ID, effective tier et items nécessaires à la reprise; garder request IDs et diagnostics hors transcript. Engineering to confirm backward compatibility.
- **Cache:** quelle identité non secrète clé le catalogue? Recommandation: provider ID + base URL normalisée + account/workload identity fingerprint, sans token. Engineering to confirm lifecycle on account switch.
- **WebSocket:** quelles bornes de buffers et quel renouvellement avant la limite de 60 minutes reproduisent le mieux la référence? Recommandation: limites `tokio-tungstenite` explicites et reconnexion sur l'erreur provider dédiée. Engineering to confirm provider defaults.
- **Bedrock:** faut-il le SDK AWS officiel ou une frontière HTTP/SigV4 existante? Recommandation: SDK officiel si l'auth chain et ConverseStream ne peuvent pas être couverts sans code cryptographique maison. Engineering to mesurer coût binaire et temps de build avant décision.
- **Migration:** les champs additifs doivent-ils être versionnés dans un nouvel event de contexte ou ajoutés en `Option`? Recommandation: `Option` avec défaut de décodage pour les sessions existantes, nouvel event uniquement si l'état influence une reprise. Engineering to confirmer avec fixtures.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Familles d'écarts de l'audit ouvertes | 16 | 0 | Month-6 | matrice US-001 et fixtures liées |
| Transports inference conformes | SSE seulement | SSE et WebSocket, 100 % des fixtures identiques | Month-1 | suite de conformité provider |
| Payloads d'items non function/custom conservés | type seul ou ignoré | 100 % des items baseline round-trip | Month-1 | fixtures output item added/done |
| Résolution distante headless | 0 % des lancements headless | 100 % des scénarios de refresh valides | Month-1 | tests CLI/provider headless |
| Familles de providers | 1 | 3 | Month-6 | tests de construction et streams ChatGPT/OpenAI/Bedrock |
| Familles d'API auxiliaires | 0 sur 6 | 6 sur 6 | Month-6 | tests clients compact/memories/images/search/files/realtime |
| Fuites de credentials dans diagnostics | pas de preuve automatisée globale | 0 sur 100 fixtures adversariales | Month-1 | assertions de redaction et captures transport |
| Succès après fermeture sans terminal | possible selon le transport absent | 0 sur 1 000 scénarios injectés | Month-6 | fault-injection SSE/WS |

## Open Questions

- Engineering, avant US-008: quelle URL WebSocket et quels headers le backend ChatGPT subscription accepte-t-il au commit baseline? Réponse produite par US-007; US-008 et US-009 en dépendent.
- Engineering, avant US-002: quelles métadonnées doivent être durables pour permettre une reprise exacte sans stocker des diagnostics sensibles? Décision enregistrée dans la matrice de contrat.
- Engineering, avant US-016: le SDK AWS officiel respecte-t-il les budgets de build et de taille du workspace? Mesure requise dans la story avant ajout définitif.
- Product/runtime, avant EP-006: quelles API auxiliaires doivent être exposées immédiatement dans l'app-server? L'implémentation client n'en dépend pas; la projection externe peut rester capability-gated.
[/PRD]
