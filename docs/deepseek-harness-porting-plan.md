# Plan de reprise depuis DeepSeek Harness

Suite opérationnelle de [`deepseek-harness-comparison.md`](deepseek-harness-comparison.md). Ce document ne compare plus : il ordonne. Chaque lot dit quel code de `dsh` lire, où l'insérer dans Pyxis, ce qui le débloque et comment prouver qu'il fonctionne.

**Nature de la reprise.** `dsh` est en TypeScript sur Cordis, Pyxis est en Rust sur un graphe de crates fermé. Aucun fichier ne se copie. Ce qui se reprend, ce sont des décisions de conception déjà éprouvées : un seuil, une clé de canonicalisation, un ordre de repli, une frontière de propriété. Les colonnes « Source dsh » pointent le fichier à lire, pas un fichier à transposer ligne à ligne.

**Ordre.** Les lots sont classés par dépendance réelle, pas par valeur perçue. Les deux premiers ne produisent aucun code mais changent la façon dont tous les suivants sont enregistrés. Les lots 3 et 4 se branchent sur des coutures qui existent déjà. Les lots 5 à 8 construisent l'outillage qui rend les lots 9 à 11 vérifiables. Les trois derniers touchent `agent-runtime` et sont les seuls à mériter une entrée de feuille de route.

**Convention de chemins.** Relatifs à la racine de chaque dépôt. `dsh` est `/home/arthur/dev/deepseek-harness`, Pyxis est `/home/arthur/dev/pyxis`.

---

## Tableau d'implémentation

| # | Lot | Source dsh à lire | Cible Pyxis | Dépend de | Effort | Signal de vérification |
|---|---|---|---|---|---|---|
| 1 | Guide d'agent à la racine | `AGENTS.md`, `packages/AGENTS.md`, `docs/AGENTS.md` | `AGENTS.md` (à créer), extrait de `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, `CONTRIBUTING.md` | rien | 1 jour | relecture : un agent neuf produit un diff conforme sans lire `ARCHITECTURE.md` |
| 2 | Arbre de notes de décision | `.agents/notes/README.md`, `scripts/verify-agent-note-format.ts`, `scripts/agent-note-tree.ts` | `docs/notes/{proposed,implemented,rejected}/{feature,bug-fix,architecture,process,testing}/`, lien depuis `AGENTS.md` | 1 | 1 jour | script de format en pre-commit ; `docs/parity-audit-2026-07-*.md` absorbés |
| 3 | Escalade du garde-fou de boucle | `packages/guard/repeat-tool-reminder/src/index.ts` | `crates/agent-core/src/guardrail.rs`, appel dans `crates/agent-core/src/agent.rs` | 1 | 1 à 2 jours | tests unitaires sur `LoopGuard` : escalade, reset sur message utilisateur, comptage des appels refusés |
| 4 | Débordement de sortie d'outil | `packages/spill/spill/src/types.ts`, `packages/spill/spill-local/src/store.ts`, `packages/spill/spill-policy/src/index.ts` | `crates/agent-core/src/tools.rs` (`ToolResultTruncation`), `crates/agent-tools/src/tool.rs` (`truncate_tail`), nouveau module de stockage sous `crates/agent-tools/src/` | 1 | 3 à 5 jours | test : sortie de 10 Mio, contexte borné, fichier complet relisable, échec de persistance sans `isError` |
| 5 | Graphe de portes | `scripts/run-gates.ts` (interface `Gate`, `needs` contre `after`), `lefthook.yml` | `justfile` (à créer), `.github/workflows/ci.yml` | rien | 1 à 2 jours | `just check` reproduit le CI en local ; un échec de `fmt` coupe avant les tests |
| 6 | Catalogues générés | `scripts/gen-doc-graphs.ts`, `docs/tool-catalog.md`, `docs/config-catalog.md`, `docs/module-graph.md` | `crates/agent-tools/src/registry.rs` et `crates/agent-cli/src/settings.rs` comme sources, sorties dans `docs/`, modèle dans `crates/agent-app-server/src/schema.rs` | 5 | 2 à 3 jours | test de régénération et comparaison octet à octet, comme `docs/app-server/protocol.schema.json` |
| 7 | Expérience du modèle documentée | `scripts/verify-package-readme-model-experience.ts`, `docs/cookbook/adding-a-package.md` | `crates/agent-tools/README.md`, `crates/agent-core/README.md` (à créer), données dans `crates/agent-core/src/budget.rs` et `crates/agent-core/src/prompt.rs` | 1, 6 | 1 jour | porte dans `just check` : un crate model-facing sans section échoue |
| 8 | Tests par instantanés | `docs/testing.md`, `packages/test-support/` | `crates/agent-cli/tests/e2e_headless.rs`, fixtures sous `crates/agent-cli/tests/` | 5 | 3 à 5 jours | un rejeu JSONL produit une transcription identique octet à octet |
| 9 | Registre de tâches de fond | `packages/jobs/jobs/src/types.ts`, `packages/jobs/jobs-local/src/index.ts`, `packages/jobs/tool-jobs/src/index.ts` | `crates/agent-runtime/src/supervisor.rs`, `crates/agent-runtime/src/store.rs`, nouvel outil dans `crates/agent-tools/src/` | 4, 5 | 1 à 2 semaines | un `bash` long survit à la fin du tour, son résultat revient, un redémarrage le retrouve |
| 10 | Rappels planifiés | `packages/schedule/schedule/src/types.ts`, `packages/schedule/schedule/src/domain.ts`, `packages/schedule/schedule/src/runtime.ts`, `packages/schedule/schedule/src/persistence.ts` | `crates/agent-runtime/src/inputs.rs`, `crates/agent-runtime/src/thread.rs`, `crates/agent-session/src/thread_store.rs` | 9 | 1 à 2 semaines | un rappel créé, le processus arrêté, redémarré : le rappel arrive comme un tour ordinaire |
| 11 | Registre de fournisseurs de sous-agents | `packages/subagent/subagent/src/types.ts` (`SubagentProvider`, `SubagentCapabilities`), `packages/subagent/subagent-codex/`, `packages/subagent/subagent-acp/` | `crates/agent-tools/src/agent.rs`, `crates/agent-cli/src/subagents.rs`, `crates/agent-runtime/src/supervisor.rs` | 5, 9 | 2 semaines et plus | `spawn_agent` délègue à un binaire Codex externe et rend un résultat au même format |

