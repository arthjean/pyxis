# agent-mcp

The MCP client: server discovery, the tool and resource catalogs a server
publishes, and the `Tool` adapter that exposes them to the agent. Everything it
carries comes from outside the repository, which is the whole reason its defaults
are what they are.

## Model Experience

### The bridged server tools

#### What the model sees

One entry per tool a configured MCP server publishes, added to the same `tools`
array as the built-in ones. The name and the input schema come from the server;
the prose does not, because a server-supplied description would be untrusted text
sitting in the highest-authority region of the request.

#### Token effect

Unbounded and outside this repository's control: a server publishing forty tools
adds forty entries to the prefix of every request of the session.

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

Around 780 bytes of description across the three, present whenever at least one
server is configured.

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
