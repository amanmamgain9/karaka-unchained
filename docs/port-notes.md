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
- **Attempt streams were empty** until step five, part B, which embeds the
  exact stream, as dsh does.
- **The `agent/request` hook and `startsRequestSeries` aren't ported**, so a
  plugin can't change the config per step or start a series itself.

## Agent loop, step three: retries

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-21.*

### What is done

- **The policy belongs to the provider** (`karaka-model`, `retry.rs`; dsh
  `llm/src/retry-policy.ts`).
  - `RetryPolicy` is either normal (a retry limit, retryable codes and a
    backoff) or always (a backoff only). It has dsh's defaults: 5 retries,
    500 ms initial delay, 10 s longest wait, 0.1 jitter, and
    `DEFAULT_RETRYABLE_CODES`.
  - `RetryPolicyConfig::resolve` checks a config as `resolveRetryPolicy`
    does.
  - `Model::retry_policy` returns the policy, with dsh's default unless a
    provider overrides it. `AnthropicConfig` and `OpenAiConfig` take an
    optional `retry_policy`, and their models return it.
  - `ErrorKind::code` moved from the loop into `karaka-model`, so the
    policy's codes and the codes the loop writes come from one table. A test
    checks that the default policy retries exactly the kinds that
    `is_retryable` says.
- **The request-error hook** (`karaka-agent`, `request_error.rs`; dsh's
  `agent/request-error` waterfall).
  - It is the first hook, and is built as a registry port, not a kernel
    change. Plugins register a `RequestErrorHandler` in
    `RequestErrorHooks`. Each handler gets the failed request and a `Next`
    for the rest of the chain, and returns `Retry` or nothing. Handlers run
    in the order they were registered.
  - The payload holds the session, turn, step, provider, the `Failure` (with
    `providerRetryAfterMs`), the policy and the turn's `Cancellation`.
  - The loop's plugin provides the port, so a plugin that registers into it
    starts after the loop.
  - It lives in `karaka-agent` because it is the agent's extension point,
    as dsh declares it beside `Agent`. The turn's cancel switch moved there
    too, as `CancelSwitch` and `Cancellation`, so a handler can wait on it.
  - A general hook mechanism in the kernel could replace this port later.
- **The loop runs it.** It runs after a failed attempt's `assistant/attempt`.
  On retry it goes round again, as dsh's `continue` does: the system prompt,
  model lookup, header and context run again, and the queued message is
  logged only once. A cancel during the chain, or after it, wins and the
  turn ends aborted. A handler that fails, or panics, means no retry: the
  turn ends with that error, as when a dsh listener rejects.
- **`karaka-retry`** (dsh `llm/llm-retry`). `RetryPlugin` registers
  `RetryHandler`:
  - Normal mode retries the listed codes, up to the limit, then calls
    `next`. Always mode calls `next` first, then retries anyway.
  - A provider's `Retry-After` is used when it is positive and no more than
    the longest wait. Past it, normal mode calls `next` and always mode backs
    off. Otherwise the wait is dsh's jittered exponential backoff
    (`localDelay`).
  - `llm/retry` is logged before the wait and `llm/retry-started` after it,
    with dsh's data and key order, `policyKey` included, as passthrough
    events. A cancel during the wait means no `llm/retry-started`.
  - The count is read from the log, as dsh's `llmRetry` projection reads
    it: the latest `llm/retry` for the provider and policy key since the
    last `step/start` or `turn/end`.
  - The jitter's random source can be injected (`RetryPlugin::with_random`,
    dsh's `RetryInternals.random`). Waits use Tokio's timers, so the tests
    run on paused time.
- **The binary.** `karaka chat` runs `RetryPlugin`, and `KARAKA_MAX_RETRIES`
  sets the provider's retry limit, with 0 turning retries off.

### Where step three differs from dsh

- **The provider in the payload and in `llm/retry` is the provider's name**,
  as in the header (step two), where dsh gives the route. The count is
  keyed by it.
- **Retry ids name a seq**, `retry-{seq}` after the chain's first
  `llm/retry`, in the style of the loop's message ids. dsh uses a random
  UUID. Both are unique in the session.
- **The default random source** hashes the time with `std`'s randomly
  keyed hasher, so no new crate is needed. Jitter needs no strong
  randomness.
