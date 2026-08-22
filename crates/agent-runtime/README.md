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
verbatim under the diagnostic `` section `{}` unreadable, reused ``. The model
sees no gap and no error, because a transient read failure is not information it
can act on.

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

### The background job completion notice

#### What the model sees

One user-role message opening a turn nobody asked for, composed by
`completion_notice` in `crates/agent-runtime/src/jobs.rs`: the job identifier,
its terminal status with its exit code or its cause, the command it ran cut at
160 characters with its control characters neutralized, and the sentence naming
`list_jobs` as where the output is. Never the output itself, which stays in the
registry until the model asks for it.

#### Token effect

Around 60 tokens per announcement, bounded by construction: the command is the
only variable part and it is capped, so a job whose process wrote megabytes
costs the same as one that wrote nothing. At most three such messages can enter
a thread without an intervening human input (`MAX_CONSECUTIVE_WAKES`), and a
given job can produce at most one of them, because delivery marks it reported.

#### KV Cache effect

Préfixe stable répété. The notice is appended to the transcript exactly like a
human message, so it extends the prefix instead of rewriting it. This is why the
announcement is an index into `list_jobs` rather than the process output: an
unbounded transcript spliced into the thread would push every later turn past
the cached window for a result the model may never read.

### The scheduled reminder notice

#### What the model sees

One user-role message opening a turn nobody asked for, composed by
`reminder_notice` in `crates/agent-runtime/src/thread.rs`: the identifier of the
reminder that came due and the prompt its creation stored, verbatim. When several
reminders fall due in the same wake, they arrive as a single message listing one
line per occurrence, never as one message each. The model is not told the slot it
is answering for, because the slot is a property of the log and not of the task.

#### Token effect

The prompt dominates, and it is bounded at creation by `MAX_SCHEDULE_PROMPT_CHARS` in
`crates/agent-runtime/src/schedule.rs`; the frame around it costs around 25
tokens for a single reminder and around 10 per line for a batch. The batch is
what bounds the worst case: a thread holding many due reminders pays one frame,
not one per reminder. At most three such messages can enter a thread without an
intervening human input, and the budget is the SAME
`MAX_CONSECUTIVE_WAKES` a job completion spends (ADR-17), so a thread cannot be
woken three times by its jobs and three more times by its reminders.

#### KV Cache effect

Préfixe stable répété. The notice is appended like a human message, so it extends
the prefix rather than rewriting it. A reminder that comes due while a turn is
running is steered into that turn instead of opening a second one, which is also
a cache decision: opening a turn would have restarted the prefix for text the
running turn was about to read anyway.
