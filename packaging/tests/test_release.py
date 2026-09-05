import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
import tempfile
import subprocess
import json
import hashlib
import os
import tarfile

spec = importlib.util.spec_from_file_location("release", Path(__file__).parents[1] / "release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_tag_matches_workspace_version(self):
        release.validate_tag(f"v{release.version()}")
        for tag in ["main", "v0.0.0", f"v{release.version()}-rc.1", "../../evil"]:
            with self.assertRaises(ValueError):
                release.validate_tag(tag)

    def test_archive_rejects_arbitrary_targets(self):
        with self.assertRaises(ValueError):
            release.archive("../../evil")


class SignedReleaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "build").mkdir()
        (self.root / "crates/splice-proto/src").mkdir(parents=True)
        (self.root / "crates/splice-update").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.2.0"\n')
        (self.root / "crates/splice-proto/src/lib.rs").write_text("pub const PROTO_VERSION: u16 = 4;\n")
        key = self.root / "test-signing.pem"
        subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", key], check=True, capture_output=True)
        self.key = key.read_text()
        self.public = self.root / "test-public.pem"
        subprocess.run(["openssl", "pkey", "-in", key, "-pubout", "-out", self.public], check=True, capture_output=True)
        public = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])
        (self.root / "crates/splice-update/release-public-key.bin").write_bytes(public[-32:])
        for target in release.TARGETS:
            (self.root / f"build/splice-{target}.tar.gz").write_bytes(target.encode())
        self.environment = {"GITHUB_REF_NAME": "v1.2.0", "GITHUB_SHA": "b" * 40, "SPLICE_UPDATE_SIGNING_KEY": self.key, "SPLICE_BUILD_COMMIT": "b" * 40}
        self.enterContext(patch.object(release, "ROOT", self.root))
        self.enterContext(patch.dict(os.environ, self.environment))

    def test_manifest_signature_binds_every_archive_and_release_identity(self):
        release.make_manifest()
        path = self.root / "build/splice-update.json"
        manifest = json.loads(path.read_text())
        self.assertEqual(manifest["version"], "1.2.0")
        self.assertEqual(manifest["commit"], "b" * 40)
        self.assertEqual(manifest["protocol"], 4)
        self.assertEqual(set(manifest["assets"]), release.TARGETS)
        for asset in manifest["assets"].values():
            data = (self.root / "build" / asset["name"]).read_bytes()
            self.assertEqual(asset["size"], len(data))
            self.assertEqual(asset["sha256"], hashlib.sha256(data).hexdigest())
        command = ["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", self.public, "-in", path, "-sigfile", self.root / "build/splice-update.sig"]
        self.assertEqual(subprocess.run(command, capture_output=True).returncode, 0)
        path.write_text(path.read_text().replace('"version": "1.2.0"', '"version": "9.0.0"'))
        self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)

    def test_wrong_signing_key_and_missing_archive_refuse_publication(self):
        public_path = self.root / "crates/splice-update/release-public-key.bin"
        public = public_path.read_bytes()
        public_path.write_bytes(bytes(32))
        with self.assertRaisesRegex(ValueError, "does not match"):
            release.make_manifest()
        public_path.write_bytes(public)
        next((self.root / "build").glob("*.tar.gz")).unlink()
        with self.assertRaises(FileNotFoundError):
            release.make_manifest()

    def test_archive_checks_embedded_identity_and_preserves_executable(self):
        target = "x86_64-unknown-linux-gnu"
        binary = self.root / "target/release/splice"
        binary.parent.mkdir(parents=True)
        build = {"version": "1.2.0", "commit": "b" * 40, "target": target, "protocol": 4, "dirty": False}
        def executable():
            binary.write_text("#!/bin/sh\nprintf '%s\\n' '" + json.dumps(build) + "'\n")
            binary.chmod(0o755)
        executable()
        release.archive(target)
        with tarfile.open(self.root / f"build/splice-{target}.tar.gz") as archive:
            self.assertEqual(archive.getnames(), ["splice"])
            self.assertEqual(archive.extractfile("splice").read(), binary.read_bytes())
            self.assertEqual(archive.getmember("splice").mode, 0o755)
        for field, value in [("dirty", True), ("commit", "c" * 40), ("version", "9.0.0"), ("target", "aarch64-apple-darwin")]:
            previous = build[field]
            build[field] = value
            executable()
            with self.assertRaisesRegex(ValueError, "identity"):
                release.archive(target)
            build[field] = previous
