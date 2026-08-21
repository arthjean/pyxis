# agent-tokenizer

Token counting for the models this workspace speaks to: the BPE tables and the
estimate used when a provider returns a turn without usage.

## Model Experience

Indirectly, through the compaction threshold its count crosses, which decides
what the next turn will still be allowed to see.

#### KV Cache effect

Remplacement de tokens antérieurs: this crate writes nothing, but a count that
drifts high fires compaction early, and compaction is the one operation that
replaces already-sent messages with a summary. An estimate that is wrong by
enough to move the threshold therefore throws away a prefix that was still
valid.
