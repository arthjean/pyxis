# Numen

Agent de code IA dans le terminal — la qualité de Claude Code, mais ouvert à **tous les modèles frontier**. Full Rust natif pour la performance, cœur headless partagé avec [Paneflow](https://paneflow.dev). La commande s'appelle `numen` et s'ouvre directement dans ton shell.

> *Numen* (latin) : la présence, la volonté qui anime. Garde l'âme de *daimon* — un esprit-guide pour ta base de code.

---

## Statut

**ÉTUDE / DESIGN — pré-implémentation.** Aucun code n'est écrit. Ce dépôt contient pour l'instant la spécification d'architecture et les décisions de conception. La phase d'implémentation démarre par un spike de dé-risquage (voir [`docs/ROADMAP.md`](docs/ROADMAP.md)).

---

## Ce qui rend Numen différent

Pas un wrapper de plus. **Quatre** partis pris structurants :

- **Full Rust natif, ultra-perf.** Pas de runtime Node, pas de couche TS. Binaire unique, démarrage immédiat, empreinte mémoire serrée. Le coût assumé : une vélocité de dev solo plus lente — c'est le risque d'exécution numéro un, pas un détail caché.
- **Multi-provider first-class.** Là où Claude Code est Anthropic-only, Numen traite Anthropic, OpenAI, Gemini, Ollama, OpenRouter, Bedrock/Vertex/Azure comme des citoyens de première classe. Format canonique interne *Anthropic-like* (content blocks), couche maison (`reqwest` + `eventsource-stream`), divergences localisées par adapter. Positionnement **model-agnostic** par design.
- **Intégration profonde avec Paneflow.** `agent-core` est *headless* : il n'émet que des événements structurés, jamais d'ANSI. Le frontend terminal n'est qu'un client. Conséquence : Paneflow (GPUI, donc Rust) peut embarquer `agent-core` **in-process** — pas d'IPC, types partagés — et rendre les events en GPU (diffs accélérés, arbre de plan, review par hunk) via un protocole d'enrichissement, sans jamais casser le mode terminal par défaut.
- **Frontend Ratatui monochrome.** UI cible : épurée, moderne, esthétique Rauch/Vercel — pas un TUI « à l'ancienne » avec bordures doubles et couleurs criardes. Rappel : Ink (Claude Code) *est* un TUI qui rend de l'ANSI ; le plafond visuel d'un terminal est identique pour Ink et Ratatui. C'est le design qui fait toute la différence.

---

## L'expérience

`numen` s'ouvre dans le shell, ce n'est **pas** une fenêtre. Tu lances, tu parles, l'agent boucle (stream → outil → reboucle).

```console
$ numen
  numen · ollama/qwen2.5-coder · ~/dev/myproject

› refactore le module auth pour retirer les unwrap() en prod

  ⠋ lecture de src/auth/mod.rs
  ⠙ édition de src/auth/token.rs  (3 hunks)
  ✓ cargo clippy --no-deps  ·  0 warning

  Remplacé 4 unwrap() par ? / ok_or(...). Diff ci-dessus.

# mode headless, scriptable, sans Ratatui
$ numen -p "résume les changements du dernier commit" --model openai/gpt-4o
```

Le mode headless (`-p`) ne dépend pas de Ratatui : `agent-core` tourne sans I/O terminal, ce qui le rend aussi testable sans API.

---

## Espace de noms

Les crates `numen`, `numen-cli` et `numen-core` sont confirmées **libres** sur crates.io — atout décisif pour un projet cargo, et la raison du choix du nom après un sweep où *daimon*, *sigil*, *pneuma*, *eidolon* et *glyph* étaient tous en collision majeure dans l'espace agent IA 2026. Le workspace interne raisonne en crates `agent-*` (`agent-core`, `agent-tui`, `agent-provider`…) ; voir [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) et [`docs/DECISIONS.md`](docs/DECISIONS.md) pour le mapping entre noms publiés et crates internes.

---

## Documentation

| Document | Contenu |
|---|---|
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Workspace de crates, boucle d'agent, système d'outils, compaction, sessions, sandbox |
| [`docs/PROVIDERS.md`](docs/PROVIDERS.md) | Couche multi-provider, format canonique, divergences par provider, retry & cache |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Phase 0 (spike) → Phase 3 (durcissement & distribution) |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | Décisions fermes et arbitrages écartés (langage, frontend, couche réseau, nom) |

---

## Risque produit n°1

Depuis janvier 2026 (durci en avril 2026), Anthropic bloque les outils tiers qui s'authentifient via un abonnement Pro/Max. Un agent tiers ne peut plus utiliser un abonnement Max. Mitigation : provider MVP **non-bloqué** (Ollama local + OpenAI au token), positionnement model-agnostic. Le spike auth Anthropic est le go/no-go de Phase 0.

---

## Licence / OSS

À définir.
