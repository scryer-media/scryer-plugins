# Repository instructions

## Code Review Rules

- Before performing any code review, read the repository-root `SECURITY.md` completely.
- Apply the security posture and threat model documented there when deciding whether behavior is a security finding and when assigning severity.
- Do not report behavior that `SECURITY.md` defines as expected merely because it would be risky under a different deployment model. Require a concrete violation of this project's stated security boundaries.

## Versioning

- Every PR must include the proper semver version bump for each plugin or crate whose shipped behavior or artifact it changes. Do not leave the bump for a later release commit.
- Choose the bump by semver: patch for fixes and internal changes, including dependency or codec swaps that keep behavior the same; minor for new backward-compatible capabilities; major for breaking changes to the plugin's contract, configuration, or outputs.
- Update every place the version is recorded, including the package's `Cargo.toml` and its `Cargo.lock` entry, and confirm the lockfile is consistent with `cargo check --locked`.
- A PR that touches several plugins bumps each of them independently.
