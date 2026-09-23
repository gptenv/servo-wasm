# Bundled Worker fonts

The Worker has no system fonts, so these faces are compiled into the WASM
module and registered at bootstrap. They provide the CSS generic families
(`sans-serif`, `serif`, `monospace`) and are metric-compatible with Arial,
Times New Roman and Courier New.

- Liberation Sans Regular and Bold, Liberation Serif Regular, Liberation Mono
  Regular, version 2.1.5 (https://github.com/liberationfonts/liberation-fonts)
- License: SIL Open Font License 1.1, see `LICENSE-Liberation`.

Hosts can add more fonts (for example CJK or emoji) at runtime with the
adapter's `registerFont()`; see `WORKER-ABI.md`.
