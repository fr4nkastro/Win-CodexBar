# SignPath Code Signing Setup

This guide configures SignPath for the GitHub-hosted release path of
Win-CodexBar.

## Current onboarding state

The production release-signing policy currently requires SignPath-side
completion of the Release certificate 2026 CSR. Keep the production tag
workflow fail-closed until the certificate is issued and the policy becomes
valid.

v0.60.3 was released before this integration and remains immutable. The first
production-signed release is the next normal version.

## Repository secrets

In nesszer/Win-CodexBar, open Settings -> Secrets and variables -> Actions and
add:

| Secret | Value |
|---|---|
| SIGNPATH_API_TOKEN | API token for a SignPath submitter |

The workflow pins organization 9f67194d-974c-4e9c-bac7-20248e8b6c39, project
Win-CodexBar, release-signing for production, test-signing for the manual test,
and codexbar-installer. Do not add mutable organization, project, or
policy-slug secrets.

After each certificate is issued, set the corresponding nonsecret Actions
variable: SIGNPATH_RELEASE_CERT_THUMBPRINT for production and, optionally,
SIGNPATH_TEST_CERT_THUMBPRINT for the test policy. The production workflow
fails closed if its certificate thumbprint variable is missing and the
finalizer checks the thumbprint on all three signed executables.

## Artifact configuration

Upload the ZIP containing .signpath/artifact-configuration.xml to the
Win-CodexBar SignPath project as codexbar-installer.

The configuration signs these files:

- CodexBar-${version}-Setup.exe
- CodexBar-${version}-portable.exe
- codexbar-cli.exe nested inside
  CodexBarCLI-v${version}-windows-x64.zip

The outer GitHub Actions artifact contains exactly the first two executables
and the CLI ZIP. The CLI ZIP is the third top-level file; its nested executable
is signed by the artifact configuration.

## Trusted build system

1. Add the predefined GitHub.com trusted build system to the SignPath
   organization.
2. Link it to the Win-CodexBar project.
3. Install the SignPath GitHub App and allow access to
   nesszer/Win-CodexBar.
4. Keep origin verification and trusted build verification enabled.
5. Keep one manual approval for release-signing with the authorized approver.
6. After the test run, inspect SignPath's verified origin value before narrowing
   the policy's allowed branch names. Do not guess it from the tag name.

SignPath's GitHub connector must observe the build, artifact upload, and
GitHub-hosted runner provenance. Do not route the production artifact through
CircleCI or a proxy workflow.

## Manual test

After test-signing is available, run the SignPath Test workflow from the
Actions tab:

1. Select SignPath Test.
2. Choose Run workflow.
3. Set source_ref to main (or another reviewed ref containing the workflow).
4. Enter a canonical version tag matching the source version files, such as
   v0.60.3 while main still reports version 0.60.3.
5. Review the retained codexbar-signed-test artifact.
6. Verify the installer and portable signatures with:

~~~powershell
Get-AuthenticodeSignature .\CodexBar-0.60.3-Setup.exe
Get-AuthenticodeSignature .\CodexBar-0.60.3-portable.exe
~~~

Extract the retained CLI ZIP and run Get-AuthenticodeSignature against
codexbar-cli.exe. The workflow never creates a GitHub Release and fails if
SignPath does not return exactly the expected three files. The selected
version tag is used as the artifact version label; the build itself comes from
source_ref so the workflow and finalizer are present.

Do not create a throwaway tag or temporarily change project versions for this
test. The manual workflow is the controlled nonpublishing test path.

## Production release

Before creating the first signed production tag, confirm:

- Release certificate 2026 is issued and usable.
- release-signing is valid.
- GitHub.com trusted build verification and origin verification pass.
- The SignPath GitHub App can read the repository.
- The repository secrets are present.
- SIGNPATH_RELEASE_CERT_THUMBPRINT is set to the issued production
  certificate thumbprint.
- The manual test proves the nested CLI signature.
- CircleCI's tag-release jobs are absent from the active configuration.

The production workflow waits for SignPath, verifies the returned Authenticode
signatures, computes SHA-256 sidecars from signed bytes, validates the exact
six-asset bundle, and creates only a draft release. A maintainer publishes the
draft after review.
