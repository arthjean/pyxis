# spikes/ — Phase 0 (EP-001), code JETABLE

Workspace de dé-risquage. Chaque crate prouve **une** hypothèse, pas une feature.
**Rien ici n'est du MVP** : le futur workspace `agent-*` vivra à la racine du repo et
ne réutilisera pas ce code. Verdicts détaillés : [`../tasks/spike-verdicts.md`](../tasks/spike-verdicts.md).

| Crate | Story | Prouve |
|---|---|---|
| `s1-provider-access` | US-001 | accès provider non-bloqué (go/no-go auth) |
| `s2-canonical-sse` (lib `spike_canon`) | US-002 | flux SSE → `StreamEvent` canoniques (reqwest + eventsource-stream) |
| `s3-agent-loop` (lib `spike_loop`) | US-003 | boucle `Transition` : stream → Bash → réinjection → reboucle |
| `s4-tui-stream` (lib `spike_tui`) | US-004 | tube `core → mpsc<AgentEvent> → Ratatui`, jamais d'ANSI |
| `s5-sandbox` | US-005 | Landlock FS kernel + proxy réseau allow-list |
| `s6-code-mode-v8` (lib `spike_v8`) | US-005 de `prd-parite-totale-codex-cli` | enveloppe V8 : interruption externe, plafonds heap/natif distincts, shutdown joint |

## Gates
```bash
cargo test --workspace          # 18 tests déterministes (preuves sans réseau)
cargo clippy --workspace --all-targets --no-deps
cargo fmt --all --check
```

## Runs live
```bash
# US-005 (parité Codex) — enveloppe V8 : mesures reproductibles
cargo run --release -p s6-code-mode-v8 -- report          # texte
cargo run --release -p s6-code-mode-v8 -- report --json   # même rapport, sérialisé

# US-005 — sandbox (réel, Linux/Landlock)
cargo run -p s5-sandbox -- landlock
cargo run -p s5-sandbox -- proxy

# US-001 / US-002 / US-003 — via Ollama local (aucune clé)
OLLAMA_MODEL=arthurjean/mistral-trismegistus:7b-q6_K cargo run -p s1-provider-access -- ollama
OLLAMA_MODEL=arthurjean/mistral-trismegistus:7b-q6_K cargo run -p s2-canonical-sse -- "Compte de 1 à 3."
OLLAMA_MODEL=devstral-small-2:24b                     cargo run -p s3-agent-loop          # tool-capable

# US-004 — TUI : interactif si TTY, dump headless sinon
cargo run -p s4-tui-stream                            # lance dans un vrai terminal truecolor

# US-001 — legs à clés (à compléter par Arthur, non bloquant)
OPENAI_API_KEY=sk-...        cargo run -p s1-provider-access -- openai
ANTHROPIC_OAUTH_TOKEN=...    cargo run -p s1-provider-access -- anthropic   # capture le message de blocage
```
