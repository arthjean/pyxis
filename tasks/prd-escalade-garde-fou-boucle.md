[PRD]
# PRD: Escalade du garde-fou de boucle

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-20 | Arthur Jean | Rédaction initiale, lot #3 du plan de portage DeepSeek Harness |

## Problem Statement

`LoopGuard` (`crates/agent-core/src/guardrail.rs`) détecte le batch d'outils identique répété et, au troisième, refuse de l'exécuter. C'est un veto, plus strict que le rappel consultatif de la référence. Cinq défauts mesurés sur l'état actuel du dépôt :

1. **Le tour meurt au cran suivant.** `observe` rend `Signal` quand `count == threshold` et `Abort` dès `count == threshold + 1` (`guardrail.rs:74-80`). Un modèle qui reçoit le signal, ne le comprend pas et retente une fois tue la session : `agent.rs:1469` émet `ExhaustReason::ToolLoop` et sort. Un seul message est envoyé avant la mort, dans un registre unique, alors que le cas courant est un modèle qui corrige au second rappel.
2. **Le garde imbriqué de Code Mode écrase les deux décisions.** `crates/agent-code-mode/src/nested.rs:300` teste `loop_decision != LoopDecision::Proceed` et rend le même message dans les deux cas. L'escalade n'existe pas sur ce site, et le cran terminal n'y fait rien de plus que le cran de signal : l'appel suivant du même code de cellule repart au dispatcher.
3. **Une répétition de part et d'autre d'une intervention humaine compte comme une boucle.** `LoopGuard::reset()` existe et n'a aucun appelant sur ce déclencheur. Le point sûr de steering (`agent.rs:919-928`, US-007) vide la file d'entrées utilisateur sans toucher le garde. Un utilisateur qui écrit « oui, relance exactement ce test » voit sa demande vétoée.
4. **Un appel exempt blanchit une boucle.** `guarded_batch_signature` rend `None` quand tout le batch est exempt, et les deux sites appellent alors `reset()` (`agent.rs:1465`, `nested.rs:120`). La série `read(x)`, `wait`, `read(x)`, `wait` ne déclenche donc jamais rien, alors que `wait` et `write_stdin` non terminant (`crates/agent-tools/src/code_mode.rs:297`, `crates/agent-tools/src/exec_session.rs:669`) sont exactement les outils qu'un modèle bloqué sur une session d'exécution intercale.
5. **Rien n'enregistre ni ne borne ces décisions.** `LoopGuard::new` remplace silencieusement un seuil de 0 par 1 (`guardrail.rs:47`), ce que la référence refuse explicitement. Aucun ADR ne gouverne le garde-fou, et `docs/ARCHITECTURE.md` ne contient pas une occurrence de `LoopGuard` : le commentaire de module de `guardrail.rs:35` renvoie à une « ARCHITECTURE section 3 guardrails » inexistante. Le chemin `Signal` construit son message hors de `bound_feedback`, appliqué aux seules sorties de dispatch (`agent.rs:1539-1550`) : le jour où ce message cite les arguments, rien ne le borne.

**Why now :** le lot #4 du plan de portage (débordement de sortie d'outil) va traverser `crates/agent-core/src/tools.rs` et la même zone de bornage. Le lot #3 doit fixer la règle « borner le rappel, jamais la clé de détection » avant que le lot #4 ne l'écrive une seconde fois ailleurs. Par ailleurs les lots #1 et #2 sont livrés : `AGENTS.md` existe, l'arbre de notes et `agent-doc-gates` aussi, donc la question du registre où enregistrer cette décision est tranchable pour la première fois.

## Overview

Le lot transforme un garde-fou à un cran en une échelle à trois crans, sans jamais rendre le garde plus permissif sur ce qui compte : le batch fautif n'est exécuté à aucun cran. Ce qui change est le registre du message et l'endroit où le tour meurt. `LOOP_GUARD_THRESHOLDS: [u32; 3] = [3, 5, 8]` devient une constante de crate, validée à la compilation, et `LoopDecision::Signal` porte le registre à employer. En dessous de 3, exécution normale. À 3 et 4, rappel doux, batch non exécuté. À 5, 6 et 7, rappel détaillé nommant l'outil, la longueur de la série et les arguments canoniques tronqués. À 8, arrêt déterministe.

Le rappel détaillé est le premier message du cœur qui cite le contenu d'un appel. Il est construit sous un plafond d'octets constant, sur une frontière de caractère, avec un suffixe qui dit combien a été retiré. La clé de chaîne, elle, reste la chaîne canonique complète : deux corps de `write` d'un mégaoctet qui ne diffèrent qu'au-delà du plafond restent deux chaînes distinctes. Borner la clé transformerait le garde-fou en générateur de faux positifs sur les gros arguments, exactement le scénario où il compte.

Deux frontières de chaîne sont corrigées en sens inverse l'une de l'autre. L'interjection utilisateur, aujourd'hui invisible du garde, remet la chaîne à zéro au point sûr de steering et se propage au garde imbriqué par une méthode de trait à défaut no-op. L'appel exempt, aujourd'hui remise à zéro, devient transparent : il ne compte pas et ne remet pas à zéro, ce qui ferme le blanchiment décrit au défaut 4. Ce second changement a un coût assumé, écrit dans les risques et compensé par le fait que le cran 1 est un message et non une mort.

