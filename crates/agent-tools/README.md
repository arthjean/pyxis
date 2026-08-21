# agent-tools

The built-in tool implementations and the registry that holds them: filesystem,
shell, search, planning, subagents, permissions, spill policy. Every tool the
model can call directly without an MCP server or an app-server client behind it
is defined here.

## Model Experience

### Tool descriptions

#### What the model sees

The `description()` of every registered tool, one entry per tool in the `tools`
array of the request, rendered for a reader in
[`docs/tool-catalog.md`](../../docs/tool-catalog.md): 29 tools today, from
[`apply_patch`](../../docs/tool-catalog.md#apply_patch) to
[`write_stdin`](../../docs/tool-catalog.md#write_stdin), each with its JSON input
schema.

#### Token effect

The catalog section rendering those descriptions is about 26 KB of Markdown,
which puts the tool block in the low thousands of tokens on every single request
of the session, before a word of conversation is counted.

#### KV Cache effect

Préfixe stable répété: the tool array is assembled the same way on every request
of a run, so it is the part of the prefix a provider can reuse for free.
Rewording one `description()` moves bytes at the very front of that prefix and
therefore invalidates the cache of every open session, not only the next request.
That is why a description edit is a contract change and not a style change.

### Tool results

#### What the model sees

The `content` of every `ToolResult`, verbatim: the bytes a `read` returned, the
stdout and stderr of a `bash`, the match lines of a `grep`. Nothing rewrites
them, and `returns_untrusted()` marks them as data rather than instructions
without removing a byte.

#### Token effect

Unbounded in principle and bounded in practice by the truncation and spill
policies below. A single unbounded `bash` output is the largest thing a turn can
put in front of the model.

#### KV Cache effect

Croissance en ajout seul: a result is appended after everything already sent, so
it extends the prefix without moving it. The cost lands later, when the
accumulated results push the budget over its threshold and force a compaction
that does replace earlier tokens.

### Truncation and spill notices

#### What the model sees

Two fixed literals framing an output that did not fit, the truncation marker with
its default continuation hint and the spill notice with its locator:

```text
[tool output truncated; strategy={}; continuation={hint}]
Re-run the tool with a narrower query or explicit range.
(Omitted {omitted} bytes from the middle. Full output saved to {locator}. Read it with `read` using `offset` and `limit`, or search it with `grep`.)
```

#### Token effect

A few dozen tokens each, in exchange for the thousands the omitted output would
have cost. The trade only works because the notice names a recovery, so the model
spends those tokens instead of asking the user for the missing text.

#### KV Cache effect

Croissance en ajout seul: a notice is part of the result it closes and is
appended with it. The one loop it must not close is the reason
`NEVER_SPILLED = &["read"]` exists: spilling a `read` would invite the model to
read the spill file, which would spill again, and each round would add a result
the provider has to bill.

### The context budget notice

#### What the model sees

The answer of
[`get_context_remaining`](../../docs/tool-catalog.md#get_context_remaining),
which is either a remaining-token figure or the literal `The context budget has
not been published yet (no model turn has completed in this run). Proceed without
a remaining-token figure.`

#### Token effect

One short result, negligible against what it prevents: a fabricated "100% free"
is the answer that makes a model open another twenty files.

#### KV Cache effect

Requête indépendante: the figure changes at every turn, so this result is never
reused across turns. Being a tool result rather than an injected section, it sits
in the suffix and costs nothing to the stable prefix ahead of it.
