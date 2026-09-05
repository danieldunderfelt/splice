# Configure signed releases

These steps configure automated releases for Linux x86-64, macOS Apple Silicon, and macOS Intel.
The workflows build and test native binaries before packaging them. macOS releases require
Developer ID signing and successful notarization.

## Set the secrets

1. Export your **Developer ID Application** certificate and private key from Keychain Access as a password-protected `.p12` file.
2. Authenticate GitHub CLI with access to `danieldunderfelt/splice`.
3. Upload the certificate from your Mac:

```sh
base64 -i developer-id.p12 | gh secret set SPLICE_MAC_CERT_P12 --repo danieldunderfelt/splice
```

4. Set each remaining Apple secret. Each command prompts for its value without putting it in shell history:

```sh
gh secret set SPLICE_MAC_CERT_PASSWORD --repo danieldunderfelt/splice
gh secret set SPLICE_CODESIGN_IDENTITY --repo danieldunderfelt/splice
gh secret set SPLICE_APPLE_ID --repo danieldunderfelt/splice
gh secret set SPLICE_APPLE_TEAM_ID --repo danieldunderfelt/splice
gh secret set SPLICE_APPLE_APP_PASSWORD --repo danieldunderfelt/splice
```

5. From the KDE checkout containing the generated update key, upload the PEM directly:

```sh
gh secret set SPLICE_UPDATE_SIGNING_KEY --repo danieldunderfelt/splice < .git/splice-update-signing-key.pem
```

6. Back up that PEM in your password manager or encrypted key storage. GitHub does not let you retrieve a stored secret.
7. Create a GitHub Actions environment named `release` in the repository settings.

See the [secret reference](release-secrets.md) for exact formats. Apple describes the notarization
requirements in [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution).

## Publish a version

Complete the [native raw-input acceptance checks](raw-input-macos-handoff.md) before publishing 1.2.0.

1. Update `workspace.package.version` in `Cargo.toml` and refresh `Cargo.lock`.
2. Run the checks:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p splice-core --release --test engine_e2e --locked
python3 -m unittest discover -s packaging/tests -v
```

3. Commit the release changes and push them.
4. Tag that commit with the exact stable workspace version, such as `v1.2.0`, and push the tag.
5. Check the **Release** workflow. All three native test jobs and all package jobs must pass before publication.

The release contains three `splice-TARGET.tar.gz` archives, `splice-update.json`, and its detached
Ed25519 signature, `splice-update.sig`. The publisher signs a manifest containing the version,
commit, KVM protocol, and each archive's exact size and SHA-256 digest.
The release job fails on missing secrets, mismatched tags, dirty binaries, wrong build identities,
signing errors, notarization failures, or a manifest signing key that differs from the pinned key.

The workflow can also be dispatched against an existing release tag. It refuses branch names and
prerelease tags. An already published release is not overwritten.

## Bootstrap existing computers

Protocol 2 clients do not include the updater. Install this version manually on every computer once.
Splice 1.2.0 uses KVM protocol 4 and refuses older KVM protocols.

On Linux, build and use the per-user installer:

```sh
cargo build -p splice-app --release --locked
packaging/linux/install.sh
systemctl --user restart app-splice.service
```

On macOS, install the signed, notarized `Splice.app` from the release archive in `/Applications`.
If replacing a bundle signed with `Splice Dev`, grant Accessibility permission to the Developer ID
signed app. Keep the same Developer ID team and bundle identifier for later releases.

After this bootstrap, use the [Updates panel](updates.md) for newer published versions.
A local development build with version `1.2.0` cannot install another `1.2.0` build through OTA.

## Build from a source archive

A source archive has no Git metadata. Set both build metadata variables explicitly:

```sh
SPLICE_BUILD_COMMIT=FULL_40_CHARACTER_COMMIT SPLICE_BUILD_DIRTY=false cargo build -p splice-app --release --locked
```

Use the commit that produced the unmodified archive. For a checkout with local changes, let the
build script detect the dirty state. Inspect the embedded identity with `splice --version-json`.