La décision est enregistrée en ADR-14 dans `docs/DECISIONS.md` et non en note. Le test de frontière de `AGENTS.md` tranche seul : une pull request touchant `guardrail.rs` peut violer l'échelle, la transparence et le bornage, donc rien dans l'arbre de notes n'a compétence. Un ADR n'a pas de note miroir. La porte `agent-doc-gates` du lot #2 devient de ce fait un signal de vérification du lot #3 : oublier la ligne d'index fait échouer la suite.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Crans de rappel avant l'arrêt déterministe | 2 (à 3 et à 5), arrêt à 8 | idem, sans régression |
| Batch fautif exécuté à un cran quelconque | 0 | 0 |
| Sites d'appel portant la même échelle | 2/2 (`agent.rs`, `nested.rs`) | 2/2 |
| Octets de message modèle imputables au garde-fou, pour un argument d'un Mio | borné, plafond prouvé par test | idem |
| Déclencheurs de remise à zéro de la chaîne | 2 (interjection utilisateur, fin de tour) | idem |
| Enregistrements gouvernant le garde-fou | 1 ADR, indexé, porte verte | 1 |

## Target Users

### Modèle en cours de tour
- **Role:** le modèle qui émet les batches d'outils, seul destinataire des messages du garde-fou.
- **Behaviors:** répète un appel identique quand un résultat ne le satisfait pas, corrige souvent après un rappel explicite, intercale un `wait` ou un `write_stdin` vide en pilotant une session d'exécution.
- **Pain points:** un unique message avant la mort de la session, dans un registre qui ne dit ni quel outil ni quelle longueur de série ; aucune information sur les arguments qu'il est en train de répéter.
- **Current workaround:** aucun. Le tour est terminé par `ExhaustReason::ToolLoop`.
- **Success looks like:** deux occasions de corriger, la seconde nommant l'outil, le compte et les arguments, avant tout arrêt.

### Utilisateur qui pilote un tour en cours
- **Role:** Arthur, ou tout utilisateur du TUI, qui écrit pendant qu'un tour tourne (steering US-007).
- **Behaviors:** interrompt pour rediriger, demande parfois explicitement de refaire l'appel qui vient d'échouer.
- **Pain points:** sa demande de relance est comptée comme la continuation d'une boucle et vétoée par le garde-fou.
- **Current workaround:** annuler le tour et en ouvrir un neuf, ce qui perd le transcript en cours.
- **Success looks like:** une intervention casse la série, et la relance demandée s'exécute.

### Agent de codage modifiant le garde-fou
- **Role:** un agent avec un contexte vierge qui reçoit une tâche touchant `guardrail.rs` ou l'un des deux sites d'appel.
- **Behaviors:** lit `AGENTS.md`, cherche l'enregistrement qui justifie une constante avant de la changer, suit un pointeur de commentaire de module.
- **Pain points:** le pointeur de `guardrail.rs:35` vers `ARCHITECTURE.md` ne résout pas ; aucun ADR n'explique pourquoi le batch n'est pas exécuté ni pourquoi la clé n'est pas tronquée.
- **Current workaround:** reconstruire l'intention depuis le code et les tests.
- **Success looks like:** ADR-14 répond, et une modification qui viole l'échelle casse un test nommé.

## Research Findings

### Competitive Context
- **Fingerprint plus fenêtre glissante :** hachage de `(nom, arguments)` sur les N derniers appels, blocage au-delà d'un compte. Détecte les boucles non consécutives, au prix d'un état plus lourd et de faux positifs sur les séries légitimes.
- **Échelle avertissement puis blocage :** un rappel injecté au premier seuil, arrêt dur au dernier. Valeurs publiées allant de `[3, 5]` à `warn=10 / critical=20 / circuit-breaker=30`. C'est la forme majoritaire en 2026, et le blocage sec en un cran est cité comme cause de sessions tuées à tort.
- **Détection sensible au progrès :** le triplet `(nom, argsHash, resultHash)` doit être stable sur toute la fenêtre pour déclencher, ce qui n'arrête jamais une série dont le résultat change. C'est la réponse de principe au faux positif sur un journal qui grossit.
- **Tables d'exemption :** universelles, sous le nom `exempt_tools`, pour les opérations légitimement répétitives.
- **Market gap :** aucune des implémentations relevées n'est vétoïste au cran de rappel. Toutes exécutent le batch et ajoutent un message. Pyxis conserve son veto et n'emprunte que la forme de l'échelle, ce qui est une position plus stricte assumée.

### Best Practices Applied
- Seuil initial à 3 : assez large pour un retry sur défaillance transitoire, assez serré pour un spin silencieux. C'est la valeur consensuelle, et c'est déjà celle de Pyxis.
- Fail-fast sur valeur invalide plutôt que remplacement silencieux par un défaut : la position dominante en conception de bibliothèque. Le repli silencieux n'efface pas le problème, il le déplace en aval.
- Ne jamais tronquer en silence ce sur quoi le modèle raisonne : une troncature doit dire qu'elle a eu lieu et de combien.
- Distinguer steering et interruption : le steering injecte dans un tour vivant et remet les compteurs de série à zéro chez ceux qui l'implémentent.

*Recherche web de cette session, sources principales : docs.openclaw.ai/tools/loop-detection, github.com/openclaw/openclaw/issues/77474, strandsagents.com/docs/user-guide/concepts/agents/agent-loop, enterprisecraftsmanship.com/posts/fail-fast-principle, tianpan.co/blog/2026-05-10-silent-tool-truncation-8kb-default-agent-reasons-blind.*

### Implémentation de référence : DeepSeek Harness

Racine du dépôt de référence, en lecture seule : **`/home/arthur/dev/deepseek-harness`**, commit `141eb6fef8` du 2026-08-19, licence MIT. Tous les chemins ci-dessous sont relatifs à cette racine, et les numéros de ligne valent pour ce commit. **Aucune ligne de TypeScript n'est transcrite** : dsh est en TypeScript, Pyxis en Rust, ce qui se reprend est la décision de conception, jamais le code, et cette contrainte suffit à écarter toute question de licence ou d'inventaire de portage.

Trois fichiers à lire, dans cet ordre :

