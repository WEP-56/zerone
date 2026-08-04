# Source snapshot and curation notes

- Upstream: https://github.com/earendil-works/pi
- Snapshot commit: `a96fb984d8c8b065fc5d193309fc812a882adee0`
- Snapshot date: 2026-08-03
- License: MIT, retained in `LICENSE`

The repository was shallow-cloned and its nested Git metadata was removed. The snapshot
was then reduced to a teaching reference for the Zerone harness.

Most retained implementation and test files are unchanged from the snapshot. These
files were deliberately curated to remove references to discarded subsystems:

- `packages/agent/src/index.ts`: exports only retained agent, tool, environment, and session code.
- `packages/agent/src/harness/types.ts`: keeps workspace, tool, session, and typed-error contracts.
- `packages/ai/src/index.ts`: exports only the retained three-API surface and retry/error utilities.
- `packages/ai/src/types.ts`: narrows known APIs/providers to the three teaching cases.
- `packages/ai/src/models.ts`: keeps the API dispatch and model-cost/thinking helpers; auth and catalog refresh were removed.

This directory is intentionally not a package workspace and has no lockfile or build
configuration. Relative imports inside retained TypeScript files are kept closed, but
external npm dependencies are not vendored or installed here.
