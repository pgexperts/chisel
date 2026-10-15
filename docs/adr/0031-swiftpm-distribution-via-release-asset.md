---
id: 0031
title: SwiftPM distribution via release asset
date: 2026-08-23
status: Accepted
summary: The SPM manifest lives at the repo root with a binary target that switches between a local xcframework and a GitHub release asset, so the package resolves both for this repo's own CI and for URL consumers.
---

# 0031. SwiftPM distribution via release asset

## Context

SwiftPM resolves a package manifest only from the root of the repository it is
given — it will not look inside a subdirectory such as `swift/` — and a binary
target must name an artifact SwiftPM can actually fetch: either a local path
that exists in the checkout, or a URL plus checksum it can download. The repo
had `Package.swift` inside `swift/`, with its binary target pointing at
`Chisel.xcframework` by a local path that is itself gitignored build output.
The combination meant the package was unconsumable by anyone outside this
repo: SwiftPM would never find the manifest by URL, and even a checkout that
found it would need to have already run the build script to produce the path
the manifest names. Compounding this, the repo has four tags and zero GitHub
releases, so there is no release asset yet for a URL-based target to point at.

## Decision

Move the manifest to the repository root, using `path:` overrides on the
`Chisel` and `ChiselTests` targets so their sources stay where they are under
`swift/`. The binary target switches on the `CHISEL_LOCAL_XCFRAMEWORK`
environment variable: set, it resolves the local artifact
`swift/build-xcframework.sh` just produced (this repo's own CI and local
development); unset, it resolves a GitHub release asset by URL and checksum
(consumers). The tag itself carries its own correct url and checksum — written
by `swift/release/prepare.sh` before the tag is created, not patched in by the
release workflow afterward — because a consumer pinning tag `vX.Y.Z` resolves
that tag's `Package.swift` as committed, and there is no later point at which
the release workflow's checksum could reach that already-tagged content. The
release workflow (`swift-release.yml`) re-runs the test suite against the tag
and rebuilds the xcframework, verifying the rebuild's checksum matches the one
already committed, before uploading the asset.

## Alternatives considered

- **Commit the xcframework to the repository** — would sidestep the
  fetch-vs-local-path problem entirely by making the artifact always present.
  Rejected: the built xcframework is roughly 974 MB, exactly the build output
  the `.gitignore` rules from SWIFT-2 (issue #110) exist to keep out of
  history; committing it would reintroduce the problem those rules were
  written to prevent, at every future rebuild.
- **Have the release workflow patch `Package.swift` after uploading the
  asset** — build and upload first, then commit the resulting url and
  checksum back onto the tag. Rejected: a tag is meant to be immutable, and
  more concretely, a consumer who resolves the tag before the patch lands (or
  if the patch is ever skipped) gets a manifest describing a different
  artifact — or no artifact — than the one actually uploaded. Computing and
  committing the checksum before the tag exists is the only ordering where the
  tag's manifest and the tag's asset are guaranteed to agree.

## Consequences

- Cutting a release is now a scripted two-phase operation — `prepare.sh` then
  tag then publish — rather than a single tag push; anyone cutting a release
  needs to run the script and commit its output before tagging.
- The release workflow's checksum-verification gate can fail on a legitimate
  re-tag of the same source, because Rust staticlib builds are not
  bit-reproducible byte-for-byte across builds. This is documented in the
  workflow itself so the first occurrence reads as an expected, explained
  condition rather than a supply-chain alarm; the escape hatch is to re-run
  `prepare.sh`, commit the new checksum, and move the tag — or, if the
  mismatch becomes chronic, switch the workflow to upload `prepare.sh`'s
  artifact directly instead of rebuilding in CI.
- Until the first release is published, only the `CHISEL_LOCAL_XCFRAMEWORK=1`
  path is usable; the root manifest's consumer-mode branch carries a
  placeholder checksum of 64 zeros that will not resolve until `prepare.sh`
  overwrites it at the first real release.
