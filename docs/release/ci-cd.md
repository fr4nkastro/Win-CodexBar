# Win-CodexBar CI and release delivery

## Responsibilities

CircleCI is the primary hosted Windows validation system. Its
pr-check workflow runs the canonical scripts/local-check.ps1 -Slice ci contract
for pull requests and protected branch pushes. CircleCI does not build or
publish release tags.

GitHub Actions is the sole canonical release producer. The tag workflow in
.github/workflows/release.yml runs on a GitHub-hosted Windows runner, because
SignPath's GitHub trusted-build integration verifies that the build and the
uploaded signing artifact came from GitHub Actions.

The manual .github/workflows/signpath-test.yml workflow exercises the same
three-file signing bundle with the fixed test-signing policy. It retains the
verified result as a workflow artifact and never creates or modifies a GitHub
Release.

## Production release flow

A maintainer creates a protected canonical tag such as v0.60.4 on main. The
tag workflow then performs this sequence:

1. Check out the exact tag and freeze its full 40-character commit SHA.
2. Run scripts/release-preflight.ps1 to validate the canonical repository,
   tag/SHA identity, main ancestry, and every committed version file.
3. Fail immediately if the required SignPath credentials are absent.
4. Build fresh unsigned artifacts with the existing Windows release builder.
5. Create a signing input containing exactly these top-level files:
   - CodexBar-X.Y.Z-Setup.exe
   - CodexBar-X.Y.Z-portable.exe
   - CodexBarCLI-vX.Y.Z-windows-x64.zip
6. Upload that directory as a GitHub Actions artifact and submit it to the
   pinned codexbar-installer configuration and release-signing policy.
7. Wait for SignPath to finish. Denial, timeout, approval failure, origin
   verification failure, missing output, or any malformed output fails the job.
8. Verify Authenticode on both top-level executables and on codexbar-cli.exe
   inside the returned CLI ZIP.
9. Build a new final bundle only from the verified SignPath files, compute all
   three SHA-256 sidecars after signing, and regenerate release-manifest.json.
10. Validate the exact six publishable assets, hashes, byte counts, and sidecars.
11. Run scripts/publish-github-release.ps1, which creates or updates a draft
    release without replacing divergent assets. A maintainer publishes the
    draft manually after review.

The unsigned build tree, SignPath output, and final bundle are separate. There
is no unsigned fallback after a signing failure.

The final public asset set is:

- CodexBar-X.Y.Z-Setup.exe and its .sha256 sidecar
- CodexBar-X.Y.Z-portable.exe and its .sha256 sidecar
- CodexBarCLI-vX.Y.Z-windows-x64.zip and its .sha256 sidecar

release-manifest.json is retained for verification but is not a publishable
release asset.

## SignPath onboarding boundary

The repository wiring can be reviewed before production signing is enabled.
Do not create a production tag until the SignPath project has a valid
release-signing policy, an issued production certificate, the GitHub.com
trusted build system linked to the project, the SignPath GitHub App installed
with repository access, and the repository secrets configured.

The production policy is intentionally fail-closed. v0.60.3 remains the
immutable unsigned release created before this cutover. The first signed
production release is the next normal version, such as v0.60.4.

After the manual test-signing run, inspect the origin value reported by
SignPath before narrowing the policy's allowed branch names. Do not guess the
branch value from the tag name.

## Local checks

Run the dependency-free release checks:

~~~powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\release-pipeline.tests.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\install-release-prerequisites.ps1 -AssertOnly
~~~

The build and preflight helpers accept explicit tag and SHA values:

~~~powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\release-preflight.ps1 -Tag vX.Y.Z -Sha <full-40-character-sha>
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\circleci-release-build.ps1 -Tag vX.Y.Z -Sha <full-40-character-sha>
~~~

The second script retains its historical filename for compatibility; the
release workflow passes all identity values explicitly and does not depend on
CircleCI environment variables.

## Administrator setup

Configure SIGNPATH_API_TOKEN as the only SignPath GitHub Actions repository
secret. The workflow pins the reviewed organization ID, project slug,
release-signing/test-signing policies, and codexbar-installer configuration in
source. After SignPath issues the certificates, set the nonsecret Actions
variable SIGNPATH_RELEASE_CERT_THUMBPRINT; production verification requires
it and compares all three signed executables against it.
- GITHUB_TOKEN is provided by GitHub Actions

The workflow pins release-signing, test-signing, and codexbar-installer in
reviewed source. The policy slug is not selected by a mutable secret.

Keep CircleCI credentials and release contexts disabled for tag publication.
CircleCI only needs its existing validation configuration and CI budget
settings. Protect main and the canonical vX.Y.Z tag namespace so only
authorized maintainers can create release tags.