- **A failure further down the chain is dropped silently in always mode.**
  dsh logs a warning first; Karaka has no logger yet.
- **A handler that panics** ends the turn with `UNKNOWN`, "a request-error
  handler panicked". dsh has no panics; a thrown error ends the turn with
  its text under `UNKNOWN`, as here.
- **The policy is always there.** dsh's payload has none when no adapter
  served the request. In Karaka a route with no model never reaches the
  hook (step two).
- **No plugin disposal.** dsh aborts and drains running waits when the
  plugin unloads. Karaka's kernel can't stop plugins yet.
- **The retry config is snake_case** (`max_retries`, `retryable_codes`,
  `backoff.initial_delay_ms`, ...), like the rest of a Karaka provider
  config. dsh's is camelCase. The logged events keep dsh's names.
- **`TIMEOUT` is in the default codes but never written.** Karaka files a
  timeout under `TRANSPORT`, which is in them too.
- **Counting across a restart.** The count is never held in memory, so
  whatever reads the log mid-step carries on counting. But opening a
  session closes a turn that a crash left open, as dsh's resume does, and
  that `turn/end` resets the count, so a restarted agent starts its next
  turn from zero, as dsh's does.
- **`karaka ask` didn't retry** at first, since it ran outside the loop.
  Step four moved it into the loop, so it retries now.
- **The retry tests use the real loop**, so `karaka-retry` has
  `karaka-agent-loop` as a dev-dependency. Its library depends only on
  seams.

## Agent loop, step four: live stream frames

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-22.*

### What is done

- **The frame type** (`karaka-agent`, `stream.rs`; dsh `AssistantStreamFrame`,
  `core/agent/src/runtime-types.ts:128-167`): `Start`, `Chunk` and `End`, each
  with an attempt id `{session}:{n}`, where `n` counts the agent's attempts,
  and a revision that counts up across all of the agent's frames.
  `End.outcome` is `Committed { event_type, seq }` or `Abandoned`.
- **Subscribing** (dsh: the agent-scoped `agent/assistant-stream` emit,
  `runtime-types.ts:355`). `Agent::subscribe(listener)` returns a
  `FrameSubscription` that unsubscribes when dropped. A listener is a plain
  function, called in order on the turn's task, as dsh's emit is synchronous.
  `FrameListeners` keeps the list, so every loop subscribes and catches
  panics the same way. A listener that panics is caught and dropped, and the
  turn and the other listeners carry on.
- **The loop emits them** (`karaka-agent-loop`, `live.rs`; dsh
  `AssistantStreamAttempt`, `core/agent-loop/src/assistant-stream.ts`), in
  dsh's order (`agent.ts:440-560`):
  - `start` only once the stream is open; a call that never starts sends
    nothing;
  - a `chunk` for each event, before the loop acts on it;
  - `end` committed only after the `assistant/message` or
    `assistant/attempt` append has returned its seq;
  - `abandoned` when that append fails, or when an attempt that started is
    dropped without settling, as dsh's `if (!live.ended) live.abandon()` does.
  - A retried call is a new attempt, with a new id.

### Left out

- **The durable compact `stream` records** that dsh's `assistant/message`
  and `assistant/attempt` embed (`AssistantStreamAccumulator`). Done in step
  five.

### Where step four differs from dsh

- **A chunk is a `karaka_model::Event`.** Since step five, part A, that is
  dsh's `StreamChunk` vocabulary, so the frames carry dsh's chunks.
- **The failure chunk carries the model's own error.** dsh's failed stream
  ends with a `finish` chunk of kind `error`, carrying its `LlmFailure`.
  Karaka's chunk is `StreamChunk::Event(Event)` or `StreamChunk::Failed(ModelError)`,
  and a failed attempt's last chunk is the `ModelError` itself, refusal details
  included, which the logged `Failure` can't hold.
