# Port notes

Things to remember while moving Karaka and Play This Life off dsh. Each entry
says where it came from and whether it has been checked here.

## Service tokens between the app and Karaka

*Source: a review in another session, 2026-11-17. Not re-checked here.*

### What exists today

`founder-game-todo/server/agents/access/service-auth.ts` defines the tokens the
app (on Vercel) and the Karaka runtime (on Lightsail) use to prove who is
calling. Each request carries a short-lived ES256 token. It is the only place
the `karaka-service+jwt` token type is written; there is no Rust copy anywhere.

Three Play This Life plugins use it, not two:

| Caller | Signs | Verifies |
| --- | --- | --- |
| App: `karakaServiceToken()` | app → `service-api` | — |
| `delegated-access` | karaka → `tools`, `approvals`, `admission` (`tools` and `approvals` can carry a user's delegation) | the app's `service-api` token |
| `assessment-events` | — | the app's `service-api` token |
| `usage-outbox` (in `application-run-policy`) | karaka → `usage` | — |
| App routes: tool-access, admission, approvals, usage | — | Karaka's tokens |

The plugins reach it with a relative import,
`../../../server/agents/access/service-auth.ts`, added in `23c99b875`
(2026-09-30, "scope Karaka access with signed service and user grants").

### The contract both sides must agree on exactly

- **Issuer:** `play-this-life:app` or `play-this-life:karaka`.
- **Audience:** `play-this-life:karaka:service-api`, or
  `play-this-life:app:<purpose>` for Karaka's tokens.
- **Header:** `typ` and `kid`.
- **Lifetime:** 60 seconds, and every token has a `jti`.
- **Delegation:** only the `tools` and `approvals` purposes may carry a user's
  delegation.
- **Size:** tokens over 12,000 characters are rejected.

If any of these drift, auth fails closed at best. At worst a token is accepted
for the wrong purpose.

### Why it was built this way, and what's wrong with it

Karaka's stock `@karaka-ai/server-auth` only compares a fixed secret per
application. That can't express per-purpose scoping, a 60-second lifetime, or
a user's delegation inside a `tools` or `approvals` call. So the plugins
borrowed the app's code.

Keeping one implementation was right; it lives on the wrong side. The platform
depends on the application:

- The plugins import `jose` without declaring it. It resolves only because the
  app's `node_modules` sits higher up the tree.
- The plugins only run inside this repo, so they can't be reused, published or
  moved.

### Better design

The platform defines the token; the application only uses it, the way an app
uses Supabase or Stripe without them importing its code.

1. Add short-lived, purpose-scoped ES256 tokens with an optional delegation to
   Karaka's server auth, next to the fixed secrets.
2. Expose sign and verify in Karaka's SDK. `service-auth.ts` shrinks to
   choosing keys and purposes.
3. Plugins become thin: auth is configuration, and nothing imports from the
   app's `server/` folder.

A cheaper halfway step is moving the file into its own package that declares
`jose`. That makes the dependency honest but still points the wrong way.

### What this means for the Rust port

- Token auth belongs in the runtime itself, not in an application plugin.
  Purpose scoping, short lifetimes and delegation are needed by any app that
  delegates tool access, not just Play This Life.
- Write the token down as a spec first, then implement it on a standard JWT
  library.
- Agreement between the TypeScript and Rust sides only holds if tests enforce
  it. Use shared test tokens from the spec, and run both directions: TypeScript
  signs and Rust verifies, and the reverse. Include rejection cases for:
  - wrong audience or purpose
  - delegation on a purpose that can't carry one
  - expired token
  - `kid` mismatch
  - token over 12,000 characters
- This is the one place where two copies diverging is a security bug, not just
  a functional one.

### Not done

The review proposed changing `@karaka-ai/server-auth`, the SDK and
`delegated-access`. Under founder-game-todo's `AGENTS.md`, each of those needs
explicit approval first. Nothing has been changed.

## Audit against dsh, 2026-11-21

Three read-only audits compared the kernel, the binary, the model seam,
both providers and the session log with dsh and with what Play This Life
actually runs. The overall verdict was that nothing is wildly off, but
several gaps must close before Play This Life can run on the port.

### The reference was the wrong dsh version

The local `aug-finalizations/deepseek-harness` checkout is dsh
**0.1.2-alpha.2** (commit `0a53fb55be`, 2026-08-30), and its session
format is version 0. Play This Life runs the copy bundled in
`founder-game-todo/karaka/node_modules/@karaka-ai/agent/node_modules/@deepseek-ai/`,
which is dsh-session **0.2.1-rc.2**, with session format **v4**.
`session-postgres` stores v4 events verbatim as JSON. From now on, check
the bundled v4 code, or a fetched newer dsh, before porting anything that
touches stored data.

What changes in v4:

- **Surface events:** there are five (system, developer, user,
  assistant, tool result), not three.
- **System prompt:** it is logged as surface node 0.
- **Tool results:** they have role `tool`, with `toolCallId`, `isError`
  and `content` at the top level.
- **Assistant messages:** failed attempts are `assistant/attempt` events,
  and `assistant/message` embeds the stream.

### What Play This Life reads that Karaka doesn't keep yet

- **Original seqs.** `agentUsage` is unique on `(playerKey, chatId,
  eventSequence)`, and follow rejects gaps in seq. Migration needs a
  passthrough event that keeps every original seq, so other event types
  can't be dropped.
- **User message `source`** (`kind`, `rpcId`). The chat client shows
  only `kind: user`, prompt idempotency uses `rpcId`, and the tool guard
  allows write tools only when `batchId == rpcId`.
- **Turn, step and `tool/call` events.** The tool guard, run limits
  (`maxSteps`, the time budget), approvals, questions, usage records, and
  the client's `assistant:${turn}:${step}` bubble ids all depend on them.
- **Tool result `error { name, code, reason }`.** The client shows its
  upgrade prompt on `pro_required`.

### Model seam gaps

- **No thinking or effort settings on the Anthropic request.** Play
  This Life's `pi-ai` sends `thinking: { type: adaptive, display:
  summarized, block_binding: { prefix_mismatch_behavior: drop_block } }`
  and the configured effort (`medium`).
- **No prompt-cache markers.** `pi-ai` marks the system prompt, the last
  tool and the last user block `ephemeral`.
- **Empty blocks are not filtered** on the Anthropic request. Unsigned
  reasoning should become text, not be dropped.
- **Refusals and content filters count as a normal stop.** dsh treats
  `refusal`, `sensitive` and `content_filter` as errors, and turns a stop
  with no content into `EMPTY_RESPONSE`, which it retries.
- **No error kind for context-window overflow.** Compaction will need it.

### Kernel gaps

- **Hooks.** Every Play This Life plugin wraps an ordered hook with
  `ctx.on(event, (payload, next) => …)`: `tools/execute`,
  `approval/request`, `agent/pre-step`, and others. They can be built as
  typed hook-registry ports beside the kernel.
- **Per-agent composition.** The three agent presets each get their own
  tools, prompt and skills. main-agent's approval gate must fire only for
  main-agent.
- **Cleanup.** There is no `stop()`, and a failed start doesn't roll back
  the plugins that already started. session-postgres drains its buffer on
  dispose.
- **A model registry.** Play This Life loads two providers at once, but
  `dyn Model` takes only one.

### Fixed since the audit

- Fixed in `f4afc9e`:
  - **Effort, thinking and prompt caching.** Effort is a neutral
    `Request.effort`. Anthropic sends adaptive thinking with `display` and
    `block_binding` (behind the `thinking-binding-controls-2026-08-01`
    beta header) and cache markers where pi-ai puts them.
  - **Empty blocks.** They are filtered on the Anthropic request.
- **Several providers at once.** Fixed in `71f4977`: the `ModelRegistry`
  port holds one model per route, so two providers, or two
  OpenAI-compatible servers, can run side by side. Two plugins with the
  same route are caught by the kernel's duplicate-name check before
  either starts.
- **Real calls work.** Made 2026-11-21: Anthropic `claude-opus-5-5` with
  every new setting on; DeepSeek `deepseek-chat` and `deepseek-reasoner`
  through the OpenAI-compatible provider.

Still open from the audit:

- **Errors that look like success.** Refusals, content filters and
  empty responses still count as a normal stop.
- **Context-window overflow.** It has no error kind of its own.
- **Kernel gaps.** Hooks, per-agent composition and cleanup.
- **The session v4 work.** Everything listed above under session
  format v4.

## Session events at v4, 2026-11-21

`karaka-session` now follows dsh tag `dsh-v0.2.1-rc.2`, the release Play
This Life runs, with session format v4:

- **Events and their JSON.** The five surface events, turn and step
  boundaries, `tool/call`, and a passthrough for every other event type.
  The JSON is v4's envelope, and each original seq is kept.
- **Derivation.** It returns the system prompt beside the messages, and
  never makes up a tool result.
- **Repair.** It follows v4: on load, it computes closer events to append,
  scoped to the open last turn.

What doesn't round-trip exactly (value-equal, not byte-identical):

- **Key order.** Known fields come out in struct order.
- **Number text.** `1e21` is written `1e+21`.
- **`null`.** It is dropped from typed optional fields.
- **Lone UTF-16 surrogates** (`"\ud800"`). serde_json refuses them, so a
  stored log containing one would fail to load. This is a real risk:
  Play This Life's `json` column keeps them. It needs a fix before
  migrating stored chats.

Also still open:

- **Orphaned calls in closed steps.** Nothing patches them at request
  time, as pi-ai's "No result provided" does. That belongs in the loop
  or the providers.
- **Unknown event types.** They pass through even when not marked
  `ignorable`, where dsh refuses them.

## Second audit against dsh v4, 2026-11-21

Two fresh read-only audits compared the code with dsh tag
`dsh-v0.2.1-rc.2` and with pi-ai 1.1.0, the version Play This Life has
installed.

### Sessions: they follow v4 on real data

All 485 events in Play This Life's captured logs load and write back
value-equal. They also derive the same history v4 does. Repair matches
v4 on every prefix of every log. The remaining gaps, most important
first:

1. **System messages later in the history.** v4 folds only the
   *leading* system messages into the system prompt (`llm-pi-ai`
   `context.ts`, `splitSystemPrompt`, checked). A later system message
   stays where it is. Play This Life's route is `in-history`, so a
   changed prompt is appended as a new system node. Karaka joins every
   system node into one prompt, so the stale and new instructions both
   reach the model.
2. **Reasoning signatures don't record their model.** v4 and pi-ai
   replay a signature only to the provider, API and model that made it,
   and turn it into text otherwise. Karaka's `Block::Reasoning.replay`
   has no record of where it came from. It also writes `replayState`
   with no `response` half, which v4 would treat as foreign.
3. **Karaka accepts events v4 refuses.** One case: a `tool/result`
   whose `toolCallId` differs from `source.callId`. Repair and
   derivation then disagree about that call.
4. **Restore writes no `session/end-seed`**, so seqs after a resume end
   up one lower than v4's.
5. **Developer messages.** Their tool additions and removals are
   dropped, where v4 sends them.

### Providers: Anthropic close, DeepSeek wrong protocol

- **DeepSeek uses the wrong API format.** dsh v4's `llm-deepseek` speaks
  Anthropic's Messages format at `https://api.deepseek.com/anthropic`,
  checked in `config.ts:115` and `adapter.ts:144`. It sends signed
  thinking blocks, `output_config.effort` (low, high or max), `tool_result`
  blocks, and always a `max_tokens` (Play This Life: 393,216). Karaka's
  DeepSeek route uses Chat Completions. The real calls worked, but they
  don't match what dsh sends. The Messages-format crate could serve
  DeepSeek with a DeepSeek profile: `thinking: enabled|disabled`, no
  cache markers, unsigned thinking kept as thinking, and a `max_tokens`
  default.
