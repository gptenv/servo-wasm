# Local workerd smoke test

This verifies that Wrangler/workerd can load the raw Worker-compatible Servo
WASM artifact and run its Worker host adapter. It covers supplied HTML, computed
CSS, a script fetch and cancellation of an open response body. It is a local test
only; it does not deploy or access Cloudflare credentials. Every resource is a
deterministic fixture; this does not test live outbound networking or account quotas.

From the Servo repository root:

```sh
./mach build --target wasm32-unknown-unknown --no-default-features --jobs 4 \
  --profile production-stripped --manifest-path ports/servo-js-wasm/Cargo.toml
cd ports/servo-js-wasm/workerd-smoke
npx --yes wrangler@4.136.3 dev --local --ip 127.0.0.1 --port 8799
```

Request `http://127.0.0.1:8799/`. A passing response contains
`{"passed":true,"result":{"Ok":{"Number":42}},"streamCanceled":true,"pendingFetches":0}`.
`/cases?rounds=3` runs the shared fixture corpus (`tests/web-platform-cases.mjs`)
repeatedly, and `/screenshot` returns a PNG of a small fixture page with its
render time in `x-render-ms`. `npm run workerd` in the parent directory starts it.

To check the actual uncompressed bundle size without deploying, run
`npx --yes wrangler@4.136.3 deploy --dry-run --outdir /tmp/servo-wasm-worker-dry-run`
from this directory. The current bundle is **54,636.84 KiB uncompressed**
(15,793.18 KiB gzip), measured on 2026-09-23. The WASM alone is **55,923,533 bytes**.
The production
Workers Free CPU limit is 10 ms per request; this local smoke test does not
enforce it. Run `node ports/servo-js-wasm/tests/cpu-benchmark.mjs` from the
repository root for a repeatable local CPU diagnostic.

The broader raw-WASM suite is `node --test ports/servo-js-wasm/tests/wasm.test.mjs`
from the repository root (run with `npm test` in the parent directory). See
[the versioned host contract](../WORKER-ABI.md) for API behavior and limitations.
