# agent-mcp

The MCP client: server discovery, the tool and resource catalogs a server
publishes, and the `Tool` adapter that exposes them to the agent. Everything it
carries comes from outside the repository, which is the whole reason its defaults
are what they are.

## Model Experience

### The bridged server tools

#### What the model sees

One entry per tool a configured MCP server publishes, added to the same `tools`
array as the built-in ones. The description is the server's own, passed through
unrewritten (`info.description.clone()` in `crates/agent-mcp/src/tool.rs`); only
an empty one is replaced, by `Tool "{original_name}" exposed by the
MCP server "{server}".`. What this repository does refuse is the server-level prose:
`initialize.instructions` is deliberately not folded in, because a description
reaches the model inside the tool definitions, a region no tool output ever
taints, so smuggled prose would be injection the taint defense is structurally
unable to see.

#### Token effect

Unbounded and outside this repository's control, since the text is the server's:
a server publishing forty tools adds forty entries to the prefix of every request
of the session, bounded only by `MAX_SCHEMA_BYTES` per input schema. The
countermeasure is not rewriting the prose but the fail-closed policy around it,
baseline `Ask` and full taint propagation, which holds whatever the description
claims about itself, `read_only` annotation included (CVE-2025-6514).

#### KV Cache effect

Préfixe stable répété while the server list holds, remplacement de tokens
antérieurs the moment it does not. A server that reconnects with a changed
catalog rewrites the tool array, which sits ahead of everything, so it costs the
cache of the session even though not one conversational message changed.

### The resource tools

#### What the model sees

Three descriptions written in this repository, in
`crates/agent-mcp/src/resource_tools.rs`, rendered in the catalog as
[`list_mcp_resources`](../../docs/tool-catalog.md#list_mcp_resources),
[`list_mcp_resource_templates`](../../docs/tool-catalog.md#list_mcp_resource_templates)
and [`read_mcp_resource`](../../docs/tool-catalog.md#read_mcp_resource).

#### Token effect

592 bytes of description across the three, present whenever at least one server
is configured.

#### KV Cache effect

Préfixe stable répété: unlike the bridged tools, these three are ours and their
text is fixed at compile time, so they are the part of the MCP surface that
cannot move under a session.

### The resource contents

#### What the model sees

Whatever a server returns for a read, verbatim, inside a tool result. The adapter
in `crates/agent-mcp/src/tool.rs` is fail-closed about it: `is_read_only()` is
false, `is_sensitive()` is true, `is_taint_sensitive()` is true, and
`returns_untrusted()` is true, because a server cannot declare its own output
trusted (CVE-2025-6514).

#### Token effect

Bounded by the same truncation and spill policies as any other tool result, and
by nothing else.

#### KV Cache effect

Croissance en ajout seul: a resource read is appended like any tool result and
moves nothing ahead of it. The taint it carries travels with the result rather
than rewriting the prefix, which is what keeps a defensive default from being
expensive.
