# agent-code-mode

Code mode: the `exec` tool, the nested tool catalog it renders as TypeScript
declarations, and the bridge from a JavaScript cell to the registry. In this mode
the model calls two tools directly and reaches every other one from code, so what
it reads about a tool is not the tool's own description.

## Model Experience

### The exec tool description

#### What the model sees

The description `exec_description` composes in
`crates/agent-code-mode/src/tools.rs`, rendered in the catalog as
[`exec`](../../docs/tool-catalog.md#exec): the helper list, the paragraph saying
that this surface exists only in code mode, and the `EXEC_GRAMMAR` the input is
constrained by, adopted verbatim from the baseline so a model trained on Codex
sends bytes Pyxis accepts unchanged.

#### Token effect

Several hundred tokens for the fixed part, before the rendered catalog is
appended to it, which makes `exec` by far the largest single tool description of
the workspace.

#### KV Cache effect

Préfixe stable répété: the fixed part is a compile-time constant and moves only
when this repository changes. Because it is one entry of the tool array, it sits
ahead of the transcript, so editing it invalidates every open session exactly as
editing any other description does.

### The nested tool catalog

#### What the model sees

Every nested tool as a TypeScript declaration inside a fenced block, sorted by
binding, each preceded by at most `CATALOG_DESCRIPTION_LINES = 4` comment lines
of its own description:

```ts
// Read a file from the filesystem.
declare function read(input: { path: string }): Promise<string>;
```

#### Token effect

Four description lines per tool instead of the whole description, which is what
keeps a 29-tool catalog from doubling the `exec` entry. A cut description says it
was cut, with the marker `// [description truncated]`, so the visible part is
never read as the whole contract.

#### KV Cache effect

Préfixe stable répété, on a deliberately narrow surface. Sorting by binding is
part of it: an insertion changes the position of one declaration and not the
order of the rest, and the four-line cut means most description edits do not
reach the rendered catalog at all.

### The exec cell result

#### What the model sees

What the cell returned, or the exception it threw, brought back from the V8
isolate by `agent-code-mode-v8` and delivered as an ordinary tool result. A
nested tool called from the cell produces no tool result of its own: only the
cell's own return value reaches the transcript.

#### Token effect

One result per cell instead of one per nested call, which is the trade code mode
makes: a loop over twenty files costs one result rather than twenty.

#### KV Cache effect

Croissance en ajout seul, and less of it than the direct mode would produce. The
saving is real but bounded: a cell that returns everything it read puts the same
bytes in the transcript as the twenty results it replaced.
