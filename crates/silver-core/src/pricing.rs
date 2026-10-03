//! Approximate USD cost of a run from a model's list price, which the daemon supplies. No known
//! price means no estimate; cost is advisory, never billing-accurate.

/// List price in USD per million tokens. A missing cache or tier rate falls back to the base input
/// rate, so a cache hit never costs more than an uncached token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPrice {
    /// Prompt (input) tokens, USD per million.
    pub input_per_million_usd: f64,
    /// Completion (output) tokens, USD per million.
    pub output_per_million_usd: f64,
    /// Cache-read (cache hit) prompt tokens, USD per million.
    pub cache_read_per_million_usd: Option<f64>,
    /// Cache-write (cache creation) prompt tokens, USD per million.
    pub cache_write_per_million_usd: Option<f64>,
    /// Whole-request context tier: when the total prompt token count exceeds this, the
    /// above input rate applies to the entire request.
    pub tier_threshold_tokens: Option<u64>,
    /// Input rate applied above the tier threshold, USD per million.
    pub input_per_million_usd_above: Option<f64>,
}

/// One request's token buckets for a cache-aware cost estimate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageCostInput {
    /// Total prompt (input) tokens as the provider reports them. OpenAI-style usage
    /// includes cached tokens in this total, so the cache buckets are subtracted out
    /// before the base input rate is applied.
    pub prompt_tokens: u64,
    /// Completion (output) tokens.
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider prompt cache.
    pub cached_tokens: u64,
    /// Prompt tokens written to the provider prompt cache.
    pub cache_write_tokens: u64,
    /// Completion tokens attributable to reasoning. A subset of completion_tokens and
    /// therefore never billed twice; carried so a caller can surface the split.
    pub reasoning_tokens: u64,
}

/// USD cost of one request. Cache tokens bill at their own rates, else the (tiered) input rate;
/// reasoning tokens are part of the completion and add nothing.
pub fn estimate_cost_usd_for_usage(price: &ModelPrice, usage: UsageCostInput) -> f64 {
    const TOKENS_PER_MILLION: f64 = 1_000_000.0;

    let input_rate = match price.tier_threshold_tokens {
        Some(threshold) if usage.prompt_tokens > threshold => price
            .input_per_million_usd_above
            .unwrap_or(price.input_per_million_usd),
        _ => price.input_per_million_usd,
    };
    let cache_read_rate = price.cache_read_per_million_usd.unwrap_or(input_rate);
    let cache_write_rate = price.cache_write_per_million_usd.unwrap_or(input_rate);

    // The cache buckets are already part of the provider's prompt total; clamp so a caller
    // cannot subtract more than it reported and never double-bill the same token.
    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let cache_write = usage
        .cache_write_tokens
        .min(usage.prompt_tokens.saturating_sub(cached));
    let uncached = usage
        .prompt_tokens
        .saturating_sub(cached)
        .saturating_sub(cache_write);

    let input = uncached as f64 / TOKENS_PER_MILLION * input_rate;
    let cache_read = cached as f64 / TOKENS_PER_MILLION * cache_read_rate;
    let cache_write = cache_write as f64 / TOKENS_PER_MILLION * cache_write_rate;
    let output = usage.completion_tokens as f64 / TOKENS_PER_MILLION * price.output_per_million_usd;
    input + cache_read + cache_write + output
}

/// Estimate the USD cost of one request from its token totals.
///
/// For callers that only track prompt/completion tokens: zero cache and reasoning buckets.
pub fn estimate_cost_usd(price: &ModelPrice, prompt_tokens: u64, completion_tokens: u64) -> f64 {
    estimate_cost_usd_for_usage(
        price,
        UsageCostInput {
            prompt_tokens,
            completion_tokens,
            ..UsageCostInput::default()
        },
    )
}
