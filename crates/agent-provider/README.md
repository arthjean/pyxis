# agent-provider

The `Provider` trait and its implementations: the ChatGPT and OpenAI Responses
backends, Bedrock, the model catalog, and the wire encoding of a
`CanonicalRequest`. It is the last crate a byte passes through before it becomes
a request, so it owns the shape of the prefix rather than its content.

## Model Experience

### The Responses request body

#### What the model sees

The body `build_responses_body` composes in
`crates/agent-provider/src/chatgpt_request.rs`: the system text as
`instructions`, a string and never an `input[]` item, the tool array as `tools`,
and the whole client-side transcript as `input`. The `Lite` dialect makes the
order literal instead, moving the tools to `input[0]` and the instructions to
`input[1]`.

#### Token effect

The transport adds no prose of its own. What it decides is that the transcript is
sent whole on every turn: the backend is stateless, there is no
`previous_response_id`, so every request pays for the full history again.

#### KV Cache effect

Préfixe stable répété, and this is where the ordering becomes load-bearing. The
tools and the instructions sit ahead of the transcript in both dialects, so they
are the region the backend can reuse across turns, and any change to them
invalidates everything behind. Sending the whole transcript every turn is only
affordable because of that reuse.

### The prompt cache key

#### What the model sees

Nothing, but the backend does: `session_id`, a UUID v4 generated once per
provider instance and sent as `prompt_cache_key` on every request, in
`crates/agent-provider/src/chatgpt.rs`.

#### Token effect

None. The key is metadata and is not part of the prompt.

#### KV Cache effect

Préfixe stable répété, and the key is what makes it addressable. The identifier
is deliberately stable for the life of the instance, so the backend recognizes
the requests of one session as sharing a prefix. Rotating it mid-run would keep
the bytes identical and lose the cache anyway, which is why nothing rotates it
outside `set_prompt_cache_key`.

### The embedded instructions fallback

#### What the model sees

One of the two prompts of `crates/agent-cli/prompts/`, compiled into this crate
by `include_str!` as `GENERIC_INSTRUCTIONS` and `CODEX_INSTRUCTIONS`
(`crates/agent-provider/src/models/embedded.rs`) and carried as the
`instructions` of an embedded `ModelDescriptor`. They answer only when the remote
catalog at `https://chatgpt.com/backend-api/codex/models` is unreachable, so an
offline start has a prompt rather than none. What this crate owns is the
fallback; how the text is composed with the harness contract belongs to
`agent-cli` and is written in
[`crates/agent-cli/README.md`](../agent-cli/README.md), not duplicated here.

#### Token effect

3 449 bytes for the two files together, of which exactly one is ever resolved in
a given run. A remote catalog that answers replaces them entirely, so the
fallback adds nothing on the normal path.

#### KV Cache effect

Préfixe stable répété: the descriptor is resolved once and its `instructions` do
not change during the run. The two starts that differ, one reaching the catalog
and one falling back, produce two different prefixes, which is a cache miss
between runs and never inside one.

### The repaired tool-call pairing

#### What the model sees

A synthetic result inserted for a `function_call` that never got one, and an
orphan `function_call_output` dropped. The repair happens in memory while the
body is built and never rewrites the `.jsonl` on disk.

#### Token effect

A few tokens for the synthetic result, against a backend that rejects the entire
request when a call is left unanswered.

#### KV Cache effect

Remplacement de tokens antérieurs, confined to the point of interruption: the
repair alters the transcript at the orphan call, so everything from there onward
is a cache miss. That is the cost of a session that stays usable, and it is paid
once rather than at every turn, because the repair is deterministic and produces
the same bytes on the next request.
