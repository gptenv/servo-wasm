# Bundled Worker fonts

The Worker has no system fonts, so these faces are compiled into the WASM
module and registered at bootstrap. They provide the CSS generic families
(`sans-serif`, `serif`, `monospace`).

- Noto Sans Regular and Bold, Noto Serif Regular, Noto Sans Mono Regular
  version 2.015 (https://notofonts.github.io/)
- License: SIL Open Font License 1.1, see `LICENSE-Noto`.

Noto is not metric-compatible with Arial/Helvetica, so pages that name those
fonts lay out slightly differently than in browsers that have them installed.

Hosts can add more fonts (for example CJK or emoji) at runtime with the
adapter's `registerFont()`; see `WORKER-ABI.md`. The host-font test registers
`NotoSansMono-Regular.ttf` from this directory.
