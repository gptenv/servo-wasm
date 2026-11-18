# Servo browser Worker

This private Cloudflare Worker owns Servo browser sessions, in-page HTTP, web
search, screenshots and recordings. `gptenv/servo-mcp` binds directly to its
`ServoBrowserSession` Durable Objects using Workers RPC. Website HTTP and
WebSocket traffic uses the Cloudflare `MESH` VPC network (`cf1:network`).
The transport applies the Chrome 157 Windows desktop request profile, with
default headers selected by destination and explicit caller values preserved.

From this directory, install dependencies and stage the engine built by the
[Worker WASM workflow](https://github.com/gptenv/servo-wasm/actions/workflows/worker-wasm.yml).

```sh
npm ci
SERVO_WASM_GITHUB_TOKEN="$(gh auth token)" npm run engine:fetch
npm run typecheck
npm test
npm run deploy:dry-run
npm run deploy
```

The artifact fetch verifies the manifest, source commit and hashes before
staging the adapter and WASM. For a local build, place the compiled WASM and
`../worker-adapter.mjs` in `src/vendor/servo-worker/`.

The production config transfers the existing SQLite session namespace from
the `servo-mcp` Worker. Deploy this Worker first and then deploy servo-mcp
with its cross-Worker binding. For an independent account or a fresh
environment, configure a new SQLite class migration instead of the production
transfer. Cloudflare deployment credentials need Workers Scripts:Edit,
Account Settings:Read and Connectivity Directory:Bind on the target account.

To run the whole stack locally once the engine is staged, run `npm run dev`
from servo-mcp.
