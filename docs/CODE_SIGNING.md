# Code signing policy

Win-CodexBar uses SignPath.io and the SignPath Foundation certificate for
Windows release signing.

> **Status:** The repository wiring is prepared, but the production policy is
> blocked until SignPath finishes issuing the Release certificate 2026 and the
> release-signing policy becomes valid. The release workflow fails closed while
> that onboarding is incomplete. v0.60.3 remains the immutable unsigned release;
> the first signed production release is the next normal version.

## Project identity

- Project: Win-CodexBar
- Source: https://github.com/nesszer/Win-CodexBar
- Releases: https://github.com/nesszer/Win-CodexBar/releases
- License: MIT

## Build and signing system

CircleCI owns PR and protected-branch validation through
.circleci/config.yml. GitHub Actions owns canonical tag releases through
.github/workflows/release.yml so SignPath can verify the GitHub-hosted build
and GitHub Actions artifact provenance.

The release workflow builds three top-level signing inputs:

- CodexBar-<version>-Setup.exe
- CodexBar-<version>-portable.exe
- CodexBarCLI-v<version>-windows-x64.zip

The SignPath artifact configuration signs both top-level executables and
codexbar-cli.exe nested inside the CLI ZIP. The workflow waits for the
release-signing request to complete, verifies all three signed objects, creates
the final six assets from signed bytes, and creates only a draft GitHub Release.
A maintainer publishes the draft after review.

The manual SignPath Test workflow uses test-signing and retains its verified
bundle as a workflow artifact. It never publishes a release.

## Roles

| Role | Responsibility |
|---|---|
| Author | Finesssee |
| Reviewer | Finesssee |
| Approver | Finesssee (@Finesssee) |

Each production signing request requires the configured manual approval.
Unsigned fallback is disabled.

## Privacy

See [docs/PRIVACY.md](PRIVACY.md) for the project's privacy policy.

## Notes

Certificates are issued in the SignPath Foundation's name, so signed binaries
show SignPath Foundation as the publisher. See [.signpath/SETUP.md](../.signpath/SETUP.md)
for the onboarding and trusted-build checklist.
