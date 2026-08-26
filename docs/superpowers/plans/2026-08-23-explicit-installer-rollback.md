# Explicit installer rollback

## Objective

Add an explicit, opt-in rollback mode to the macOS package builder and
Developer ID signing pipeline. Normal packages must continue to reject a
lower app bundle version, while rollback packages may replace a higher
installed bundle version without touching runtime state or user data.

## Design

- Add `--rollback` to `cargo xtask pkg-dev` and `cargo xtask sign`.
- Keep the existing fixed component/product identifiers and package version.
- Generate `BundleIsVersionChecked=true` for normal packages and `false` only
  for rollback packages.
- Suffix rollback artifacts, notary logs, and metadata with `-rollback` so the
  operator can distinguish them before installation.
- Preserve the existing exact-path Monitor-only `preinstall` behavior. Runtime,
  LaunchAgents, configuration, secrets, models, caches, and other user data
  remain outside the installer scope.
- Record the rollback mode in signed-build metadata and dry-run output.

## Verification

1. Add failing unit tests for the normal/rollback component plist and artifact
   naming/metadata behavior.
2. Implement the CLI plumbing and package plist generation.
3. Run formatting, unit/all-target tests, clippy, and package artifact checks.
4. Build a correctly versioned rollback package from the existing build 10
   bundle, sign/notarize it, and verify its package structure and Gatekeeper
   acceptance.
5. With the user-approved change window, install rollback build 10 over build
   11 on both nodes, verify the installed app version and version-mismatch
   behavior, then restore build 11.

## Safety boundaries

- No automatic downgrade: rollback must be explicitly selected at build time.
- No `preinstall` process broadening beyond the exact Monitor executable.
- No deletion or replacement of runtime state, model files, caches, secrets,
  or configuration.
- No Git history or remote changes are part of this implementation step.
