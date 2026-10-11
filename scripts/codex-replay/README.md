# Codex parser replay in CI

The W09 integration test calls a harness linked to the actual upstream `codex-protocol` crate. CI checks out Codex rust-v0.153.4 at `3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`, builds the same harness used in the saved 2026-10-06 acceptance evidence, and sets `CODEX_REPLAY_BIN` for all test steps.

`rebuild.py` rejects a source commit mismatch and preserves the upstream lockfile and dependency patches. Generated source, dependencies, and binaries stay under ignored build or runner temporary directories. A missing parser is a test failure, never a skipped or successful gate.

For a local build, supply a clean upstream checkout, an isolated output directory, and the expected upstream commit to `rebuild.py`. No running Siderostat service or user data is accessed.