| Fichier dsh | À quoi il sert ici |
|---|---|
| `packages/guard/repeat-tool-reminder/README.md` (90 lignes) | Le contrat en prose : configuration fail-loud, sémantique de chaîne, texte exact des deux rappels, limitations connues. C'est la lecture d'entrée |
| `packages/guard/repeat-tool-reminder/src/index.ts` (233 lignes) | La mécanique : canonicalisation, escalade, bornage, validation, points d'accroche |
| `.agents/notes/archived/feature/2026-07-08-repeat-tool-guard.md` | La note de décision d'origine, lisible pour l'intention, jamais pour le périmètre : elle décrit un garde consultatif |

Ancres exactes, décision par décision. Chaque ligne dit ce qui se reprend et ce qui ne se reprend pas.

| Décision | Ancre dsh | Ce qui se reprend | Ce qui ne se reprend pas | Story |
|---|---|---|---|---|
| Échelle de seuils | `src/index.ts:46` (défaut `[3, 5, 8]`), `README.md:19` | Les trois crans et leur ordre croissant | La configurabilité par plugin (`Config`, `src/index.ts:30-42`), interdite par l'invariant 15 | US-059 |
| Validation fail-loud | `src/index.ts:124-138` (`validateThresholds`), `src/index.ts:169-171`, `README.md:19` | Le refus de la liste vide, du non-entier, de la valeur inférieure à 2 et du doublon ; l'absence de repli silencieux sur un défaut | Le lancement d'exception au chargement du plugin : Pyxis n'a pas de chargement, la validation remonte à la compilation | US-059 |
| Deux registres de message | `src/index.ts:60-67` (`GENTLE_REMINDER`), `src/index.ts:69-80` (`detailedReminder`), `README.md:45-72` | Le premier cran générique et court, les suivants nommant l'outil, la longueur de la série et les arguments canoniques | Le texte anglais littéral, réécrit pour un garde qui n'exécute pas le batch | US-060 |
| Bornage du rappel, jamais de la clé | `src/index.ts:114-121` (`previewArguments`), `src/index.ts:36-42`, `README.md:19` | Le plafond sur les seuls arguments cités, le marqueur d'omission, la clé comparée sur la chaîne canonique complète | La valeur en caractères : Pyxis borne en octets sur une frontière de caractère | US-060 |
| Canonicalisation | `src/index.ts:83-104` (`sortJsonValue`, `canonicalize`), `README.md:25` | La propriété : deux objets ne différant que par l'ordre des propriétés donnent la même clé | Le tri explicite : `serde_json` sans `preserve_order` la donne déjà, il reste à la prouver | US-067 |
| Comptage des appels refusés | `src/index.ts:183-188` (commentaire de `observe`), `README.md:28` | La règle : un modèle qui martèle un appel refusé est exactement la boucle à casser | L'accroche `tools/post-execute` : Pyxis compte en amont de la dispatch, ce qui produit la même propriété | US-066 |
| Remise à zéro sur interjection | `src/index.ts:226-232` (accroche `agent/pre-step`), `README.md:30` | La règle : une répétition de part et d'autre d'une intervention humaine n'est pas une boucle | Le `WeakMap<Agent, Chain>` et le keying par agent, sans objet dans un garde porté par le tour | US-063, US-064 |
| Transparence des appels exclus | `README.md:27` | Un appel exclu ne compte ni ne remet à zéro, pour qu'un outil de bookkeeping ne blanchisse pas une boucle | Les motifs `include` / `exclude` (`README.md:15`) : Pyxis interroge le dispatcher par `loop_guard_exempt` | US-065 |
| Limitations connues | `README.md:85-90` | Détection sur correspondance exacte, chaîne non remise à zéro par la compaction : deux limites héritées telles quelles | La rétrogradation au silence au delà du dernier seuil, incompatible avec un garde vétoiste | Non-Goals |

**Divergences assumées.** Quatre, et elles sont toutes conséquences d'un seul écart : dsh est consultatif, Pyxis est vétoiste.

1. **dsh exécute puis rappelle, Pyxis refuse d'exécuter.** Le rappel de dsh est un message utilisateur injecté à côté d'un résultat d'outil réel ; celui de Pyxis remplace le résultat. Cela rend le faux positif plus cher chez Pyxis, ce qui est la raison pour laquelle l'arrêt terminal recule à 8 plutôt que de rester à 4.
2. **dsh ne parle qu'aux comptes exacts, Pyxis parle sur des plages** (`README.md:90`). Un garde qui n'exécute pas doit rendre un `tool_result` par `tool_use` à chaque batch, donc il ne peut pas se taire entre deux crans. Les comptes 4, 6 et 7 reprennent le registre du cran atteint.
3. **dsh compte par appel, Pyxis par batch.** La clé de dsh est `(nom, arguments canoniques)` pour un appel (`src/index.ts:195`) ; celle de Pyxis est la concaténation triée du batch (`guarded_batch_signature`). Un modèle qui émet trois appels en parallèle est une seule observation côté Pyxis.
4. **dsh valide au chargement, Pyxis à la compilation.** Même contrat de refus, déplacé d'un cran vers l'amont parce que les seuils sont des constantes de crate et non une configuration.

## Assumptions & Constraints

**Assumptions**
- La propriété de canonicalisation tient tant que `serde_json` reste résolu sans `preserve_order`. **Risque moyen**, converti en test par US-067 : une activation transitive de la feature fera échouer un test nommé au lieu de casser la détection en silence.
- Un modèle qui reçoit un second rappel, détaillé, corrige plus souvent qu'il ne persiste. **Non validable hors ligne**, versé en Open Questions. Le coût du pari est borné : quatre allers-retours modèle supplémentaires, sans aucune exécution d'outil.
- Aucun appelant hors du workspace ne construit `RunConfig` en fixant `loop_guard_threshold`. Vérifié : `grep` sur `crates/` ne rend que la définition (`agent.rs:53`), le défaut (`agent.rs:78`) et l'usage (`agent.rs:869`).