---

## Détail par lot

### 1. Guide d'agent à la racine

**Ce qu'on reprend.** La forme, pas le contenu. `AGENTS.md` de `dsh` fait 16 Ko de règles impératives numérotées, chacune formulée comme une contrainte vérifiable et non comme un conseil : « les enregistrements sont des effets », « visible du modèle ⟺ journalisé », « une mauvaise configuration échoue bruyamment ». Chaque règle porte un lien vers la note de décision qui la justifie, donc le fichier reste court et le raisonnement reste accessible.

**Ce qu'on écrit pour Pyxis.** Les 15 invariants de `docs/ARCHITECTURE.md`, les défauts fermés du trait `Tool` (`crates/agent-tools/src/tool.rs`), la règle de propagation de souillure (`crates/agent-tools/src/taint.rs`), l'interdiction de dépendance de `agent-core` (`crates/agent-core/Cargo.toml`), la convention `agent-*` (ADR-8), la règle « les constantes de `agent-runtime` ne deviennent pas des clés de configuration » (ADR-12).

**Pourquoi en premier.** `docs/ARCHITECTURE.md` fait 642 lignes. Aucun agent, humain ou non, ne le lit intégralement avant d'éditer un fichier. Un fichier racine de 150 lignes impératives est lu.

### 2. Arbre de notes de décision

**Ce qu'on reprend.** Le chemin encode deux axes, `{cycle de vie}/{classe}/aaaa-mm-jj-sujet.md`, et le cycle de vie est le répertoire : une note se déplace de `proposed/` vers `implemented/` ou `rejected/`. Les trois premières lignes du fichier sont fixes (`# Agent Note: <titre>`, ligne vide, `Status: <statut>`), le statut doit s'accorder avec le répertoire, et la porte croise les deux. Le corps commence toujours par `## Problem`, écrit pour tenir sans la solution. Une note rejetée porte sa raison dans le statut lui-même, parce que c'est le fait que le lecteur vient chercher.

