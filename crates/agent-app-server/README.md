# agent-app-server

The JSON-RPC protocol a graphical or remote client speaks to Pyxis: methods,
notifications, generated schemas, and the bridge that turns a client-declared
tool into a registered one.

## Model Experience

### The client-bridged tools

#### What the model sees

One entry per tool a connected client declared, built by `ClientTool` in
`crates/agent-app-server/src/bridge.rs` from the `name`, `description` and
`input_schema` the client sent. They are registered into the same registry as the
built-in ones, so they meet the same permission mode, the same taint defense, the
same hooks and the same cancellation.

#### Token effect

Decided entirely by the client: the description it sends is the description the
model reads, with no bound this crate imposes.

#### KV Cache effect

Remplacement de tokens antérieurs whenever a client re-declares its catalog
mid-session. The bridged entries sit in the tool array ahead of the transcript, so
a client that reconnects with one renamed tool invalidates the cached prefix of
the whole session. A client that keeps its catalog stable pays nothing.

### The bridged tool results

#### What the model sees

Whatever the client returned for a call, verbatim, inside a tool result. The
bridge holds it to the same fail-closed metadata as any external surface:
`is_read_only()` false, `is_sensitive()` true, `is_taint_sensitive()` true,
`returns_untrusted()` true.

#### Token effect

Unbounded by this crate and bounded downstream by the truncation and spill
policies of `agent-tools`.

#### KV Cache effect

Croissance en ajout seul: a bridged result is appended after everything already
sent, exactly like a built-in one, and the sameness is the point. Routing client
tools through one registry means their cache behavior needs no separate reasoning.