- **A stream that ends with neither its last event nor an error** fails as
  `Network`, "the stream ended before the model finished": the error
  Karaka's providers give for a body that breaks off. dsh's adapters call
  that `STREAM_CLOSED`, in their own words ("DeepSeek Messages stream ended
  before message_stop"), and don't retry it; Karaka retries it, as
  `karaka-model` already decided. For `karaka ask`, the only change in
  wording is for a model that breaks the stream contract this way: it used
  to print the bare sentence, and now prints it as any other error, "the
  connection failed: the stream ended before the model finished (trying
  again may help)". A real provider's cut-off stream reads as before.
- **A listener that panics is dropped.** dsh's emit has no such case.
- **A slow listener holds the turn up**, as in dsh. The listener contract
  says to hand slow work on.

### `karaka ask` through the loop

`ask` is now a one-turn chat on a fresh session (`chat::loop_plugins`, the
same plugins as `chat`), so it retries as the provider's policy says. It
prints from the live frames: text on stdout as it is written, reasoning
dimmed on stderr for a terminal, the `stop: …` summary from the `Done`
chunk, and on failure the model's own error line from the last failure
chunk, as before. Its output, messages and exit codes are unchanged; the
CLI test of a 429's error line sets `KARAKA_MAX_RETRIES=0`, since it checks
the line, not the retry.

**A CLI-only behaviour: a retry after text.** dsh's views replace a failed
attempt's text with the retry's, by attempt id. A terminal can't take
printed text back, so `ask` ends the failed attempt's line, and when the
next attempt starts, says `karaka: trying again` on stderr if the failed
one had printed text. A failed attempt that printed nothing retries
silently.

## Agent loop, step five, part A: dsh's chunks in the model seam

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-22.*

### What is done

- **`karaka_model::Event` is dsh's `StreamChunk`** (`llm/src/types.ts:470-482`),
  field for field:
  - `BlockStart { index, block_type }`
  - `TextDelta { index, text }` and `ReasoningDelta { index, text }`
  - `ToolCallDelta { index, id, name, arguments_delta }`
  - `BlockEnd { index, block }`
  - `Usage { usage }`
  - `Finish { reason }`

  Blocks are numbered from 0 in the order they start. `BlockType` is `text`,
  `reasoning` or `tool-call`.
- **`FinishReason`** replaces `StopReason`. It has dsh's `stop`, `tool-calls`
  and `max-tokens`, and Karaka's own `paused`, recorded as an extension;
  dsh's `FinishReasonMap` is merge-extensible. `Response.stop` is now
  `Response.finish`, and `Response.usage` is optional, as dsh's usage is
  until its chunk arrives.
- **Failures stay the stream's `Err(ModelError)`.** dsh's `finish` of kind
  `error` waits for part B, where it is written into the logged stream.
- **The providers stream dsh's chunks.**
  - Anthropic and DeepSeek map `content_block_start`, `_delta` and `_stop`
    one to one onto `block-start`, deltas and `block-end`. `message_stop`
    gives `usage` and then `finish`, as dsh's `llm-deepseek/src/translate.ts`
    does for the same events. DeepSeek also sends what dsh sends with a
    start: a block's opening text as a delta, and a tool call's id and name
    in an empty `tool-call-delta`.
  - The Anthropic profile names the call in every `tool-call-delta`, as
    pi-ai does (`llm-pi-ai/src/stream.ts:189-201`).
  - OpenAI-compatible streams build their blocks as pi-ai's
    `openai-completions` does: one text block, one reasoning block, and one
    block per tool call, each opened by its first piece. Every piece goes
    out as a delta at once. All blocks end when the stream ends, in index
    order.
  - Tool ids are there from the start in all three formats: Anthropic's and
    DeepSeek's `content_block_start` carries the id and name, and an OpenAI
    call's first piece carries its id. A server that sends no id gets
    `call_<index>` at the block's end, as before.
- **Nothing is held back any more.** A tool call cut off by `max_tokens`
  streams and ends like any other, as dsh's adapters send it ("Truncated tool
  JSON is retained in the stream, then pruned by the shared assembler",
  `translate.ts:150`). Assembling drops it. This replaces the providers'
  holding back of tool calls (`4b619b2`), and no consumer that assembles an
  answer ever sees a cut-off call as a finished one.
- **`BlockAssembler`** (`karaka-model`, `assembler.rs`) ports dsh's
  `llm/src/assembler.ts`: `blocks`, `interrupted_blocks`, `usage`, `finish`,
  and `response` once the finish arrives. `complete()` and the loop both use
  it, in place of the loop's own `Answer`.
- **`stream_of(blocks, reason, usage)`** scripts an answer as dsh's mock
  adapter does: each block's start, its content as one delta, its end, then
  `usage` and `finish`. The test fakes use it.
