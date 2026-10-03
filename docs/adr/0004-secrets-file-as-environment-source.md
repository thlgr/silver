# 0004. Load provider secrets from a 0600 secrets file as an environment source

> The `silver setup` command named below was retired: write `secrets.env` by hand
> ([configuration.md](../configuration.md#secrets)). Keys typed into the web UI go to
> `<data_dir>/auth.json` (mode 0600) instead ([provider-setup.md](../provider-setup.md#signing-in)).
> The loading rules and precedence below are unchanged.

Status: accepted

## Context

The design requires API keys to come from environment variables, never stored in SQLite or logs
and never returned in capabilities, persistence, events or logs. At the same time `silver setup`
must persist the key it captures so the daemon can be started again after a reboot without the
operator re-exporting it, and a daemon started by a supervisor often has a minimal environment.

Writing the key into `config.toml` would turn a user-editable settings file into a secret store
and break the invariant. Requiring the operator to export the variable on every start would defeat
the purpose of the setup command. The project therefore needed a durable place for the key that is
still, semantically, an environment variable.

## Decision

`silver setup` stores captured keys in `<platform config dir>/secrets.env`:

- The file is `KEY=VALUE`, one credential per line; blank lines and `#` comments are skipped and
  matching surrounding quotes are stripped.
- It is created with mode 0600 inside a directory set to 0700 on Unix; on platforms without mode
  bits the per-user config directory ACL is the protection.
- At startup `load_secrets_env()` imports each pair into the process environment with
  `std::env::set_var`, and only when the variable name is not already present. The process
  environment therefore always wins.
- `config.toml` stores only the variable NAME (`model.api_key_env`); there is no `api_key`
  field in `config.toml`, and the raw key is never written there.
- The effective credential precedence is:
  CLI flags > process environment > `secrets.env` > `config.toml` > defaults.
- The key is never written to SQLite, never returned by the HTTP API (including
  `/v1/capabilities`), never emitted in an SSE event and never logged. Provider errors are
  redacted against the key and truncated to 200 characters, and setup's `--show` redacts every
  value it prints.

`secrets.env` is deliberately treated as an environment source: it is the persistence mechanism
for the environment variable the daemon would otherwise require the operator to export, so the
rule "secrets via environment variables" still holds. It is not a second configuration
file: the daemon does not merge it into `config.toml`, and settings such as
`tools.write_requires_approval` cannot be set through it.

## Consequences

- `secrets.env` must be excluded from backups and from any future `silver export` unless the
  user explicitly asks for secrets, and it must never be committed.
- A future OS keychain or secret-service provider can replace the file behind the same
  `credential(name)` lookup without changing `config.toml` or the API.
- Because the file is imported into the environment before configuration is read, a `secrets.env`
  entry for a `SILVER_*` variable would also act as an environment override. The file is
  intended for credentials; configuration belongs in `config.toml`.
- `silver setup` upserts a single key line and preserves unrelated lines, so rotating one
  provider does not remove other credentials.
- Tests must prove the precedence rule (flag > env > file > config > default) and that a real
  environment value is never overwritten by the file.

## Alternatives considered

- Inline `api_key` in `config.toml`: rejected; it makes a user-editable settings file a secret
  store and breaks that rule.
- OS keychain now: deferred; it is platform-specific and not needed for the MVP.
- Requiring the operator to export the variable every time: rejected; it defeats the purpose of
  `silver setup`.

## Related cleanup

`server.bearer_token` is also a credential. It currently lives in `config.toml` and has an
environment override (`SILVER_BEARER_TOKEN`); it should be placed in `secrets.env` instead.
See [docs/security.md](../security.md).