- **Failures that look like success**, on both providers:
  - refusal, `sensitive`, `content_filter` and unknown stop reasons
    finish as a normal stop;
  - a reply with no content isn't `EMPTY_RESPONSE`;
  - there's no error kind for context-window overflow, which compaction
    needs before it can recover a full context;
  - tool calls cut off by `max_tokens` are kept.
- **Unsafe default.** Thinking display and `block_binding` are off
  unless configured. pi-ai always sends them for Haiku 5.5.
- **Effort wire.** pi-ai also sends per-message effort markers behind a
  beta header. The effective level is the same, but changing effort
  mid-session breaks Karaka's cache.
- **Tools.** pi-ai sends a trimmed tool schema and
  `eager_input_streaming`. It also adds synthetic results for orphaned
  tool calls.

### Fixed after the second audit

- Fixed in `4b619b2`:
  - **Failures that looked like success.** Refusals, content filters,
    empty replies, context overflow and unknown stop reasons are now
    errors.
  - **Cut-off tool calls.** Tool calls from a `max_tokens` finish never
    reach a consumer.
  - **New error kinds** follow dsh's codes and its retry policy.
- **DeepSeek's API format.** `karaka-anthropic` has a DeepSeek profile
  that reproduces dsh v4's `llm-deepseek`:
  - the `https://api.deepseek.com/anthropic` endpoint, with
    `anthropic-version` 2023-06-01;
  - `thinking: enabled|disabled`, and effort limited to low, high or max;
  - no cache markers, and `max_tokens` always sent;
  - tool history checked before sending;
  - strict decoding.

  A real call to `deepseek-flash` worked on 2026-11-21 without dsh's
  `x-deepseek-harness-*` headers.

  Still unverified: whether DeepSeek signs thinking blocks, and whether
  it accepts unsigned thinking back. That needs a multi-turn call, so
  it waits for the loop.

  DeepSeek signatures are stored as a small envelope that names the
  model, so they go back only to the model that made them. That isn't
  v4's on-disk `replayState` shape yet, and the gap stays open with the
  signature-origin work.

