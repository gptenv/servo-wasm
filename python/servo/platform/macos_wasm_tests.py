# Copyright 2026 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

import os
import hashlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from . import macos, macos_wasm


class MacOSWorkerBootstrapTests(unittest.TestCase):
    def test_macos_bootstrap_runs_worker_setup_after_platform_bootstrap(self) -> None:
        events: list[str] = []
        platform = macos.MacOS("aarch64-apple-darwin")
        with (
            patch.object(macos.Base, "bootstrap", side_effect=lambda *args: events.append("base")),
            patch.object(macos, "bootstrap_worker_wasm", side_effect=lambda: events.append("worker")),
        ):
            platform.bootstrap(False, False, False, False, False, False)
            self.assertEqual(events, ["base", "worker"])
            events.clear()
            platform.bootstrap(False, False, True, False, False, False)
            self.assertEqual(events, ["base"])

    def test_default_macos_bootstrap_installs_gstreamer(self) -> None:
        platform = macos.MacOS("aarch64-apple-darwin")
        with (
            patch.object(macos.subprocess, "check_output", return_value=b"Homebrew bundle complete"),
            patch.object(platform, "_platform_bootstrap_gstreamer", return_value=False) as install,
        ):
            platform._platform_bootstrap(False, False)
            self.assertTrue(install.call_args.args[2])

    def test_bashrc_preserves_existing_exports_and_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            sdk = home / "opt/wasi-sdk-29.0-arm64-macos"
            bashrc = home / ".bashrc"
            original = (
                "# Existing configuration\n"
                'export SERVO_WASI_SDK_DIR="$HOME/opt/wasi-sdk-29.0-arm64-macos"\n'
                'export WASM_CXX_LIB_DIR="$SERVO_WASI_SDK_DIR/share/wasi-sysroot/lib/wasm32-wasi"\n'
                'export PATH="$SERVO_WASI_SDK_DIR/bin:$PATH"\n'
            )
            bashrc.write_text(original)
            macos_wasm.update_bashrc(home, sdk, "arm64")
            first = bashrc.read_text()
            macos_wasm.update_bashrc(home, sdk, "arm64")
            self.assertEqual(bashrc.read_text(), first)
            self.assertTrue(first.startswith(original))
            self.assertEqual(first.count("export SERVO_WASI_SDK_DIR="), 1)
            self.assertEqual(first.count("export WASM_CXX_LIB_DIR="), 1)
            result = subprocess.check_output(
                [
                    "/bin/bash",
                    "--noprofile",
                    "--norc",
                    "-c",
                    'source "$1"; source "$1"; printf "%s" "$PATH"',
                    "_",
                    str(bashrc),
                ],
                env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
                text=True,
            ).split(":")
            self.assertEqual(result[:2], [str(home / macos_wasm.TOOLCHAIN_REL), str(sdk / "bin")])
            self.assertEqual(result.count(str(home / macos_wasm.TOOLCHAIN_REL)), 1)
            self.assertEqual(result.count(str(sdk / "bin")), 1)
            bashrc.write_text(first + 'export PATH="$SERVO_WASI_SDK_DIR/bin:$PATH"\n')
            macos_wasm.update_bashrc(home, sdk, "arm64")
            self.assertTrue(bashrc.read_text().rstrip().endswith(macos_wasm.END))
            self.assertIn('export PATH="$SERVO_WASI_SDK_DIR/bin:$PATH"', bashrc.read_text())

    def test_bashrc_exports_existing_unexported_values(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            sdk = home / "opt/wasi-sdk-29.0-arm64-macos"
            bashrc = home / ".bashrc"
            bashrc.write_text('SERVO_WASI_SDK_DIR="$HOME/custom-sdk"\nWASM_CXX_LIB_DIR="$HOME/custom-lib"\n')
            macos_wasm.update_bashrc(home, sdk, "arm64")
            output = subprocess.check_output(
                ["/bin/bash", "--noprofile", "--norc", "-c", 'source "$1"; /usr/bin/env', "_", str(bashrc)],
                env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
                text=True,
            )
            self.assertIn(f"SERVO_WASI_SDK_DIR={home}/custom-sdk\n", output)
            self.assertIn(f"WASM_CXX_LIB_DIR={home}/custom-lib\n", output)

    def test_rejects_incomplete_markers_and_unmanaged_compilers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            bashrc = home / ".bashrc"
            bashrc.write_text(macos_wasm.BEGIN + "\n")
            with self.assertRaisesRegex(RuntimeError, "markers"):
                macos_wasm.update_bashrc(home, home / "sdk", "arm64")
            self.assertEqual(bashrc.read_text(), macos_wasm.BEGIN + "\n")
            wrapper = home / macos_wasm.TOOLCHAIN_REL / "clang"
            wrapper.parent.mkdir(parents=True)
            wrapper.write_text("user compiler\n")
            with self.assertRaisesRegex(RuntimeError, "unmanaged compiler"):
                macos_wasm.install_wrappers(home)
            self.assertEqual(wrapper.read_text(), "user compiler\n")

    def test_rejects_download_with_wrong_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {}, clear=True):
            home = Path(directory)

            def download(_description: str, _url: str, destination: str) -> None:
                Path(destination).write_bytes(b"not the official SDK")

            with patch.object(macos_wasm.util, "download_file", side_effect=download):
                with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                    macos_wasm.install_sdk(home, "arm64")
            self.assertFalse((home / "opt/wasi-sdk-29.0-arm64-macos").exists())

    def test_install_and_reuse_verified_sdk(self) -> None:
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"PATH": "/usr/bin:/bin"}, clear=True):
            root = Path(directory)
            home = root / "home"
            name = "wasi-sdk-29.0-arm64-macos"
            sdk_fixture = root / name
            for file in (
                "bin/clang",
                "bin/clang++",
                "share/wasi-sysroot/lib/wasm32-wasi/libc.a",
                "share/wasi-sysroot/lib/wasm32-wasi/libc++.a",
                "share/wasi-sysroot/lib/wasm32-wasi/libc++abi.a",
                "share/wasi-sysroot/lib/wasm32-wasi/libwasi-emulated-getpid.a",
                "share/wasi-sysroot/include/wasm32-wasi/stdio.h",
                "share/wasi-sysroot/include/wasm32-wasi/c++/v1/cstdio",
            ):
                target = sdk_fixture / file
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text("fixture")
            archive = root / "fixture.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                tar.add(sdk_fixture, arcname=name)
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()

            def download(_description: str, _url: str, destination: str) -> None:
                shutil.copyfile(archive, destination)

            with (
                patch.dict(macos_wasm.SDK_SHA256, {"arm64": digest}),
                patch.object(macos_wasm.util, "download_file", side_effect=download) as fetch,
            ):
                sdk = macos_wasm.install_sdk(home, "arm64")
                self.assertEqual(macos_wasm.install_sdk(home, "arm64"), sdk)
                fetch.assert_called_once()
            macos_wasm.validate_sdk(sdk)

    def test_installs_missing_rust_target_and_sources_bashrc(self) -> None:
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.dict(os.environ, {"PATH": "/usr/bin:/bin"}, clear=True),
            patch.object(Path, "home", return_value=Path(directory)),
            patch.object(macos_wasm.platform, "machine", return_value="arm64"),
            patch.object(macos_wasm, "install_sdk", return_value=Path(directory) / "sdk"),
            patch.object(macos_wasm.subprocess, "check_output", return_value="aarch64-apple-darwin\n"),
            patch.object(macos_wasm.subprocess, "run") as run,
        ):
            macos_wasm.bootstrap_worker_wasm()
            self.assertEqual(run.call_args_list[0].args[0], ["rustup", "target", "add", "wasm32-unknown-unknown"])
            self.assertIn('source "$1"', run.call_args_list[1].args[0][4])
            self.assertTrue((Path(directory) / ".bashrc").is_file())

    def test_bootstrap_sources_bashrc_and_can_run_twice(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            sdk = home / "sdk"
            for file in (
                "bin/clang",
                "bin/clang++",
                "share/wasi-sysroot/lib/wasm32-wasi/libc.a",
                "share/wasi-sysroot/lib/wasm32-wasi/libc++.a",
                "share/wasi-sysroot/lib/wasm32-wasi/libc++abi.a",
                "share/wasi-sysroot/lib/wasm32-wasi/libwasi-emulated-getpid.a",
                "share/wasi-sysroot/include/wasm32-wasi/stdio.h",
                "share/wasi-sysroot/include/wasm32-wasi/c++/v1/cstdio",
            ):
                target = sdk / file
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text("fixture")
            fake_bin = home / "test-bin"
            fake_bin.mkdir()
            rustup = fake_bin / "rustup"
            rustup.write_text('#!/bin/bash\nprintf "wasm32-unknown-unknown\\n"\n')
            rustup.chmod(0o755)
            env = {"HOME": str(home), "PATH": f"{fake_bin}:/usr/bin:/bin", "SERVO_WASI_SDK_DIR": str(sdk)}
            with (
                patch.dict(os.environ, env, clear=True),
                patch.object(macos_wasm.platform, "machine", return_value="arm64"),
            ):
                macos_wasm.bootstrap_worker_wasm()
                original = (home / ".bashrc").read_text()
                macos_wasm.bootstrap_worker_wasm()
                self.assertEqual((home / ".bashrc").read_text(), original)
                self.assertEqual(os.environ["PATH"].split(":").count(str(home / macos_wasm.TOOLCHAIN_REL)), 1)

    @unittest.skipUnless(sys.platform == "darwin", "macOS host compiler required")
    def test_compilers_route_native_and_wasm_targets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            sdk = home / "sdk"
            (sdk / "bin").mkdir(parents=True)
            for name in ("clang", "clang++"):
                compiler = sdk / "bin" / name
                compiler.write_text('#!/bin/bash\nprintf "%s\\n" "$0" "$@"\n')
                compiler.chmod(0o755)
            wrappers = macos_wasm.install_wrappers(home)
            env = {**os.environ, "SERVO_WASI_SDK_DIR": str(sdk)}
            for name, language in (("clang", "c"), ("clang++", "c++")):
                subprocess.run(
                    [str(wrappers / name), "-x", language, "-fsyntax-only", "-"],
                    input="int value;\n",
                    text=True,
                    env=env,
                    check=True,
                )
                args = subprocess.check_output(
                    [str(wrappers / name), "--target=wasm32-unknown-unknown", "-c", "x"], env=env, text=True
                ).splitlines()
                self.assertEqual(args[0], str(sdk / "bin" / name))
                self.assertIn(f"--sysroot={sdk}/share/wasi-sysroot", args)
                if name == "clang++":
                    self.assertIn(f"{sdk}/share/wasi-sysroot/include/wasm32-wasi/c++/v1", args)


if __name__ == "__main__":
    unittest.main()
