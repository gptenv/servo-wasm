import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Unlike ports/servo-js-wasm (which needs WASI for SpiderMonkey's clock/
// error-reporting host calls), this module is pure computation -- glyph
// rasterization touches no host imports at all, so a plain instantiate
// with no import object is enough. There is no wasm32-unknown-unknown
// `cargo test` runner configured in this workspace (bare wasm32 has no
// standard entry point without wasm-bindgen-test or a WASI target), so
// this mirrors ports/servo-js-wasm's approach of exporting plain
// `extern "C"` functions and driving them from Node instead.
const wasmPath = process.env.SERVO_FONT_WASM_PATH ??
  new URL('../../../target/wasm32-unknown-unknown/debug/servo_font_wasm_check.wasm', import.meta.url);
const wasm = new WebAssembly.Module(readFileSync(wasmPath));
const { exports } = new WebAssembly.Instance(wasm, {});

// Failure codes from lib.rs's rasterize_capital_a, for readable assertions.
const FAILURE_REASON = {
  1: "no glyph found for 'A'",
  2: 'get_glyph_dimensions returned None',
  3: 'degenerate (<=0) glyph dimensions',
  4: 'non-positive advance width',
  5: 'rasterize_glyph returned Err',
  6: 'dimensions disagree between get_glyph_dimensions and rasterize_glyph',
  7: 'rasterized bytes buffer is the wrong size for width*height*4',
  8: 'rasterized bytes are all zero (nothing drawn)',
  9: 'alpha channel is entirely transparent',
  10: 'a codepoint with no glyph was confused with glyph 0',
};

function assertPass(code) {
  assert.equal(code, 0, `expected 0 (pass), got ${code}: ${FAILURE_REASON[code] ?? 'unknown code'}`);
}

test('raw TrueType (Ahem.ttf) rasterizes a real glyph', () => {
  assertPass(exports.check_ttf());
});

test("Ahem's 'A' at 32px is an exact 32x32 fully-opaque square", () => {
  // Ahem's whole design point is that every glyph is a solid square
  // exactly matching the em box -- this is the one case where we can
  // assert an *exact* expected value rather than just "plausible".
  assert.equal(exports.check_ttf_ahem_is_exact_square(), 0);
});

test('WOFF1 (GentiumPlus-R.woff) decompresses and rasterizes a real glyph', () => {
  assertPass(exports.check_woff1());
});

test('WOFF2 (HasubiMono-Regular.woff2) decompresses and rasterizes a real glyph', () => {
  assertPass(exports.check_woff2());
});
