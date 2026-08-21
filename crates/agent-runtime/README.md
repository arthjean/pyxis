# agent-runtime

The thread and turn lifecycle: `TurnRunner`, the step context assembly, the
cancellation tree, and the durability of an accepted operation. It reaches
`run_agent` and never reimplements it, so what it owns of the model experience is
not the loop but the bytes injected around it.

## Model Experience

### The step context sections

#### What the model sees

The sections `StepContext::build` resolves in
`crates/agent-runtime/src/context.rs`, each rendered as a named block: the
project instructions, the environment, and whatever else a step source declared.
Stable sections are injected first and volatile ones last, so a section that
changes on its own cannot displace one that does not.

#### Token effect

Each section is bounded at `MAX_SECTION_BYTES = 32 * 1024` and the whole
injection at `MAX_STEP_CONTEXT_BYTES = 64 * 1024`, which is around 16 000 tokens
of ceiling on everything injected into a single step.

#### KV Cache effect

Préfixe stable répété for the stable half, remplacement de tokens antérieurs
avoided by construction for the volatile half. Ordering stable before volatile is
the cache decision of this crate: a date that moves is appended after the project
instructions rather than in front of them, so a rollover costs the tail and never
the head.

### The unreadable-section memo

#### What the model sees

The previously resolved text of a section whose source became unreadable, reused
verbatim under the trace `section "{}" unreadable, reused`. The model sees no gap
and no error, because a transient read failure is not information it can act on.

#### Token effect

None: the memo substitutes the same bytes the previous step already carried.

#### KV Cache effect

Préfixe stable répété, and that is the reason the memo exists rather than a
fallback to an empty section. Dropping the section would have rewritten the
prefix on a transient failure and rewritten it back on the next step, paying two
invalidations for a file that was briefly locked.

### The step generation counter

#### What the model sees

Nothing. `StepContext.generation` is bumped only when the injected bytes actually
moved, so two steps sharing a generation are known to have produced the same
prefix.

#### Token effect

None: the counter never leaves the runtime.

#### KV Cache effect

Préfixe stable répété, made observable. The counter is the runtime's own record
of whether a prefix survived, which is what lets a change of injected context be
diagnosed as a cache event rather than guessed at from latency.
