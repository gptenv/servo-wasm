# Copyright 2026 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

"""Set up the macOS host and WASI compilers for the Worker build."""

import hashlib
import os
import platform
import re
import shlex
import subprocess
import tempfile
from pathlib import Path

from .. import util

SDK_VERSION = "29.0"
SDK_SHA256 = {
    "arm64": "e11552913e3f99e834d7fe7da1bd081abaf764759ed76b6097a34c63fc83665e",
    "x86_64": "d0de2fd3ea5c57060efa87e4356c164bec3689972f2386f0c9a89c58e10cec8d",
}
BEGIN = "# >>> servo-wasm macOS bootstrap >>>"
END = "# <<< servo-wasm macOS bootstrap <<<"
WRAPPER_MARKER = "# Managed by Servo macOS bootstrap."
TOOLCHAIN_REL = ".local/share/servo-wasm/toolchain/bin"
PROJECT_ROOT = Path(__file__).resolve().parents[3]


def validate_sdk(sdk: Path) -> None:
    for relative in (
        "bin/clang",
        "bin/clang++",
        "share/wasi-sysroot/lib/wasm32-wasi/libc.a",
        "share/wasi-sysroot/lib/wasm32-wasi/libc++.a",
        "share/wasi-sysroot/lib/wasm32-wasi/libc++abi.a",
        "share/wasi-sysroot/lib/wasm32-wasi/libwasi-emulated-getpid.a",
        "share/wasi-sysroot/include/wasm32-wasi/stdio.h",
        "share/wasi-sysroot/include/wasm32-wasi/c++/v1/cstdio",
    ):
        if not (sdk / relative).is_file():
            raise RuntimeError(f"WASI SDK at {sdk} is incomplete: missing {relative}")


def install_sdk(home: Path, arch: str) -> Path:
    if arch not in SDK_SHA256:
        raise RuntimeError(f"Unsupported macOS architecture for WASI SDK: {arch}")
    name = f"wasi-sdk-{SDK_VERSION}-{arch}-macos"
    sdk = Path(os.environ.get("SERVO_WASI_SDK_DIR") or home / "opt" / name).expanduser()
    if sdk.exists():
        validate_sdk(sdk)
        return sdk

    sdk.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="servo-wasi-", dir=sdk.parent) as temp:
        temporary = Path(temp)
        archive = temporary / f"{name}.tar.gz"
        url = f"https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-29/{archive.name}"
        util.download_file("WASI SDK 29", url, str(archive))
        with archive.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if digest != SDK_SHA256[arch]:
            raise RuntimeError(f"WASI SDK checksum mismatch for {archive.name}")
        subprocess.run(["tar", "-xzf", str(archive), "-C", str(temporary)], check=True)
        extracted = temporary / name
        validate_sdk(extracted)
        extracted.rename(sdk)
    return sdk


def install_wrappers(home: Path) -> Path:
    directory = home / TOOLCHAIN_REL
    directory.mkdir(parents=True, exist_ok=True)
    contents = (Path(__file__).parent / "worker-clang-macos.sh").read_text()
    for name in ("clang", "clang++"):
        target = directory / name
        if target.is_symlink() or (target.exists() and WRAPPER_MARKER not in target.read_text()):
            raise RuntimeError(f"Refusing to overwrite an unmanaged compiler at {target}")
        if not target.exists() or target.read_text() != contents:
            target.write_text(contents)
        target.chmod(0o755)
    return directory