### Fixed: in-history system messages and signature origin

- **System messages later in the history.** They now stay in place, as
  in dsh v4's `splitSystemPrompt`. Only the leading run becomes the
  request's system prompt. `Role` gains `System` and `Developer`.
  - Anthropic sends them in place on models that accept mid-conversation
    system messages, and folds them into `system` on the others.
  - The DeepSeek profile follows `llm-deepseek`.
  - OpenAI-compatible servers send them in place only with
    `system_messages_in_history`.
  - Developer tool additions and removals are still left out.
    `Request.tools` already carries the current tool set.
- **Reasoning signatures.** They carry a typed `Origin` (api, provider,
  model). A signature goes back only to the origin that made it, and is
  otherwise degraded as pi-ai and `llm-deepseek` do. Sessions write a
  complete v4 `replayState`, including the `response` half, and read it
  back with v4's own checks. The DeepSeek envelope string is gone.
- **Still open:**
  - ~~The loop must pass the real provider name (`anthropic`, `deepseek`)
    as `source.provider`.~~ Done in the agent loop, step one, through
    `Model::provider`.
  - pi-ai's per-message effort markers aren't ported.
  - Assistant messages that stopped with an error aren't dropped from the
    history.

## Agent loop, step one

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-21.*

Four pieces, one crate each where they are seams:

