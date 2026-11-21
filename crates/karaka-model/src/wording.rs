//! Reading a provider's error message for two failures that its error type
//! and HTTP status don't name: a conversation too long for the model, and
//! an account out of credit.
//!
//! Providers word these differently, so the patterns come from the two
//! projects that collected them. pi-ai 1.1.0, which Play This Life runs,
//! has its list in `utils/overflow.js`. dsh adds its own in
//! `llm/src/error.ts`, and checks both lists (`llm-pi-ai/src/stream.ts:81-84`).
//! Karaka checks both too.
//!
//! Only these few failures are read from the text. Everything else goes by
//! the error type or the HTTP status, which don't change when a provider
//! rewords a message.

use std::sync::LazyLock;

use regex::RegexSet;

/// Whether `detail` says the request didn't fit in the model's context
/// window. `detail` is everything the provider said: its error code, type
/// and message, joined with spaces.
pub fn is_context_overflow(detail: &str) -> bool {
    (PI_AI_OVERFLOW.is_match(detail) && !PI_AI_NOT_OVERFLOW.is_match(detail))
        || DSH_OVERFLOW.is_match(detail)
}

/// Whether `detail` says the account has run out of quota, balance or
/// credit, which waiting won't fix, rather than hit a rate limit, which
/// waiting will. `detail` is as for [`is_context_overflow`].
pub fn is_out_of_quota(detail: &str) -> bool {
    DSH_QUOTA.is_match(detail)
}

/// Compiles a list of patterns once, the first time it is used. Every
/// pattern ignores case, as all the originals do.
fn patterns(list: &[&str]) -> RegexSet {
    RegexSet::new(list.iter().map(|pattern| format!("(?i){pattern}")))
        .expect("the patterns are valid")
}

/// pi-ai's `OVERFLOW_PATTERNS` (`utils/overflow.js:35-61`), in the same
/// order, with the provider each one is for.
static PI_AI_OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    patterns(&[
        // Anthropic and z.ai.
        r"prompt (?:is )?too long",
        // z.ai's China endpoint.
        r"prompt exceeds max length",
        // Anthropic's limit on the request's size in bytes (HTTP 413).
        r"request_too_large",
        // Amazon Bedrock.
        r"input is too long for requested model",
        // OpenAI.
        r"exceeds the context window",
        // OpenAI-compatible proxies such as LiteLLM.
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        // Google Gemini.
        r"input token count.*exceeds the maximum",
        // xAI.
        r"maximum prompt length is \d+",
        // Groq.
        r"reduce the length of the messages",
        // OpenRouter.
        r"maximum context length is \d+ tokens",
        // OpenRouter with Poolside.
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        // Together AI.
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        // GitHub Copilot.
        r"exceeds the limit of \d+",
        // llama.cpp.
        r"exceeds the available context size",
        // LM Studio.
        r"greater than the context length",
        // MiniMax.
        r"context window exceeds limit",
        // Kimi.
        r"exceeded model token limit",
        // Mistral.
        r"too large for model with \d+ maximum context length",
        // DS4.
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        // z.ai's finish reason, and Anthropic's stop reason of the same name.
        r"model_context_window_exceeded",
        // Ollama.
        r"prompt too long; exceeded (?:max )?context length",
        // DashScope and Qwen.
        r"range of input length should be",
        // Wording any provider might use.
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ])
});

/// pi-ai's `NON_OVERFLOW_PATTERNS` (`utils/overflow.js:72-76`): rate limits
/// that a pattern above would take for an overflow, such as Bedrock's
/// "Too many tokens, please wait".
static PI_AI_NOT_OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    patterns(&[
        r"^(Throttling error|Service unavailable):",
        r"rate limit",
        r"too many requests",
    ])
});