- `karaka ask`'s printer and every test moved to the new chunks. ask's output
  is unchanged; the CLI tests pass as they were.

### Where part A differs from dsh

- **No `replayState` on the finish.** dsh's adapters put reasoning signatures
  in the `finish` chunk's `replayState`. Karaka's blocks carry their own
  signatures, with where they came from (`Block::Reasoning.replay`), so
  `BlockEnd` holds the signature and the finish holds none. The logged
  message's `source.replayState` is still written from the blocks, as
  before. Part B's logged `finish` record can compute the envelope from the
  assembled blocks if dsh's replay tools need it there.
- **An interrupted reasoning block keeps no signature unless the finish had
  arrived.** This matches dsh, whose signatures only come with the finish.
- **The finish is required.** dsh's assembler counts a missing finish as a
  stop. A Karaka stream always ends with a finish or an error, so a stream
  that just ends is a `Network` failure, as before.
- **`tool-call-delta` names.** The Anthropic profile sends the name in every
  delta, as pi-ai does. DeepSeek sends it once, at the start, as dsh's own
  adapter does. OpenAI sends it in every piece once it is known.
- **OpenAI block order** now follows pi-ai: within one chunk, text before
  reasoning before tool calls, and blocks numbered in the order they first
  appear. Before, reasoning always came first.
- **No `totalTokens` or `reasoningTokens`** in `Usage`. dsh's DeepSeek
  adapter computes a total; Karaka's `Usage` has no field for one.

## Agent loop, step five, part B: the compact stream records

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-22.*

### What is done

- **The records** (`karaka-session`, `stream.rs`; dsh's
  `llm/src/assistant-stream.ts`).
  - `StreamAccumulator` packs one call's timed chunks into dsh's records:
    `text-chunks`, `reasoning-chunks` and `tool-call-chunks`, each
    `{ time0, index, dt, texts | args }`, and a raw `chunk` record for
    everything else. The packing rules are dsh's: a delta joins the run before
    it when it is the same kind and block, at a safe gap. A tool call's
    deltas also need the same id and the same name or none. A tool-call
    delta with an empty id or name is kept raw.
  - `expand_stream` gives the exact timed chunks back, checking each record
    as dsh's `validateRecord` does, in its words.
  - Times are epoch milliseconds, as dsh's `Date.now()`; Karaka's `Time` is
    already that. Times, gaps and indices must be JavaScript safe integers.
- **Chunks are kept as dsh's JSON.** A record a dsh adapter wrote, a
  `finish` with `replayState`, one of kind `error`, a block kind Karaka
  doesn't model, or any field it doesn't name, expands and packs again
  unchanged. `chunk_json` writes a Karaka `Event` as dsh's chunk, and
  `chunk_event` reads one back where Karaka has a type for it.
- **The loop feeds the records from the same chunks the frames carry**,
  with the same times (`live.rs`, as dsh's `AssistantStreamAttempt` owns its
  accumulator). Every `assistant/message`, the interrupted one included,
  and every `assistant/attempt` embeds its snapshot.
- **The finish record.**
  - It waits for the settlement, so it can carry the answer's replay state:
    the same envelope the message's `source.replayState` gets, built once
    from the assembled blocks, as a dsh adapter's finish carries it.
  - A failed call's stream ends with a raw `finish` record of kind `error`,
    made from the `ModelError`, with the failure as `turn/end` writes it.
  - A cancelled call has no finish, as in dsh.
- **Checking at the durable boundary**, as dsh does it. dsh's session keeps
  embedded streams opaque: it checks only that `stream` is a list
  (`core/session/src/index.ts:281-293`), which Karaka's type guarantees. Its
  JSONL storage verifier expands each stream and checks that it assembles to
  the logged answer (`session-persistence-jsonl/src/generation.ts:553-577`).
  Karaka has no durable backend yet; `expand_stream` with `chunk_event` and
  the `BlockAssembler` is the check one would run, and the loop tests run it
  on what the loop logs.
- **Tests.** dsh's `assistant-stream.spec.ts` cases: packing, run breaks,
  unsafe values and empty identities, and every malformed record it lists.
  Also round trips (Karaka chunks to records and back, exactly), dsh-written
  records kept whole, and records assembling to the same answer. Loop tests
  cover the embedded stream of an answer, of a signed answer's finish, of a
  failed attempt, and of a cancel.