- **`Model::provider`** in `karaka-model`. Each answer's `source.provider`
  is the provider's own name (`anthropic`, `deepseek`), the one its
  signatures' `Origin` names, not the route. `karaka-openai` writes no
  `Origin` and answers `openai`.
- **`karaka-tools`** (dsh `packages/core/tools`): the `Tool` trait, the
  `ToolRegistry` port and `ToolsPlugin`.
- **`karaka-agent`** (dsh `packages/core/agent`): the `Agent` trait and the
  `Agents` port, which opens one agent per session.
- **`karaka-agent-loop`** (dsh `packages/core/agent-loop`): a port of
  `ReactLoopAgent`, and `AgentLoopPlugin`, which provides `Agents`.

`karaka chat` drives it from stdin, with one tool, `current_time`.

### Left out of step one

Each lives in dsh at the path given, under `packages/`.

- **Retry**, through the `agent/request-error` hook
  (`core/agent-loop/src/agent.ts:511-526`) and `llm/llm-retry`. A failed
  model call ends the turn.
- **Parallel tool calls and exclusive barriers.** `runGroup`,
  `core/agent-loop/src/tool-calls.ts`. Calls run one after another.
- **Steering, `inject` and the next-step inbox.** `send`, `steer` and
  `inject` in `core/agent-loop/src/agent.ts:154-173`, and
  `core/agent-loop/src/inbox.ts`. Also the `agent/turn-stopping` and
  `agent/pre-step` hooks that feed it (`core/agent/src/runtime-types.ts`).
