# ADR 0004: GitHub Actions release and CircleCI validation boundary

Date: 2026-09-16
Status: Accepted; supersedes the release portion of the previous ADR 0004

## Context

Win-CodexBar needs reproducible hosted Windows validation and a release path
whose build provenance can be verified by SignPath. SignPath's GitHub trusted
build integration requires the build, the uploaded signing artifact, and the
jobs leading to signing to be observed as GitHub Actions work on GitHub-hosted
runners. CircleCI is retained for ordinary PR and protected-branch validation,
but it cannot be the producer of the SignPath release artifact.

Release credentials must remain unavailable to validation and build steps that
do not need GitHub release write access. v0.60.3 was released before this
change and is immutable.

## Decision

CircleCI's .circleci/config.yml contains the pr-check validation workflow only.
Its canonical local-check CI slice remains the primary hosted Windows gate.
CircleCI has no tag release jobs, approval job, or release publisher.

GitHub Actions owns the canonical tag release in
.github/workflows/release.yml. The workflow accepts only canonical vX.Y.Z tags,
checks out the exact tag SHA, runs release preflight, and builds on a
GitHub-hosted Windows runner. It creates a signing input with exactly the
installer, portable executable, and CLI ZIP, uploads that input as a GitHub
Actions artifact, and submits it to the pinned SignPath configuration and
release-signing policy.

The workflow waits for SignPath, verifies Authenticode on the two top-level
executables and codexbar-cli.exe at the root of the nested CLI ZIP, computes
sidecars from signed bytes, and validates the exact six-asset final bundle.
Only the verified bundle is passed to a separate publisher job with contents
write permission. The publisher creates or updates a draft release and never
replaces a divergent asset or finalizes a release.

The manual SignPath Test workflow uses test-signing, builds a reviewed source
ref with a matching version label, and retains its verified output as an
Actions artifact. It never writes a GitHub Release.

## Trust boundaries

- CircleCI validation has no GitHub release-write credential.
- The GitHub signing job has read-only repository permission plus artifact
  read access; it receives only the SignPath API token needed for submission.
- The GitHub publisher job is the only job with contents write permission.
- SignPath trusted-build and origin verification cover the GitHub workflow and
  its artifact lineage.
- Repository administrators protect main and the canonical vX.Y.Z tag
  namespace and manually publish or roll back draft releases.

## Consequences

- There is one release build lineage and one signing artifact lineage.
- CircleCI Windows credits continue to cover PR and protected-branch checks.
- GitHub-hosted Windows is used for release builds because SignPath requires
  that trusted provenance.
- A signing, verification, or publication mismatch fails closed. Re-running
  the publisher is safe for exact matching assets, while a different digest
  is a hard failure.
- The first production-signed release is the next normal version after
  SignPath certificate and policy onboarding is complete.

## Compiled-output caching

CircleCI may continue to cache Cargo registry data, Cargo targets, and the
pnpm store for its validation job. The GitHub Actions release job may use its
own GitHub-hosted cache strategy, but the final release assets are always
built fresh from the frozen tag SHA and then signed. A cached output is never
accepted as the final signed artifact without the full post-sign verification
and manifest generation steps.