### Where part B differs from dsh

- **Chunk JSON is Karaka's writing of dsh's shapes.** A `block-end` holds
  v4's content block, as a message holds it, so a reasoning block's
  signature is in the finish's `replayState`, not the block, as in dsh.
  `usage` always writes the two cache counts, as Karaka's logged usage
  does; pi-ai writes them only when not zero.
- **The finish's `replayState` is Karaka's envelope**, pi-ai's v2 or
  DeepSeek's shape (see `convert.rs`), the same one the message's source
  gets, not something an adapter wrote straight off the wire.
- **The full storage verifier isn't ported**, only the pieces it needs.
  It belongs with the first durable backend.
- **No stream reader helpers** (first token time, visible content, joined
  text and the like). Nothing in Karaka reads them yet.


## Durable sessions, part A: the seam, the raw v4 backend, and the store on it

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-22.*

### What is done

- **`SessionHeader`** (`karaka-session`, `header.rs`; dsh's
  `core/session/src/types.ts`): id, `createdAt`, cwd, parent, `isSeeded`,
  origin, delegation depth and agent preset. The version is the constant
  `SESSION_FORMAT_VERSION`, 4. `KNOWN_EVENT_TYPES` is dsh's generated
  vocabulary (`known-event-types.ts`): every type dsh v4 defines, modeled
  by Karaka or not.
- **The store's feed** (`karaka-session`, `store.rs`). A `SessionListener`
  hears every appended event, under the session's lock so in log order,
  and is asked to `flush` a session: dsh's `session/event` and
  `session/flush`. A restored seed is not heard, as dsh never publishes
  one; repair's closers are. `SessionStore::flush(id)` is dsh's
  `sessions.flush`: it waits for every listener and says whether any keeps
  the session. With no listener, sessions are in memory, as before.
- **The seam** (`karaka-session-persistence`; dsh's
  `session/session-persistence`).
  - `SessionPersistence`: `create`, `open(id, Read | Write)`, `stat`,
    `list` and the service `flush`. `SessionHandle`: `read(offset,
    length)`, `append`, `flush`, `close`. Both are object safe, with boxed
    futures, so the backend is a port chosen at startup.
  - `PersistenceError` has dsh's kinds and messages: not found, already
    exists, already owned, read only, ownership lost, handle closed,
    corruption, format unsupported (naming the raw log), plus I/O, an
    aggregate, and `Other` for dsh's plain `Error`s.
  - `contract.rs` is dsh's `storage-contract.ts`: the stored id, a create's
    header, contiguity, and `validate_stored_events`, which refuses an
    unknown non-ignorable type or the legacy `fallback` reason on the raw
    rows, then reads each row as an `Entry` and restores the whole log in a
    `SessionLog`.
  - `LiveWriter` (`live.rs`) is the live write path in front of any write
    handle: a fixed 200 ms window started by the first pending event,
    `flush` as the barrier, a failed batch kept in order with the window
    quiet until the next drain, `close` draining first and closing anyway.
  - `DurableSessions` (`durable.rs`) keeps the store on a backend. Its
    router is the store listener that sends each event to the session's
    writer, as dsh's `JsonlBackendTracker.install`. `open(id, cwd)` is the
    agent loop's `resumeWith` with `restoreOrCreateConfigured`'s fallback:
    take the write handle, read, restore, repair, flush the closers; or
    create on not found. `flush` and `close` cover every session.
    `DurableSessionsPlugin` provides it.
- **The backend** (`karaka-session-jsonl`; dsh's
  `session-persistence-jsonl` with `compression: 'none'`).
  - dsh's layout exactly: `--<project key>--/` or `_no-cwd/`, the session's
    directory named by the injective `~XXXX` escaping over UTF-16 code
    units (with `.` and `..` escaped whole), `session.v4.jsonl`, and
    `session.lock`.
  - The header line in v4's key order, and one row per event in the
    lossless source-event form: runs of three or more `sourceEventSeqs`
    stored as `[start, end]`, expanded back exactly.
  - The scanner (`format.rs`) is dsh's `SessionLogScanner` with v2's row
    decoder: it drops an incomplete last line, and everything from a
    damaged row on unless a later row is a `turn/end`, which refuses the
    file. `admission.rs` runs v4's structural checks on every row before
    that recovery, as dsh's scanner does, and the writer runs them too.
  - Lazy materialization: nothing on disk until the first append or flush,
    which publishes the header and batch through a synced temporary file
    and `link`, after syncing each new directory. A creator closed before
    then leaves no trace.
  - Each append is written and `fsync`ed; a failed write or sync cuts the
    file back to its earlier length, and a failed cut is reported with
    the failure. A write handle cuts a torn tail before its first append.
  - The lock: `flock` through Rust's `File::try_lock` (no new dependency),
    with dsh's inode check and bounded retry, taken at write open, or just
    before a created session's first write. Released by closing; the file
    is never removed.
  - Root: required, resolved once, an existing root must be a readable
    directory. A root holding compressed logs, the old flat layout, an id
    in two project directories, or a header that doesn't name its own
    file are refused with dsh's wording.