/// dsh's `isContextWindowExceededError` (`llm/src/error.ts:54-89`). Each
/// pattern ties the limit to the context, so "temperature exceeds maximum
/// allowed value" doesn't match.
static DSH_OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    patterns(&[
        // "context_length_exceeded", "context window overflow", and so on.
        r"(?:^|[^a-z0-9])context[\s_-](?:length|window)[\s_-](?:exceed(?:ed|s)?|overflow(?:ed)?|limit[\s_-]exceeded)(?:$|[^a-z0-9])",
        r"\b(?:maximum|max)(?:\s+(?:allowed|supported))?\s+context\s+(?:length|window)\b",
        r"\b(?:request|prompt|input|messages?)\s+(?:is\s+|are\s+)?too\s+(?:large|long)\s+for\s+(?:(?:this|the)\s+)?(?:model(?:'s)?\s+)?context(?:\s+window)?\b",
        r"\b(?:input|prompt|request)\s+(?:is\s+)?too\s+(?:long|large)\s+for\s+(?:this|the)\s+model\b",
        r"\b(?:input|prompt|request|messages?)\b.{0,40}\b(?:exceed(?:s|ed)?|overflows?|is\s+larger\s+than)\b.{0,40}\b(?:the\s+)?(?:model(?:'s)?\s+)?context(?:\s+(?:length|window))?\b",
    ])
});

/// dsh's `isQuotaExceededError` (`llm/src/error.ts:97-103`).
static DSH_QUOTA: LazyLock<RegexSet> = LazyLock::new(|| {
    patterns(&[
        // OpenAI's `insufficient_quota`, DeepSeek's "Insufficient Balance".
        r"\binsufficient[\s_-]+(?:quota|balance|credits?)\b",
        r"\b(?:quota|usage[\s_-]+limit)[\s_-]+(?:exceeded|exhausted|reached)\b",
        // OpenAI's "You exceeded your current quota".
        r"\bexceed(?:ed|s)?[\s_-]+(?:(?:your|the)[\s_-]+)?(?:current[\s_-]+)?quota\b",
        r"\b(?:balance|credits?)[\s_-]+(?:exhausted|depleted)\b",
        r"\bout[\s_-]+of[\s_-]+(?:credits?|budget)\b",
    ])
});

#[cfg(test)]
mod tests {
    use super::{is_context_overflow, is_out_of_quota};

    /// The overflow cases from dsh's tests: `llm-pi-ai/tests/convert.spec.ts`
    /// ("maps routable HTTP-ish error messages to stable codes", "uses pi-ai
    /// provider-specific overflow classification") and
    /// `llm-deepseek/tests/runtime.spec.ts` ("recognizes context-capacity
    /// details independently of the HTTP status").
    #[test]
    fn context_capacity_wording_is_an_overflow() {
        for detail in [
            // Anthropic, matched by pi-ai's patterns.
            "invalid_request_error prompt is too long: 213462 tokens > 200000 maximum",
            "request_too_large Request exceeds the maximum size",
            // OpenAI's code, matched by both.
            "context_length_exceeded invalid_request_error This model's maximum context length is 65536 tokens.",
            // Matched by dsh's patterns only.
            "HTTP 400: input exceeds the model context window limit",
            "HTTP 400: request too large for model context",
            "maximum context length exceeded",
            // z.ai's finish reason.
            "model_context_window_exceeded",
        ] {
            assert!(is_context_overflow(detail), "{detail}");
        }
    }

    #[test]
    fn other_limits_are_not_an_overflow() {
        for detail in [
            "HTTP 400: invalid input: temperature exceeds maximum allowed value",
            "max_tokens: 200000 > 128000, which is the maximum allowed number of output tokens",
            "HTTP 413: Payload Too Large",
            "vector length limit exceeded",
            // A rate limit that mentions tokens, which pi-ai rules out.
            "ThrottlingException: Too many tokens, rate limit reached",
            "",
        ] {
            assert!(!is_context_overflow(detail), "{detail}");
        }
    }

    /// The quota cases from dsh's tests: `llm-deepseek/tests/stream.spec.ts`
    /// ("classifies status ... and error ...") and `runtime.spec.ts`
    /// ("distinguishes terminal quota exhaustion from transient HTTP 429
    /// throttling").
    #[test]
    fn running_out_of_credit_is_out_of_quota() {
        for detail in [
            "insufficient balance",
            "invalid_request_error Insufficient Balance",
            "insufficient_quota account credits exhausted",
            "You exceeded your current quota, please check your plan and billing details.",
        ] {
            assert!(is_out_of_quota(detail), "{detail}");
        }
    }

    #[test]
    fn a_rate_limit_is_not_out_of_quota() {
        for detail in ["request rate limit exceeded", "Slow down.", ""] {
            assert!(!is_out_of_quota(detail), "{detail}");
        }
    }
}
