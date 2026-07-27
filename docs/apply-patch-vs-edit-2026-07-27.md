# `apply_patch` contre `edit` : mesure du taux d'échec d'édition

Validation de l'hypothèse déclarée en assumptions de
`tasks/prd-parite-codex-par-le-code.md` et exigée par le critère AC6 d'US-010 :
« les modèles `*-codex` produisent de meilleurs résultats d'édition avec
`apply_patch` qu'avec `edit`. Fondé sur le fait qu'ils y sont entraînés, **non
mesuré sur Pyxis**. »

Mesure conduite le 2026-07-27, sur Pyxis à EP-003, modèle `gpt-5.3-codex-spark`
(le seul slug `*-codex` servi par le canal d'abonnement ChatGPT au moment de la
mesure ; `gpt-5.1-codex` est refusé par le backend).

## Résultat

| Outil | Tâches | Appels d'édition | Appels en échec | Taux d'échec | Tâches abouties | Durée moyenne |
|---|---|---|---|---|---|---|
| `edit` | 22 | 39 | 3 | **7,7 %** | 22/22 | 11,8 s |
| `apply_patch` | 22 | 24 | 0 | **0,0 %** | 22/22 | 8,4 s |

**L'hypothèse est confirmée dans son sens, et corrigée dans sa portée.**

`apply_patch` n'échoue jamais là où `edit` échoue une fois sur treize, mais la
différence de taux d'échec n'est **pas statistiquement significative** à cet
effectif (Fisher bilatéral, p = 0,28). Ce qui l'est, c'est le coût du même
travail : **39 appels contre 24 pour un résultat identique, soit 38 % d'appels
en moins**, et un écart mécaniquement explicable plutôt que statistique.

Surtout, **les deux outils aboutissent : 22/22 des deux côtés**. Le modèle
récupère de ses échecs d'ancrage en réessayant. L'écart n'est donc pas sur ce
que l'agent finit par produire, il est sur le nombre d'aller-retours qu'il lui
faut pour y arriver, et donc sur les tokens et le temps consommés.

## Où `edit` échoue

Les trois échecs sont tous des échecs d'ANCRAGE, sur des éditions à plusieurs
sites :

| Tâche | Message rendu au modèle |
|---|---|
| `add-field` | `anchor not found in src/config.rs after 4 passes (exact, trim_end, trim, Unicode)` |
| `dup-blocks` | `ambiguous anchor in src/client.rs: 2 matches` |
| `const-rename` | `ambiguous anchor in src/client.rs: 2 matches` |

C'est le mode de défaillance attendu : `edit` exige une ancre unique, donc une
modification répétée à l'identique (renommer une constante utilisée trois fois,
corriger deux boucles jumelles) le met en difficulté par construction.
`apply_patch` groupe ces sites dans un seul patch et le problème ne se pose pas.

L'écart de coût se concentre exactement là :

| Tâche | Appels `edit` | Appels `apply_patch` |
|---|---|---|
| `add-field` | 5 | 1 |
| `dup-blocks` | 5 | 1 |
| `const-rename` | 4 | 1 |
| `rename-fn`, `py-field`, `sh-flag`, `go-error` | 2 | 1 |
| les 15 autres | 1 | 1 |

Sur une édition à un seul site, les deux outils font le même travail en un
appel. **La différence n'existe que sur les éditions à plusieurs sites.**

## Ce que la mesure ne dit pas

- **Effectif faible.** 39 et 24 appels, ce qui satisfait le seuil de 20 éditions
  du critère mais ne suffit pas à rendre significatif un écart de 7,7 points.
  Une mesure en usage réel sur plusieurs semaines de sessions trancherait mieux.
- **Un seul modèle.** `gpt-5.3-codex-spark`. Rien n'est mesuré sur les modèles
  non `*-codex`, alors que l'hypothèse du PRD porte spécifiquement sur les
  premiers.
- **Tâches synthétiques.** Fixtures écrites pour le banc, pas extraites de
  sessions passées. Elles sont réalistes (Rust, Python, Go, JavaScript, shell,
  Markdown, TOML) et l'instruction décrit toujours le RÉSULTAT voulu, jamais une
  ancre ni un patch, mais ce ne sont pas des éditions tirées d'un travail réel.
- **Deux runs ont utilisé `bash`** malgré la consigne d'exclusivité
  (`go-error`, des deux côtés). Comptabilisés comme appels hors outil, sans
  effet sur les taux, qui ne portent que sur l'outil imposé.

## Décision

**`edit` est conservé.** Son retrait serait de toute façon une rupture de
comportement hors périmètre du PRD, et la mesure ne le justifie pas : les deux
outils aboutissent aussi souvent. C'est la réponse à la question ouverte « faut-il
conserver `edit` après `apply_patch` ? ».

La règle de choix est celle déjà inscrite dans les `behavioral_guidelines` de
l'outil, et la mesure la confirme : `apply_patch` pour plusieurs sites ou
plusieurs fichiers, `edit` pour une ancre unique.

## Rejouer la mesure

Le harnais est dans `docs/bench/edit-vs-apply-patch/` : `tasks.py` porte les 22
tâches (fixtures, instruction, oracle de résultat), `run.py` exécute chaque tâche
deux fois en headless dans un workspace neuf, avec l'outil d'édition imposé par
une consigne de prompt, puis dépouille les sessions JSONL en appariant chaque
`tool_use` à son `tool_result`. `results.json` conserve les 44 runs de cette
passe.

```
python3 docs/bench/edit-vs-apply-patch/run.py
```

Un biais a été corrigé en cours de route : l'oracle de la tâche `py-guard`
n'acceptait que `sys.exit(2)` alors que `raise SystemExit(2)` est équivalent.
`apply_patch` avait produit la seconde forme et était compté en échec de tâche à
tort. L'oracle accepte désormais les deux, et le run concerné est rejugé dans
`results.json` (`oracle_fixed: true`). Aucun taux d'échec d'appel n'était touché
par cette correction.
