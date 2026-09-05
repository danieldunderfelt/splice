# Release secrets

The [release workflow](../.github/workflows/release.yml) uses these GitHub Actions secrets in `danieldunderfelt/splice`.
Store all seven as repository secrets. The macOS package jobs run outside the `release` environment.
The manifest signing key can instead be an environment secret in `release`.

| Secret | Value | Used for |
|---|---|---|
| `SPLICE_MAC_CERT_P12` | Base64 of a password-protected `.p12` export containing the Developer ID Application certificate and its private key | Import into a temporary macOS keychain |
| `SPLICE_MAC_CERT_PASSWORD` | Password for that `.p12` export | Unlock the certificate |
| `SPLICE_CODESIGN_IDENTITY` | Full certificate name, such as `Developer ID Application: Your Name (TEAMID1234)`, or its SHA-1 fingerprint | Sign `dev.splice.app` with hardened runtime and a timestamp |
| `SPLICE_APPLE_ID` | Apple Account email with access to the developer team | Authenticate to Apple's notary service |
| `SPLICE_APPLE_TEAM_ID` | Ten-character Apple Developer team identifier | Select the signing team |
| `SPLICE_APPLE_APP_PASSWORD` | App-specific password for that Apple Account | Authenticate `notarytool` |
| `SPLICE_UPDATE_SIGNING_KEY` | Raw Ed25519 private key in PEM format, including the BEGIN and END lines | Sign the update manifest for every platform |

`SPLICE_UPDATE_SIGNING_KEY` is separate from the Apple certificate. Its public key must match
[`release-public-key.bin`](../crates/splice-update/release-public-key.bin), which is embedded in every client.
A private key for the currently pinned public key was generated in this working checkout at
`.git/splice-update-signing-key.pem`. It is excluded from Git because it lives in `.git`.
The release script refuses a key that does not match the pinned public key.
Replacing the pinned key requires an explicit client trust migration or manual installation.

GitHub provides `GITHUB_TOKEN` for publishing. No personal access token is required by the workflow.
The publisher requests `contents: write`; build and package jobs have read-only repository access.

See [Configure signed releases](releasing.md) for setup steps. GitHub documents repository and
environment secrets in [Using secrets in GitHub Actions](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets).
