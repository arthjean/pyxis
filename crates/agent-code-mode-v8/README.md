# agent-code-mode-v8

The V8 isolate that runs an `exec` cell: one JavaScript engine, a host bridge for
the `tools` object, and the deadline that stops a runaway cell. It knows nothing
about the agent loop and nothing about the model.

## Model Experience

Indirectly, through the `exec` tool result that `agent-code-mode` builds from
what this isolate returned or threw.

#### KV Cache effect

Requête indépendante: an isolate that takes longer, or that throws instead of
returning, changes the bytes of one tool result and therefore the suffix of the
transcript from that result onward. Nothing this crate does can move a token that
was already sent, so a slow cell costs wall clock, never a cache invalidation.