**Ce qu'on écarte.** Les 1 452 notes, la traduction chinoise, l'arbre `archived/` gelé avec manifeste de hachages, la règle « toute PR non triviale doit en contenir une ». Pyxis a un mainteneur : la contrainte utile est « toute décision qu'on pourrait vouloir rejouer », pas « toute PR ».

**Migration.** `docs/parity-audit-2026-07-24.md`, `-25`, `-27` et `docs/parity-strategy-2026-07-27.md` sont déjà des notes datées qui s'ignorent. Elles deviennent `docs/notes/implemented/process/2026-07-24-parity-audit.md` et suivantes.

### 3. Escalade du garde-fou de boucle

**État réel de Pyxis.** `crates/agent-core/src/guardrail.rs` contient déjà un `LoopGuard` : signature déterministe de batch (`name\0json` triée, jointe), `DEFAULT_LOOP_GUARD_THRESHOLD = 3`, décision ternaire `Proceed` / `Signal` / `Abort`, et exemption déclarée par le dispatcher (`loop_guard_exempt`). C'est un garde vétoiste par batch, plus strict que celui de `dsh` sur un point : il n'exécute pas le batch fautif.

**Ce que `dsh` apporte en plus.** Quatre décisions absentes de `guardrail.rs`.

1. **Escalade multi-seuils.** `thresholds` par défaut `[3, 5, 8]` au lieu d'un seuil unique, avec deux registres de message : un rappel doux au premier seuil, puis un rappel détaillé nommant l'outil, la longueur de la série et les arguments canoniques. Pyxis passe de `Signal` à `Abort` en un cran, ce qui tue une session qu'un second rappel aurait sauvée.
2. **Comptage des appels refusés.** `dsh` compte dans `tools/post-execute` précisément parce qu'un refus passe par la même cascade : un modèle qui martèle un appel refusé est exactement la boucle qu'il faut casser. Vérifier où `LoopGuard::observe` est appelé dans `crates/agent-core/src/agent.rs` et si un batch refusé par permission avance le compteur.
3. **Remise à zéro sur interjection utilisateur.** `dsh` supprime la chaîne de l'agent dès qu'un message de source `user` apparaît dans `agent/pre-step` : une répétition de part et d'autre d'une intervention humaine n'est pas une boucle. Pyxis a `LoopGuard::reset()` mais aucun appelant sur ce déclencheur.
4. **Bornage du rappel, jamais de la détection.** `argumentsPreviewChars` (500 par défaut) tronque les arguments cités dans le message, alors que la clé de chaîne compare toujours la chaîne canonique complète. Sans cela, un corps de `write` volumineux repart dans la requête suivante, précisément dans le scénario de boucle.

**Détail à ne pas manquer.** La canonicalisation de `dsh` est un tri de clés en profondeur avant sérialisation, donc deux objets d'arguments qui ne diffèrent que par l'ordre des propriétés produisent la même clé. Pyxis obtient déjà cette propriété gratuitement : `Display` de `serde_json::Value` trie les clés en l'absence de `preserve_order`, et `guarded_batch_signature` le documente.

**Fail-loud.** `dsh` valide les seuils au chargement : liste vide, non-entier, valeur inférieure à 2, doublon, tout lève. Un seuil mal configuré n'est jamais silencieusement remplacé par un défaut.

### 4. Débordement de sortie d'outil

**État réel de Pyxis.** La troncature existe et elle est déjà instrumentée. `crates/agent-tools/src/tool.rs:584` expose `truncate_tail`, et `crates/agent-core/src/tools.rs:45` définit :

```rust
pub struct ToolResultTruncation {
    pub original_bytes: usize,
    pub kept_bytes: usize,
    pub strategy: TruncationStrategy,
    pub continuation_hint: String,
}
```

Le champ `continuation_hint` est exactement l'emplacement du localisateur. Le lot ne crée pas de structure, il remplit un champ qui existe.

**Ce qu'on reprend de `dsh`.** Le découpage en trois responsabilités, qui est le seul point non évident.

