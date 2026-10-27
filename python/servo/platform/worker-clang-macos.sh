#!/usr/bin/env bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Managed by Servo macOS bootstrap.

set -o errexit
set -o nounset
set -o pipefail

compiler="${0##*/}"
wasm_target=false
previous=""
for argument in "$@"; do
  case "${argument}" in
    --target=wasm32-unknown-unknown|-target=wasm32-unknown-unknown)
      wasm_target=true ;;
    wasm32-unknown-unknown)
      if [[ "${previous}" == --target || "${previous}" == -target ]]; then
        wasm_target=true
      fi ;;
  esac
  previous="${argument}"
done

if "${wasm_target}"; then
  sysroot="${SERVO_WASI_SDK_DIR:?SERVO_WASI_SDK_DIR is not set}/share/wasi-sysroot"
  includes=( -isystem "${sysroot}/include/wasm32-wasi" )
  if [[ "${compiler}" == clang++ ]]; then
    includes=( -isystem "${sysroot}/include/wasm32-wasi/c++/v1" "${includes[@]}" )
  fi
  exec "${SERVO_WASI_SDK_DIR}/bin/${compiler}" "--sysroot=${sysroot}" "$@" "${includes[@]}"
fi

exec "/usr/bin/${compiler}" "$@"
