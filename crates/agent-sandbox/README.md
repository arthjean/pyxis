# agent-sandbox

Confinement of a child process: the Landlock filesystem ruleset, the seccomp
filter, and the loopback HTTP proxy that answers a request to a host outside the
allow-list. Nothing here composes a message; it decides what a process may reach.

## Model Experience

Indirectly, through the 403 body the network proxy returns, which travels inside
the output of the execution tool that made the request.

#### KV Cache effect

Croissance en ajout seul: a refusal enters the transcript as one more tool
result, appended after everything already sent. The literal is fixed
(`blocked by pyxis network allow-list: ...`), so two identical refusals produce
identical bytes and neither rewrites a token the provider had cached.