- **Tool policies and approvals.** `tools/pre-execute` and
  `tools/post-execute` in `core/tools/src/index.ts`, and
  `interaction/user-approval`.
- **Runtime context.** `RuntimeContextProjection`,
  `core/agent-loop/src/runtime-context.ts:113-164`.
- **The system-prompt plugin.** `core/system-prompt`. The prompt is plain
  loop config, `LoopConfig::system`.
- **`concludesTurn`.** `ToolRunContext.concludeTurn` in
  `core/tools/src/index.ts`, read in `core/agent-loop/src/tool-calls.ts:158`.
- **Developer messages with `tool-addition` and `tool-removal` blocks.**
  `buildRequest`, `core/agent-loop/src/agent.ts:652-666`. dsh writes one
  after a `request/header` whose tools differ from the one before, such as
  after a restart with other tools, or a tool registered between turns.
  Karaka's model seam has no block for them, so only the header records the
  change (see step two).
- **Assistant stream frames for a live UI.** `AssistantStreamAttempt`,
  `core/agent-loop/src/assistant-stream.ts`, sent as `agent/assistant-stream`.
- **Compaction.** `compaction/compaction` and its siblings.

### A failed turn stops the driver

As in dsh, a turn that ends with an error stops the driver and leaves any
queued messages where they are (`kick`, `core/agent-loop/src/agent.ts:252-265`).
The next followup starts a driver that runs them first, oldest first. So
`when_idle` means no driver is running, as dsh's `whenIdle` does, not that
the queue is empty.

### Where step one differs from dsh

- **The inbox is in memory.** dsh logs every queue change as an
  `agent/inbox/spliced` event (`core/agent-loop/src/inbox.ts`), so queued
  messages survive a restart. Karaka's queue is lost with the process.
- **Cancelling drops the running work.** dsh aborts an `AbortSignal` and
  waits for the tool to settle, then logs `Error: tool call aborted`
  (`ABORTED`), and for calls it never started, a `tool/call` plus `Error:
  tool call aborted before dispatch` (`ABORTED_BEFORE_DISPATCH`)
  (`core/agent-loop/src/tool-calls.ts:238-260`). Karaka drops the tool's
  future, and `ToolCallRecovery` answers every call left without a result:
  "outcome unknown" (`TOOL_OUTCOME_UNKNOWN`) for the one that started,
  "not started" (`TOOL_NOT_STARTED`) for the rest, with no `tool/call` for
  those. The wording is v4's for an interrupted turn.
- **Message ids name their seq**, such as `assistant-7`. dsh uses random
  ids. Karaka has no id generator yet, and only the agent writes to its
  session, so the next seq is known.
- **The system prompt.** dsh appends a changed prompt in place on routes
  that declare `in-history`, and replaces node 0 on the rest
  (`SystemPromptProjection.project`,
  `core/agent-loop/src/runtime-context.ts:88-103`). Karaka's models
  declare nothing, so it always takes the `in-history` path, which dsh's
  DeepSeek and pi-ai adapters both declare, and lets each provider decide
  how to send it. Clearing the prompt uses dsh's replacement path, as dsh
  does.
- **Interrupted reasoning has no signature.** dsh's replay state comes
  only with the stream's last chunk, so an interrupted message never has
  one. Karaka gets each signature with its block, and drops it the same.
- **`Paused` ends the turn as `max-tokens`.** pi-ai calls a paused turn a
  stop. `karaka-model` asks callers to treat it as unfinished until
  server-side tools can resume.
- **Error codes.** A `ModelError`'s kind becomes dsh's code (`AUTH`,
  `RATE_LIMIT`, `SERVER`, `TRANSPORT`, ...). A refusal becomes
  `REFUSED`; dsh would file it as `PI_AI_ERROR`. A route with no model is
  `NO_ADAPTER`, in Karaka's words. No status or request id is kept:
  `ModelError` doesn't carry them.
