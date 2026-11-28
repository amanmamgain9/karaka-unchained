# Open ends

Everything left undone, in one place. `port-notes.md` explains each item
in the section named after it. When a step finishes, add what it left
undone here, and delete anything it closed.

Last updated 2026-11-28, after persona, the tool policy and presets in the binary (step 3 of presets).

## Needs you

- **The Anthropic key is refused.** The key in founder-game-todo's
  `.env.local` gets "invalid x-api-key". It may have been rotated or
  revoked. Real calls use DeepSeek until it's replaced.
- **Four file-watch tests skip on this machine.** This account has used
  up its inotify instances, about 219 against a limit of 128. To run them,
  close some editors, or run
  `sudo sysctl fs.inotify.max_user_instances=512`, then
  `cargo test -p karaka-fs-local --test watch -- --nocapture` and
  `cargo test -p karaka-skill-filesystem --test filesystem the_real_watches -- --nocapture`.
  The skill provider's real-watch case has never run here; its scripted
  watch cases do. For the same reason `karaka chat` shows no skill
  catalog here (tried live: the model answered "none"): its watches
  won't start, so the catalog is incomplete and left out, as dsh leaves
  it. `karaka ask` doesn't watch, and a live `ask` loaded the skill.
  (Skills, step C)
- **Directory listing order.** Karaka sorts names by their bytes, so
  capitals come first. dsh sorts by the machine's locale. Keep bytes, or
  switch to a case-insensitive sort that comes close to dsh. Skill
  discovery follows the same order, where dsh sorts a root's entries with
  `localeCompare`.
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
- **The skill catalog scans the whole log each step.** It looks for the
  newest catalog still on the surface over a copy of every event, as dsh
  does. A long session would want the latest catalog kept as the log
  grows. (Skills, step C)
- **Piped answers aren't echoed.** With `printf '2\n' | karaka ask …`,
  the model's reply starts on the question's `> ` prompt line, because
  nothing ends that line. In a terminal, the Enter key does. The terminal
  answerer should end the line itself when stdin isn't a terminal. (The
  ask_user_question tool)

## Waiting on missing infrastructure

### The kernel

- **No teardown.** Plugins never unload, and a failed start doesn't roll
  back the plugins that already started. Because of this:
  - the tool registry can't remove a tool, scoped ones included, so
    `todo_write` and `working_directory` stay registered for the
    runtime's life; a restriction can be lifted;
  - `WorkingDirectory::stop()` and `UserQuestions::close()` have to be
    called by hand;
  - session-postgres will need a drain on dispose;
  - a broken preset's plugins keep what they registered before failing:
    under its scope, where no chain reaches it, and anything registered
    globally, which stays live. dsh disposes the preset's scope.
    (Presets, step 2)
- **Hooks.** Some exist as small registry ports: request errors, inbox
  signals, the pre-step hook, prompt assembly listeners, the
  user-questions and approval answerer chains, and the tool registry's
  `tools/pre-execute` gate. Still missing: `tools/execute`,
  `tools/post-execute`, `agent/turn-stopping`. The pre-step decision has
  no `startsRequestSeries`, which waits on compaction. (Kernel gaps;
  Skills, step C)
- **Tool guards.** dsh's registry runs monotonic guards (`ToolGuard`)
  after `tools/pre-execute`, which can only deny, globally or per scope.
  They aren't ported, and nor are `presentAs` or `tools/result`.
  (Approvals, step B; Presets, step 1)
- **Per-agent composition.** Every registry files registrations under a
  preset's or a session's scope, and an agent sees them through its
  chain. Presets exist (`karaka-agent-presets`), each agent's chain
  names its session's preset, and the binary reads presets from
  `KARAKA_PRESETS` with `persona`, `tool-policy` and `skill-filesystem`
  rows. What is left:
  - Play This Life's other per-preset plugins, as `PresetPlugin`s, and
    the binary's file accepting them: only those three rows exist;
  - Play This Life's persona `guides` field: dsh's persona has none, so
    it is Play This Life's own wrapper, not ported;
  - main-agent's approval gate, to be scoped to main-agent's preset;
  - `composeFrom`: a child agent joining its parent's preset. It waits on
    child agents.

  `RequestErrorHooks` and `InboxHooks` are still global only. (Kernel
  gaps; Skills, steps A, B and C; Presets, steps 1, 2 and 3)
- **The user's default preset.** dsh's registry config has a
  `selectedDefault`, the user's choice of default, edited through
  Settings, which new sessions use over the deployment's `default`. It
  waits on a settings service. (Presets, step 2)
- **Picking a preset when creating a session.** dsh's session controller
  resolves a requested preset and writes it into the new session's header.
  Karaka's loop always writes the default; another preset means writing
  the header first, or selecting while the session is blank. This comes
  with the session controller. (Presets, step 2)
- **No `agent-preset/selected` notice.** dsh emits it when a session logs
  a selection. Karaka has no event bus; a client can watch the
  `agentPreset` projection instead. (Presets, step 2)
