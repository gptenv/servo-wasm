# WASM rendering and font backend plan

This fork targets `wasm32-unknown-unknown` in a Cloudflare Worker. The Worker
does not provide a browser DOM, Canvas, WebGL/WebGPU context, system fonts, or
OS font APIs, so the rendering backend must be CPU-based and self-contained.

## Decision

Use a Worker-specific glyph backend built around:

- **Swash** for glyph outline loading, scaling, hinting, and rasterization.
- **HarfRust** only where the Worker needs to shape text independently of
  Servo's existing layout pipeline.
- A bounded in-memory glyph atlas and an RGBA software compositor for the
  initial screenshot path.

Servo's layout/display-list code already supplies positioned glyph IDs to the
WebRender layer. Therefore the first backend does not need to replace Servo's
text shaping. It needs to turn those glyph IDs into alpha/color bitmaps.

## Candidate evaluation

| Candidate | Role | WASM fit | Decision |
| --- | --- | --- | --- |
| `swash` | Font parsing, glyph outlines, rasterization | Pure Rust and portable; supports scaling, hinting, and rasterization | **Primary rasterizer** |
| `harfrust` | Complex-script/OpenType shaping | Pure Rust, current HarfBuzz Rust port, no native font library | **Primary shaping fallback** |
| `fontdue` | Small/simple TTF/OTF rasterization | Excellent portability and small surface area, but does not solve complex shaping | **Test/fallback backend** |
| `cosmic-text` | Complete text system: discovery, fallback, shaping, layout, rasterization | Portable, but duplicates Servo layout and adds a larger abstraction | **Do not use initially** |
| `rustybuzz` | Older pure-Rust HarfBuzz port | Portable, but archived and superseded by HarfRust | **Do not adopt** |
| Pathfinder/faf_text | GPU/vector text rendering | Requires a graphics context; unsuitable inside a Worker | **Not for Worker runtime** |

## Integration boundary

The native FreeType/CoreText/DirectWrite implementation remains unchanged for
desktop targets. The WASM build will provide a separate implementation behind
the existing glyph-rasterizer boundary. Fonts will be explicitly supplied by
the embedder or fetched and cached; the Worker cannot discover host system
fonts.

The initial screenshot renderer will support text, solid fills, borders,
images, and basic SVG. Filters, video, WebGL canvas, and advanced GPU-style
compositing are follow-up work.

## References

- Swash: <https://docs.rs/swash/latest/swash/>
- HarfRust: <https://github.com/harfbuzz/harfrust>
- Fontdue: <https://github.com/mooman219/fontdue>
- Servo WebRender: <https://github.com/servo/webrender>
