#!/usr/bin/env bash
# Produce the source tarballs the spec expects and build the RPM locally.
# Needs: rpm-build, rpmdevtools, cargo. Output lands in ~/rpmbuild/RPMS.
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(sed -n 's/^Version:[[:space:]]*//p' "${repository_root}/packaging/rpm/splice.spec")"
sources="${HOME}/rpmbuild/SOURCES"
mkdir -p "$sources"

git -C "$repository_root" archive --format=tar.gz --prefix="splice-${version}/" \
    -o "${sources}/splice-${version}.tar.gz" HEAD

vendor_directory="$(mktemp -d)"
trap 'rm -rf "$vendor_directory"' EXIT
(cd "$repository_root" && cargo vendor --locked "${vendor_directory}/vendor" >/dev/null)
tar -C "$vendor_directory" -cJf "${sources}/splice-${version}-vendor.tar.xz" vendor

rpmbuild -ba "${repository_root}/packaging/rpm/splice.spec"