- **Le vocabulaire** (`packages/spill/spill/src/types.ts`) : un localisateur opaque que le consommateur affiche mais ne parse jamais, un propriétaire réduit à l'identifiant de session, une source descriptive (outil, identifiant d'appel, étiquette) explicitement non utilisée pour le contrôle d'accès, et un résultat `{ locator, bytes, retrievalHint }`.
- **Le stockage** (`packages/spill/spill-local/src/store.ts`, 120 lignes, à lire intégralement) : racine privée en 0700 créée par `mkdtemp` pour que le suffixe soit imprévisible, répertoire par session nommé par un hachage court, nom de fichier composé d'un préfixe hexadécimal aléatoire et du nom suggéré assaini, ouverture exclusive `wx` en 0600 qui échoue sur tout chemin existant, symlink compris. L'encodage de segment est injectif sur toutes les chaînes et neutralise `../`, les chemins absolus, le NUL et les séparateurs, avec `.` et `..` échappés en entier.
- **La politique** (`packages/spill/spill-policy/src/index.ts`) : quand déverser, et rien d'autre. Quatre règles à reprendre telles quelles. Seuil omis vaut désactivation totale et non un défaut. Texte simple uniquement, un résultat portant un bloc non textuel reste intact. `read` est exclu du bras visible du modèle pour éviter la boucle `read → déversement → read`. Et surtout : meilleur effort strict, une panne de stockage journalise et rend le résultat original, un échec de déversement ne transforme jamais un appel réussi en erreur.

**Adaptation Pyxis.** La racine n'est pas le répertoire temporaire du système mais `.pyxis/`, déjà protégé au niveau politique (`crates/agent-tools/src/path.rs`). Le localisateur est relu par `read_file`, qui existe. Aucun nouvel outil n'est nécessaire.

### 5. Graphe de portes

**Ce qu'on reprend.** Une seule idée, la distinction que porte l'interface `Gate` de `scripts/run-gates.ts` :

```ts
needs?: string[]   // doit RÉUSSIR avant
after?: string[]   // doit être RETOMBÉ avant, quel que soit le verdict
```

`needs` coupe court sur échec, `after` sérialise sans conditionner. C'est ce qui permet à `fmt` de bloquer tout le reste, à `clippy` et aux tests de partager une compilation, et à une porte lente de s'exécuter sans bloquer le verdict des rapides.

**Ce qu'on écarte.** Les 967 lignes de `run-gates.ts`, les 14 modes nommés, les 142 scripts. Pyxis a besoin d'un `justfile` avec trois ou quatre agrégats (`just check`, `just check-fast`, `just docs`), pas d'un ordonnanceur.

**Pourquoi maintenant.** Les lots 6, 7 et 8 ajoutent chacun une vérification. Sans agrégat nommé, elles finissent en étapes copiées dans `.github/workflows/ci.yml` et personne ne les exécute en local.

### 6. Catalogues générés

**Ce qu'on reprend.** Le principe de `scripts/gen-doc-graphs.ts` : les documents qui décrivent la structure sont dérivés, jamais rédigés. `dsh` en produit six, dont le graphe de coutures de capacité qui trace pour chacune des 70 environ le propriétaire, le fournisseur et les consommateurs.

**Ce que Pyxis peut générer sans effort disproportionné.** Le catalogue d'outils depuis `crates/agent-tools/src/registry.rs` (nom, schéma, lecture seule, sûreté de concurrence, sortie non fiable, différable, échéance), le catalogue de configuration depuis `crates/agent-cli/src/settings.rs` (clé, couche, précédence, honorée depuis un fichier d'espace de travail ou non), et le graphe de crates depuis `Cargo.toml`.

**Modèle existant.** `crates/agent-app-server/src/schema.rs` génère déjà `docs/app-server/protocol.schema.json` et un test compare les octets. Le lot applique la même mécanique à trois nouveaux fichiers, avec la même porte.

**Bénéfice non évident.** Le catalogue d'outils rend l'invariant de souillure inspectable : une ligne du tableau montre un outil `returns_untrusted = false`, ce qui devient une décision visible en revue plutôt qu'un défaut de trait silencieusement surchargé.

