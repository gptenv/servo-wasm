# Vendored H.264 MP4 encoder

`h264-mp4-encoder-aot.js` and `h264-mp4-encoder.wasm` are generated from
[`h264-mp4-encoder` v1.0.12](https://github.com/TrevorSundberg/h264-mp4-encoder/tree/v1.0.12),
upstream commit `6d177fd043157606224cef4702e134dd31f6adfa`. Its source submodules
are pinned to `libmp4v2` commit `d49b4466ed76fc23b59e31a5f7f10f34b30ad7c4`
and `minih264` commit `25f441086ac8f2eef1c883476c095f9397843ac8`.

The checked-in build was produced with Emscripten 6.0.10 and C++17. Relative to
upstream's CMake configuration, it uses `ENVIRONMENT=web,worker`,
`DYNAMIC_EXECUTION=0`, `EMBIND_AOT=1`, ES module output, and a separate WASM
file. The module is initialized with the precompiled WASM module imported by
the Worker bundler. A Node smoke test with string-based code generation disabled
confirmed that it encodes a valid MP4 (`ftyp` box).

License notices for the encoder, libmp4v2, and minih264 are included beside the
generated files. Emscripten's generated JavaScript runtime is also distributed
as part of the generated ES module.

To regenerate the artifacts, install Git, CMake, Python 3, and Emscripten, then
run `scripts/build-h264-mp4-encoder-aot.sh` from the repository. `JOBS` can be
set to choose the parallel build count.