- **Tool results carry no `error` code.** dsh writes `{ name, code }` for an
  unknown tool (`ToolNotFoundError`, `UNKNOWN_TOOL`) and for bad arguments
  (`ToolArgsError`, `INVALID_ARGS`). Karaka writes the same text for the
  model, but `ToolOutput` holds only text and the error flag.
- **Arguments are checked only for being a JSON object.** dsh's typed tools
  check them against their schema. Karaka's registry rejects anything that
  isn't an object, with the text dsh's schema check gives for that case.
- **One loop config for every session.** dsh gives each agent its own
  options and lets the `agent/request` hook change the request per step.

## Agent loop, step two: what the loop logs around each model call

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-21.*

`karaka-session` now models `request/header`, `request/context` and
`assistant/attempt` as typed events, where they were passthrough (`request.rs`):

- Their JSON is v4's both ways, and unknown fields stay in `extra`.
- A header's tools are kept as raw JSON, so each keeps its key order: dsh
  compares tools by their JSON text.
- `canonical_header`, `header_equals` and `call_config_equals` follow
  `request-header.ts` and `call-config.ts`.
- `Session::request_header` and `Session::request_context` give the latest
  of each, as v4's session folds them.
- A header is checked as v4 checks it on append and on load: no `system`, no
  empty `tools` or `adapterDefaults`, adapter-default markers only for fields
  the config has, and only v4's four reasons.
- `Session::surface_generation` counts replacements, as v4's surface counts
  `contentGeneration`.
- The fixture's header and attempt write back byte for byte. The rest of the
  fixture still round-trips as equal values, not bytes: the other typed
  events write their keys in struct order, as before.

The loop logs them where dsh's `buildRequest` and `step` do: after the
system and user messages, before the stream.

- **`request/header`.** The loop's first request logs `initial`, or `resume`
  over a log that already has a header. Later requests log `change` when the
  header differs from the one in force. When nothing changed but part of the
  history was replaced since the last request, they log `series`. A `resume`
  or `change` that starts a new series carries `startsSeries`. In Karaka
  today only clearing the system prompt replaces history, and only a tool
  registered between turns can cause a `change`, since the config is fixed.
- **`request/context`.** Logged when the provider, model, context window or
  system prompt update mode differs from the last one.
- **`assistant/attempt`.** `{ turn, step, stream: [] }` for a model call that
  failed, and for one cancelled before anything worth keeping arrived. A
  cancel after content still logs the interrupted message instead.
- **Effort on resume.** With no effort configured, the loop takes the header
  in force's `reasoningEffort`, if that header is for the same provider and
  model and doesn't mark effort as a provider default (`prepareRequest`,
  `agent.ts:569-582`).

### Where step two differs from dsh

- **The provider is the provider's name, not the route**, in the header's
  config and in the context, as in `source.provider`. dsh writes the route
  in all three. So a provider routed under a new name over an old log
  resumes with the same header.
- **No `adapterDefaults`.** Karaka's providers fill in their defaults, such
  as DeepSeek's `max_tokens` or Anthropic's effort, inside the request, and
  the loop never learns the values. A header names only what the loop
  asked for. So an effort the provider chose is never restored, and nothing
  marks a field as a provider default.
- **No context window.** Karaka's models advertise none.
- **`systemPromptUpdate` is always `in-history`**, matching how the loop
  updates a changed prompt (step one's system prompt deviation): it appends
  a changed prompt where it changed.
- **A route with no model logs no header or context.** dsh still logs both,
  naming the route, and then fails with `NO_ADAPTER`. Karaka can't name the
  provider without the model. Both log no attempt: the call never started.
- **An effort Karaka has no level for doesn't carry on.** dsh's effort is
  the adapter's own string, such as DeepSeek's `off`. Karaka maps only
  `low`, `medium`, `high`, `xhigh` and `max`, so another string is dropped
  on resume, and the resumed header leaves it out.
- **No `developer/message` for tool changes**, as above: only the `change`
  header records them.
- **Attempt streams are empty.** dsh embeds the exact stream it received.
  Karaka keeps no streams, and v4 accepts an empty one.
- **The `agent/request` hook and `startsRequestSeries` aren't ported**, so a
  plugin can't change the config per step or start a series itself.
