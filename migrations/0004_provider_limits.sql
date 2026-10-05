-- The usage limits last read for each provider, so a bot's bar shows at once after a restart and
-- while the provider's endpoint refuses to answer.
CREATE TABLE provider_limits (
    provider TEXT PRIMARY KEY,
    windows TEXT NOT NULL
);