**Constraints**
- **Invariant 15** (`docs/ARCHITECTURE.md:645`) : aucune clé de configuration publique pour l'orchestration. L'échelle et le plafond de bornage sont des constantes de crate. Rien n'entre dans `crates/agent-cli/src/settings.rs`.
- **Invariant 2** : `agent-core` n'émet que des `AgentEvent` structurés. Le message de rappel voyage en `ToolResultView` et en `Message::tool_result_from_model`, comme aujourd'hui.
- Un `tool_result` par `tool_use` : le chemin `Signal` doit continuer à répondre à chaque appel du batch pour que le transcript reste valide.
- Langue : `docs/` en français, code et commentaires en anglais. Le texte des messages du garde-fou est du contenu modèle, donc anglais.
- `agent-core` ne dépend pas de `agent-tools` : le garde-fou interroge le dispatcher par le trait `ToolDispatch`, jamais un type d'outil concret.
- Le baseline Codex résolu par `$PYXIS_CODEX_BASELINE` est en lecture seule. Aucun artefact de parité n'est normatif pour ce lot : aucune matrice ne référence le garde-fou.

## Quality Gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace --no-fail-fast
```

Signaux ciblés : `cargo test -p agent-core` (unitaires de `guardrail.rs` et `crates/agent-core/tests/loop_guardrails.rs`), `cargo test -p agent-code-mode` (`nested_tests.rs`), `cargo test -p agent-doc-gates` (registre ADR et liens internes).

## Epics & User Stories

### EP-018: Échelle d'escalade à trois crans

**Priority:** P0
**Definition of done:** `LOOP_GUARD_THRESHOLDS` est une constante validée à la compilation, `LoopDecision::Signal` porte son registre, les deux sites d'appel rendent le rappel doux à 3, le rappel détaillé borné à 5 et l'arrêt à 8, et aucun cran n'exécute le batch fautif.

#### US-059: Échelle de seuils validée à la compilation
**Priority:** P0 | **Size:** M | **Depends on:** aucune

En tant qu'agent de codage modifiant les seuils, je veux qu'une échelle incohérente refuse de compiler, afin qu'aucun seuil invalide ne soit silencieusement remplacé par un défaut.

**Source de conception :** `packages/guard/repeat-tool-reminder/src/index.ts:46` et `:124-138`, `README.md:19`, sous `/home/arthur/dev/deepseek-harness` au commit `141eb6fef8`.

**Acceptance Criteria:**
- [ ] `pub const LOOP_GUARD_THRESHOLDS: [u32; 3] = [3, 5, 8];` remplace `DEFAULT_LOOP_GUARD_THRESHOLD` dans `crates/agent-core/src/guardrail.rs`, et la ré-exportation de `crates/agent-core/src/lib.rs:54` suit.
- [ ] Une `const fn` de validation rend faux sur une échelle non strictement croissante, sur un doublon et sur un premier cran inférieur à 2 ; un bloc `const _: () = { assert!(...) };` l'applique à la constante, donc une échelle invalide écrite dans le fichier fait échouer `cargo build`.
- [ ] Un test unitaire appelle cette même `const fn` sur `[3, 3, 8]`, `[8, 5, 3]` et `[1, 5, 8]` et prouve qu'elle les refuse, la validation elle-même étant ainsi couverte sans dépendance à `trybuild`.
- [ ] `LoopGuard::new` prend l'échelle et ne contient plus `threshold.max(1)` ; un `debug_assert!` de croissance stricte remplace le repli silencieux.
- [ ] Le champ `RunConfig::loop_guard_threshold` (`agent.rs:53`) et son défaut (`agent.rs:78`) sont retirés, `run_agent` lisant la constante : un `u32` ne peut pas porter une échelle, et l'invariant 15 préfère la constante.
- [ ] **Unhappy path :** un test prouve qu'une série de deux batches identiques suivie d'un batch différent ne franchit aucun cran et remet le compte à 1.

#### US-060: Deux registres de message, arguments bornés
**Priority:** P0 | **Size:** M | **Depends on:** US-059

En tant que modèle en boucle, je veux un second rappel qui nomme l'outil, la longueur de la série et les arguments répétés, afin de corriger sans que la session soit tuée.

**Source de conception :** `packages/guard/repeat-tool-reminder/src/index.ts:60-80` et `:114-121`, `README.md:19` et `:45-72`.

**Acceptance Criteria:**
- [ ] `LoopDecision::Signal` porte un registre (`Gentle` au premier cran atteint, `Detailed` au deuxième), et la variante reste `PartialEq` pour ne pas casser les comparaisons existantes.
- [ ] Une fonction publique de `guardrail.rs` construit le texte anglais depuis le registre, le nom de l'outil, le compte et les arguments canoniques : les deux sites d'appel l'utilisent, aucun ne compose son propre message.
- [ ] `pub const LOOP_GUARD_ARGS_PREVIEW_BYTES: usize = 500;` borne les seuls arguments cités ; la troncature tombe sur une frontière de caractère et le suffixe dit combien d'octets ont été retirés.
- [ ] La clé de détection reste la chaîne canonique complète : un test construit deux invocations d'un Mio ne différant qu'au-delà du plafond et prouve qu'elles ne forment pas une chaîne.
- [ ] Le rappel doux ne cite aucun argument, ce qui garde son coût constant.
- [ ] **Unhappy path :** un argument d'un Mio dont un caractère multi-octets chevauche exactement le plafond produit un message UTF-8 valide dont la longueur reste sous `LOOP_GUARD_ARGS_PREVIEW_BYTES` plus un préambule constant.

#### US-061: Site externe sur l'échelle
**Priority:** P0 | **Size:** S | **Depends on:** US-059, US-060

En tant qu'utilisateur d'un tour qui boucle, je veux deux rappels avant l'arrêt, afin qu'un tour récupérable ne soit pas tué au quatrième batch.

**Source de conception :** `packages/guard/repeat-tool-reminder/README.md:19` et `:90`. Divergence assumée numéro 2 : dsh se tait entre deux crans, un garde vétoiste ne le peut pas.

**Acceptance Criteria:**
- [ ] `agent.rs:1468` traite les trois décisions, `Signal` portant son registre jusqu'au message ; le batch n'est exécuté à aucun cran.
- [ ] Un `tool_result` est toujours émis par `tool_use` du batch signalé, avec `ToolErrorKind::Semantic`, et le transcript reste valide.
- [ ] `ExhaustReason::ToolLoop { count }` n'est émis qu'au dernier cran et porte le compte réel, donc 8 et non 4.
- [ ] `crates/agent-core/tests/loop_guardrails.rs` prouve la séquence complète : exécution jusqu'à 2, rappel doux à 3 et 4, rappel détaillé à 5, 6 et 7, `Exhausted` à 8.
- [ ] **Unhappy path :** un batch de trois appels au cran terminal produit exactement un `Exhausted` et aucun second état terminal, conformément à l'invariant 11.

#### US-062: Site imbriqué sur l'échelle, avec verrou terminal
**Priority:** P0 | **Size:** M | **Depends on:** US-059, US-060

En tant que modèle pilotant une cellule Code Mode, je veux que le cran terminal arrête réellement la dispatch imbriquée, afin que le code de cellule ne relance pas indéfiniment le même effet.

**Source de conception :** aucune. dsh n'a pas d'équivalent de la dispatch imbriquée, et son garde ne verrouille jamais : décision propre à Pyxis, à argumenter comme telle dans ADR-14.

**Acceptance Criteria:**
- [ ] `crates/agent-code-mode/src/nested.rs:300` distingue `Signal` de `Abort` au lieu de tester `!= Proceed`, et emploie la fonction de message commune.
- [ ] Au cran terminal, `NestedLoopGuard` se verrouille : tout appel imbriqué ultérieur rend l'erreur terminale sans atteindre le dispatcher, un code de cellule pouvant sinon avaler l'erreur et retenter, ce qui est précisément la boucle à casser.
- [ ] `NestedLoopGuard::reset` et la fin de tour lèvent le verrou ; `nested_tests.rs` prouve qu'un tour suivant repart libre.
- [ ] **Unhappy path :** après le verrou, un dispatcher enregistreur prouve zéro appel reçu pour les invocations suivantes, y compris pour un outil différent de celui qui a bouclé.

### EP-019: Frontières de la chaîne

**Priority:** P0
**Definition of done:** une intervention humaine casse la chaîne sur les deux sites, un appel exempt ne la casse plus, et chaque déclencheur restant est prouvé par un test nommé.

#### US-063: Remise à zéro sur interjection utilisateur
**Priority:** P0 | **Size:** S | **Depends on:** aucune

En tant qu'utilisateur qui écrit pendant un tour, je veux que ma demande casse la série, afin qu'une relance explicitement demandée ne soit pas vétoée comme une boucle.

**Source de conception :** `packages/guard/repeat-tool-reminder/src/index.ts:226-232`, `README.md:30`.

**Acceptance Criteria:**
- [ ] Au point sûr de steering (`agent.rs:919-928`), `loop_guard.reset()` est appelé si et seulement si au moins un message a été retiré de la file.
- [ ] Le commentaire existant du point sûr est étendu d'une phrase disant pourquoi la chaîne s'y casse, sans déplacer le point sûr : rien n'entre entre un `tool_use` et son résultat.
- [ ] Un test d'intégration pilote un `InputQueue` : deux batches identiques, une interjection, deux batches identiques, et prouve qu'aucun cran n'est franchi.
- [ ] **Unhappy path :** un tour sans interjection, dont la file est vide à chaque passage, franchit les crans exactement comme avant ; un `take()` rendant zéro message ne remet rien à zéro.

#### US-064: Propagation de l'interjection au garde imbriqué
**Priority:** P1 | **Size:** S | **Depends on:** US-063

En tant que modèle pilotant Code Mode, je veux que l'intervention humaine casse aussi la chaîne imbriquée, afin que l'invariant vaille sur les deux sites et non sur un seul.

**Source de conception :** même ancre que US-063. dsh n'a qu'un site de comptage, la propagation à un second garde est propre à Pyxis.

**Acceptance Criteria:**
- [ ] `ToolDispatch` reçoit une méthode à défaut no-op signalant l'entrée d'un message utilisateur ; le défaut est le choix conservateur, ne pas remettre à zéro étant plus strict que remettre à zéro.
- [ ] Le dispatcher de `crates/agent-tools` la transmet à `NestedLoopGuard::reset` via `code_mode.rs:131`.
- [ ] `run_agent` l'appelle au même point sûr et sous la même condition que US-063, en un seul endroit.
- [ ] **Unhappy path :** un dispatcher de test qui n'implémente pas la méthode compile et conserve son compte, ce qui prouve que le défaut ne rend rien silencieusement plus permissif.

#### US-065: Transparence des appels exempts
**Priority:** P1 | **Size:** M | **Depends on:** US-061, US-062

En tant qu'utilisateur payant les tours, je veux qu'un `wait` intercalé ne blanchisse pas une boucle, afin qu'une alternance appel identique et sondage soit détectée.

**Source de conception :** `packages/guard/repeat-tool-reminder/README.md:27`.

**Acceptance Criteria:**
- [ ] Quand `guarded_batch_signature` rend `None`, les deux sites rendent `Proceed` sans appeler `reset()` : l'appel exempt ne compte ni ne casse la série.
- [ ] `NestedLoopGuard::finish_cell` cesse de remettre à zéro sur une cellule sans effet gardé, pour la même raison ; le champ `effect_cells` est retiré s'il n'a plus d'usage.
- [ ] La documentation de `LoopGuard::reset` (`guardrail.rs:58-60`) est réécrite : le seul déclencheur restant est l'intervention humaine, plus le passage à un tour neuf.
- [ ] Un test prouve la série `read(x)`, `wait`, `read(x)`, `wait`, `read(x)` : trois occurrences comptées, cran 1 atteint.
- [ ] `nested_terminal_polls_reset_the_effect_loop_guard` est réécrit et renommé pour énoncer la règle nouvelle, un test qui prouve l'ancienne devenant faux.
- [ ] **Unhappy path :** une série de `wait` purs, sans aucun appel gardé, ne franchit aucun cran et laisse le compte inchangé, ce qui protège le sondage légitime d'un terminal.

### EP-020: Preuves des propriétés déjà acquises

**Priority:** P0
**Definition of done:** les deux propriétés que le dépôt tient par construction et que rien ne vérifie ont chacune un test nommé qui échoue si elles se perdent.

#### US-066: Un appel refusé par permission compte
**Priority:** P0 | **Size:** S | **Depends on:** US-061

En tant qu'agent de codage, je veux qu'un test prouve qu'un refus de permission avance le compteur, afin qu'un déplacement futur de `observe` sous la dispatch soit détecté.

**Source de conception :** `packages/guard/repeat-tool-reminder/src/index.ts:183-188`, `README.md:28`.

**Acceptance Criteria:**
- [ ] Un test de `crates/agent-core/tests/loop_guardrails.rs` emploie un dispatcher rendant systématiquement `ToolErrorKind::PermissionDenied` et prouve que la répétition franchit les crans jusqu'à l'arrêt.
- [ ] Le test nomme dans son identifiant la propriété prouvée, en phrase complète, et cite la raison dans un commentaire : `observe` est en amont de `dispatch`, donc un refus est compté, faute de quoi un modèle martelant un appel refusé ne serait jamais arrêté.
- [ ] Un second cas couvre le nom d'outil inconnu, la ligne de conception de `crates/agent-tools/src/registry.rs:941-949` déclarant explicitement qu'il n'est pas exempt.
- [ ] **Unhappy path :** un refus suivi d'un appel accepté différent remet le compte à 1, ce qui prouve que le refus compte comme un appel ordinaire et non comme un état spécial.

#### US-067: La clé est insensible à l'ordre des clés JSON
**Priority:** P0 | **Size:** XS | **Depends on:** aucune

En tant qu'agent de codage, je veux qu'un test tienne l'hypothèse de canonicalisation, afin qu'une activation transitive de `preserve_order` casse un test au lieu de casser la détection en silence.

**Source de conception :** `packages/guard/repeat-tool-reminder/src/index.ts:83-104`, `README.md:25`.

**Acceptance Criteria:**
- [ ] Un test unitaire de `guardrail.rs` prouve que `{"a":1,"b":2}` et `{"b":2,"a":1}` produisent la même signature, sur un objet imbriqué à deux niveaux.
- [ ] Un test prouve que l'ordre des appels dans le batch ne change pas la signature, le tri de `guarded_batch_signature` étant la propriété correspondante.
- [ ] Le commentaire de `guardrail.rs:84-88` cite le test par son nom, l'hypothèse et sa preuve étant ainsi liées dans les deux sens.
- [ ] **Unhappy path :** deux objets aux mêmes clés et aux valeurs permutées produisent des signatures différentes, ce qui prouve que le test ne passe pas par accident.

### EP-021: Enregistrement de la décision

**Priority:** P0
**Definition of done:** ADR-14 existe, est indexé, la porte documentaire est verte, et le pointeur de commentaire de `guardrail.rs` résout.

#### US-068: ADR-14, échelle du garde-fou de boucle
**Priority:** P0 | **Size:** M | **Depends on:** US-061, US-062, US-063, US-065

En tant qu'agent de codage modifiant le garde-fou, je veux un ADR qui dise pourquoi le batch n'est pas exécuté et pourquoi la clé n'est pas tronquée, afin de ne pas re-litiger une décision déjà pesée.

**Acceptance Criteria:**
- [ ] `docs/DECISIONS.md` reçoit `## ADR-14` au format du registre : Contexte, Décision, Justification, Alternatives écartées, Conséquences et risques.
- [ ] Les alternatives écartées nomment au moins la détection sensible au résultat, la fenêtre glissante non consécutive et la clé de configuration publique, chacune avec la raison du refus.
- [ ] La ligne d'index du tableau récapitulatif est ajoutée ; `cargo test -p agent-doc-gates` est vert, l'oubli de cette ligne étant précisément ce que la porte du lot #2 détecte.
- [ ] Aucune note miroir n'est écrite dans `docs/notes/` : la règle de frontière de `AGENTS.md` l'interdit pour une décision qu'une pull request sur `crates/` peut violer.
- [ ] **Unhappy path :** retirer temporairement la ligne d'index fait échouer `cargo test -p agent-doc-gates`, ce qui est vérifié une fois puis annulé.