### 7. Expérience du modèle documentée

**Ce qu'on reprend.** La section `## Model Experience` obligatoire dans les README de paquets de `dsh`, avec trois champs canoniques : `#### What the model sees`, `#### Token effect`, `#### KV Cache effect`. Le troisième est le plus intéressant : il force à déclarer si le paquet insère du texte variable en tête de contexte, donc s'il invalide le cache à chaque tour.

**Ce qui rend la porte utilisable.** `scripts/verify-package-readme-model-experience.ts` ne se contente pas d'exiger la section : il maintient une liste blanche de paquets réellement agnostiques, chacun avec sa justification écrite en clair dans le script. Une section absente ne peut donc pas passer pour un oubli.

**Cible.** `crates/agent-tools/README.md` et `crates/agent-core/README.md`, qui n'existent pas. La donnée existe déjà dans `crates/agent-core/src/budget.rs` (`ContextBudget` calculé une fois par modèle) mais n'est exposée à aucun mainteneur.

### 8. Tests par instantanés

**Ce qu'on reprend.** Le niveau de test que Pyxis n'a pas : un rejeu déterministe sans clé d'API qui produit une transcription comparée octet à octet. `dsh` en a deux variantes, un rejeu ACP et un rejeu JSONL headless. `docs/testing.md` porte les quatre règles qui les rendent utiles : préférer l'implémentation réelle au simulacre, vérifier le monde et non l'auto-déclaration, tester le vrai chemin d'entrée, et n'exiger un instantané que là où le texte visible du modèle est le contrat.

**Cible.** `crates/agent-cli/tests/e2e_headless.rs` existe déjà et constitue le point d'ancrage. Le lot ajoute des fixtures de flux fournisseur enregistrées et l'assertion de transcription.

**Pourquoi après le lot 5.** Un instantané qui n'est pas dans un agrégat nommé n'est jamais régénéré au bon moment, et un instantané périmé se met à échouer pour de mauvaises raisons.

### 9. Registre de tâches de fond

**Ce qu'on reprend.** `packages/jobs/jobs/src/types.ts` définit le vocabulaire complet en 160 lignes : `JobStatus` fermé à `running | stopping | completed | killed | failed`, `JobKind` extensible par fusion de déclarations mais fermé à l'usage (`bash`, `subagent`), un `JobSnapshot` qui porte l'horodatage de début et de fin, la limite de sortie en octets, la session propriétaire et un drapeau `reported` qui distingue un travail terminé d'un travail dont le résultat a été rendu au modèle. C'est ce dernier champ qui évite qu'un résultat soit livré deux fois ou jamais.

**Cible.** `crates/agent-runtime/src/supervisor.rs` détient déjà la supervision des enfants et les bornes (4 actifs, 8 créés). Un travail de fond est un enfant d'une autre nature, pas un nouveau sous-système.

**Prérequis réel.** Le lot 4. Un travail de fond produit par construction une sortie volumineuse et différée ; sans mécanisme de déversement, chaque relève de travail empoisonne le contexte.

### 10. Rappels planifiés

**Ce qu'on reprend.** `packages/schedule/schedule/src/types.ts` porte trois formes d'enregistrement (`after`, `at`, `every`), un état à deux valeurs (`scheduled`, `overdue`), un mode de livraison explicitement unique (`session-local`) et surtout un jeu d'erreurs de validation nommées : `InvalidPrompt`, `InvalidSelector`, `InvalidRule`, `InvalidTimeZone`, `NotFuture`, `TimeOutOfRange`, `FrequencyTooHigh`. La séparation `domain.ts` (807 lignes, pur) et `runtime.ts` (324 lignes, effets) est la partie à imiter : le domaine calcule les échéances sans horloge injectée dans la logique.

**Cible.** `crates/agent-runtime/src/inputs.rs` porte déjà une file d'entrées en attente bornée à 16. Un rappel arrivé est une entrée comme une autre : c'est la propriété qui rend le mécanisme peu coûteux.

**Ordre.** Après le lot 9, parce que la persistance d'un registre durable et le réveil d'un fil sont le même travail, fait une fois.

