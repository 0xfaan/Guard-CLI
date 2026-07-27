# Release process

Use this checklist when publishing Soroban Guard crates to crates.io.

## Prerequisites

- Confirm the workspace builds and tests pass locally or in CI.
- Confirm `CHANGELOG.md` has an entry for the version being released.
- Confirm you are authenticated to crates.io with `cargo login`.

## Steps

1. Bump the version in the root workspace `Cargo.toml` under `[workspace.package]`.
2. Update `CHANGELOG.md`: rename `Unreleased` to the release version and date, then add a new empty `Unreleased` section.
3. Publish crates in dependency order:

   ```bash
   cargo publish -p soroban-guard-checks
   cargo publish -p soroban-guard-analyzer
   cargo publish -p soroban-guard-cli
   ```

4. Tag the release in git:

   ```bash
   git tag v<version>
   git push upstream v<version>
   ```

5. Create a GitHub release from the tag and include the changelog notes.

Publish order matters because `soroban-guard-analyzer` depends on `soroban-guard-checks`, and `soroban-guard-cli` depends on both library crates.