#### US-069: Section garde-fou dans le document d'architecture
**Priority:** P2 | **Size:** S | **Depends on:** US-068

En tant qu'agent de codage suivant un pointeur de code, je veux que la référence de `guardrail.rs` résolve, afin de ne pas chercher une section inexistante.

**Acceptance Criteria:**
- [ ] `docs/ARCHITECTURE.md` reçoit une sous-section de la partie 3 décrivant les deux garde-fous déterministes, l'échelle, le veto et les deux sites d'appel, en français, et liant ADR-14.
- [ ] Le commentaire de module de `crates/agent-core/src/guardrail.rs:35` cite la section réelle, ou est corrigé si le titre diffère.
- [ ] La porte de liens Markdown internes de `agent-doc-gates` reste verte.
- [ ] **Unhappy path :** aucun seizième invariant n'est ajouté à la liste numérotée, ADR-14 portant la décision ; un invariant redondant avec un ADR est une seconde source de vérité.

## Functional Requirements

| ID | Requirement | Story |
|----|-------------|-------|
| FR-01 | L'échelle des seuils est une constante de crate `[3, 5, 8]`, validée à la compilation | US-059 |
| FR-02 | Une échelle non strictement croissante, ou de premier cran inférieur à 2, ne compile pas | US-059 |
| FR-03 | Le batch fautif n'est exécuté à aucun cran de l'échelle | US-061, US-062 |
| FR-04 | Le rappel du premier cran ne cite aucun argument ; celui du deuxième cite l'outil, le compte et les arguments canoniques | US-060 |
| FR-05 | Les arguments cités sont bornés à un plafond constant, sur une frontière de caractère, avec mention de ce qui est retiré | US-060 |
| FR-06 | La clé de détection n'est jamais tronquée | US-060 |
| FR-07 | Le cran terminal émet `ExhaustReason::ToolLoop` portant le compte réel, une seule fois | US-061 |
| FR-08 | Le cran terminal imbriqué verrouille la dispatch pour le reste du tour | US-062 |
| FR-09 | Une interjection utilisateur remet la chaîne à zéro sur les deux sites | US-063, US-064 |
| FR-10 | Un appel exempt ne compte ni ne remet la chaîne à zéro | US-065 |
| FR-11 | Un appel refusé par permission, ou portant un nom inconnu, compte comme un appel ordinaire | US-066 |
| FR-12 | Deux jeux d'arguments ne différant que par l'ordre des clés produisent la même signature | US-067 |
| FR-13 | La décision est enregistrée en ADR indexé, sans note miroir | US-068 |