def update_bashrc(home: Path, sdk: Path, arch: str) -> Path:
    bashrc = home / ".bashrc"
    original = bashrc.read_text() if bashrc.exists() else ""
    if (BEGIN in original) != (END in original) or original.count(BEGIN) > 1 or original.count(END) > 1:
        raise RuntimeError(f"Incomplete or duplicate Servo bootstrap markers in {bashrc}")
    if BEGIN in original:
        before, after = original.split(BEGIN, 1)
        _, after = after.split(END, 1)
        outside = before + after.removeprefix("\n")
    else:
        outside = original

    before = outside
    if before and not before.endswith("\n"):
        before += "\n"
    if before and not before.endswith("\n\n"):
        before += "\n"

    lines = [BEGIN]
    if not re.search(r"(?m)^\s*export\s+SERVO_WASI_SDK_DIR(?:\s*=|\s*(?:#.*)?$)", outside):
        default = home / "opt" / f"wasi-sdk-{SDK_VERSION}-{arch}-macos"
        value = f'"$HOME/opt/{default.name}"' if sdk == default else shlex.quote(str(sdk))
        if re.search(r"(?m)^\s*SERVO_WASI_SDK_DIR\s*=", outside):
            lines.append("export SERVO_WASI_SDK_DIR")
        else:
            lines.append(f"export SERVO_WASI_SDK_DIR={value}")
    if not re.search(r"(?m)^\s*export\s+WASM_CXX_LIB_DIR(?:\s*=|\s*(?:#.*)?$)", outside):
        if re.search(r"(?m)^\s*WASM_CXX_LIB_DIR\s*=", outside):
            lines.append("export WASM_CXX_LIB_DIR")
        else:
            lines.append('export WASM_CXX_LIB_DIR="$SERVO_WASI_SDK_DIR/share/wasi-sysroot/lib/wasm32-wasi"')
    # Put our host-aware wrappers before the WASI-only clang even when an
    # earlier user PATH assignment runs again on a subsequent source.
    lines.extend(
        [
            f'_servo_worker_bin="$HOME/{TOOLCHAIN_REL}"',
            '_servo_path_left="$PATH"',
            '_servo_path_rest=""',
            '_servo_separator=""',
            "while :; do",
            '  _servo_part="${_servo_path_left%%:*}"',
            '  if [[ "$_servo_part" != "$_servo_worker_bin" && "$_servo_part" != "$SERVO_WASI_SDK_DIR/bin" ]]; then',
            '    _servo_path_rest="${_servo_path_rest}${_servo_separator}${_servo_part}"',
            '    _servo_separator=":"',
            "  fi",
            '  [[ "$_servo_path_left" == *:* ]] || break',
            '  _servo_path_left="${_servo_path_left#*:}"',
            "done",
            'export PATH="$_servo_worker_bin:$SERVO_WASI_SDK_DIR/bin:$_servo_path_rest"',
            "unset _servo_worker_bin _servo_path_left _servo_path_rest _servo_separator _servo_part",
            END,
        ]
    )
    updated = before + "\n".join(lines)
    if not updated.endswith("\n"):
        updated += "\n"
    if updated != original:
        bashrc.write_text(updated)
    return bashrc


def bootstrap_worker_wasm() -> None:
    home = Path.home()
    arch = platform.machine()
    sdk = install_sdk(home, arch)
    wrappers = install_wrappers(home)
    installed = subprocess.check_output(["rustup", "target", "list", "--installed"], cwd=PROJECT_ROOT, text=True)
    if "wasm32-unknown-unknown" not in installed.splitlines():
        subprocess.run(["rustup", "target", "add", "wasm32-unknown-unknown"], cwd=PROJECT_ROOT, check=True)
    bashrc = update_bashrc(home, sdk, arch)
    # A child shell can verify the startup file, but cannot change the calling
    # terminal's environment. The caller needs to source it in that terminal.
    check = (
        'source "$1" && test "$SERVO_WASI_SDK_DIR" = "$3" && '
        'test -f "$WASM_CXX_LIB_DIR/libc.a" && test "$(type -P clang)" = "$2/clang"'
    )
    try:
        subprocess.run(
            ["/bin/bash", "--noprofile", "--norc", "-ic", check, "_", str(bashrc), str(wrappers), str(sdk)],
            check=True,
            timeout=30,
        )
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(f"Could not source and verify {bashrc}; check its WASI exports and PATH") from error
    os.environ["SERVO_WASI_SDK_DIR"] = str(sdk)
    os.environ["WASM_CXX_LIB_DIR"] = str(sdk / "share/wasi-sysroot/lib/wasm32-wasi")
    worker_paths = [str(wrappers), str(sdk / "bin")]
    other_paths = [part for part in os.environ.get("PATH", "").split(os.pathsep) if part not in worker_paths]
    os.environ["PATH"] = os.pathsep.join(worker_paths + other_paths)
    print(f"WASI SDK and compiler wrappers are ready. In your current terminal run: source {shlex.quote(str(bashrc))}")