- **A logger.** dsh warns on a torn log tail and a failed background
  batch. Karaka writes nothing, or prints to stderr. The skill registry
  keeps its warnings (ignored duplicates, skipped providers, panicking
  change listeners, and what a provider reports through its control's
  `warn`, such as skipped skill files and failed watches) for
  `SkillRegistry::warnings()`, until a logger takes them. dsh also warns
  `agent preset <id>: <reason>` when a preset breaks; Karaka keeps the
  reason in the roster only. (Durable sessions, parts A and B; Skills,
  steps A and B; Presets, step 2)

### The agent loop

- **Parallel tool calls and exclusive barriers.** Calls run one after
  another. (Agent loop, step one)
- **`concludesTurn`**, which lets a tool end the turn. (Agent loop, step
  one)
- **Tool calls have no cancel signal.** A cancelled call's future is
  dropped. Questions and approvals both take a `karaka-fs` `AbortSignal`,
  but a tool has none to pass on. So `ask_user_question`'s request carries
  no signal, and an answerer sees a cancel only as its own future being
  dropped. An approval asked from a tool call is the same: it can't log
  `cancelled`, since the dropped future logs no `approval/decided`. The
  gate has no signal checks either, so dsh's "cancellation overtakes the
  approval" result can't arise; a dropped call simply never runs. The
  `skill` tool's lookups, and the pre-step's catalog and invocation
  lookups, get no signal either: the loop drops the pre-step chain on a
  cancel. (The ask_user_question tool; User approval; Approvals, step B;
  Skills, step C)
- **Tool results carry text only.** dsh's also carry images, a
  structured error `{ name, code, reason }` (a gate denial's `info`
  included), presentation data and extra context for the next step. Play This Life shows its upgrade prompt on
  the `pro_required` code. (Audit: what Play This Life reads)
- **Developer messages for tool additions and removals.** Only the
  request header records a change of tools. (Agent loop, step one)
- **Inbox:** the `inserted` signal and the inbox's `replace` and `remove`
  aren't ported. (Agent inbox)
- **Child agents.** There are none, so `DELEGATED_CALLER` in user
  questions isn't checked, and no `approval/policy` with `source:
  "delegation"` is seeded into a child. (User questions; User approval)
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
  stream for now. The preset registry's `list` and `select` remotes are
  Rust methods (`roster`, `select`), with dsh's error codes and details.
  This comes with the webserver. (User questions; Presets, step 2)
- **Late replies from the terminal.** The binary mounts the legacy
  `ask_user_question`, as dsh's bundles do, so its questions block and
  never go pending. With the timed tool, a pending question would need a
  way to answer it later from `chat` (`UserQuestions::answer`), which
  isn't built. (The ask_user_question tool)
- **No presentation layer.** `presentCall` and other card data for UIs
  aren't ported, `skill`'s `Load skill <name>` among them. (The
  todo_write tool; Skills, step C)

### Platform

- **Windows.** `karaka-fs-local` and the JSONL session backend build on
  Unix only. (Filesystem; Durable sessions, part A)

## Waiting on a plugin not yet ported

- **The tool policy's `ptc` mode and application tools.** Karaka's
  `tool-policy` presents tools natively only; its `ptc` mode, and taking
  the application's MCP tools out of the allow and deny lists
  (`karakaApplicationTools`, `registerPolicy`), wait on the MCP client.
  Until then a policy naming a tool no plugin registered breaks its
  preset. (Presets, step 3)
- **Sandbox policy.** File-write sandboxing, and the policy line in the
  runtime context. (Filesystem; Working directory)
- **The filesystem observation policy:** the `fs/write-intent`,
  `fs/edit-intent` and `fs/observed` events. Until then nothing calls
  the skill provider's `observe_host_mutation`, which dsh calls on
  `fs/observed` after a model's write or edit. (Filesystem; Skills, step B)
- **Compaction.** It needs the context-overflow error, which exists now.
  (Agent loop, step one)
- **System-prompt sections** from other plugins: plan mode, per-tool
  guidance, MCP servers and the rest. (System prompt)

## Still to port, toward Play This Life

From Play This Life's plugin list:

- **Models and storage:** `agent-default-model` (needs dsh's settings
  service), `session-query-sqlite`, `storage`, `credentials-local`.
- **The rest:** `mcp-client`, `session-reference`, `webserver`,
  `agent-tool-presentation`, `deepseek-llm-api-extensions`.

Karaka's own: `token-meter`, `compaction-basic`,
`compaction-tool-result-pruner`, `session-checkpoint-policy`,
`timeout-policy`.

## Decided, not to do

- **Preset revisions.** dsh's presets are rows a running process can add,
  replace and remove, each change a new revision that live agents keep
  until they let go. Karaka's presets are compiled in and set up once, so
  there is one revision each, for the runtime's life: no generations,
  retiring, leases (`acquireScope`) or rebinding of a live tree, and no
  `readDocument`, `compositionInventory`, `inspectCompositions` or
  `serviceFor`, which read a Cordis tree. (Presets, step 2)
- **No migration of older session formats.** Karaka reads and writes v4
  only, and refuses anything older. (Durable sessions, part C)
- **Cases Rust rules out**, such as non-integer versions and Promise
  views. They are listed in each section and need no work.
