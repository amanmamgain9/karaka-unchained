# Open ends

Everything left undone, in one place. `port-notes.md` explains each item
in the section named after it. When a step finishes, add what it left
undone here, and delete anything it closed.

Last updated 2026-11-26, after the argument checks in the tool registry.

## Needs you

- **The Anthropic key is refused.** The key in founder-game-todo's
  `.env.local` gets "invalid x-api-key". It may have been rotated or
  revoked. Real calls use DeepSeek until it's replaced.
- **Three file-watch tests skip on this machine.** This account has used
  up its inotify instances, about 219 against a limit of 128. To run them,
  close some editors, or run
  `sudo sysctl fs.inotify.max_user_instances=512`, then
  `cargo test -p karaka-fs-local --test watch -- --nocapture`.
- **Directory listing order.** Karaka sorts names by their bytes, so
  capitals come first. dsh sorts by the machine's locale. Keep bytes, or
  switch to a case-insensitive sort that comes close to dsh.
- **Service tokens between the app and Karaka.** The proposed changes to
  `@karaka-ai/server-auth`, the SDK and `delegated-access` need your
  approval under founder-game-todo's `AGENTS.md`. Nothing has been
  changed. (Service tokens between the app and Karaka)

## Small fixes in what is built

- **Working-directory keeps a lock per session forever.** Its `turns`
  map never shrinks. dsh's is freed with the session. This matters only
  for a long-running server, so fix it with the webserver. (Working
  directory)
- **Compare agents by `instance()`, not by `Arc`.** The loop gives tool
  calls a different `Arc` from the one `Agents::open` returns.
  `Agent::instance()` gives both the same identity. Nothing is wrong
  today, but it's easy to trip over. (User questions)
- **Unverified: DeepSeek thinking signatures.** It isn't known whether
  DeepSeek signs thinking blocks, or accepts unsigned ones back. Multi-turn
  calls now work, so this can be checked. (Second audit against dsh v4)
- **pi-ai's per-message effort markers** aren't ported. (Second audit)
- **Assistant messages that stopped with an error** stay in the history
  sent to the model. (Second audit)

## Waiting on missing infrastructure

### The kernel

- **No teardown.** Plugins never unload, and a failed start doesn't roll
  back the plugins that already started. Because of this:
  - the tool registry can't remove a tool, so `todo_write` and
    `working_directory` stay registered for the runtime's life;
  - `WorkingDirectory::stop()` and `UserQuestions::close()` have to be
    called by hand;
  - session-postgres will need a drain on dispose.
- **Hooks.** Some exist as small registry ports: request errors, inbox
  signals, prompt assembly listeners. Still missing: `tools/execute`,
  `tools/pre-execute`, `tools/post-execute`, `agent/pre-step`,
  `agent/turn-stopping`, `approval/request`. (Kernel gaps)
- **Per-agent composition.** Each agent preset needs its own tools,
  prompt and skills, and main-agent's approval gate must fire only for
  main-agent. Prompt contributions and answerers can be scoped to one
  session, but tools can't yet. (Kernel gaps)
- **A logger.** dsh warns on a torn log tail and a failed background
  batch. Karaka writes nothing, or prints to stderr. (Durable sessions,
  parts A and B)

### The agent loop

- **Parallel tool calls and exclusive barriers.** Calls run one after
  another. (Agent loop, step one)
- **`concludesTurn`**, which lets a tool end the turn. (Agent loop, step
  one)
- **Tool results carry text only.** dsh's also carry images, a
  structured error `{ name, code, reason }`, presentation data and extra
  context for the next step. Play This Life shows its upgrade prompt on
  the `pro_required` code. (Audit: what Play This Life reads)
- **Developer messages for tool additions and removals.** Only the
  request header records a change of tools. (Agent loop, step one)
- **Inbox:** the `inserted` signal and the inbox's `replace` and `remove`
  aren't ported. (Agent inbox)
- **Child agents.** There are none, so `DELEGATED_CALLER` in user
  questions isn't checked. (User questions)
- **`Agents::open` is synchronous.** (Durable sessions, part B)

### Sessions

- **v4's cross-row checks** (`assertReleasedV4Relationships`): turn, step
  and tool-call lifecycles, retries, compaction and the rest. (Part C)
- **Full payload adoption.** The typed reader checks the shapes it
  models, in its own words. (Part C)
- **Forks and seeded sessions.** (Part C)
- **Message projections** other than `image/offload`'s admission.
  (Part C)
- **The projection cache:** `hydrate` and `cachedSnapshot`, which come
  with the cache plugin. (Session projections)
- **The checkpoint policy's per-request barrier.** `chat` flushes once per
  turn instead. (Part A)
- **Smaller ones:** the cold-log memo, list concurrency, and
  frame-by-frame cancellation of zstd decoding. (Parts A and B)
- **User message `source.rpcId`**, which Play This Life uses for prompt
  idempotency and its tool guard. (Audit: what Play This Life reads)

### Clients

- **No wire to UI clients.** dsh's typert `@Remote` methods aren't
  ported. The user-questions "time remaining" stream is a plain Rust
  stream for now. This comes with the webserver. (User questions)
- **No presentation layer.** `presentCall` and other card data for UIs
  aren't ported. (The todo_write tool)

### Platform

- **Windows.** `karaka-fs-local` and the JSONL session backend build on
  Unix only. (Filesystem; Durable sessions, part A)

## Waiting on a plugin not yet ported

- **Sandbox policy.** File-write sandboxing, and the policy line in the
  runtime context. (Filesystem; Working directory)
- **Approvals** (`user-approval`): the approval-policy line in the runtime
  context, and the approval half of working-directory's admission test.
  (Working directory)
- **The filesystem observation policy:** the `fs/write-intent`,
  `fs/edit-intent` and `fs/observed` events. (Filesystem)
- **Compaction.** It needs the context-overflow error, which exists now.
  (Agent loop, step one)
- **System-prompt sections** from other plugins: plan mode, per-tool
  guidance, MCP servers and the rest. (System prompt)

## Still to port, toward Play This Life

From Play This Life's plugin list:

- **Next:** `tool-ask-user`, with a terminal answerer in the binary.
- **Questions and approvals:** `user-approval`.
- **Models and storage:** `agent-default-model` (needs dsh's settings
  service), `session-query-sqlite`, `storage`, `credentials-local`.
- **Presets and personas:** `agent-preset`, `agent-preset-registry`,
  `persona`.
- **Skills:** `skill`, `tool-skill`, `skill-filesystem`.
- **The rest:** `mcp-client`, `session-reference`, `webserver`,
  `agent-tool-presentation`, `deepseek-llm-api-extensions`.

Karaka's own: `token-meter`, `compaction-basic`,
`compaction-tool-result-pruner`, `session-checkpoint-policy`,
`timeout-policy`.

## Decided, not to do

- **No migration of older session formats.** Karaka reads and writes v4
  only, and refuses anything older. (Durable sessions, part C)
- **Cases Rust rules out**, such as non-integer versions and Promise
  views. They are listed in each section and need no work.
