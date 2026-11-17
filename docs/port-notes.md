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