### 11. Registre de fournisseurs de sous-agents

**Ce qu'on reprend.** `packages/subagent/subagent/src/types.ts` définit `SubagentProvider` comme une interface et `SubagentCapabilities` comme un contrat vérifié avant le démarrage, pas pendant. Un fournisseur qui ne sait pas reprendre une session ne déclare pas `prepareContinuable`, et la continuation est refusée à la construction plutôt que par une erreur à l'exécution. Six fournisseurs coexistent sous cette interface, dont un qui pilote Codex et un qui parle ACP.

**Cible.** `crates/agent-tools/src/agent.rs` construit aujourd'hui un enfant Pyxis par intersection d'autorité. Le trait à introduire est celui du lanceur, pas celui de l'agent : l'intersection d'autorité reste côté Pyxis et devient une contrainte transmise au fournisseur.

**Pourquoi en dernier.** C'est le seul lot qui ouvre une frontière de confiance vers un binaire externe. Il n'a de sens qu'une fois le déversement (4), les catalogues (6) et les instantanés (8) en place, faute de quoi le comportement d'un enfant étranger n'est ni borné, ni inspectable, ni rejouable.

---

## Évalué puis écarté

| Mécanisme dsh | Chemins | Raison de l'écart |
|---|---|---|
| Échéance d'outil déclarée (`timeout-policy`) | `packages/guard/timeout-policy/src/index.ts` | Déjà présent dans Pyxis. `crates/agent-tools/src/tool.rs:398` expose `fn timeout(&self, ctx) -> Duration` par outil, `crates/agent-tools/src/registry.rs:659` l'applique, et l'expiration produit un `ToolErrorKind::Timeout` structuré. La seule idée non couverte, restaurer le signal amont après l'appel pour que les écouteurs postérieurs ne voient pas un signal déjà avorté, n'a pas d'équivalent dans un modèle à `CancellationToken` arborescent. |
| Cordis et le modèle « tout est plugin » | `vendor/cordis/`, `vendor/loader/`, `packages/core/scope/` | Détruirait l'avantage central de Pyxis : des invariants vérifiés par le compilateur. Si un point d'extension devient nécessaire, le borner au registre d'outils et au registre de fournisseurs, jamais au noyau. |
| Auto-modification du runtime | `packages/extensions/tool-cordis/` | Incompatible avec un binaire natif, et `dsh` reconnaît lui-même que le bac à sable des globales n'est pas une frontière de sécurité. |
| Couverture à 100 % par fichier | `scripts/run-gates.ts` (mode `ci-coverage`) | Pertinente en TypeScript où le compilateur garantit peu. En Rust, couvrir des branches déjà rendues inatteignables par le système de types achète du travail, pas de la confiance. |
| Documentation bilingue | `docs/**/*.i18n.yaml`, `scripts/verify-translation-pairing.ts` | Double le coût de chaque modification de documentation pour un public que Pyxis n'adresse pas. |
| Domaine d'objectifs événementiel | `packages/goal/goal/` | `crates/agent-tools/src/plan.rs` couvre le besoin actuel. Le domaine événementiel avec révisions en compare-and-set ne se justifie qu'avec des équipes d'agents concurrentes, que Pyxis borne délibérément à une profondeur de 1. |
| Équipes d'agents | `packages/experimental/agent-team/` | Expérimental chez `dsh`, et contradictoire avec les bornes fixées par l'ADR-12. À reconsidérer seulement si le lot 11 aboutit et si un besoin concret apparaît. |

---

## Séquence résumée

Semaine 1 : lots 1, 2 et 5. Aucun code produit, mais tout ce qui suit devient enregistrable et exécutable localement.
Semaines 2 et 3 : lots 3 et 4. Les deux seuls lots qui améliorent directement le comportement de l'agent en boucle.
Semaine 4 : lots 6 et 7. L'architecture devient inspectable, la porte est déjà là.
Semaine 5 : lot 8. Le filet nécessaire avant de toucher `agent-runtime`.
Au-delà : lots 9, 10 puis 11, à traiter comme des entrées de `docs/ROADMAP.md` et non comme des améliorations incrémentales.
