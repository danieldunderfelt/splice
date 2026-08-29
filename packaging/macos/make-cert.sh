#!/usr/bin/env bash
set -euo pipefail

certificate_name="Splice Dev"
login_keychain="${HOME}/Library/Keychains/login.keychain-db"

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf '%s\n' 'make-cert.sh must run on macOS.' >&2
    exit 1
fi

printf 'Checking for the "%s" code-signing identity...\n' "$certificate_name"
if security find-identity -p codesigning 2>/dev/null | grep -F "$certificate_name" >/dev/null; then
    printf 'The "%s" identity already exists. Nothing to do.\n' "$certificate_name"
    exit 0
fi

temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/splice-dev-cert.XXXXXX")"

cleanup() {
    rm -rf -- "$temporary_directory"
}
trap cleanup EXIT

private_key="${temporary_directory}/splice-dev.key"
certificate_pem="${temporary_directory}/splice-dev.pem"
certificate_der="${temporary_directory}/splice-dev.cer"
pkcs12_file="${temporary_directory}/splice-dev.p12"
pkcs12_password="$(openssl rand -hex 32)"
pkcs12_options=()

if openssl pkcs12 -help 2>&1 | grep -q -- '-legacy'; then
    pkcs12_options+=(-legacy)
fi

printf 'Generating the "%s" private key and certificate...\n' "$certificate_name"
openssl genrsa -out "$private_key" 3072
openssl req \
    -new \
    -x509 \
    -sha256 \
    -days 3650 \
    -key "$private_key" \
    -out "$certificate_pem" \
    -subj "/CN=${certificate_name}/O=Splice/OU=Development" \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,digitalSignature,keyCertSign' \
    -addext 'extendedKeyUsage=critical,codeSigning'
openssl x509 -in "$certificate_pem" -outform DER -out "$certificate_der"

printf 'Exporting the certificate and key as PKCS#12...\n'
openssl pkcs12 \
    -export \
    "${pkcs12_options[@]}" \
    -out "$pkcs12_file" \
    -inkey "$private_key" \
    -in "$certificate_pem" \
    -name "$certificate_name" \
    -passout "pass:${pkcs12_password}"

printf 'Importing the identity into the login keychain...\n'
security import "$pkcs12_file" \
    -k "$login_keychain" \
    -P "$pkcs12_password" \
    -T /usr/bin/codesign

if security add-trusted-cert \
    -d \
    -r trustRoot \
    -k "$login_keychain" \
    "$certificate_der"; then
    printf 'The "%s" certificate is trusted in the login keychain.\n' "$certificate_name"
else
    printf '%s\n' 'Automatic trust setup needs approval in Keychain Access.' >&2
    cat >&2 <<'EOF'
Open Keychain Access and select the login keychain. Find "Splice Dev", open the
certificate, expand Trust, set "When using this certificate" to "Always Trust",
and close the certificate window. Authenticate if macOS asks for your password.
EOF
fi

if security find-identity -p codesigning 2>/dev/null | grep -F "$certificate_name" >/dev/null; then
    printf 'Created the "%s" code-signing identity.\n' "$certificate_name"
else
    printf 'The certificate was imported, but the identity is not available yet.\n' >&2
    printf 'Check the login keychain and run security find-identity -p codesigning.\n' >&2
    exit 1
fi