## Non-Functional Requirements

| ID | Category | Requirement | Measurement |
|----|----------|-------------|-------------|
| NFR-01 | Coût | Le message du garde-fou reste sous `LOOP_GUARD_ARGS_PREVIEW_BYTES + 256` octets par appel du batch, quelle que soit la taille des arguments | Test avec un argument d'un Mio |
| NFR-02 | Coût | L'échelle ajoute au plus 5 allers-retours modèle par série avant l'arrêt, et zéro exécution d'outil supplémentaire | Comptage des `AgentEvent::ToolCall` dans le test de séquence |
| NFR-03 | Déterminisme | Aucune horloge, aucun aléa, aucune I/O dans `guardrail.rs` : la décision est une fonction pure de la suite des signatures | Tests unitaires sans runtime asynchrone |
| NFR-04 | Mémoire | L'état du garde-fou reste borné à une signature et un compteur par site, sans fenêtre glissante | Revue de `LoopGuard` et `NestedLoopState` |
| NFR-05 | Surface publique | Zéro clé de configuration ajoutée à `crates/agent-cli/src/settings.rs` | `grep` sur le diff |
| NFR-06 | Concurrence | `NestedLoopGuard` reste utilisable depuis plusieurs threads de cellule sans interblocage | Tests existants de `nested_tests.rs` verts |

