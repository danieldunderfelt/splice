import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
TARGETS = {"x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-apple-darwin"}


def version():
    with (ROOT / "Cargo.toml").open("rb") as stream:
        return tomllib.load(stream)["workspace"]["package"]["version"]


def validate_tag(tag):
    if tag != f"v{version()}" or not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("Release tag must exactly match the stable workspace version")


def archive(target):
    if target not in TARGETS:
        raise ValueError("Unsupported release target")
    build = json.loads(subprocess.check_output([ROOT / "target/release/splice", "--version-json"]))
    commit = os.environ["SPLICE_BUILD_COMMIT"]
    if build["dirty"] or build["target"] != target or build["version"] != version() or build["commit"] != commit:
        raise ValueError("Binary build identity does not match this release")
    source = ROOT / "build/Splice.app" if "apple" in target else ROOT / "target/release/splice"
    destination = ROOT / "build" / f"splice-{target}.tar.gz"
    destination.parent.mkdir(exist_ok=True)
    with tarfile.open(destination, "w:gz", format=tarfile.USTAR_FORMAT) as output:
        output.add(source, arcname=source.name)


def make_manifest():
    validate_tag(os.environ["GITHUB_REF_NAME"])
    commit = os.environ["GITHUB_SHA"]
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise ValueError("Invalid release commit")
    protocol_source = (ROOT / "crates/splice-proto/src/lib.rs").read_text()
    protocol = int(re.search(r"pub const PROTO_VERSION: u16 = (\d+);", protocol_source)[1])
    assets = {}
    for target in sorted(TARGETS):
        path = ROOT / "build" / f"splice-{target}.tar.gz"
        payload = path.read_bytes()
        if not 0 < len(payload) <= 256 * 1024 * 1024:
            raise ValueError("Invalid release archive size")
        assets[target] = {"name": path.name, "sha256": hashlib.sha256(payload).hexdigest(), "size": len(payload)}
    path = ROOT / "build/splice-update.json"
    path.write_text(json.dumps({"schema": 1, "version": version(), "commit": commit, "protocol": protocol, "assets": assets}, indent=2) + "\n")
    with tempfile.TemporaryDirectory() as directory:
        key = Path(directory) / "signing.pem"
        descriptor = os.open(key, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        with os.fdopen(descriptor, "w") as stream:
            stream.write(os.environ["SPLICE_UPDATE_SIGNING_KEY"])
        public = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])
        if public[-32:] != (ROOT / "crates/splice-update/release-public-key.bin").read_bytes():
            raise ValueError("Signing key does not match the key trusted by Splice clients")
        subprocess.run(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", path, "-out", ROOT / "build/splice-update.sig"], check=True)


if __name__ == "__main__":
    if sys.argv[1:] == ["manifest"]:
        make_manifest()
    elif len(sys.argv) == 3 and sys.argv[1] == "validate":
        validate_tag(sys.argv[2])
    elif len(sys.argv) == 3 and sys.argv[1] == "archive":
        archive(sys.argv[2])
    else:
        raise SystemExit("usage: release.py validate TAG | archive TARGET | manifest")
