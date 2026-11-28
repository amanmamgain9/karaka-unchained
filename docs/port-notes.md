# Port notes

Things to remember while moving Karaka and Play This Life off dsh. Each entry
says where it came from and whether it has been checked here.

Everything still undone is collected in `open-ends.md`, which names the
section here that explains each item.

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
  (Ported since, without the hooks: see "Agent inbox".)
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

- **The inbox was in memory** at this step. It is durable since, as dsh's
  is: see "Agent inbox".
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

## Agent inbox

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-26:
`core/agent-loop/src/inbox.ts` and the parts of `agent.ts` that use it
(`send`, `followup`, `steer`, `inject`, `cancel`, the pre-step `claim`, and
the turn's end), with `core/agent/src/runtime-types.ts` for the contracts.*

### What is done

- **The event.** `agent/inbox/spliced` is typed
  (`SessionEvent::InboxSpliced`): `{ target, start, removedCount?,
  inserted, outcome? }`, with `target` as `next-turn` or `next-step` and
  `outcome` as `canceled`. Before, it was a passthrough of a known type.
  Typing it is what v4 asks for: its messages are ordinary user messages
  (`session-format-v3-to-v4/src/developer.ts`), and it isn't
  surface-eligible, so a `surfaceOp` on it is refused, as on the other
  lifecycle events.
- **The inbox** is the `inbox` projection of `karaka-session-projection`,
  folded from those events, as in dsh. The loop's plugin now requires the
  projections port, and registers `InboxProjection` at setup.
  - Clients see `{ "next-turn": [...], "next-step": [...] }`.
  - The fold refuses a splice outside its list ("invalid inbox splice"),
    and one that would make an id pending twice ("message "id" is already
    pending"), as "invalid persisted inbox splice at session seq N".
- **The agent** (`karaka-agent`):
  - `steer` and `inject` are new on `Agent`.
  - `followup`, `steer` and `inject` now return a `Result`, as dsh's
    `send` throws. The errors are an id that is already pending, an
    invalid history, or a log that refuses the splice.
  - `Agents::open` returns `OpenError`, which adds an invalid inbox
    history to the session's errors.
- **The loop** (`karaka-agent-loop`):
  - `send` splices at the end of its list. `followup` (next-turn) and
    `steer` (next-step) wake an idle agent; `inject` (next-step) doesn't.
  - Each step claims all of next-step, and a turn's first step also the
    oldest of next-turn, before assembling, as dsh's pre-step does. The
    claimed messages are logged as `user/message`s, in that order, in the
    step's first attempt.
  - A turn goes on while a step leaves next-step non-empty, so steering or
    context that lands during a step gets another one. `max-tokens` sticks
    once a step hits it.
  - The driver runs another turn while anything is pending.
  - A cancel clears both lists durably, next-step first, as `canceled`
    splices; an empty list logs nothing.
  - Waking input sent after a cancel, while the cancelled turn winds down,
    goes to next-turn and runs next (dsh's latched wake). Input that doesn't
    wake waits.
- **Restore.** What is pending is folded from the log, so it survives a
  restore. As in dsh, a restored agent doesn't wake on a non-empty inbox:
  dsh's resume runs setup as maintenance, and only a waking send made
  meanwhile latches a wake. The next followup or steer runs the pending
  input first: its turn claims all of next-step and the oldest of
  next-turn.
- **Tests:**
  - From `agent.spec`, `loop.spec` and `inbox.spec`:
    - an idle inject is staged and runs nothing;
    - a later turn takes it first;
    - an inject during a tool is taken after the result;
    - an idle steer starts a turn;
    - steering during a tool, or during a final answer, runs another step;
    - a failed turn keeps steering until the next wake;
    - a cancel clears both queues durably, and twice adds nothing;
    - an invalid history (out of range, or a duplicate id) is refused with
      dsh's errors;
    - a pending id can't be queued again.
  - Karaka's own:
    - a followup's exact splices;
    - an inject after a cancel waits;
    - a steer after a cancel runs next;
    - the queues survive a restore without waking;
    - the fold's unit tests;
    - the typed event's JSON.

### Where it differs from dsh

- **A history that doesn't fold is refused at `open`**, with dsh's message
  and its cause as the error's source. dsh lets the agent open, and throws
  from the first read of its inbox. Every operation of such an agent would
  fail, so Karaka refuses it up front. The inbox's operations still give
  the same error if they ever meet one.
- **The fold records its failure** in the state (`invalid`, the seq and the
  cause) and folds nothing after it, where dsh's `apply` throws. A
  projection unit here can't fail.
- **A start or count that isn't a non-negative integer is refused when the
  log is read**, as JSON that doesn't fit the type. dsh's fold refuses it
  with its own message.
- **No `agent/inbox/inserted`, `claimed` or `discarded` events.** Karaka
  has no equivalent hook. `replace` and `remove`, the inbox's editing API,
  aren't ported either: nothing calls them yet.
- **No tool-call context.** dsh's tools can return additional context for
  the next step (`tool-calls.ts`, `acceptContext`), which the loop splices
  into next-step. Karaka's `ToolOutput` has no such field.
- **No `agent/pre-step` or `agent/turn-stopping` hook.** The pre-step's
  default decision is the one taken, and a turn stops when a step that
  ended it leaves next-step empty. So a turn whose first claim is empty
  ends without a step even with runtime context to show, where dsh's
  default decision would count the context snapshot as input. In Karaka
  that happens only after a cancel, which ends the turn as aborted anyway.
- **The driver is a task, not synchronous.** In dsh, a steer to an idle
  agent claims before the call returns, so a second steer right after it
  waits for the next step. Here both can land before the first claim and go
  to the same step. dsh's "starts idle steering synchronously" case is
  ported as steering that lands while the first answer streams.
- **`cancel` stops the turn even if the log refuses the clear.** dsh's
  `cancel` throws first.
- **The harness's `events()` leaves out the inbox's splices.** Each turn
  now logs two (the followup and the claim). The older loop tests read the
  turn's own events through it; the inbox tests read the full log.

## Working directory

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-26:
`session/working-directory` (`src/index.ts`, `tests/working-directory.spec.ts`,
and the directory half of `tests/admission.spec.ts`).*

### What is done

- **The crate** `karaka-working-directory`, with `WorkingDirectoryPlugin`.
  It requires the filesystem, the session store and projections, the
  system prompt and the agents, and provides the `WorkingDirectory` port.
  The binary mounts it after fs-local.
- **Config.** `default_directory` must be absolute ("working-directory:
  defaultDirectory must be absolute"). When unset, it is the process's
  directory.
- **The `workingDirectory` projection.** Its initial state is the header's
  `cwd`, or `None`. Each `working-directory/change { cwd }` sets it, and
  clients see the state itself.
- **The context.** `working-directory:current` is required and literal, at
  `ContextOrder::WorkingDirectory` (100). Its text is `Current working
  directory: "<dir>".`, the directory as a JSON string. It is empty for an
  assembly with no agent.
- **The listener.** It is added before every other listener (the new
  `SystemPrompt::on_assemble_first`, dsh's `{ prepend: true }`). It calls
  `ensure`, runs the rest of the chain, then overwrites the context, or puts
  it back first, so a later listener can't drop or change it.
- **`get`, `ensure` and `set`:**
  - `get` reads the projection, or the default.
  - `ensure` checks the directory with the filesystem's `resolve` and
    `stat`. If it is gone, it returns to the header's project or the
    default. A session whose state is still `None` (no project in its
    header) gets the directory recorded.
  - `set` resolves relative to the current directory and takes the
    filesystem's `process_path`.
  - All keep dsh's messages: "working-directory: cd must not be empty" and
    "working-directory: directory does not exist: "path"".
- **Commits.** A commit logs the change, then injects the notice, with
  dsh's two texts, as a `{ kind: "working-directory" }` user message. An
  unchanged directory commits nothing. A notice that can't be queued is
  warned of, and the change stands.
- **Serialization and cancellation.**
  - Each session's operations take turns, first come first served.
  - Each races the caller's signal and the service's stop, and is dropped
    by either, so it commits nothing.
  - `stop` cancels what is pending, waits until nothing runs, and later
    calls are refused with "working-directory service disposed".
- **System prompt.** `PromptError::new` is public, so a listener in another
  crate can fail an assembly.
- **Tests.** Every case of `working-directory.spec` but the two below, and
  the admission case.
  - The admission case runs as a change between two attempts of one
    request, made by a request-error handler. The retried attempt shows the
    new directory, the notice gets its own step, and the turn completes.
  - A real `karaka ask` request now carries the directory in its runtime
    context (`crates/karaka/tests/cli.rs`).

### Deferred

- The admission case's file-policy and approval lines, which need
  `dsh-sandbox-policy` and `dsh-user-approval`.
- The `working_directory` tool, a plugin of its own, and its case. (Ported
  since: see "The working_directory tool".)
- "Refuses an Agent disposed during directory inspection": Karaka's
  agents live as long as the runtime, so there is no fiber to assert active.

### Where it differs from dsh

- **The agent has no `session`.** The service finds the session in the
  store by the agent's id, and refuses an agent with none.
- **The assembly context has no agent.** The listener opens the agent of
  the assembly's scope through `Agents`. A scope is an agent's id.
- **No signal from the assembly.** The listener's `ensure` takes none,
  since Karaka's assembly context carries none.
- **A cancel has no reason.** Karaka's `AbortSignal` carries none, so a
  cancelled operation is "working-directory: cancelled", where dsh rejects
  with the signal's reason.
- **Stopping drops what is pending** rather than waiting for the
  filesystem call under it to return. Either way nothing commits once the
  stop starts. `stop()` stands in for dsh's fiber disposal, since the
  kernel unloads no plugin.
- **Warnings go to a sink**, `with_warnings`, which is stderr by default.
  dsh uses `ctx.logger.warn`. The error in the text is Rust's Display
  ("inbox unavailable"), where dsh's `String(error)` adds "Error: ".
- **A notice's id** is `working-directory-<seq>`, after the change's seq.
  dsh's is random.
- **The projection's state isn't checked as absolute** when read from a
  checkpoint, as dsh's `stateSchema` checks it. A change whose `cwd` isn't
  a string is ignored rather than folded.
- **The admission case**, without dsh's pre-step and `prepareCall` hooks,
  changes the directory between attempts. So the failed attempt's snapshot
  (the project) stays in history, superseded by the next attempt's.
- **Absolute means starting with `/`**, as Karaka targets Unix.

## The working_directory tool

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-26:
`session/tool-working-directory/src/index.ts`, with the tool layer's rules
from `core/tools/src` (`schema.ts`, `json-schema.ts`, `index.ts`).*

### What is done

- **Tools know their caller.** `ToolRun` has `agent:
  Option<Arc<dyn Agent>>`, as dsh's `ToolExecutionInput` has an optional
  `agent`. `ToolRegistry::execute(call, agent)` passes it on; a call made
  outside any agent passes `None`.
  - The loop's driver makes one `Arc<dyn Agent>` handle on its own inner
    state, held as long as the driver runs, and passes a clone to each tool
    call. Nothing inside the agent holds the handle, so there is no cycle,
    and a call's clone is dropped with the call.
  - Existing tools ignore the new field and behave as before.
- **The tool** is the new crate `karaka-tool-working-directory`, registered
  by `WorkingDirectoryToolPlugin`, which requires the tool registry and the
  working-directory service. The binary mounts it after working-directory.
  - It has dsh's name, description and `cd` description. Its parameters are
    what dsh's `defineTool` compiles `{ cd }` to:
    `{"type":"object","properties":{"cd":{"type":"string","description":…}}}`.
    That is an open object, with no `required` and no
    `additionalProperties`, so other keys are ignored.
  - With no `cd` it calls `ensure`; with `cd`, `set`. The result is the
    directory, as dsh's `render` gives it.
  - The error results follow dsh's tool layer, which shows a thrown error
    as `Error: <message>` (`toolErrorResult`):
    - a `cd` that isn't a string, `null` included, is
      `Error: invalid arguments: "cd" must be a string`, the `ToolArgsError`
      of `validateArgs`, which the registry now gives (see "Argument checks
      in the tool registry");
    - a call with no agent is
      `Error: working_directory requires an agent Session`;
    - the service's failures are its own messages after `Error: `.
- **Tests:**
  - the deferred `working-directory.spec` case, without its unregistering;
  - the schema exactly as dsh's;
  - the argument and failure results;
  - the registry passing the agent, and holding it no longer than the call;
  - end to end: a model calls `working_directory` with `cd`. The change is
    logged, the tool's result is the directory, the notice reaches the
    next step's request as a user message after the tool result, and that
    step's runtime context shows the new directory, where the first
    request's showed the project.
  - The `ask` CLI case now expects the tool in its request.

### Where it differs from dsh

- **No unregistering.** dsh's registration is undone when its plugin
  unloads. The kernel unloads no plugin, and the registry has no removal,
  so the tool stays for the runtime's life. The deferred case is ported
  without that part.
- **No signal.** dsh passes the call's signal to `ensure` and `set`. Here
  a cancelled call is dropped. The service's commit has no await in it, so
  a drop can't land between the check and the change.
- **Only `cd` is checked.** Karaka's registry has no schema validation, so
  the tool checks its one argument itself, with dsh's wording. There is no
  output-schema check.
- **The agent is a handle, not the agent the loop registered.** The
  `Arc<dyn Agent>` a tool gets wraps the same agent state as the one
  `Agents::open` gives, but it is another `Arc`. Tools that compare agents
  by identity would need the id.

## The todo_write tool

*Ported from dsh tag `dsh-v0.2.1-rc.2`, 2026-11-26: `todo/tool-todo`
(`src/index.ts`, `src/types.ts`, and the `tool-todo`, `projection`,
`integration` and `loader-composition` specs), with the tool layer's
schema compiler and validator from `core/tools/src`.*

### What is done

- **The crate** `karaka-tool-todo`, with `TodoToolPlugin`. It requires the
  tool registry, the session projections and the session store, and
  registers the `todos` projection and the `todo_write` tool.
- **Config.** `TodoConfig { allow_parallel_in_progress }` has no default,
  so the deployment chooses, as dsh's required config does. The binary sets
  it to `true`, as every dsh bundle does (`bundle/base/cordis.patch.yml`,
  and the web-app presets `standard`, `cordis` and `ptc`).
- **The projection.** `todos` is at state version 2. It is `None` at
  first. A `todo/write` sets the whole list, the next `turn/start` clears it
  (a `turn/end` keeps the finished list in view), and every other event
  leaves it. Clients see the state.
- **The tool:**
  - It has dsh's name, and dsh's description: the head, then the parallel
    or the single clause, then the tail.
  - Its parameters are exactly what dsh's `defineTool` compiles. Each
    `required: true` becomes its parent's `required` list, after
    `properties`. The item keeps `additionalProperties: false`, before
    `properties`. Annotations follow `type`, and `enum` follows them. The
    root stays open.
- **Execution follows dsh's order:**
  1. The schema check, which dsh's registry makes before any typed tool
     runs, and Karaka's registry now makes too (see "Argument checks in the
     tool registry"), with the validator's wording and walk order: an object's
     missing keys, then each declared property, then undeclared keys,
     joined by `; ` after `invalid arguments: `. For example:
     `Error: invalid arguments: missing required property "todos[0].content"; "todos[0].status" must be a string; "todos[0].extra" is not a declared property (additionalProperties: false)`.
     A bad status gives
     `"todos[0].status" must be one of ["pending","in_progress","completed"]`.
  2. `toTodoList`'s rules, with dsh's texts: trimmed non-empty content, no
     duplicate content, and at most one `in_progress` unless parallel work
     is allowed.
  3. The agent check: "todo_write requires an owning agent session".
  4. The append of `todo/write { todos }` to the agent's session, found in
     the store by the agent's id.

  The result is "Updated todo list: N pending, N in progress, N completed."
  Every error is `Error: <message>`.
- **The event.** `todo/write` is in v4's known types, and v4 doesn't check
  its payload, leaving it to its plugin. So it is stored as a passthrough,
  as `working-directory/change` is, with `TodoItem` as its shape. It isn't
  surface-eligible, and adds no message.
- **Tests:**
  - every `tool-todo.spec` and `projection.spec` case but those below;
  - both `integration.spec` cases;
  - the config cases of `loader-composition.spec`, through `TodoConfig`;
  - a loop case: the model calls `todo_write`, the write lands between
    the call's start and its result, the projection holds the list, and
    the next turn's `turn/start` clears it;
  - the schema to the byte, the validator's wording and order, and the list
    rules as unit tests;
  - the `ask` CLI case now expects `todo_write` in its request.

### Left out

- **`presentCall`.** Karaka has no presentation layer.
- **Unregistering when the plugin unloads** (`tool-todo.spec`,
  `projection.spec`). The kernel unloads no plugin.
- **`loader-composition.spec`'s Cordis parts:** the cordis.yml boot, the
  namespace-export shape, and the missing or non-boolean flag, which a
  Rust `bool` field can't be.
- **"Isolates the log from later mutation".** An owned Rust value can't be
  mutated through the log.

### Where it differs from dsh

- **The output schema isn't checked,** and no structured `value` is returned,
  only the rendered text, which is all Karaka's `ToolOutput` carries.
- **A session the store doesn't hold** for the calling agent is
  `todo_write: no session "id"`. dsh's agent always carries its session.
- **A `todo/write` whose `todos` doesn't read as a list** leaves the
  projection unchanged. dsh folds whatever the event holds; storage
  doesn't check it in either.

## User questions

`karaka-user-questions` ports dsh's
`packages/interaction/user-questions`: `types.ts`, `projection.ts`,
`timed-wait.ts` and `index.ts`. The model-facing tool and the binary's
answerer are in "The ask_user_question tool".

### What is done

- **The types**, in dsh's camelCase JSON, with absent optionals left out:
  questions, options, the plan-review intent, answer batches, pending and
  settled questions, the projection's view, and the timed result
  (`{pending: true, callId}` or the batch). The late reply's source is
  `{kind: "user-question-reply", callId, outcome: "answered"}`.
- **`ask`**, with dsh's order and texts:
  1. ASK_ABORTED for a signal that already fired;
  2. EMPTY_QUESTIONS;
  3. CALLER_NOT_LIVE;
  4. both BAD_INTENT checks;
  5. the answerer chain.

  A failure in the service's terms passes through. Any other becomes
  ASK_ABORTED, with it as the cause, when the signal has fired, and
  passes through otherwise.
- **The answerer chain**, dsh's `user-questions/request` waterfall:
  - A `QuestionAnswerer` gets the request and an `AnswerNext`, and answers
    or runs the rest.
  - An answerer is global, or scoped to one agent by its session id, as
    the system prompt's listeners are. An agent's request runs the global
    ones and its own, in registration order; a request with no agent runs
    only the global ones.
  - The end of the chain fails with NO_PROVIDER.
  - The cancel is `karaka-fs`'s `AbortSignal`. The agent seam's
    `Cancellation` needs a `CancelCause`, which a question's signal
    doesn't have, so the crate depends on `karaka-fs` for it.
- **`ask_timed`** and `TimedQuestionWait`:
  - BAD_TIMEOUT is for 0 or more than 2,147,483,647 ms. The window is a
    `u64`, so dsh's non-integer and negative cases can't be written.
  - DUPLICATE_WAIT is per (agent, call).
  - The chain gets the wait's own signal and `wait {callId, timed: true}`.
  - An answerer's ASK_TIMED_OUT, or the window closing with no claim,
    gives the pending result. With no answerer the window is waited out.
  - The turn's signal, or `close`, gives ASK_ABORTED.
  - The wait is closed and forgotten however the ask ends, including when
    its future is dropped.
- **`attach_wait`** is a plain method returning a `Stream` of one
  `{remainingMs}` frame. While a claim is held the host doesn't count the
  window down; when the last claim leaves, the countdown resumes toward the
  original deadline.
- **The `userQuestions` projection**, version 2, with dsh's rules for
  timed headers, open and continued calls, settling, dropping, PTC
  sub-calls and late replies. Events before the inherited count are
  skipped. The restored state is strict, refuses duplicate or empty call
  ids ("callIds must be unique"), and requires at least one question.
  `TIMED_WAIT_PARAMETER`, `is_timed_ask_user_question_schema` and
  `questions_of` are public, for the tool.
- **`answer`**:
  - CALLER_NOT_LIVE first.
  - A call that isn't continued gives `false`.
  - REPLY_QUEUED when the service has a reply queued, or the durable inbox
    holds one, read from the `inbox` projection's view.
  - BAD_ANSWER unless the batch names each question exactly once.
  - The reply's text is dsh's JSON in dsh's key order:
    `{kind: "answer_to_pending_question", tool: "ask_user_question",
    callId, questions, answers}`. It is steered into the agent.
  - A refused steer releases the reservation, so the reply can be sent
    again.
- **The queued-reply bookkeeping**, from dsh's listeners:
  - A claim notes its turn, or reserves a reply restored with the inbox.
  - A discard releases the reply.
  - The reply's `user/message` releases it.
  - A `turn/end` releases what that turn claimed.
- **The inbox signals.** `karaka-agent` has a new port, `InboxHooks`, with
  an `InboxListener` trait. The loop provides it beside
  `RequestErrorHooks`, now bundled with it in `LoopHooks` for
  `AgentLoop::new`.
  - `claimed(agent, message, turn)` fires for each message a step takes,
    next-step first, after the claim is logged and after `turn/start`. Its
    agent is the driver's handle.
  - `discarded(agent, message)` fires for each message a cancel's clear
    drops, next-step first.
  - Both fire just after the inbox mutation, once the agent's lock is
    released. dsh fires them inside it.
  - `inserted` has no listener here, so it isn't ported.
- **The agent's identity.** `Agent::instance()` and `Agents::get(id)` are
  new. The loop's agent gives the address of its shared core, so the
  tool-call handle and `Agents::open`'s handle compare equal, and a stale
  agent reusing a live id doesn't.
- **The plugin** provides `UserQuestions` and registers the projection. It
  requires the session store, the session projections, `Agents` and
  `InboxHooks`.
- **Tests:**
  - all 16 `projection.spec` cases;
  - all 6 `timed-wait.spec` cases, on Tokio's paused clock, plus one for a
    single end;
  - `user-questions.spec` but what is listed below;
  - Karaka cases: scoped answerers, `attach_wait` and `answer` from a
    stale agent, and a run through the real loop. In that run a stand-in
    timed tool asks, nobody answers in the window, a late answer is steered
    in, and the next turn admits and settles it.
  - two loop cases: the inbox hooks hear claims and discards, and `get`
    finds live agents.

### Left out

- **DELEGATED_CALLER.** Karaka has no child agents, so every live agent is
  a root and the check isn't made. The code constant is kept for
  answerers. dsh's "rejects a live runtime-owned agent" case isn't ported.
- **The typert wire:**
  - the transported-error restore and its tests ("restores a transported
    provider rejection", "restores a transported UserQuestionError");
  - the namesake and non-`Error` rejection cases;
  - `@Remote`.
- **"Answers nothing when the composition carries no Session
  projections".** The plugin requires them.
- **HMR disposal.** The kernel has no teardown, so dsh's disposal is
  `UserQuestions::close`. A consumed registration can't be removed twice.
- **The timer-count checks** of the timed-wait and askTimed specs. Tokio
  has no counterpart; a wait's single end covers them.
- **The seeded header with `tools: "{{tools}}"`** in projection.spec.
  Karaka's typed header refuses it when it is read.

### Where it differs from dsh

- **Errors.** dsh's `UserQuestionError` and any other rejection are
  `QuestionError::Question` and `QuestionError::Other` here.
- **A parent cancel has no reason.** The signal is cause-less, so an ask
  ended by the turn's cancel is ASK_ABORTED with no cause, where dsh's
  cause is the turn's reason.
- **Claim release timing.** A claim is released when its stream is next
  read or dropped after its signal fires. dsh releases it on the abort
  event itself.
- **Bookkeeping keys.** Queued replies are kept by session id, where dsh's
  `WeakMap` is keyed by the session object. Waits are kept by agent
  instance and call.
- **The reply's message id** is `user-question-reply-{callId}-{len}`,
  after the session's length. dsh's is random.
- **`AskUserQuestionItem` accepts `intent` when restored.** dsh's restore
  schema leaves it out, but the fold never records one.

## Argument checks in the tool registry

dsh's validator, `tools/src/json-schema.ts`, is now `karaka-tools`'
`json_schema.rs`, and the registry runs it before every checked tool, as
dsh's `defineTool` does.

### What is done

- **The whole enforced subset.** `check_schema`
  (`assertSupportedJsonSchema`) and `check_object_schema`
  (`assertObjectJsonSchema`) accept and refuse exactly what dsh's do, with
  its texts, and report every violation in dsh's walk order. The subset
  covers:
  - any JSON root, and annotation-only nodes;
  - one scalar `type`;
  - `properties`, `required` and a boolean `additionalProperties` on
    objects;
  - `items` on arrays;
  - type-correct scalar `enum` and `const`;
  - exact-one `oneOf` with at least two branches.

  Any other keyword is refused, not ignored.
- **The validator.** `validate` (`validateJsonSchemaValue`) has dsh's
  wording. Its order is an object's missing required keys, then each
  declared property in declaration order, then its undeclared keys when it
  is closed. An empty path names the root `arguments` and its properties
  bare, as `validateArgs` does. `integer` follows `Number.isInteger`, so
  `1.0` is one. Enum and const compare numbers by value, as `===` does.
- **The registry:**
  - `register` parses a checked tool's parameters and checks them against
    the subset, refusing the tool otherwise: `tool "x": unsupported JSON
    schema: …`, or `tool "x" parameters are not JSON: …`. dsh's
    `defineTool` refuses such a tool when it is defined.
  - `execute` checks the arguments after the object check and before the
    tool, giving
    `Error: invalid arguments: <violations joined by "; ">`.
- **The escape hatch.** dsh validates only tools made with `defineTool`. A
  raw `ToolDefinition`, such as an MCP tool, goes unchecked, and its
  parameters can be any JSON Schema. Karaka's `Tool` trait has
  `registry_checks_arguments()`, which defaults to `true`. A tool returns
  `false` to have its schema sent as it is and its arguments left
  unchecked.
- **The tools' own checks are gone.** `todo_write` loses `schema.rs`, and
  `working_directory` loses its `cd` check. Their tests, which go through
  the registry, are unchanged and pass. `current_time`'s
  `{"type":"object","properties":{}}` is in the subset.
- **Tests:**
  - `json-schema.spec`, where a case fits plain JSON;
  - the `validateArgs` cases of `tools.spec`;
  - the two `defineTool validation` result cases;
  - a raw tool's unchecked arguments, and a checked tool refused at
    registration.

### Left out

- **What JSON can't hold.** dsh's checks for `undefined`, `NaN`,
  `Infinity`, `-0`, sparse or decorated arrays, forged or other-realm
  prototypes, proxies, getters, symbols, cycles, `Date` and `Map` have
  nothing to check in a `serde_json::Value`. Their cases aren't ported.
- **The 5,000-deep `oneOf` cases.** dsh walks with explicit stacks. Here
  the walk recurses as deep as the schema, which a tool's author writes,
  not the model.
- **The structured error.** dsh puts `{ name: "ToolArgsError", code:
  "INVALID_ARGS" }` on the result. Karaka's results carry text only; see
  that open end.

### Where it differs from dsh

- **A raw tool still gets an object.** The registry refuses arguments
  that aren't a JSON object for every tool. dsh hands a raw tool whatever
  parsed, and MCP's adapter falls back to `{}`.
- **The schema is checked when the tool is registered,** not when it is
  defined. Rust tools have no `defineTool`.

## The ask_user_question tool

`karaka-tool-ask-user` ports dsh's `packages/interaction/tool-ask-user`
(`index.ts` and `timed.ts`). The binary mounts it, with an answerer that
asks on the terminal.

### What is done

- **The config.** `AskUserConfig { mode, timeout }` defaults to the legacy
  tool and 120 seconds. `timeout` is -1 or 1 to 2,147,483 seconds, and the
  timed tool checks it when it is made, with dsh's `validateTimeout` text.
- **Both tools.** They have dsh's descriptions and its `pendingNotice`.
  Their parameters are byte for byte what dsh's `defineTool` compiles:
  - `type`, then the annotations, then `additionalProperties`, then
    `properties`, then `required`;
  - the root is open;
  - the timed tool adds `timeout`, described with its default filled in.
- **Timed execution**, in dsh's order:
  1. The registry's argument check. `1.5` gives `"timeout" must be an
     integer`.
  2. `validateTimeout`.
  3. `validateQuestionIds`.
  4. With a window, `ask_timed`, which needs an agent: "timed questions
     require a live agent". A pending result is `{pending, callId,
     message}`.
  5. With -1, `ask` with `wait {callId}`, not timed, and the agent when
     there is one.
- **Legacy execution** is `ask`, with the agent when there is one and no
  wait.
- **Results.** A result is the JSON text of the value, as dsh's `render`
  gives it. A failure is `Error: <message>`, as dsh's tool layer gives a
  thrown error. That includes the service's errors, such as `Error: no
  user-questions answerer accepted the request`.
- **The plugin**, `AskUserPlugin`, requires the tool registry and
  `UserQuestions`.
- **The binary mounts the legacy tool.** dsh's only bundles that mount
  `tool-ask-user` are the web-app presets (`cordis.patch.yml`,
  `standard.patch.yml`, `ptc.patch.yml`), and they give it no config. The
  CLI app takes its composition from them. So the late-reply path isn't
  needed on the terminal, and isn't wired (see open ends).
- **The terminal answerer** (`karaka/src/questions.rs`) writes each
  question to stderr, so stdout stays the answer:
  - its place when there are several, `[header]`, the question, the
    detail, the numbered options with their descriptions, and how to
    answer.

  It reads one line per question:
  - a number picks an option;
  - for a multi-select question, numbers separated by commas pick each,
    in order, once;
  - any other line, including a number that names no option, is `custom`;
  - an empty line skips the question.

  At the end of stdin it fails with `ASK_ABORTED`, and the model reads
  that as the tool's error. `UserQuestionError::aborted` is now public for
  it.
- **Stdin has one reader.** `Lines` reads on a thread of its own, and
  only when asked for a line. `chat` asks for its next message only while
  the agent is idle, and a question asks only during a turn, so while a
  question is open the next lines answer it. Nothing is read ahead, so a
  piped stdin splits the same way, and `printf '1\n' | karaka ask "…"`
  answers with option 1. A line read for a caller that stopped waiting is
  kept for the next one. The thread isn't the runtime's, so an exit never
  waits on a read.
- **Tests:**
  - `tool-ask-user.spec`, but for what is listed below, with the schema
    compared to the byte;
  - the terminal answerer's rendering and parsing, and `Lines`, as unit
    tests;
  - three CLI cases: chat routes the line after a question to it and the
    next line to the chat, `ask` answers from piped stdin, and the end of
    stdin aborts the question;
  - the `ask` CLI case expects the new tool in its request, first, in
    the registry's order.

### Left out

- **"passes the tool abort signal".** A Karaka tool has no signal; its
  future is dropped instead (see open ends).
- **The DELEGATED_CALLER case.** There are no child agents.
- **Unregistering when the plugin unloads.**
- **The structured `error.info`** (`UserQuestionError` and its code). A
  Karaka result is text only. The text is checked instead.

### Where it differs from dsh

- **An option's undeclared keys are dropped** on their way to the service.
  dsh copies options as they are, and the service's option type here has
  only `label` and `description`.
- **The CLI prints a step's text on a line of its own**, so an `ask` whose
  model asked a question prints a blank line before the answer. That is
  the printer's behaviour after any tool call.

## User approval

`karaka-user-approval` ports dsh's `packages/interaction/user-approval`
(`types.ts` and `index.ts`). The `tools/pre-execute` gate that asks it is
"Approvals, step B".

### What is done

- **The types:**
  - `ApprovalOutcome`: `allowed-once`, `rejected`, `cancelled` and
    `unavailable`.
  - `ApprovalRequestId`.
  - `ApprovalPolicy`: `ask` or `never`. A string becomes one through
    `FromStr` or serde, with dsh's `approval policy must be one of "ask"
    or "never"`.
  - `ApprovalRequest`: the agent, tool name, call id, reason, display
    reason and signal.
  - The three events' shapes: `ApprovalAsked`, `ApprovalDecided` and
    `ApprovalPolicyChange`, the last with an optional `source:
    "delegation"`.
- **The events stay passthroughs.** The session crate already lists
  `approval/asked`, `approval/decided` and `approval/policy` among v4's
  known types, and keeps every plugin type it doesn't model as a
  passthrough, as `todo/write` and `working-directory/change` are. So the
  service writes them through serde structs, in dsh's key order, with no
  surface op.
- **`request`**, in dsh's order:
  1. Refuse outside an open turn, with dsh's message, before anything is
     logged.
  2. Log `approval/asked` `{id, toolName, callId?, reason?}`.
     `displayReason` goes to the answerers and is never logged.
  3. Decide: an aborted signal gives `cancelled`; then `never` gives
     `rejected`, before any answerer; then the chain, raced against the
     signal.
  4. Log `approval/decided` `{id, outcome}`.
- **The answerer chain** is dsh's `approval/request` waterfall:
  - `ApprovalAnswerer::answer(request, next)` answers or hands on.
  - Answerers are global, or scoped to one session id, in registration
    order, as `karaka-user-questions` does them.
  - The end of the chain is `unavailable`.
  - An answerer that returns an error, panics in its future, or panics
    before returning one gives `unavailable`. dsh contains a throw,
    synchronous or not, the same way.
  - dsh also turns a value outside the vocabulary into `unavailable`. A
    Rust answerer can't return one, because `ApprovalOutcome` is a closed
    enum.
- **The cancel** is `karaka-fs`'s `AbortSignal`, the type
  `karaka-user-questions` uses. A fired signal wins the race (`biased`,
  signal first), and the answerer's future is dropped, so its late answer
  has nowhere to go. A tool call has no signal to hand on (see the open
  end).
- **Policy:**
  - `set_approval_policy(session, policy)` appends `approval/policy`.
  - The effective policy is the last override, else the config default,
    `ask`.
  - `set_policy(agent, policy)` does nothing if the policy is unchanged.
    Otherwise it appends the event and injects dsh's notice: `The approval
    policy changed from "…" to "…" (changed by the user).`, with source
    `{kind: "user-approval"}`.
- **A projection instead of a log scan.** dsh scans the log backwards on
  every read, both for whether a turn is open and for the override. Here
  a host-only projection, `ApprovalStateProjection` (key `approval`),
  folds both, so neither a request nor the context line copies the log.
- **The runtime context**: `approval:policy` at a new
  `ContextOrder::ApprovalPolicy`, 115, with dsh's exact ASK and NEVER
  sentences. An assembly for no agent, or for an agent with no session,
  gets an empty string. Like dsh's, it is optional runtime context,
  neither literal nor required.
- **The plugin**, `ApprovalPlugin::new(ApprovalConfig { policy })`,
  provides `ApprovalService`. It requires the session store, the session
  projections and the system prompt.
- **Tests:**
  - `approval.spec`, but for what is listed below;
  - a restored session keeps its override;
  - working-directory's deferred approval half of `admission.spec`: the
    policy switches from `never` to `ask` between two attempts of one
    request, and the retried request shows `Approval policy: ask.` exactly
    when optional context is on, and never the `never` sentence.

### Left out

- **The two observer-throw cases.** A Karaka `SessionListener` can't fail.
- **"propagates an append failure".** A passthrough always fits the log,
  so a test can't make that append fail. The service does propagate a
  `LogError`.
- **The rogue-value case.** The enum is closed.
- **HMR disposal** of an answerer and of the context. The kernel unloads
  nothing; a removed registration stands in for the answerer case.

### Where it differs from dsh

- **The request id** is `approval-{seq}`, after the session's length.
  dsh's is a random UUID. It is still fresh per request in a session.
- **The notice's message id** is `user-approval-{seq}`. dsh's is random.
- **No session for the agent** is an error, `approval: no session "id"`.
  dsh's agent always carries its session.
- **A dropped request.** If the future of `request` is dropped
  mid-question, `approval/asked` is logged with no `approval/decided`. In
  dsh the signal ends it as `cancelled`. This waits on tool calls getting
  a signal.

## Approvals, step B: the tools/pre-execute gate

dsh's `tools/pre-execute` waterfall, its `ask` resolution (`serviceAsk`),
and the gate's place in `prepareExecution` (`core/tools/src/index.ts`)
are now `karaka-tools`' `gate.rs`. The binary mounts user-approval, with a
terminal approver and two options.

### What is done

- **The decisions:** `PreToolDecision::{Allow, Deny { reason }, Cancel,
  Ask { reason, display_reason }}`. `DisplayReason` now lives in
  `karaka-tools`, and `karaka-user-approval` re-exports it.
- **The chain.** `ToolRegistry::on_pre_execute(scope, listener)` takes a
  `PreExecuteListener`, whose `decide(call, next)` decides or hands on.
  - A listener is global, or scoped to one session id, as the other
    registry ports scope theirs. A call with no agent runs only the global
    listeners.
  - The end of the chain allows.
  - A listener that fails gives `Error: <message>`, and the tool doesn't
    run.
  - A registration can be removed.
- **dsh's order** (`prepareExecution`, then dispatch):
  1. Parse the arguments: empty is `{}`, and text that isn't JSON stays a
     string, as dsh's loop hands it on.
  2. Run the gate.
  3. Resolve `ask`.
  4. `Cancel` gives `Error: tool call aborted before dispatch`, and `Deny`
     gives `Error: <reason>`.
  5. Then the unknown-name check, the object check and the schema check,
     in that order.
  6. Then the tool.

  So the gate sees every call that reaches the registry, an unknown
  name's and unfit arguments' too. This moves the argument check behind
  the gate; it used to come first.
- **`ask`** goes through `ToolApproval`, a small trait in `karaka-tools`.
  `karaka-user-approval` implements it for `ApprovalService`, and its
  plugin installs it with `ToolRegistry::set_approval`. The reasons are
  dsh's:
  - no seam: the ask's own reason, else `tool "x" requires approval (not
    yet supported)`;
  - no agent: `tool "x" requires approval, but the call has no agent to
    route it through`, without asking;
  - `rejected`: `the user rejected tool "x"`;
  - `cancelled`: `approval for tool "x" was cancelled`;
  - `unavailable`: `tool "x" requires approval, but no approval channel is
    available`;
  - an approval that fails, such as one outside an open turn: `Error:
    <message>`, as dsh's catch gives a thrown error.
- **The binary:**
  - It mounts `ApprovalPlugin`, whose context line now appears in every
    request.
  - `TerminalApprover` shares `questions.rs`'s one stdin reader. On stderr
    it shows `Allow tool "x"?`, the reason (the display reason's English
    if there is one), and `Allow it once? [y/N]`. `y` or `yes` allows
    once; anything else, an empty line included, rejects; the end of
    stdin cancels.
  - `--ask-before <tool>` (repeatable) adds a global pre-execute listener
    that asks before each call to those tools, with reason `karaka was
    started with --ask-before <tool>`.
  - `--approval-policy ask|never` sets the default policy.
  - Options come before the question, `--` ends them, and `--name=value`
    works too. A bad policy prints dsh's message and the usage, and exits
    with 2.
  - dsh's CLI has no equivalent flags. It sets approval through its
    config, `user-approval`'s `policy`, and permission presets, so the
    names follow the config key.
- **Tests:**
  - tools.spec's pre-execute and "ask routing through ctx.approval" cases,
    with a stand-in seam in `karaka-tools`;
  - three more through the real service in `karaka-user-approval`: a
    granted ask with its audit pair, `never` and no answerer, and outside
    an open turn;
  - the approver's rendering and parsing;
  - CLI cases: `--ask-before` with `y`, with an empty line and with EOF,
    the `never` policy rejecting without a prompt, chat routing the line
    after an approval to it, and a bad policy;
  - the deepseek `ask` body now expects the approval line.

### Left out

- **Tool guards** (`ToolGuard`), which run after the chain and can only
  deny. See open ends.
- **The signal checks.** dsh checks the caller's signal before the
  gate, after it, and before dispatch, and "cancellation overtakes the
  approval" gives the before-dispatch result. A Karaka call has no signal;
  dropping its future is the cancel, and nothing after the drop runs.
- **The deny `info`.** Results are text only.
- **The rogue-outcome backstop.** The outcome enum is closed.
- **The PTC collapse** that ends a call before the gate. There is no
  `run_code` yet.

### Where it differs from dsh

- **The tool is looked up when the call starts.** dsh looks it up at
  dispatch, after the gate, so a tool registered while the gate waits
  would run there. The unknown-name error still comes after the gate.
- **An empty chain skips the gate.** It would allow anyway.
- **Approval requires the tool registry.** The kernel has no optional
  port, so `ApprovalPlugin` requires `ToolRegistry` to install itself as
  the seam. dsh's registry takes `ctx.approval` when one is mounted.

## Skills, step A: the registry

`karaka-skill` ports dsh's `packages/skill/skill` (`index.ts`). The
filesystem provider and the `skill` tool are steps B and C. Its API is
shaped for them: the provider trait, the control's `invalidate` and
signal, `snapshot` with `complete`, lookups by `cwd` and scope, and
`render_skill_content`.

### What is done

- **The types**, with dsh's camelCase JSON where they serialize:
  `SkillSummary`, `SkillCandidate`, `SkillDefinition`,
  `SkillRegistration`, `SkillInvocationPolicy`, `SkillResourceBase`
  (`directory`, `url`, `opaque`), `SkillCatalogSnapshot`,
  `SkillProviderObservation` and `SkillLookupOptions`.
  - The source is a string, with dsh's buckets as constants in `source`.
  - `Invocable` gives `is_model_invocable` and `is_user_invocable` on
    summaries, candidates and definitions.
  - `is_skill_name` is dsh's `^[a-z0-9]+(?:-[a-z0-9]+)*$`, checked
    without a regex.
  - `BUNDLED_SKILL_RANK` is 600, and runtime skills rank 250.
  - `render_skill_content` and `escape_text` produce dsh's output byte
    for byte.
- **The message source.** dsh declares `skill-invocation` on dsh-llm, not
  `skill`: `{kind: "skill-invocation", name, form: "instructions"}`.
  `skill_invocation_source(name)` builds it as a `UserSource::Other`, as
  the other ports build theirs.
- **The registry:**
  - `register_provider(scope, create)` hands `create` a
    `SkillProviderControl`. Its `invalidate` works only while that exact
    registration is live, and its `signal` fires when the registration is
    refused or removed. `runtime` is reserved, and a provider name is
    unique per layer, with dsh's two messages.
  - `register(scope, skill)` adds a runtime skill. A bad name or an empty
    description is refused with dsh's text. A duplicate in the same layer
    is warned of and gets a registration that removes nothing.
  - Every change bumps the revision, drops the cache, and calls the
    `on_change` listeners (dsh's `skills/change`). A panicking listener is
    contained and warned of.
  - `list`, `snapshot` and `get` take `SkillLookupOptions { cwd, signal,
    scope }`. Providers get the same options.
- **dsh's merge rules.** Within a layer, runtime skills come first in name
  order, then providers in registration order. The lowest rank wins, then
  the earliest registration, then the provider's own order; a lower
  duplicate is warned of. Across layers, the scope's entry replaces the
  global one whatever its rank.
- **A failed provider** is skipped with dsh's warning, and the catalog is
  incomplete and uncached. A candidate that breaks the contract (bad name,
  empty description, non-finite rank, another provider's name) fails the
  read with dsh's text.
- **`get`:**
  - `None` for an invalid name or an unknown skill.
  - Cancellation is rechecked after the catalog, even a cached one, and
    the load is raced against the signal.
  - A load failure is `SkillError::Load` with the provider's message.
  - The loaded definition is re-validated. One whose name changed gives
    `None`, and invalidates only if its exact registration is still live.
- **The cache** is keyed by `(cwd, scope, revision)` and holds at most
  `collect_cache_max_entries`, 128 by default and at least 1, oldest out
  first. A read whose revision changed while it ran tries once more, and
  then gives its result uncached. A provider's incomplete observation is
  listed and loadable but never cached.
- **The plugin**, `SkillsPlugin`, provides `SkillRegistry`. It requires
  nothing.
- **Tests:** 35. They cover `skill.spec`'s cases, but for what is listed
  below, plus the name rule, the source, `escape_text`, the
  duplicate-name warning and the plugin.

### Left out

- **The type checks.** Candidates, definitions, invocation policies and
  provider observations can't be the wrong type in Rust, so most of
  dsh's validation and its "malformed" cases have nothing to check. What a
  typed value can still get wrong is checked and tested.
- **A factory that throws, or a failing Cordis effect.** A factory here
  can't fail, and there are no effects.
- **A hostile thrown value or abort reason.** A Rust error always
  renders, and a Karaka signal has no reason. An abort is
  `SkillError::Aborted`, "This operation was aborted", the web default.
- **A rejecting async change listener.** Listeners are synchronous.
- **Scope chains.** dsh's scope can have a parent (an agent inherits its
  preset's layer), and a recompose re-parents it. Karaka's scope is a
  session with no parent, so a view is the global layer plus that one.
  This waits on presets (see open ends).
- **HMR disposal.** It is a registration's `remove` here.

### Where it differs from dsh

- **Warnings are kept, not logged.** There is no logger, so the registry
  keeps its last 256 warnings for `warnings()`. A provider failure's text
  is `Error: <message>`, as dsh's `String(error)` gives for an `Error`.
- **Registration takes the scope as an argument.** dsh files it by the
  calling context's scope.
- **A provider that panics** isn't contained, unlike a failing one: a
  panic is a bug.
- **Runtime skill bodies** are cloned when loaded. dsh hands back the
  registered object itself.

## Skills, step B: skill-filesystem

`karaka-skill-filesystem` ports dsh's `packages/skill/skill-filesystem`
(`index.ts`). It is a crate of its own, not a module of `karaka-skill`, for
two reasons. dsh keeps it a package apart, and the registry stays free of
the filesystem seam and the YAML parser.

### What is done

- **The roots, ranks and sources are dsh's:**
  - the project's `.dsh/skills` (100, `project-dsh`) and `.agents/skills`
    (200, `project-agents`), from the nearest directory up from the
    lookup's `cwd` holding `.git`, else the `cwd` itself;
  - each custom directory (300, `custom`);
  - `~/.dsh/skills` (400, `user-dsh`), with its `.system` skipped, and
    `~/.agents/skills` (500, `user-agents`);
  - the bundled root (600, `bundled`).

  `include_default_roots: false` drops the project and user roots and the
  environment's bundled root. The homes come from the config, then
  `$DSH_HOME` and `$DSH_AGENTS_HOME` (blank counts as unset), then
  `~/.dsh` and `~/.agents`, with `~` expanded. dsh's environment names are
  kept.
- **Discovery:**
  - A root's entries are listed through the filesystem seam. A directory
    is a bundle, `<name>/SKILL.md`, with that directory as its resource
    base. A `.md` file is a flat skill, with the root as its base.
  - A candidate's `path` is the file's real path (`process_path`), and its
    base is the path as listed, so a linked skill keeps its link's root.
  - A missing root is empty. Any other failure fails the provider's
    listing, which the registry skips as incomplete.
- **Reading** follows dsh's filesystem path:
  - resolve, stat, then read;
  - absent, or not a regular file, is `None`;
  - a file that isn't text is `None`, with dsh's warning;
  - anything else is an error;
  - the lookup's signal reaches `stat` and `read_text`.
- **Frontmatter**, exactly as dsh splits it:
  - a first line of `---`, YAML, and a closing `---` line, `\r\n` or
    `\n`;
  - YAML that isn't a mapping counts as missing;
  - the body is trimmed.

  The YAML parser is `yaml-rust2` (=0.13.0, a YAML 1.2 parser, as dsh's
  `yaml` is), turned into JSON with a mapping's keys as strings.
- **Validation** has dsh's warning texts: invalid YAML, missing
  frontmatter, `frontmatter requires name and description`, an invalid
  name, and invalid invocation frontmatter.
  - `disable-model-invocation` and `user-invocable` take dsh's spellings:
    a boolean, `1`/`0`, and `true`/`yes`/`on`/`false`/`no`/`off` in any
    case.
  - The legacy keys are refused.
  - `whenToUse` and a `metadata` mapping are read.
  - The invocation errors carry dsh's `Error:` or `TypeError:` name, as
    dsh's `String(error)` gives them.
- **Watching:**
  - Each root is watched through the filesystem seam: the root and each
    of its directories, or, while it is missing, its nearest existing
    ancestor.
  - Every change re-plans the root's watches and invalidates the catalogs,
    coalesced into one invalidation per burst.
  - A watch failure is warned of (`skill-filesystem: watcher for … failed:
    …`), and the root is watched again.
  - A watch that won't start is warned of (`skill-filesystem: failed to
    watch …: …`) and makes the listing incomplete, so it isn't cached and
    is tried again.
  - Project roots are held least-recently-used, at most
    `watch_max_projects` (128). An evicted project's watches close and
    the catalogs are invalidated.
  - The registration's end, or `dispose`, closes everything. Later events
    and observations do nothing.
- **`observe_host_mutation(path)`** invalidates at once when the path may
  be a skill file under a held root. dsh calls it on `fs/observed` after
  a model's write or edit.
- **Warnings go through the registry.** `SkillProviderControl` gains
  `warn(message)`, which keeps the message among the registry's warnings,
  since there is no logger. This is Karaka's own addition: dsh's providers
  log through their context.
- **The plugin**, `SkillFilesystemPlugin`, checks the config, then
  registers the provider globally. It requires the skill registry and the
  filesystem.
- **Tests:** 21 integration and 2 unit tests.
  - dsh's `TestFileSystem` is `Fake`, over the local filesystem, with
    injected failures and a scripted watch, so the watcher cases run
    without inotify.
  - One case watches the real filesystem, and skips where the system has
    no inotify instances left, as it does here.

### Left out

- **Chokidar's tunables:** polling, the write-stability threshold, the
  poll interval and symlink following. The seam's watch has none of them.
  A watch here follows the link to a linked skill directory, since it
  watches the listed target's real path.
- **Event filtering.** A Karaka watch says only that something changed, so
  any change to a watched directory invalidates, where dsh ignores
  non-skill files.
- **The trusted bundled root.** dsh reads it with Node's `fs`, past
  `ctx.fs`. Here every read goes through the seam.
- **dsh's native path** (Node's `fs` when no `ctx.fs` is mounted), and
  its symlink warning. Karaka always has the seam.
- **`fs/observed`,** which waits on the filesystem observation policy (see
  open ends).
- **Per-preset mounting.** Play This Life mounts the provider inside each
  preset, with `includeDefaultRoots: false`, `customSkillDirs: []` and
  guide SKILL.md files. Presets don't exist yet, so the plugin registers
  globally. `FileSystemSkillProvider` can be registered with a session
  scope by hand.
- **Spec cases:** the Cordis metadata case; the no-filesystem variant of
  the linked-skills case; the native read failure; and the watcher spec's
  Chokidar options, event filtering, close failures and disposal races.

### Where it differs from dsh

- **Entries sort by bytes**, as the seam lists them, where dsh uses
  `localeCompare`. See the listing-order open end.
- **The YAML parser's error text** differs from the `yaml` package's.
  The warning's prefix is dsh's.
- **A missing root's ancestor** is watched for its direct entries. dsh
  polls the next path down with `watchFile`.

## Skills, step C: the skill tool

`karaka-tool-skill` ports dsh's `packages/skill/tool-skill` (`index.ts`).
It gives the model three things, all looked up as the calling agent sees
the registry, from its working directory and with its session as the
scope:

- the `skill` tool, which loads one skill's full instructions;
- a catalog of the model-invocable skills, added to a step's messages;
- `/name` invocations, which load a skill a person names in a prompt.

The last two are listeners of dsh's `agent/pre-step` hook, which Karaka
lacked, so this step ports that hook first.

### The pre-step hook

`karaka-agent` gains `PreStepHooks`, a registry port like
`RequestErrorHooks` and `InboxHooks`, and the loop provides it. (dsh: the
`agent/pre-step` waterfall, `agent/src/runtime-types.ts` and the loop's
`preStep` in `agent-loop/src/agent.ts`.)

- Before each step, after the step claims its input and its prompt is
  assembled, the loop proposes a `PreStep`: the agent, the claimed
  messages, the turn, the step and the turn's cancel switch.
- Each listener gets the proposal and a `PreStepNext`, usually runs the
  rest of the chain first, and changes what it answered. The end of the
  chain enters with the claimed messages.
- Listeners are global or scoped to one session, and run in the order
  they were added, for the runtime's life.
- `PreStepDecision::Reject` ends the turn as `blocked`, with no step. A
  listener that fails ends the turn as an error, or as aborted if the
  turn was cancelled meanwhile. A panic ends it as an error too.
- The loop stops waiting for the chain the moment the turn is cancelled.

### The tool

- **Its name, description and parameters are dsh's,** as `defineTool`
  compiles them: one required string, `name`.
- **A call runs dsh's steps in order:**
  1. a name that isn't a skill name is `invalid skill name "X"`;
  2. the lookup's `cwd` is the agent's working directory (`ensure`), and
     its scope is the agent; a call with no agent looks up with neither;
  3. a name the list doesn't hold is `skill "X" is unknown or no longer
     available`;
  4. a listed skill models may not invoke is `skill "X" is not available
     for model invocation`, before its body is loaded;
  5. the body is loaded, and checked again: gone is "unknown", and
     model-disabled is "not available for model invocation".
- **The result is the skill's `<skill_content>` block,** rendered by
  `render_skill_content`, as dsh's `render` does. It goes back to the
  model as the tool's result, and stays in the history like any other
  result.
- An error is `Error: <message>`, as dsh's tool layer shows a thrown
  error.

### The catalog

The catalog is a user message, not a system-prompt section, as in dsh.
Before each step the catalog listener:

1. runs the rest of the chain, and passes a reject through;
2. takes a snapshot of the registry for the agent; while any provider's
   discovery is incomplete, it changes nothing, so the last good catalog
   stands;
3. makes the entries: each model-invocable skill's name, and its
   description with whitespace runs folded to one space, trimmed, and cut
   to `catalog_description_max_length` (500) with `...`;
4. reads the session's history, newest first, for readable `skill-catalog`
   messages: whether any was ever published, and the entries of the
   newest one still on the surface;
5. compares them, as dsh does:
   - the visible catalog matches: a matching catalog already among the
     step's messages is dropped;
   - a catalog among the step's messages matches: it is kept;
   - nothing was ever published and there are no entries: a stale
     catalog among the step's messages is dropped, and nothing is added;
   - otherwise a catalog is published, the first one or, after that, an
     update, which replaces a stale one among the step's messages or is
     added after them.

- **The texts are dsh's:**
  - the first catalog's `<system-reminder>` with its `<available_skills>`
    block;
  - the update's "The available skill catalog changed." with its two
    availability lines;
  - the empty update's tombstone lines.

  Each entry line is ``- `name`: description``, with the description
  escaped for the frame.
- **The source is dsh's:** `{kind: "skill-catalog", form: "catalog",
  entries}`, with `update: true` on a replacement. The entries are what
  identify a catalog, never its prose.
- **A readable catalog** has entries that are an array of objects, each
  with a non-empty string name and a string description. Anything else
  with the `skill-catalog` kind isn't a catalog, so a malformed seed can't
  fail the step.
- A catalog hidden by compaction is published again, as an update.

### Invocations

The invocation listener:

1. runs the rest of the chain, and passes a reject through;
2. scans the claimed messages whose source is `user`, text blocks only,
   for dsh's `SKILL_GESTURE`: a `/`, at the start or after whitespace, a
   skill name, then whitespace or the end;
3. keeps each name once, in the order first seen;
4. loads each name. A skill people may invoke joins the step as a user
   message with dsh's `skill-invocation` source (`{kind, name, form:
   "instructions"}`) and its `<skill_content>` block as the text. An
   unknown name, or a skill people may not invoke, stays plain prose.

The invocation listener is registered before the catalog listener, as in
dsh. So it extends the list the catalog listener gave back, and an invoked
skill comes after the catalog and every other message the hook added.

The `/` is matched by hand. Whitespace is JavaScript's `\s`, which counts
U+FEFF and not U+0085, where Rust's `char::is_whitespace` is the other way
round. The catalog's whitespace folding uses the same rule.

### The plugin

- `ToolSkillPlugin` checks the config. A description cap below 3 fails
  the start with `tool-skill: catalogDescriptionMaxLength must be an integer
  greater than or equal to 3`.
- It registers the tool, then the invocation listener, then the catalog
  listener, all globally.
- It requires the tool registry, the skill registry, the working
  directory, the pre-step hooks and the session store.

### The binary

`ask` and `chat` mount `SkillsPlugin`, `SkillFilesystemPlugin` and
`ToolSkillPlugin`, as dsh's base bundle mounts `skill`, `skill-filesystem`
and `tool-skill` (`bundle/base/cordis.patch.yml`).

- **The default roots are on,** with no config, as in dsh. A skill under
  the project's `.dsh/skills` or `.agents/skills`, or the user's homes,
  is in the catalog. `DSH_HOME`, `DSH_AGENTS_HOME` and
  `DSH_BUNDLED_SKILL_DIR` move the homes and add a bundled root.
- **Watching:**
  - `chat` watches the roots, dsh's default, so a skill added during a
    chat reaches its next step.
  - `ask` doesn't, since one question has no next step.
  - A watch that won't start leaves the catalog incomplete, and so leaves
    it out, as in dsh. On this machine, whose inotify instances are used
    up, `chat` should therefore show no catalog. This hasn't been tried
    live.
- **The usage text** explains skills and the three environment
  variables.
- **The CLI tests** point the three variables at a directory that doesn't
  exist, so no test reads the real `~/.dsh` or `~/.agents`. The `ask`
  request's tools now include `skill`.
- A new CLI case runs `ask "/demo please"` in a temp project with
  `.agents/skills/demo`. The request is then the question, the catalog,
  the invoked skill, and the runtime context, in that order.

### Tests

- 29 integration tests, 2 unit tests, 2 loop tests for the pre-step hook,
  and 1 CLI case.
- The integration tests run on a real runtime: sessions, projections, the
  system prompt, tools, models, the loop, the local filesystem, the
  working directory, the skills, the filesystem provider over temp homes,
  and this plugin.
- dsh's `fireStep` and `proposeStep` become `PreStepHooks::run`, with what
  enters appended to the session for `fireStep`.
- Two cases go through the loop with a scripted model:
  - the model reads the catalog and loads a skill with the tool;
  - a person invokes one with `/demo`.

### Left out

- **Tool visibility.** dsh publishes the catalog only while the calling
  agent resolves this plugin's own tool, so a restricted agent, or one
  with a scoped tool named `skill`, gets no catalog. Karaka's tools have
  no per-agent restriction or scoping yet, and a tool is never replaced,
  so the tool is always visible and the catalog always published. The
  two spec cases wait on per-agent tools. (Closed in "Presets, step 1":
  the catalog is now gated, and both cases are ported.)
- **The abort signal.** dsh passes the step's signal to `ensure` and to
  every registry lookup, and throws once it fires. A pre-step has a
  cancel switch, and the loop drops the chain on a cancel, so the lookups
  get no signal. The tool's calls have none either (see open ends). The
  "forwards the step abort signal" case is skipped.
- **The tool's output schema.** dsh's tool returns a structured value,
  checks it against an output schema, and renders it. Karaka's tools
  return text, so the rendered text is the result, and dsh's
  `INVALID_TOOL_OUTPUT` for an unknown resource-base kind can't arise: a
  resource base is a Rust enum. That case is skipped.
- **`presentCall`** (`Load skill <name>`), with the rest of the
  presentation layer.
- **Dispose.** The kernel unloads no plugin, so the tool and listeners
  stay.
- **dsh's missing-event error** (`skill catalog cannot read seq …`): a
  Karaka session's events have no holes.

### Where it differs from dsh

- **Catalogs are compared by their entries,** not by sha256 digests of
  them. The result is the same.
- **The history is scanned each step,** newest first, over a copy of the
  session's events, as dsh's `eventAt` loop scans it.
- **Message ids** are `skill-catalog-<n>` and `skill-invocation-<name>-<n>`,
  with `n` the session's length, where dsh's `createUserMessage` makes
  random ids.
- **Runtime context.** dsh's end of the pre-step chain adds the
  runtime-context snapshot to the step's messages. Karaka's loop logs it
  with each attempt, after the step's messages. So the order in a request
  is the same: the claimed messages, the catalog, an invoked skill, and
  then the runtime context. But a listener never sees the snapshot.
- **No `startsRequestSeries`** on the decision. It waits on compaction.
- **Cutting a description.** dsh's `slice` counts UTF-16 units and can
  split a surrogate pair. Here lengths are counted in UTF-16 units too,
  but a character that would be split is dropped whole.

## Presets, step 1: the scope chain

This step gives every per-agent registry one shared notion of scope, so a
registration can belong to an agent preset as well as to one session.
It also ports scoped tools and restrictions to the tool registry. The
preset registry itself is step 2. (dsh: `packages/core/scope`,
`src/index.ts` and `store.ts`; the scoped half of `core/tools`.)

### The chain

A new crate, `karaka-scope`, holds the shared types. It depends on
`karaka-session` alone, for `SessionId`, so every registry crate can use
it. `karaka-system-prompt` now reaches `karaka-session` through it.

- **`ScopeKey`** is `Preset(PresetId)` or `Session(SessionId)`. A
  registration takes `Option<&ScopeKey>`, where `None` means global.
  `PresetId` is new.
- **`ScopeChain { preset, session }`** is what a lookup sees.
  - `farthest_first()` gives preset, then session; `nearest_first()` gives
    the reverse; `nearest()` is the viewer's own scope.
  - `admits(scope)` says whether a registration filed under `scope` is
    seen: a global one always, a scoped one when its key is on the chain.
  - A preset's id and a session's never match each other.
- **`ScopedLayers<L>`** is dsh's `ScopedLayers`:
  - the global layer always exists;
  - a scoped layer is made by its first registration, never by a read;
  - `chain_layers(chain)` gives the layers that exist, farthest first;
  - `peek(key)` reads one scope's own layer and nothing else;
  - `prune` forgets an empty layer.

  The system prompt and the tool registry keep their layers in it. The
  skill registry kept its own map, keyed by `ScopeKey` now.
- **`Agent::scope()`** gives the agent's chain. The default is the
  session alone. The loop's agent answers from `Inner::scope()`, which
  the loop's prompt assembly and pre-step hook use too, so there is one
  source when step 2 fills in the preset.

dsh's chain can be any length, and a parent can be moved while a session
is still blank. Karaka's is fixed at three levels: global, then preset,
then session. A child agent will join its parent's preset, as dsh's
`composeFrom` does, so three levels still cover it.

### The registries

Each one keeps its rules, now across the chain:

- **The system prompt.** Its own `Scope(String)` type is gone, and
  `AssembleContext` holds `chain: ScopeChain`.
  - Sections, contexts and variables merge from the global layer, then
    the chain's layers, farthest first, so the nearest scope has the last
    word on a name. dsh's `Map.set` keeps a name's first place, and the
    merge does too.
  - Suppression holds if any layer on the chain suppresses.
  - Tool sources come from every layer on the chain.
  - Listeners run if the chain admits them.
  - The working-directory and approval contexts read the session from
    `context.chain.session`.
- **The user-questions and approval answerers.** A request runs the
  answerers its agent's chain admits. A request with no agent runs only
  the global ones.
- **The `tools/pre-execute` listeners and the pre-step hooks** work the
  same way. `PreStepHooks::is_empty_for` takes a chain.
- **The skill registry.** `SkillLookupOptions::scope` is a `ScopeChain`.
  - A read merges the global layer, then the preset's, then the
    session's, and the nearest layer's entry of a name wins outright,
    whatever the ranks, as dsh's does.
  - The cache is keyed by `(cwd, chain, revision)`.

**Listener order.** In dsh, scoped listeners run through Cordis's
filtered dispatch (`scopeTarget`): every listener the dispatch key
admits, global or on an ancestor scope, in registration order. Here the
admitted listeners keep the order they were added, whatever their
scopes. `on_assemble_first` still puts a listener at the front.

### Scoped tools

`ToolRegistry` keeps a `ScopedLayers` of tool layers. Each layer has its
tools in registration order, names unique within the layer, and its
restrictions. (dsh: `ToolLayer` and `view`.)

- **Registering.** `register_in(scope, tool)` files a tool in a scope;
  `register(tool)` is global.
  - A second tool of a name in one layer is refused. The scoped error is
    dsh's `tool "x" is already registered in this scope`.
  - The global error now gives dsh's advice in Karaka's terms: `(for a
    per-agent variant, register it in that agent's scope instead)`. dsh's
    says to register through `agent.ctx`.
- **The view** (`view`), worked out per lookup in one pass over the
  chain:
  1. What the viewer inherits: the global tools, then each scope on the
     chain except its own, farthest first, a nearer tool shadowing a
     farther one of its name.
  2. Every scope's restrictions on the chain, its own included, mask
     inherited names, each intersecting the others.
  3. The viewer's own scope's tools come last. They shadow an inherited
     name and sit outside every restriction.
- **`restrict(viewer, ToolRestriction { allow, deny })`** files a
  restriction in the viewer's nearest scope. It returns a
  `RestrictionRegistration` whose `remove` lifts it.
  - It checks the names against what that scope inherits through the
    chain.
  - The errors are dsh's texts: a chain with no scope, a restriction with
    neither list, and unknown names, with "known global tools" sorted by
    UTF-16 units, or `(none)`.
  - A restriction applies to tools registered after it.
  - A preset's restriction reaches every session in the preset.
- **Lookups.** `schemas_for(chain)` gives the schemas a viewer sees, and
  `schemas()` is the global view. `get(name, chain)` gives the tool a
  viewer would call.
- **The system prompt's tool source** gives each assembly its agent's
  schemas, and every name the agent knows, restricted ones included. A
  configured tool order may still name a masked tool, as dsh's
  `wireSchemas` gives `knownNames`.
- **`execute`** resolves the tool and the gate's listeners through the
  caller's chain. A tool the caller doesn't see is `Error: unknown tool
  "x"`, as in dsh.

### The skill tool's visibility gating

`tool-skill` now publishes the catalog only while the agent's view
resolves to the very `skill` tool the plugin registered, compared by
pointer, as dsh's `ctx.tools.get(name, agent) === skillTool`.

- A restricted agent gets no catalog.
- An agent with a scoped tool named `skill` gets no catalog.
- An agent that had a catalog gets the empty replacement.

Its lookups, the tool's and the listeners', use the agent's chain.

### Tests

- **New tests:**
  - 4 in `karaka-scope`;
  - 12 scoped-tool cases in `karaka-tools/tests/scoped.rs`;
  - 1 each for a preset's prompt sections and variables, skill layer,
    question answerers and pre-step listeners;
  - 2 for the skill tool's visibility gating;
  - 1 loop case, where a request lists and runs the tools its agent sees.
- **Preset coverage.** Each preset case also checks that a chain naming
  another preset, a session alone, and a session named like the preset
  see none of it.
- **Call sites** in the existing tests now pass `ScopeKey`s and chains.
- **Ported dsh cases:**
  - `scope.spec.ts`: listener routing by key, and an ancestor-tagged
    listener admitted for a descendant but never the reverse;
  - `store.spec.ts`: eager global state, reads that make no layer, and
    merged shadows;
  - `scoped.spec.ts`: registration, shadowing, duplicates, the five
    `restrict()` cases, the three inherited-layer cases, and the scoped
    pre-execute listener;
  - `tool-skill.spec.ts`: its two visibility cases.
- **Skipped dsh cases:**
  - `scope.spec.ts`: Cordis contexts, fibers and disposal; carriers and
    base filters; `{ global: true }`; cycle checks and rebinding. A
    fixed chain has no parent links.
  - `store.spec.ts`: `NamedEntries` and `AnonymousEntries` live iteration
    and idempotent undo; effect labels; notification rollback. There are
    no Cordis effects.
  - `scoped.spec.ts`: the synchronous `tools/result` observer type,
    guards, argument snapshots and final-outcome notification, none of
    them ported. Also skipped: disposing a scope, since tools are never
    removed, and compiling a readonly filter, since a Rust restriction is
    moved into the registry.

### Left out

- **Presets themselves:** the registry, binding an agent's chain to its
  session's `agentPreset`, and mounting a preset's plugins into its scope.
  That is step 2.
- **`RequestErrorHooks` and `InboxHooks`** stay global only, as they
  were.
- **Tool guards, `presentAs` and `tools/result`,** with the rest of dsh's
  tool runtime that isn't ported.
- **Removing a tool,** which waits on teardown, as before.

### Where it differs from dsh

- **`restrict` takes the viewer's chain,** where dsh takes the calling
  scope. A Karaka scope doesn't know its parent, so what a session
  inherits depends on its preset, which only its chain names.
- **The restrict errors keep dsh's words verbatim,** `tools.restrict()`
  and `(agent.ctx)` included, though Karaka has no such method or
  context.
- **The tool view is worked out on every lookup.** dsh's is too, from its
  live maps. Here it clones the `Arc`s of every tool along the chain under
  the read lock.

## Presets, step 2: the registry

`karaka-agent-presets` ports dsh's `packages/preset/agent-preset-registry`
(`index.ts`, `mount.ts`, `session.ts`, `definition.ts`, `preset.ts`,
`types.ts`) and folds in `packages/preset/agent-preset`, the row that
declares a preset. With it, each agent's scope chain names its session's
preset, in the one place step 1 left for it.

### A preset is compiled in

- `PresetDefinition { id, name, description, order, plugins }`, where each
  plugin is a `Box<dyn PresetPlugin>`. dsh's definition is a YAML row of
  Cordis entries, loaded into a registry-owned scope at run time.
- **A `PresetPlugin`** has a `name`, an `id` (its name by default, as a
  diagnostic shows it), the host ports it `requires`, and an async `setup`.
  - The setup runs against a `PresetContext`. It can `get` the ports the
    plugin listed, and it knows the preset's `ScopeKey::Preset(id)`, which
    the plugin files every registration under.
  - It can't `provide` a port. **That rules out by construction what dsh's
    mount audits after the fact:** `leakedServices`, a preset publishing a
    process-global service (`Preset services require isolate realms: …`).
- **`AgentPresetsPlugin`** sets every preset up when the runtime starts.
  - It requires the union of its presets' plugins' ports, so the kernel's
    existing ordering starts their providers first.
  - A port nobody provides fails the start. In dsh, a row waiting for a
    missing service stays mounted, and the preset reads as broken (`…:
    waiting for X`) until the service appears, re-audited on every read.
    Here the kernel knows the whole plugin list up front, so neither
    "pending" nor that re-audit exists.

### Startup

- **Definition mistakes fail the start,** with dsh's texts: an empty id
  gives `Preset id must not be empty`; a repeated id gives `Duplicate
  agent preset: x`. dsh's `register` throws these to the declaring row and
  leaves the other presets standing. A compiled-in definition is a
  programming mistake, so the runtime doesn't start.
- **Each preset's plugins are set up in order.**
  - A plugin whose setup fails breaks its preset. The reason has one line
    per failed plugin, `id (name): message`, as dsh's audit writes it, and
    the lines are joined by newlines.
  - The preset's other plugins are still set up, as dsh's tree mounts
    every row, and every other preset stands.
  - A broken preset stays in the roster with its reason, and no session
    can run it.
  - dsh renders an aggregate error's members beneath its line. A Rust
    error has no such members, so the line is the error's text.
- **What a broken preset leaves behind.** dsh disposes a broken preset's
  scope, which takes back what its rows registered. Karaka has no teardown.
  What a broken preset's plugins registered before failing stays under its
  scope, where no chain reaches it, since no agent can run a broken
  preset. Anything a plugin registered globally stays live. It's recorded
  in open ends.
- **dsh's warning** `agent preset <id>: <reason>` has no logger to go to.
  The reason stays in the roster.

### The registry

`AgentPresets` is the port, dsh's `ctx.agentPresets`.

- **`default_id()`** is the config's `default`, which is required. dsh's
  `selectedDefault`, the user's default, is more than UI state: new sessions
  use it over the deployment's. It's edited through a settings service,
  which Karaka lacks, so it is deferred (open ends).
- **`list()`** gives every preset, broken ones included, by `order` then
  id. A preset with no order sorts as infinity, so two such presets fall
  through to the id, as dsh's NaN does. Ids compare by bytes, where dsh
  uses `localeCompare`.
- **`roster()`** gives the rows with `isDefault`, dsh's `list` remote, in
  dsh's JSON.
- **`resolve(id?)`** gives `{ id, broken? }` only, as dsh's does, or
  `agent-preset/not-found`.
- **`usable(id?)`** is dsh's `retain`, without the revision it leases. It
  gives `agent-preset/not-found` or `agent-preset/invalid`.
- **The errors** are `PresetError`, with dsh's texts, codes and details:
  - `Unknown agent preset: x`, with `{ agentPreset, available }`, where
    `available` lists the ids in registration order;
  - the reason, with `{ agentPreset, reason }`;
  - `This session has already started`, with `{ sessionId, agentPreset }`.
- **Not ported**, all Cordis-only:
  - revisions and generations, `retire` and `collect`;
  - rebinding a live tree (`bind` and `recompose`'s `rebind`);
  - `acquireScope`'s lease;
  - `readDocument`'s YAML dump of the rows;
  - `compositionInventory` and `inspectCompositions`, which read a Loader
    tree;
  - `serviceFor`, which reads a service a preset's isolate realm holds.

  A Karaka preset is set up once and never changes, so there is nothing to
  revise, lease, rebind or collect. Its plugins are Rust values, not
  entries to dump or inventory, and they can't provide services, so there
  is nothing for `serviceFor` to find.

### Sessions

- **The header already has `agent_preset`.** The JSONL format reads and
  writes it as `agentPreset`.
- **`agent-preset/selected` `{ agentPreset }`** is logged as a passthrough
  event.
- **The `agentPreset` projection** starts from the header, takes the latest
  selection, and is client-visible as its state, as dsh's `wire.view` is.
- **`select(session, id)`** follows dsh's order:
  1. `agent-preset/locked` once a turn has started (any `turn/start`,
     dsh's `turnBoundary`);
  2. nothing logged when the preset is usable and already recorded;
  3. otherwise `not-found` or `invalid`;
  4. then the event.

  Selections run one at a time, under one lock, where dsh chains them per
  agent.
- **The default on creation.** A session the loop creates names the
  default preset in its header, when the roster has it, as dsh's session
  controller writes the resolved preset into a new session's header.

### Binding

`karaka-agent` gains `AgentComposition`, a port the loop provides empty,
and the `AgentComposer` trait. The registry sets itself as the composer.
dsh's analogue is the `setup` callback an agent is created with, where the
session controller calls `agentPresets.mount`.

- **`prepare(header)`.** The loop's `Agents::open` creates a missing
  session with `SessionStore::open_or_create_with`, passing the fresh header
  through the composer, which names the default.
- **`check(session)`.** Before anything runs on the session, the composer
  checks it can run its preset: the one recorded, or the default when none
  is.
  - An unknown or broken preset refuses the open with
    `OpenError::Compose`, carrying dsh's code, message and details.
  - A session naming no preset while the default is missing refuses too,
    as dsh's `resolve` throws.
- **`preset(session)`.** `Inner::scope()` reads the composer each time, so
  a selection takes effect at once: the next request lists the new
  preset's tools and sections. dsh rebinds the agent's scope
  (`recompose`).
- `composeFrom`, a child agent joining its parent's preset, waits on child
  agents (open ends).

### Tests

- **13 integration tests** on a real runtime: sessions, projections, system
  prompt, tools, models, the loop, and the registry. dsh's `contribute`
  fixture row is a preset plugin registering one scoped tool and one
  scoped section.
- **They cover:**
  - each preset's tools and sections reaching only its agents, and the
    loop writing the default into a new header;
  - end to end, two presets and two sessions whose requests each list
    only their own preset's tool and section;
  - a broken preset's failure lines, its refusal with `invalid` and its
    details, its row in the roster, and its scoped leftovers unreachable;
  - an unknown preset's refusal with `not-found` and its details;
  - a missing default;
  - a duplicate or empty id, and a missing port, failing the start;
  - selection while blank, a locked started session, and the chain
    following the selection;
  - failed selections leaving the session selectable;
  - no event for an already recorded preset, the header counting as
    recorded;
  - the default running for a session recording none, until it is
    selected;
  - the list order, the roster JSON and `resolve`'s shape;
  - the projection, from `session.spec.ts`.
- **Skipped dsh tests:**
  - every revision, lease and rebind case;
  - `composeFrom` (child agents);
  - the Loader and Cordis cases: isolated and leaked services, a Host
    provider still activating, a row waiting for a service, the
    inventory, the YAML document, `entryListProblem`, and `mount.spec.ts`
    but its failure lines;
  - the settings cases (`selectedDefault`, `modeSelectionEnabled`, the
    Settings pages);
  - the `tools/change` observer case, since there is no such event.

  The crate's test file names each one.
- **`agent-preset/tests/skills.spec.ts`** tests the creator skills,
  templates and display resources dsh ships with its preset package.
  Karaka ships none, so none of it applies.

### Where it differs from dsh

- **A definition mistake fails the start,** rather than its row.
- **A missing port fails the start,** rather than leaving the preset
  broken until the port appears.
- **Ids sort by bytes,** where dsh uses `localeCompare`.
- **The chain follows the projection** on every read, where dsh rebinds a
  scope.
- **The default is written into the header** only when the roster has it.
  Otherwise the session is created with no preset and refused, where dsh's
  controller fails before creating it.

## Presets, step 3: persona, tool policy and the binary

This step adds three preset plugins and lets the binary read presets
from a file.

### Persona

`karaka-persona` ports dsh's `packages/preset/persona` (`src/index.ts`).
`Persona { prefix, suffix, complete, include_runtime_context }` is a
`PresetPlugin` with dsh's defaults (`Persona::new(prefix)`: an empty
suffix, not complete, runtime context kept).

- **Sections.** `apply(prompt, scope)` registers the
  `deployment:persona-prefix` and `deployment:persona-suffix` sections at
  `SectionOrder::DeploymentPersonaPrefix` and `DeploymentPersonaSuffix`,
  in the scope. They shadow the deployment persona for the agents whose
  chain holds it.
- **The options:**
  - `complete` marks the prefix as the complete section, so it alone is
    the prompt, after every listener.
  - An empty suffix, the default, shadows the deployment's suffix away.
  - `include_runtime_context: false` suppresses optional runtime context
    for the scope.
- **The texts are templates.** Karaka's system prompt already interpolates
  strictly, so a `{{name}}` resolves at render, and an unknown one fails
  the render, as in dsh.
- **A global mount fails loud,** as dsh's does. `apply(prompt, None)`
  collides with the system prompt's own registration of the deployment
  persona: `prompt section "deployment:persona-prefix" is already
  registered …`. As a preset plugin, it always registers in its preset's
  scope.
- **`guides`.** dsh's persona has no `guides` field, so Play This Life's is
  its own wrapper's. It isn't ported, and is recorded in open ends.

### The tool policy

`karaka-tool-policy` ports Karaka's own `@karaka-ai/agent/tool-policy`
(`karaka/mcp-application/src/tool-policy.ts`) as `ToolPolicy { mode,
allow, deny }`.

- **Its lists become one `restrict`** on the preset's chain. `deny` wins
  over `allow`, as `karaka-tools`' restriction already has dsh's deny.
  An omitted list stays omitted, and with neither list nothing is filed.
- **The names are checked when the preset is set up.** A name no global
  tool has breaks the preset, with `restrict`'s error as its line. The
  binary's presets therefore go last, after every plugin that registers a
  tool. `current_time`, which `chat` used to register after the start, is
  now a plugin (`CurrentTimePlugin`), so a policy can name it.
- **Only mode `native` is ported.** Karaka's `ptc` mode, and taking
  application tools out of the lists (`karakaApplicationTools`,
  `registerPolicy`), wait on the MCP client (open ends). Its
  `Presentation.apply` has nothing to do in native mode.

### Skills in a preset

`karaka-skill-filesystem` gains `SkillFilesystemPresetPlugin`. It
registers a `FileSystemSkillProvider` in the preset's skill layer, as a
preset mounts dsh's `skill-filesystem` row, so only that preset's agents
see its skills.

### The binary

- **`KARAKA_PRESETS=<file>`** is a YAML list of preset rows: `id`, `name`,
  `description`, `order`, and `plugins`. That is the config of dsh's
  `agent-preset` row, as Play This Life declares its presets. It is
  parsed with yaml-rust2, `=0.13.0`, the parser skill frontmatter already
  uses.
- **A plugin row** has a `name`, a `config`, and an optional `id` for
  diagnostics. The name is short or the package's:
  - `persona` or `@deepseek-ai/dsh-persona`, with `prefix`, `suffix`,
    `complete` and `includeRuntimeContext`;
  - `tool-policy` or `@karaka-ai/agent/tool-policy`, with `mode`, `allow`
    and `deny`;
  - `skill-filesystem` or `@deepseek-ai/dsh-skill-filesystem`, with
    `includeDefaultRoots`, `customSkillDirs` and `watch`. A relative
    directory is read from the file's own directory. `watch` defaults to
    the command's (on for `chat`, off for `ask`).
- **Mistakes fail the start** with a message that says where: an unknown
  plugin, an unknown key, a wrong type, a missing `prefix`, `mode: ptc`,
  an empty list, and so on.
- **`KARAKA_PRESET=<id>`** is the default preset. I chose it over a
  `default:` key in the file, so the file stays a plain list of rows, as
  Play This Life's are. Without it, the file's first preset is the
  default. An id the file doesn't have fails the start.
- **`--preset <id>`** selects a preset for the session after its agent
  opens and before its first turn, through `AgentPresets::select`, so a
  started session is refused with dsh's `This session has already
  started`. Without `KARAKA_PRESETS` it fails, as dsh's session
  controller refuses a named preset with no roster.
- **The presets own skill discovery.** With `KARAKA_PRESETS`, the global
  `skill-filesystem` isn't mounted, as dsh's web bundle disables the host
  row when presets own local discovery. Skills come from presets'
  `skill-filesystem` rows. The `skill` tool stays global, so a policy
  names it like any other tool.
- **Without `KARAKA_PRESETS`,** the plugins are as before, with
  `current_time` now registered by a plugin.
- `loop_plugins` returns a `Result`, and the binary's options are now
  `Options { approvals, preset }`.

### Tests

- **9 persona cases** from `persona.spec.ts`.
- **2 tool-policy cases** on a real runtime:
  - the lists restrict each preset's inherited tools: an omitted list,
    `allow`, `deny`, and `deny` winning;
  - an unknown name breaks the preset with `restrict`'s line.
- **2 unit tests of the presets file,** for a file that parses and for
  each mistake's message.
- **1 unit test of `--preset`.**
- **2 CLI cases:**
  - two presets with different personas and allow lists, one with its own
    skill. The request bodies show each session's own system prompt, tool
    list, and only the scout's skill catalog. The second session picks
    its preset with `--preset`.
  - preset mistakes failing before any call: an unknown plugin, an
    unknown `KARAKA_PRESET`, `--preset` without presets, and an unknown
    `--preset`.
- **Skipped from `persona.spec.ts`:**
  - "restores the shadowed default when its fiber unloads", and the
    disposal halves of two other cases: the kernel unloads nothing;
  - "keeps runtime context by default when apply bypasses schema
    defaults": a Rust `Persona` has no schema to bypass.

### Where it differs from dsh

- **Presets come from a file of rows the binary knows,** three plugins
  of them, not from Cordis rows naming any package.
- **The default is `KARAKA_PRESET`, or the file's first,** where dsh
  requires `default`.
- **A tool policy's names are checked at startup** against the global
  tools. Karaka's original takes application tools out first.
