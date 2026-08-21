# agent-session

Persistence of a session: the `.jsonl` transcript on disk, its rollout, and the
resume that reads it back. It stores what was already exchanged and adds no text
of its own.

## Model Experience

Indirectly, through the messages a resume puts back into the transcript, which
are the ones a previous run had already sent.

#### KV Cache effect

Préfixe stable répété: a resume replays the persisted messages in the order they
were written, so a resumed session presents the same prefix as the run it
continues. The one thing that breaks that property is a transcript repaired on
read, which is why the repair happens in memory inside `agent-provider` and never
rewrites the `.jsonl`.