## Edge Cases & Error States

| Cas | Comportement attendu | Couvert par |
|---|---|---|
| Batch mêlant appels exempts et gardés | La signature ne retient que les gardés ; le batch compte | US-065 |
| Batch entièrement exempt | Transparent : ni compté ni remise à zéro | US-065 |
| Arguments d'un Mio | Clé complète, message borné | US-060 |
| Caractère multi-octets à cheval sur le plafond | Troncature sur la frontière, UTF-8 valide | US-060 |
| Arguments différant seulement par l'ordre des clés | Même signature | US-067 |
| Appel refusé par permission | Compté | US-066 |
| Nom d'outil inconnu | Compté, non exempt | US-066 |
| File de steering vidée sans message | Aucune remise à zéro | US-063 |
| Compteur au maximum de `u32` | `saturating_add`, pas de débordement | US-059 |
| Cellule imbriquée terminant alors que le verrou est posé | Le verrou survit à la cellule, tombe au tour suivant | US-062 |
| Deux cellules observant en parallèle | Sérialisées par le mutex, comptes cohérents | US-062 |
| Tour repris après compaction ou reprise | Chaîne neuve, le garde vivant le temps d'un `run_agent` | Documenté dans ADR-14 |

## Risks & Mitigations

| Risque | Impact | Probabilité | Mitigation |
|---|---|---|---|
| L'arrêt reculé de 4 à 8 laisse un modèle bloqué dépenser plus | Moyen | Élevée | Le batch n'est exécuté à aucun cran : le surcoût est en allers-retours modèle, non en effets ; `iter_cap` et `UsageBudget` restent les bornes externes |
| La transparence des exemptions vétoie une relecture légitime d'un journal qui grossit | Moyen | Moyenne | Le premier cran est un message, pas une mort ; le modèle peut relire avec un décalage. La correction de principe, la détection sensible au résultat, est en Non-Goals avec sa raison |
| La méthode ajoutée à `ToolDispatch` élargit un trait implémenté par plusieurs dispatchers | Faible | Certaine | Défaut no-op, conservateur ; l'alternative, un invariant valable sur un site sur deux, est pire |
| Le retrait de `RunConfig::loop_guard_threshold` casse un appelant | Faible | Faible | Aucun appelant hors des trois lignes de `agent.rs`, vérifié par `grep` sur `crates/` |
| `preserve_order` activé transitivement casse la canonicalisation | Élevé | Faible | US-067 la convertit en test |
| Le verrou imbriqué masque une erreur d'outil légitime après le cran terminal | Faible | Faible | Le verrou porte le message terminal et tombe au tour suivant ; il n'est posé qu'au dernier cran d'une série de huit |

