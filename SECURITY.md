# Security policy

## Supported versions

Only the latest release of Win-CodexBar is supported. Security fixes are made
against the current release line; older releases and the historical upstream
macOS project are not patched.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting: open the
[Security tab](https://github.com/nesszer/Win-CodexBar/security) and click
"Report a vulnerability".

Do **not** open a public GitHub issue for a vulnerability. Public issues and
the bug report template are for non-security problems only.

Win-CodexBar handles provider cookies, OAuth tokens, and API keys locally, so
reports touching that surface — credential extraction, storage, redaction, or
leakage — are taken seriously. Please keep report details private and do not
paste secrets, cookies, or tokens into any report.
