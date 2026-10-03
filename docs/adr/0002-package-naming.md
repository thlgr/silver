# 0002. Package naming is silver-*, binaries silverd and silver

> Binary names superseded by [0005](0005-single-binary-embedded-web-ui.md).

Status: accepted

## Context

The original plan called the product silver but named the crates and binaries with the upstream
spelling: hermes-core, hermes-client, hermes-protocol, the CLI hermes, and a daemon hermesd. The
workspace cannot carry both spellings without making every manifest, binary target, path and
environment variable ambiguous.

## Decision

Name the workspace and its packages after the product:

- crates: silver-protocol, silver-core, silver-client;
- binaries: silverd (the daemon) and silver (the CLI client);
- configuration and environment: the SILVER_ prefix, for example SILVER_BIND, SILVER_DATA_DIR,
  SILVER_MODEL, SILVER_MODEL_BASE_URL, SILVER_BEARER_TOKEN;
- platform data directory: directories::ProjectDirs::from("dev", "silver", "silver").

## Consequences

- README, docs and shell examples use silverd and silver. References to hermesd, hermes,
  hermes-core or hermes-protocol in docs/upstream-behavior.md describe the upstream Python
  reference, not this workspace.
- No compatibility aliases are provided. Renaming a package or binary later requires changing every
  manifest, target name and documented command.
- The upstream Python agent remains the behavioural reference; only the naming differs. The ADR
  exists so a future reader does not "fix" the spelling back to the upstream one.