## Non-Goals

- **Détection sensible au résultat** (triplet `nom`, `arguments`, `résultat`). C'est la réponse correcte au faux positif sur une sortie qui change, mais elle exige de replier le résultat du batch précédent dans l'observation suivante, donc un état et un chemin de données nouveaux dans le cœur. Reportée, et nommée comme telle dans ADR-14.
- **Fenêtre glissante et détection non consécutive.** Le garde reste consécutif par batch. Une alternance A, B, A, B relève de la détection par fenêtre, hors périmètre.
- **Clé de configuration exposant les seuils.** Interdite par l'invariant 15.
- **Exposition des crans dans le TUI.** Le rappel est du contenu modèle ; aucune vue ni aucun instantané n'est ajouté.
- **Modification de `iter_cap` ou de `UsageBudget`.** Bornes externes indépendantes, non touchées.
- **Note miroir dans `docs/notes/`.** Interdite par la règle de frontière pour une décision portée par un ADR.
- **Traduction du message de rappel.** Contenu modèle, anglais, comme le reste du code.

## Files NOT to Modify

- Le clone du baseline Codex résolu par `$PYXIS_CODEX_BASELINE` : lecture seule, aucun commit, aucun checkout, aucune écriture.
- `docs/parity/codex-baseline-matrix.json` et `docs/parity/codex-client-model-matrix.json` : générés et empreintés, jamais édités à la main. Aucune matrice ne référence le garde-fou, donc aucune régénération n'est due.
- `crates/agent-cli/src/settings.rs` : aucune clé de configuration, invariant 15.
- `spikes/` : espace Phase 0 jetable et exclu.
- `docs/notes/` : aucune note pour cette décision.
- `crates/agent-app-server/src/protocol.rs` et les schémas générés : aucune méthode ni type de protocole ne change.

## Technical Considerations

Le garde-fou vit dans `agent-core` parce que le graphe de dépendances interdit `core` vers `tools` et parce qu'arrêter la boucle est une décision de terminaison qui appartient au cœur. Cette contrainte explique la forme de l'exemption : le cœur ne voit qu'un nom et une valeur JSON, ce qui ne suffit pas à distinguer une cellule d'orchestration d'un outil homonyme, donc il demande au dispatcher au lieu de deviner. La méthode ajoutée par US-064 suit exactement ce précédent, avec le même défaut fail-closed que `loop_guard_exempt`.

Le message du garde-fou est le premier contenu émis par le cœur qui cite l'entrée d'un outil. Deux chemins de bornage existaient : passer par `bound_feedback`, appliqué aujourd'hui aux seules sorties de dispatch (`agent.rs:1539-1550`), ou construire sous plafond. La construction sous plafond est retenue parce qu'elle est strictement plus forte, aucun message non borné n'existant même transitoirement, et parce qu'elle n'a pas besoin du tokenizer. Le plafond de 500 octets reste très en dessous de `MAX_MODEL_TOOL_RESULT_BYTES`, qui vaut 64 Kio.

L'ordre d'appel est la propriété qui rend le comptage des refus gratuit : `observe` est invoqué en amont de la dispatch, et les refus de permission sont produits à l'intérieur de `Registry::dispatch`. Un déplacement futur de `observe` sous la dispatch casserait silencieusement la propriété, ce que US-066 rend impossible.

`NestedLoopGuard` partage `LoopGuard` avec le site externe, ce qui est la raison pour laquelle l'échelle doit être portée par le type et non par les sites : deux échelles divergentes seraient une seconde source de vérité.

## Success Metrics

| Métrique | Baseline (2026-08-20) | Cible | Horizon |
|---|---|---|---|
| Crans de rappel avant arrêt déterministe | 1 (signal à 3, arrêt à 4) | 2 (3 et 5), arrêt à 8 | à la livraison de EP-018 |
| Batch fautif exécuté à un cran | 0 | 0, sans régression | à la livraison de EP-018 |
| Sites d'appel distinguant `Signal` de `Abort` | 1 sur 2 | 2 sur 2 | à la livraison de US-062 |
| Déclencheurs de remise à zéro de la chaîne | 1 (batch exempt) | 2 (interjection, tour neuf), le batch exempt retiré | à la livraison de EP-019 |
| Tests couvrant refus de permission, ordre des clés, bornage, steering | 0 sur 4 | 4 sur 4 | à la livraison de EP-020 |
| Enregistrements gouvernant le garde-fou | 0 | 1 ADR indexé, porte verte | à la livraison de EP-021 |
| Pointeurs de code vers une section d'architecture inexistante | 1 (`guardrail.rs:35`) | 0 | à la livraison de US-069 |

## Open Questions

| Question | Impact | Comment trancher |
|---|---|---|
| Un modèle corrige-t-il plus souvent au second rappel qu'au premier ? | Détermine si l'échelle `[3, 5, 8]` est la bonne, ou si `[3, 6]` suffirait | Non mesurable hors ligne. À observer sur les tours réels, le compte porté par `ExhaustReason::ToolLoop` disant déjà à quel cran la série est morte |
| La transparence des exemptions produit-elle des faux positifs sur des sondages de terminal réels ? | Détermine si la détection sensible au résultat doit remonter dans le périmètre | À observer après la livraison de US-065. Un seul faux positif reproductible suffit à rouvrir le Non-Goal |
| Le verrou imbriqué doit-il aussi terminer le tour externe ? | Aujourd'hui une cellule verrouillée rend des erreurs et la cellule se termine ; le tour continue | Laissé ouvert : le site externe a sa propre échelle et arrêtera le tour si le modèle relance la même cellule |
[/PRD]