- **The CLI.** `KARAKA_SESSIONS` mounts the backend and `DurableSessions`
  for `chat`; `KARAKA_SESSION` names the session (`chat` otherwise).
  `chat` resumes the session before the agent opens it, flushes after each
  turn, before printing its answer, and closes at the end of stdin.
  Without `KARAKA_SESSIONS`, nothing changes. The CLI tests clear both
  variables, so no test can reach a real store.
- **Tests.** dsh's persistence contract suite against this backend; the
  `jsonl.spec.ts` cases that apply to raw v4 (format helpers, refusals,
  generations, durability, the scanner, identity, traversal, listing);
  `current-event-admission.spec.ts`; `load-path.spec.ts` as a plugin
  test; `lease.spec.ts` with a second backend instance standing for a
  second process, and a real second process (the test binary started
  again as the holder, then killed); the v4 fixture written and read back
  event for event; the live write path on paused time; `DurableSessions`
  on the real backend; and a CLI chat resumed by a second run.

### Left out of part A

- Zstandard frames, dsh's default, with torn-frame recovery and the
  storage verifier's stream check: part B. This backend writes raw lines
  only, and refuses a root holding `.jsonl.zstd` logs, as dsh does with
  `compression: 'none'`. There is no `compression` setting yet.
- Historical generations and migration through the catalog: part C. An
  older log is refused at open with dsh's "older than the supported v4"
  wording and its raw path, is an existing id for `create`, and is never
  written: the refusal comes before the lock, so not even `session.lock`
  appears beside it. `stat` refuses it too, and `list` skips it; dsh would
  show its translated header.
- `assertReleasedV4Relationships`, v4's cross-row lifecycle checks (turns,
  steps, tool calls, retries, compaction). `SessionLog::restore` checks
  seqs and surface operations; the rest waits for the catalog in part C.
