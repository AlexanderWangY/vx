# Releasing

1. In `CHANGELOG.md`, move the notes under "Unreleased" to a new `## X.Y.Z` heading.
2. Set `version = "X.Y.Z"` in `Cargo.toml`, then run `cargo check` to update `Cargo.lock`.
3. Commit and merge to `main`.
4. Tag that commit and push the tag:

   ```
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

The Release workflow then builds macOS and Linux binaries, publishes a GitHub Release with an
installer script, and updates the formula in
[AlexanderWangY/homebrew-tap](https://github.com/AlexanderWangY/homebrew-tap).

Versions are `0.MINOR.PATCH` until 1.0: bump MINOR for features or breaking changes, PATCH for
fixes. A tag like `v0.3.0-rc.1` makes a prerelease, which doesn't touch the Homebrew formula.

## Changing the release setup

Release config lives in `dist-workspace.toml`. After changing it, regenerate the workflow with
`dist generate` instead of editing `.github/workflows/release.yml` by hand.

## One-time setup

The Release workflow pushes to the tap with a token stored as the `HOMEBREW_TAP_TOKEN` secret
in this repo: a fine-grained personal access token with **Contents: Read and write** on
`AlexanderWangY/homebrew-tap` only.
