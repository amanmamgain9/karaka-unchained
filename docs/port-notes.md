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
- **The session kept no header** at this step; it stayed with the handle.
  "Session projections" below gives every live session its header, as
  dsh's does.
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

## Durable sessions, part B: Zstandard frames, the default

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-25.*

### What is done

- **The setting.** `JsonlConfig::compression` is `Compression::Zstd` by
  default, or `Compression::None`, dsh's `'zstd' | 'none'`
  (`DEFAULT_COMPRESSION`, `JsonlCompressionSchema`). The filename rule is
  dsh's `generationLogFilename`: `session.v4.jsonl.zstd`, or
  `session.v4.jsonl` raw. `JsonlConfig::new(root)` takes the default; the
  CLI uses it, so `KARAKA_SESSIONS` now writes compressed logs.
- **A log in the other encoding**, checked as dsh does it, is not "not
  found": dsh refuses it loudly (`encodingMismatch`, from
  `resolveGenerationInDirectory`, the whole-root `ensureRootEncoding`, and
  `rejectOppositeArtifact` before a create's first write). Karaka does the
  same in both directions, with dsh's text: `uses .jsonl, but this backend
  is configured for compression "zstd"` and the reverse.
- **Frames** (`frames.rs`, dsh's `zstd.ts`).
  - `compress_frame` makes one independent frame with the checksum flag, at
    zstd's default level, as Node's `zstdCompress` does.
  - `scan_frames` is the structural scanner: magic, frame header, blocks
    and checksum, with no decompression. A frame the bytes end inside gives
    `torn_start`. It refuses an invalid magic, a reserved frame-header bit
    or a reserved block type with dsh's text and byte offset. `max_frames`
    is kept: the header-only read uses it to stop after the first frame.
  - `decompress_frame` decodes one complete frame and checks its checksum
    (libzstd's streaming decoder does by default).
  - `decompress_prefix` reads the streaming decoder until it errors or the
    input ends, keeping what it emitted. That is every block that arrived
    whole. Tested: a 300 KB batch cut mid-frame gives back its first blocks,
    with the records flushed into them, and a frame cut inside its header
    gives nothing.
  - The module is public, for tools and tests that inspect a stored log.
- **Writing.** The header line is a frame of its own, and each durable
  batch is one more frame: the lazy create publishes header frame plus
  first-batch frame, a flushed empty session is the header frame alone
  (`encodeMaterialization`, `encodeEventBatch`). Frames go through the same
  append as raw lines, so fsync per batch and rollback are unchanged: a
  failed write or sync cuts the file back to the end of the last complete
  frame.
- **Reading** (`format.rs`, `scan_zstd_log`; dsh's `readZstdPrefix`).
  - The first frame must be exactly one header line
    (`assertZstdHeaderFrame`).
  - Every complete frame is decoded and fed, in order, to the same scanner
    raw logs use, so the admission and v4 checks run unchanged on the
    decoded text.
  - Every complete frame must end on a record boundary, or the log is
    refused ("complete frame contains a torn JSONL record").
  - A torn last frame contributes the complete records its prefix decodes
    to, or nothing when it doesn't decode.
  - A frame that fails to decode, its checksum included, is refused as dsh
    words it: "frame at byte N failed validation".
  - `stat` and `list` read only the header frame (`readFirstZstdLine`). A
    damaged one refuses `stat` ("header frame failed validation") and is
    skipped by `list`.
- **Repair** (dsh's `persistContiguous` with `truncateTornTail`). A write
  open keeps the torn frame's start and its recovered records. The first
  append cuts the file there, then writes the recovered records again as a
  frame of their own, then the batch. Each step is forgotten only once it
  lands, so a failed rewrite is retried exactly once at the next append. A
  raw log has the same shape, with nothing to recover: a torn raw tail is
  one incomplete line.
- **The dependency.** The `zstd` crate (libzstd), pinned to `=0.14.3`
  (2026-10-16), without default features (pre-1.0 formats and the
  dictionary builder). The lockfile pins `zstd-safe` 8.2.0 and `zstd-sys`
  2.1.2 (libzstd 1.5.7), released with it. The newest at the time
  (0.14.5, libzstd 1.6.0) were a day old.
- **Tests.**
  - The seam contract now runs in both encodings (`tests/contract.rs` and
    `tests/contract/cases.rs`), as dsh runs it for `jsonl-none` and
    `jsonl-zstd`, each with its own torn tail.
  - `zstd.spec.ts`'s structure cases (`frames.rs`), and its backend cases
    (`tests/zstd.rs`): the default and its name, one frame per header and
    per batch, round trips, a frame torn mid-batch, one torn at its
    checksum, one cut inside its header, a torn frame with no complete
    record, a complete frame ending mid-record, bad magic and reserved
    bits, a checksum mismatch, damaged header frames, and encoding
    mismatch both ways.
  - The rollback and the retried rewrite of a recovered tail, as unit
    tests with the injected sync failure.
  - `DurableSessions` and the CLI now run on the default encoding.
- **Bytes from dsh's encoder.** dsh's tests ship no zstd fixture; they
  make frames at run time with `compressZstdFrame`, which is exactly
  `zlib.zstdCompress(input, { params: { ZSTD_c_checksumFlag: 1 } })`.
  `tests/fixtures/node-zstd-session.jsonl.zstd` is a two-turn synthetic
  session written by that same Node call
  (`tests/fixtures/generate-node-zstd-session.mjs`, standalone, importing
  nothing from dsh). Node writes single-segment frames with a content
  size, unlike the Rust encoder, so the test covers the other header
  form. Karaka reads it, lists it, and appends its own frame after Node's
  bytes. Running dsh's own TypeScript to write the fixture wasn't
  possible here.

### Left out of part B

- **The saved-stream check**: expanding each embedded stream and
  assembling it against its message. dsh runs it only in
  `verifyCurrentGeneration`, on a migrated generation (`index.ts:713`),
  so it belongs with migration in part C.
- **Making `Agents::open` async**, still out of scope.
- **dsh's Node-private multi-frame decoder, its decode time slices, and
  frame-by-frame cancellation.** Each frame is decoded on the blocking
  pool, and a dropped future doesn't stop it.
- **The torn-tail warning** dsh logs on repair: Karaka has no logger yet,
  as in part A.

### Where part B differs from dsh

- **libzstd through the `zstd` crate, not Node's zlib.** The frames are
  standard and interchange both ways. The Rust encoder writes frames
  without a content size (multi-segment, a window descriptor); Node
  writes single-segment ones. Both are dsh-valid.
- **The header-only read refuses with dsh's message, minus JavaScript's
  `Error: ` prefix** that `String(error)` adds inside it.
- **A torn last frame whose prefix doesn't decode recovers nothing,** as
  in dsh. The prefix is read with a streaming decoder rather than
  `ZSTD_e_flush`, which yields the same whole blocks.
- **A failed frame decode keeps dsh's message, without its cause.**

## Durable sessions, part C: closing the v4 gaps

*Checked against dsh tag `dsh-v0.2.1-rc.2`, 2026-11-25. Each reported gap
got a test first. Only the ones whose test failed were changed.*

### Decision: no migration of older formats

dsh's v0-to-v4 migrations are about 8,000 lines. Karaka has only ever
written v4, and Play This Life keeps its sessions in Postgres, not in
JSONL files. The user decided to drop migration. Karaka keeps refusing an
older generation with dsh's "older than the supported v4" wording and the
raw log's path. It treats one as an existing id for `create`, and never
writes it: no successor, no lock. The format catalog, the historical
codecs and the migration verifier are not ported.

### The gaps

1. **A second open of an open session: not a gap.** dsh refuses in
   exactly these cases, and Karaka matches each:
   - a second write handle in one backend instance (the contract's "write
     ownership is single-holder per instance");
   - a writer in another process or instance, through the `flock`;
   - an agent-loop `resume` while a writer is held, as `session/writer-held`
     in the session controller.

   The controller first hands back a live agent, and shares one resume
   between concurrent callers (`api/session-controller/src/agent.ts:213-249`).
   `DurableSessions::open` is that layer: it returns a session already
   live and stored, and one open at a time runs. A writer held elsewhere,
   in this process or another, is refused as `AlreadyOwned`. The new test
   `a_writer_held_elsewhere_in_this_process_is_refused_as_owned` passed
   before any change.
2. **`session/end-seed` on resume: fixed.** Not fork-only. dsh's Session,
   built from a stored log, appends an untagged `session/end-seed {}`
   unless the log already ends with one (`core/session/src/index.ts:694-698`),
   and the agent loop stores it with the unstored suffix. A fork's marker,
   `{ inherited: true }`, is the only fork part (`fork.ts`); forks aren't
   ported. `DurableSessions::open` now appends the closers and the marker
   through the handle before restoring the session. Test:
   `each_resume_stores_an_end_seed_marker_unless_the_log_ends_with_one`.
3. **Surface fields and `image/offload`: fixed** (`checks.rs`,
   `surface.rs`, `json.rs`).
   - **Surface fields.** A known event that can't join the surface, such
     as `approval/asked`, or an unknown one not marked ignorable, may not
     carry `surfaceOp` or `sourceEventSeqs`. Only an unknown ignorable
     event keeps them opaque. The refusal names the one field, as dsh's
     `surfaceOpOf` does; Karaka's lifecycle types had joined both into one
     message.
   - **`image/offload`.** dsh's stored-log reader always installs the
     image-offload projection (`session-format-catalog/src/message-projections.ts`),
     so a valid offload is admitted and a malformed one refused in its
     words: targets, a current surface node, a user message or tool
     result, strictly increasing indexes, an image that exists and isn't
     offloaded already, counting earlier offloads. Karaka sends no images
     to models, so the offload changes no derived history; only admission
     follows dsh.

   Tests: `tests/admission.rs`.
4. **Developer messages: fixed** (`checks.rs`, `json.rs`; dsh's
   `validateSessionEventData` and `assertDeveloperHeader`):
   - tool-change blocks only in developer messages;
   - a nonempty `toolName`;
   - no inline tool definition;
   - `headerSeq` exactly when tools are added, naming an earlier
     `request/header`;
   - each added tool defined once and whole there, `deferLoading` true
     when present.

   These were only in the JSONL backend's row admission; now the session
   log checks them for every append and restore. The developer role and
   event go together with dsh's message, checked on the raw JSON before
   the typed reader's own refusal. A derive test that added a tool
   without a header now removes one instead. Tests: `tests/admission.rs`.
5. **The saved-stream check: ported**, as
   `karaka_session::check_assistant_streams` (`verify.rs`; dsh's
   `assertCurrentAssistantStreams`, in its words).
   - What it checks: every stream must expand. Each answer's content must
     be what the `BlockAssembler` makes of the stream (the interrupted
     blocks for an interrupted answer), and its usage and replay state
     must be what the stream wrote.
   - Where it runs: as in dsh, not on an open. Every agent-loop harness
     runs it on every session when dropped, and every CLI `chat` run now
     stores its sessions in a fresh temporary root and checks each log.
     The v4 fixture is checked too. A loop test edits a logged answer, to
     show the check isn't vacuous.
   - What it found: no disagreement in Karaka's writer. It did find that
     the hand-written fixture broke dsh's rule: its first answer had usage
     and replay state its stream never wrote. Its stream now ends with the
     usage and the finish a writer would have logged.

### Where part C differs from dsh

- **Content is compared through Karaka's types.** dsh's assembler builds
  v4 content blocks straight from the chunks. Karaka assembles its own
  blocks and turns them into v4 content with the writer's converter. For
  Karaka's logs the two agree. A dsh log whose blocks carry fields
  Karaka's converter doesn't write could be refused by this check where
  dsh's passes. Usage and replay state are compared as the stream wrote
  them, as dsh does.
- **The resume's closers and marker are one batch.** dsh appends the
  closers, then the marker with the rest of the unstored suffix.
- **The checks' messages carry Karaka's subject**, `session event at seq
  N: ...`. dsh's subject varies with the caller: `session event at seq N`,
  `seed TYPE at index N`, or `developer/message at seq N`.
- **`image/offload` is admitted as dsh's reader admits it.** A dsh Session
  without the image-offload plugin refuses one live ("requires a message
  projection"); Karaka never writes one.

### Still different, and not small

- **v4's cross-row relationship checks** (`assertReleasedV4Relationships`):
  lifecycle nesting, tool-call lifecycles, retries, compaction, titles,
  commands, catalogs and deliveries.
- **Full payload adoption** (`adoptSessionEvent` and message validation).
  Karaka's typed reader checks the shapes it models, in its own words.
- **Forks and seeded sessions**: the tagged inherited marker, and
  `inheritedEventCount` beyond 0, in the store and `DurableSessions`.
- **Message projections in general.** Only `image/offload`'s admission is
  ported, and its effect is moot while Karaka sends no images.

## System prompt

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-25: `packages/core/system-prompt`
and its three spec files.*

### What is done

- **One crate, `karaka-system-prompt`**, the seam and its implementation
  together. dsh's `SystemPrompt` is one concrete registry with nothing
  behind it to swap: no backend, no second implementation. That is
  Karaka's session-store shape, not persistence's port-plus-backend.
  `SystemPromptPlugin` provides the `SystemPrompt` port in the static
  kernel.
- **The registry**, with dsh's rules and words:
  - **Sections:** static or worked out per assembly, ascending `order`,
    ties by name in UTF-16 code-unit order, as JavaScript compares. A
    `complete` section is the whole prompt, restored after the listeners,
    and two fail the assembly. Duplicates and non-finite orders are
    refused per layer.
  - **Variables:** `[a-z][a-z0-9_]*`, resolved per assembly, scoped ones
    shadowing global ones, providers added during an assembly read in it.
    Rendering is strict: an unknown or valueless reference and a malformed
    group fail, a lone `{{` is prose, values aren't scanned again, and
    `interpolate: false` keeps text literal.
  - **Tool sources:** their schemas taken before any is asked, then
    ordered by name or by `tool_order`, with a known-names universe for
    tools hidden in one assembly. The rest entry `<unlisted-tools>` is
    reserved.
  - **Runtime context:** order, `required`, suppression (global, scoped,
    and `include_runtime_context: false`), `refresh_context`, and the
    snapshot renderers.
  - **Listeners:** dsh's `system-prompt/assemble` waterfall, as
    `on_assemble` with a `next` that runs the rest. Scoped listeners shape
    only their scope's assemblies.
- **Config:** `include_harness_identity` (true), `include_runtime_context`
  (true), `persona_prefix` at order 0, `persona_suffix` at 10200, and
  `tool_order`. A duplicate or a missing rest entry fails at setup; a
  listed name no source knows fails every assembly.
- **Agent scoping:** a `Scope` is one agent's, named by its session id. The
  loop assembles for its session's scope, so a contribution registered
  there shadows a global one of the same name for that agent alone.
- **The loop** follows dsh's pre-step and step (`agent.ts:272`, `407-442`):
  - Each step assembles once, after claiming its input and before
    `step/start`; a failed assembly ends the turn with no step, under
    `UNKNOWN`.
  - It renders the prompt once at the step's start; a failed rendering
    fails the step.
  - Every attempt logs that prompt through the existing system-message
    projection and sends the assembly's tools. The `request/header`
    records the same tool order.
  - The loop plugin registers dsh's `provider` and `model` variables
    (`agent-loop/src/index.ts:389-390`).
  - `LoopConfig` no longer has a `system` field.
  - What the log records keeps dsh's shape: `system/message` events and the
    request header, unchanged apart from their content.
- **Tools:** `ToolsPlugin` requires the prompt port and registers the
  registry as a tool source, as dsh's tool runtime does
  (`core/tools/src/index.ts:857`). So tools now reach the model by name,
  not in registration order.
- **The CLI:** `KARAKA_SYSTEM` is the persona prefix, so a request's system
  prompt is the identity line, a blank line, then the user's text. `ask`
  and `chat` both get it. No new environment variable.
- **Tests:**
  - The three spec files, where they apply: `tests/system_prompt.rs`,
    `tests/scoped.rs`, `tests/tool_order.rs`, and the renderer's cases in
    `assembly.rs`.
  - Loop tests, each failing first: the prompt and tools come from the
    registry with the identity first and tools by name; a failed assembly
    ends the turn before its step; a prompt that doesn't render fails its
    step; a scoped section reaches its own session alone.
  - Existing loop tests run with the identity off (`quiet`), so their
    prompts stay exactly their persona.

### Where it differs from dsh

- **The identity line is `You are an AI agent powered by Karaka.`**, at
  dsh's order of −1000. dsh's names DeepSeek Harness. This is deliberate:
  Karaka is provider-neutral.
- **Only the placements of contributors Karaka has are kept**: the
  identity (−1000), the persona prefix (0) and suffix (10200). There is no
  context-order table, since no context contributor exists. Listed below.
- **No `system-prompt/change` event.** Karaka has no event bus, so the
  specs' change and rollback cases are left out.
- **A registration is taken back by `Registration::remove`.** dsh's Cordis
  effects also end when their fiber is disposed. Dropping a registration
  keeps the contribution.
- **One level of scope**, one agent's. dsh's scopes nest, farthest first.
- **`AssembleContext` is only the scope.** dsh's also carries a signal and
  merge-extensible fields, such as the agent its variables read options
  from. `provider` and `model` are the loop's one config, the same for
  every agent.
- **Listeners run in the order added.** Cordis's `prepend` isn't ported.
- **The duplicate-name message for a global layer** says to register "in
  that agent's scope" where dsh's names `agent.ctx`.
- **Tool schemas have no `deferLoading`.** Karaka's model seam doesn't
  model it.
- **Runtime context** was not yet shown to the model at this step; see
  "Runtime context" below, which ports it.

### dsh's other contributors, not ported

These are section placements dsh allocates (`SECTION_ORDERS`), whose
contributors are other packages. None is in Karaka.

- `PLAN_POLICY` 500 (plan mode)
- `TEAM_POLICY` 600
- `PTC_ONLY` 800 (tools)
- `FILE_REFERENCE` 900
- per-tool guidance:
  - shell: `TOOL_BASH` 1000, `TOOL_PWSH` 1010
  - files: `TOOL_READ` 1100, `TOOL_WRITE` 1200, `TOOL_EDIT` 1300,
    `TOOL_GLOB` 1400, `TOOL_GREP` 1500
  - processes: `TOOL_JOBS` 1600, `TOOL_PTY` 1700
  - web: `TOOL_WEB_SEARCH` 2000, `TOOL_WEB_FETCH` 2100
  - `TOOL_LSP` 2200, `TOOL_SESSION_QUERY` 2300, `TOOL_GOAL` 2400
  - agents and workflows: `TOOL_WORKFLOW` 2600, `TOOL_RALPH` 2700,
    `TOOL_SUBAGENT` 2800, `TOOL_REPORT` 2900
  - `TOOL_COMPUTER_USE` 3000
- `MCP_SERVERS` 3100
- `TOOLS_SDK` 5000 (tools: the generated SDK and collapse sections)
- `DELIVERABLE_FILE_REFERENCES` 9000
- `STRUCTURED_OUTPUT` 9900 (subagents)
- `HARNESS_SOURCE` 10000, `WEB_SURFACE` 10100 (harness dev context)

And dsh's context placements (`CONTEXT_ORDERS`):

- `WORKING_DIRECTORY` 100
- `SANDBOX_POLICY` 110
- `APPROVAL_POLICY` 115
- `SUBAGENT_DELEGATION` 120
- `TEAM_IDENTITY` 125
- `HARNESS_SOURCE` 200
- `WEB_SURFACE` 210

Personas, presets and skills are separate dsh plugins and are out of
scope. dsh's assembly pulls in no guidance of its own beyond the identity
line; all of the above comes from the packages that own those facts.

## Runtime context

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-25:
`agent-loop/src/runtime-context.ts` (`RuntimeContextProjection`) and its use
in `agent.ts`.*

### What is done

- **The snapshot** (`karaka-agent-loop`, `runtime_context.rs`):
  - It is a `user/message` with source `runtime-context`, appended to the
    surface. Its text is the registry's rendered context, opened by
    "Current runtime context. This snapshot supersedes earlier
    runtime-context snapshots."
  - Its source carries `form: "snapshot"` and the named sections. Context
    that went away after a snapshot was sent sends dsh's CLEARED text with
    a bare `{ "kind": "runtime-context" }` source.
  - A snapshot is sent only when it differs from the one in effect: the
    newest runtime-context message still on the surface, when that is one
    text block.
  - No context, and none ever sent, sends nothing.
  - A replacement that cites the snapshot in effect, as compaction does,
    drops it, so the next step sends it again, or CLEARED.
- **Resume.** When the agent is made, the snapshot in effect is worked out
  from the log, newest first, as dsh's constructor does, so a resumed
  session doesn't send an unchanged snapshot again. The projection then
  follows the log. dsh uses a `session/event` listener; Karaka reads the
  events it hasn't seen each time it is asked. Since only the log changes
  the state, the outcome is the same.
- **Where it sits in the log**, as dsh admits it (`agent.ts:407-442`):
  - Each attempt reads the context again with `refresh_context`.
  - A cancel then (`agent.ts:414`), including one a context provider
    triggers, logs nothing of the step.
  - Then the system-prompt commits, then the step's input, then the
    snapshot when it changed, then `request/header` and `request/context`
    as the request is built.
  - A retry whose snapshot was compacted meanwhile sends it again without
    logging the input twice.
  - Context that doesn't render ends the turn before its step, as dsh's
    pre-step renders it for its default decision (`agent.ts:277`).
- **Required context survives `include_runtime_context: false`**, by the
  registry's suppression rules, which the refresh applies at each attempt.
- **Order table:** `ContextOrder::WorkingDirectory`, at dsh's 100, for the
  working-directory service coming next. The other context placements stay
  listed under "System prompt".
- **Tests:**
  - dsh's `runtime-context.spec.ts`, as unit tests of the projection.
  - The `loop.spec.ts` runtime-context cases that need no pre-step hook:
    - context sent when it changes and cleared when it goes, with no
      prompt rewrite and one `initial` header;
    - a snapshot compacted away is sent again;
    - compacted context that went away is cleared;
    - an unrelated replacement sends nothing;
    - a malformed retained snapshot is replaced;
    - a retry after compaction sends the snapshot once more;
    - a cancel from a context provider commits nothing.
  - Karaka's own: required context with runtime context off; a resumed
    session that doesn't resend an unchanged snapshot but sends a changed
    one.
  - Every loop log still passes the saved-stream check and the session's
    log checks.

### Where it differs from dsh

- **No pre-step hook**, so the snapshot always goes after the step's
  input, which is where dsh's default pre-step decision puts it. dsh's
  cases that move it, or suppress it, through `agent/pre-step` aren't
  ported: "replaces a stale owned snapshot at its accepted position",
  "drops an uncommitted optional snapshot suppressed after the pre-step
  fallback", "reconciles runtime context replaced before/after the pre-step
  fallback", and the pre-step `reject`/`empty` cases.
- **The snapshot's id** is `runtime-context-{seq}`, in the loop's id
  style; dsh's is random.
- **The projection reads unseen events when asked**, where dsh listens to
  `session/event`.
- **No context contributor exists yet.** The CLI sends no snapshot until
  the working-directory service is ported.

## Session projections

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-25:
`session/session-projection` (`src/index.ts`, `src/types.ts`, the README,
and `tests/registry.spec.ts`).*

### What is done

- **The crate** `karaka-session-projection`, with
  `SessionProjectionsPlugin`. The plugin requires the `SessionStore` port
  and provides the `SessionProjections` port. The binary mounts it after
  the sessions plugin. No unit is registered yet; working-directory will
  be the first.
- **A unit** is a `Projection`:
  - `KEY`;
  - `state_version()`;
  - `init(header, inherited_event_count)`;
  - `apply(state, entry) -> Option<State>`, where `None` means unchanged.

  A unit that also implements `ClientView` (`type View`, `view(state)`) is
  client-visible. It is registered with `register`. A unit without one is
  host-only and is registered with `register_host_only`. These are dsh's
  two `register` overloads.
- **The drive.** The registry follows the store as a `SessionListener`,
  dsh's `session/event`. Every appended event passes every unit, in
  registration order.
  - A cell is built when first needed, folding the log from `init`. This
    covers a unit registered after events flowed, and a session restored
    from storage, whose seed is never heard, as in dsh.
  - When `apply` says unchanged, no view is worked out.
  - A view is worked out only while someone listens. It is heard only when
    it differs from the last view worked out.
  - A change no one heard leaves nothing to compare the next view with, so
    that next view is heard. This is dsh's two-slot buffer, and all 960
    cases of dsh's identity sweep pass.
- **The reads:**
  - `snapshot(session, keys)` gives `{asOfSeq, values}`: client-visible
    views only, every unit folded first.
  - `on_changed(listener)` gives the session id, key, view JSON and the
    seq of the event that changed it.
  - `state_of::<P>(session)` returns the state as a read-only `Arc`, with
    no view worked out.
  - `checkpoint(session)` gives every unit's `{ver, seq, val}` row as
    detached JSON.
  - `restore_floor(checkpoint)`, `restore(checkpoint, events, base_seq,
    header, inherited_event_count)` and `view_checkpoint(checkpoint, keys)`
    follow dsh's rules: the one-below anchor, a row usable only at its
    version and within the supplied end, "re-read from seq 0", the gap
    refusal, and rows that no longer read back being left out of views.
  - A seq before the first event is written -1, as dsh's
    `SessionSeqCursor`.
- **Keys.**
  - A second registrant of a key at the same state version shares the
    unit and its cells.
  - A different version is refused, with dsh's message.
  - Each registration is a handle. When the last registrant calls
    `remove`, the key and its cells go; re-registering folds the log
    again.
  - `ChangeSubscription::remove` stops a listener.
  - Dropping either handle keeps what it registered, as in the
    system-prompt crate.
- **One cut under threads.** dsh's cut is one synchronous tick. Here:
  - The drive runs under the session's log lock, then takes the
    registry's.
  - A read copies only the events its cells haven't seen, with the
    registry unlocked, then locks the registry.
  - If an append drove the cells past that copy meanwhile, the read goes
    again.
  - So every value reflects exactly the events up to `asOfSeq`. A test
    snapshots while another thread appends.
- **Sessions carry their header** (`karaka-session`):
  - `Session::header()` and `Session::inherited_event_count()`, which dsh's
    `Session` has and `init` needs.
  - `SessionStore::create(id)` stamps a fresh header with the current
    time, as dsh's `create` does.
  - `create_with(header)` and `restore_with(header, inherited_event_count,
    entries)` take a stored one.
  - `DurableSessions` now passes the backend's header, both when it
    creates a session and when it resumes one, so a resumed session keeps
    its stored cwd.
- **Listeners see the session** (`karaka-session`). `SessionListener::event`
  now takes an `Appended`: the session's id, header, inherited count, and
  its log up to and including the event, read in place under the lock.
  That is what dsh's `session/event` passes (the `Session`). The drive
  needs it to build a cell mid-stream without calling back into the
  locked session.
- **Tests:**
  - every `registry.spec.ts` case that Rust can express (see below);
  - a second registrant with another state type;
  - `snapshot` with keys;
  - a restore over a gap;
  - a restored session folded on first read, then followed;
  - one cut during concurrent appends;
  - the plugin;
  - sessions keeping their header in the store and through `DurableSessions`;
  - a listener shown the log as of each event of a batch.

### Left out

- **`hydrate` and `cachedSnapshot`.** Both serve the projection cache and
  session listing. They come with the cache plugin, which is out of scope.
- **Cases Rust rules out.** A non-integer or negative `stateVersion` can't
  be written (`u64`). A view that returns a Promise can't either, so
  "fails loud when a unit view violates its own schema" has no Rust form.
- **Cordis fibers.** "Removes registrations and change listeners when
  their owning fiber unloads" is ported as the two handles' `remove`.
  "Keeps a yielded registration live until the composite finishes using it"
  has no equivalent, because Karaka has no effect composites.
- **The `session/created` hook.** dsh builds a new session's cells at
  creation. Here they are built at its first event or read, which gives
  the same state. The only difference is when `init` runs.

### Where it differs from dsh

- **"Unchanged" is a value, not a reference.**
  - `apply` returns `None` for unchanged, where dsh returns the same
    reference.
  - Views are compared with `PartialEq`, where dsh uses `Object.is`. So a
    new view equal to the last one is not heard, where dsh publishes a
    structurally equal new object. A unit no longer has to reuse a
    reference to stay quiet.
  - The identity sweep gives each view identity a distinct value.
- **States are typed.** Each unit's state is its own Rust type behind an
  erased cell, so there is no `stateSchema`:
  - restore reads a row back with `Deserialize` and refuses one that
    doesn't fit, as dsh's schema parse does;
  - views and checkpoints go to JSON with `Serialize`. A value that
    doesn't serialize is the unit's bug, and the registry panics naming
    the key, as dsh's parse throws.
  - A key can't be shared across state types; dsh has no types to compare
    at runtime.
- **`state_of` names the unit's type**, `state_of::<P>(session)`, where
  dsh names the key and its type table. It returns an `Arc` of the live
  state rather than the live object.
- **Change listeners get the session id**, not the session. They run with
  the session's log locked, after the registry is unlocked. So, like
  units, they must not call back into the session or the registry.
- **Cells are keyed by session id**, where dsh uses a `WeakMap` by
  session. Karaka's store never drops a session, so neither does a cell
  until its key is removed.
- **The in-memory store doesn't check the inherited count** against the
  log or the header's `is_seeded`. The JSONL format does, at write time.
- **A restored session has a fresh header** when it comes in through
  `SessionStore::restore(id, entries)`, which is kept for tests and
  fixtures. Storage goes through `restore_with` and the stored header.

## Filesystem

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-25: `fs/fs` (`src/index.ts`,
`src/types.ts`, `tests/service.spec.ts`) and `fs/fs-local` (`src/index.ts`,
`src/fsio.ts`, and the `filesystem`, `fsio` and `watch` specs).*

### What is done

- **The seam** (`karaka-fs`):
  - `FileSystem` is the `dyn FileSystem` port. It has:
    - `resolve`, `process_path`, `process_path_from_host_path` (by
      default `None`), `file_url` and `contains`;
    - `stat` and `lstat`;
    - `read_text`, `stream_text`, `read_bytes` and `read_byte_range`;
    - `list_dir`;
    - `write_text` and `edit_text`;
    - `watch`, which by default refuses with dsh's `FS_IO_ERROR` message.
  - The vocabulary is all of `types.ts`:
    - `FsTargetKey`, `FsVersion` and `FsObservation`;
    - `FsTarget`, `FsInfo`, `FsPathInfo` and `FsDirEntry`;
    - `FsWriteIntent` (`CreateIfAbsent`, `ReplaceIfVersion`) and
      `FsWriteOutcome`;
    - `FsEditRequest` and `FsEditOutcome`;
    - `FsErrorCode`, with dsh's thirteen names;
    - `FsError`: a code, a message, and a chained source.
  - Calls return boxed futures, as the persistence port's do.
- **Cancellation.** Every call takes an optional `AbortSignal`, made by an
  `AbortController` (dsh's names). An aborted one ends the call with
  `FS_ABORTED` and dsh's "`verb` aborted" message, at dsh's check points.
  The agent seam's `Cancellation` wasn't reused: it carries a turn's
  `CancelCause`, and the filesystem seam shouldn't depend on the agent
  seam.
- **The backend** (`karaka-fs-local`):
  - `LocalFileSystem` takes `LocalFsConfig { cwd, diff_basis_max_bytes }`.
    The defaults are the process's directory and 10 MiB.
  - `LocalFsPlugin` provides the port, and the binary mounts it.
  - **Resolution.** The key is the realpath. A missing target keys on its
    nearest real ancestor, with the missing part appended, so the key is
    the same before and after it is created. A display path with a `..`
    segment is kept as spelled, so the kernel resolves it physically;
    `..` through a missing directory is refused. A path under a file is
    `FS_NOT_FOUND`.
  - `paths.rs` reimplements Node's POSIX `resolve`, `dirname` and
    `basename`. `std::path` reads `/a/..` differently, and dsh relies on
    Node's reading.
  - **Versions** are dsh's string: `dev:ino:size:mtimeNs:ctimeNs`.
  - **Reads.**
    - Text reads refuse a NUL in the first 8 KiB, and invalid UTF-8. A
      leading BOM is dropped, as Node's `TextDecoder` drops it, and so an
      edit writes the file back without it, as dsh's does.
    - `stream_text` decodes across 64 KiB chunks. Its refusals arrive as
      the stream's first item, as dsh's generator throws on its first
      step.
    - `read_bytes` refuses by the stat size, then reads one byte past the
      cap, so a file that grew since the stat is caught.
    - `read_byte_range` reads only the window.
  - **Listing.** Children are in name order. Each target is resolved from
    the listed directory's own realpath, with metadata only while the
    child still exists. Errors use dsh's wording (`not found`,
    `permission denied`, or the I/O error).
  - **Writes.**
    - The new content is written to an owner-only (0o600) temp file, in a
      private (0o700) staging directory beside the target. It is synced,
      given the existing file's mode, and published with `rename`.
    - A guarded create publishes with a hard link instead, which never
      replaces anything. If it fails, the target is inspected to tell
      dsh's cases apart:
      - a file there: `FS_NOT_OBSERVED`;
      - something else there: `FS_NOT_REGULAR_FILE`;
      - the link reported a collision that has since vanished:
        `FS_NOT_OBSERVED`;
      - anything else: `FS_IO_ERROR`.
    - The staging directory is always removed. A cleanup failure after
      publication is ignored; one before it is reported with dsh's
      "write failed (…) and temp cleanup failed (…)".
    - The overwrite diff basis, `before`, is best-effort. It is read from
      the opened file itself and declined when either side reaches the
      limit (in UTF-8 bytes), or when the old file is binary, invalid
      UTF-8, changed size while read, or gone.
  - **Edits.**
    - The freshness check comes before matching, and a missing target is
      stale either way.
    - Matching is literal after CRLF is normalized. An empty search or no
      match is `FS_EDIT_NOT_FOUND`; several matches without `replace_all`
      are `FS_AMBIGUOUS_EDIT`.
    - Line endings are detected on the first 4096 UTF-16 units and
      restored on write-back.
  - **Turns per target.** Writes and edits of one key take turns through a
    fair async lock, so of two at one version, one wins and the other is
    stale. A key's lock is forgotten once no one waits on it.
  - **Watch** uses `notify` 8.2.1, without default features (inotify on
    Linux). The lockfile pins `notify-types` to 2.2.0, since 2.3.x is
    under three weeks old.
    - A directory is watched itself, and a file through its parent,
      neither recursively.
    - A file hears only its own entry. Reads (`Access` events) aren't
      changes, as chokidar doesn't report them.
    - Failures are heard as `FS_IO_ERROR`. A watch that can't start is
      heard and refused.
    - Nothing is heard once `close` begins.
- **Tests:**
  - every `service.spec` case, against an in-memory backend;
  - every POSIX `filesystem.spec` and `fsio.spec` case that needs no
    module mock;
  - the `watch.spec` filtering cases, against synthetic `notify` events,
    as dsh's spec drives a mocked chokidar;
  - real-inotify watch cases;
  - Karaka's own:
    - a dropped write leaves no staging;
    - an abort while staging publishes nothing;
    - BOM handling;
    - `file_url` escaping;
    - the per-target locks are released, including after an abort and a
      race;
    - the plugin, including that the kernel refuses a second provider of
      the port.

### Left out

- **Windows** (`win32.ts`, its DACL-preserving replacement, and the
  drive-relative paths). Karaka targets Unix for now. `karaka-fs-local`
  fails to compile elsewhere with a `compile_error!` that says why.
- **Sandboxing.** `sandboxMode` and the `sandboxPolicy` argument of
  writes and edits come with `fs-sandbox`, which is out of scope, as are
  the tools, fs-search, fs-observation-policy and fs-ssh.
- **The `fs/write-intent`, `fs/edit-intent` and `fs/observed` events.**
  These are the observation policy's, so they come with it.
  `FsObservation` is kept for it.
- **Cases that mock Node's modules:**
  - stat and lstat held mid-probe;
  - a descriptor that shrinks, grows or stops being regular after its
    stat;
  - a forged non-errno fault;
  - cancellation between a diff read's chunks.

  Rust can't swap `std::fs` out. The code follows dsh's steps there, and
  the abort checks sit at the same points.
- **Registration and disposal cases.** These are Cordis's; the kernel has
  no unload. The "duplicate service" case is the kernel's refusal of a
  second provider.

### Where it differs from dsh

- **Raw errors are `FsError`s.** Where dsh lets a Node error through (a
  failed rename, a staging directory that exists, a probe's I/O fault),
  here it is `FS_PERMISSION_DENIED` for a permission error and
  `FS_IO_ERROR` otherwise. The message is "cannot `verb` "path": …" and
  the `io::Error` is the source. The I/O text is Rust's, not Node's.
- **A dropped call cleans up.** Dropping a write's future removes its
  staging directory, and dropping any call frees its target's turn. dsh's
  promises can't be dropped.
- **Aborts are checked at points**, before and after each step, not inside
  a syscall as Node's `readFile` and streams can. An abort raised before
  the future is first polled is caught at the first check, which is what
  dsh's "aborted while in flight" cases amount to.
- **Name order is byte order.** dsh sorts a listing with `localeCompare`
  (ICU collation), which differs for case and accents. Rust has no ICU.
- **Paths are UTF-8.** A realpath that isn't UTF-8 fails to resolve with
  `FS_IO_ERROR`, and a listed name that isn't is read lossily. Node reads
  both lossily.
- **The diff limit** refuses only 0, with
  "fs-local: diff_basis_max_bytes must be a positive integer". dsh's other
  refusals are fractional or unsafe numbers, which a `u64` can't be, and
  V8's string-allocation ceiling, which has no Rust counterpart.
- **Test hooks are synchronous closures** (`Internals`), and the Windows
  hooks are left out. dsh's are async functions on `internals`.
- **Watching:**
  - It is `notify`, not chokidar. Notify's watch is active as soon as
    `watch` returns, so there is no `ready` to wait for.
  - Init errors are `FsError`s, not dsh's raw rejection.
  - A missing parent directory is refused by notify; I haven't checked
    what chokidar does there.
- **The seam has no plugin.** Its port is a trait that only a backend can
  provide, as with `SessionPersistence`.
- **On this machine the real-inotify watch cases skip.** This user's
  processes hold every inotify instance the system allows
  (`fs.inotify.max_user_instances`, 128), so `inotify_init` fails with
  `EMFILE` for any new process, even in a fresh user namespace. The cases
  say so and return, as dsh's socket case does where sockets are refused.
  The watch filtering is covered by the synthetic-event unit tests either
  way.