- The cold-log memo, list concurrency (`listConcurrency`), cancellation
  signals (a dropped future cancels), `eventState`/frozen reads, Windows
  (dsh's named semaphore; the crate builds on Unix only), and the logger
  warnings dsh writes for a torn tail or a failed background batch.
- The checkpoint policy's per-request barrier
  (`session-checkpoint-policy`): `chat` flushes per turn instead.
- `karaka ask` stays in memory: its one-turn session is named `ask`, and
  storing it would make every ask carry on the last.

### Where part A differs from dsh

- **The batching window is in the seam**, written once around any handle,
  not inside each backend's handle. dsh calls it "the seam's internal
  scheduling policy" but writes it in `storage.ts`; the behavior is the
  same. The window's deadline is set when the first event arrives, as
  `setTimeout`. Concurrent drains wait for each other rather than sharing
  one result.
- **The loop doesn't resume sessions itself.** `Agents::open` is
  synchronous and storage isn't, so the caller resumes through
  `DurableSessions::open` first, and the loop finds the session in the
  store, already repaired. dsh's agent loop does both. Making
  `Agents::open` async would let the loop own it, as in dsh.
- **The session keeps no header.** dsh's `Session` carries one; Karaka's
  store doesn't need it yet, so it stays with the handle.
- **Typed decoding is strict and comes after the scan.** dsh's row
  decoder checks only the envelope, then `adoptSessionEvent` and the
  catalog validate the whole log. Karaka does the same split, so a row
  that is valid JSON with a good envelope but doesn't read as its typed
  event refuses the file as corruption, even at the tail.
- **A batch is checked and encoded before the lock is taken**, so a
  refused one leaves nothing on disk; dsh takes the lock first.
- **A dropped handle frees its claim and its lock** (`Drop`), where dsh
  relies on `close` or `await using`.
- **A read handle refuses an unreadable log at open.** dsh's read open of
  a current log reads it too, so this matches in effect.

## Fixes after the contract review

*Checked against dsh tag `dsh-v0.2.1-rc.2`, 2026-11-23. Each suspected bug
got a test first; only the ones whose test failed were changed.*

1. **A panicking tool or model stream wedged the agent: fixed.** Only
   request-error handlers and frame listeners were caught. A panic in a
   tool or a model stream killed the driver task with the turn left open,
   and `last_turn` didn't advance. The log didn't refuse what came next,
   as the review expected. It was worse: the next followup appended a
   second `turn/start` for the same turn number inside the open one,
   with the panicked tool call never answered. Now, as in dsh:
   - a tool that panics gets an error result, `Error: <message>`
     (`core/tools/src/index.ts:1917-1924`), and the turn carries on;
   - a model stream that panics, opening or mid-stream, ends its turn with
     `turn/end` error `UNKNOWN` and the panic's message, after an
     `assistant/attempt` if the stream had opened (`agent.ts:462-502`,
     `375-382`). The request-error hook isn't asked, as dsh asks it only
     about failures the model reports.

   Tests: `a_tool_that_panics_gets_an_error_result_and_the_agent_carries_on`
   and `a_model_stream_that_panics_fails_its_turn_and_the_agent_carries_on`.
   A panic elsewhere in the loop's own code still kills the driver;
   nothing in the loop is known to panic.
2. **A half-open resume: fixed.** If repair or its flush failed,
   `DurableSessions::open` left the writer registered and the session in
   the store, so the next `open` returned it with its closers unwritten.
   Now it follows `resumeWith` (`agent-loop/src/index.ts:875-921`): the
   closers are appended through the handle before the session is
   restored. If that fails, the handle is closed and nothing is
   published, so a later `open` starts over. Test (a backend whose first
   append fails):
   `a_resume_whose_closers_fail_to_store_publishes_nothing_and_can_be_retried`.
3. **Underflow in the `sourceEventSeqs` range check: fixed.** Plain
   members outnumbering the seq, then a range, panicked in a debug build;
   a release build wrapped, and a later check refused the row with
   another message. Now there is no room left, and the row is refused
   with dsh's "range exceeds its event seq". Test:
   `a_range_after_too_many_members_is_refused`.
4. **A fractional `providerRetryAfterMs`: fixed.** dsh does produce one:
   its DeepSeek transport turns `Retry-After: 1.2345` into `1234.5`
   (`llm-deepseek/src/transport.ts:37`), and `turn/end` stores the
   failure. The log loaded, contrary to the review, but the reason fell
   back to `TurnEndReason::Other`, so Karaka didn't read it as a failure.
   The field is now a JSON number, kept as written, so it reads as a
   failure and is written back the same. The retry plugin reads it as a
   float, as dsh's `llm-retry` does. Test:
   `a_fractional_retry_wait_from_dsh_loads_and_round_trips`.
5. **An empty aborted turn between queued turns: not a bug I could make
   fail.** dsh checks its signal before `turn/start` (`agent.ts:304`)
   because, in dsh, other code can run in the gap between one turn and the
   next (`while (await this.turn())`). Karaka's driver has no such gap. From
   making the next turn's cancel switch to appending its `turn/start`, it
   never awaits, so no task on the same thread can cancel in between. A
   cancel from the turn's own `turn/end` hits the old switch, and a
   followup sent after it runs as the next turn, as in dsh. The cancel
   that remains is one from another thread landing within those few
   instructions. A check before `turn/start` wouldn't close that race. It
   could still land just after the check. And what it would leave is a
   well-formed `turn/start` and `turn/end` (aborted) with no step. That is
   the same shape dsh itself logs for a cancel right after a followup,
   because there `turn/start` is appended synchronously. No code changed.

Also: an interrupted `assistant/message` now carries the usage if its
chunk came before the cancel, as dsh writes `live.usage` (`agent.ts:480`).
The comment saying Karaka's models report usage only when they finish
was wrong since step five, part A. Test:
`a_cancel_after_the_usage_keeps_it_on_the_partial_answer`.
