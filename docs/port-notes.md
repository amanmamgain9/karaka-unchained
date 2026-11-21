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
