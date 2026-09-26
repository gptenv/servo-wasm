# Worker operations and recovery

This runbook applies to the raw WASM engine and `worker-adapter.mjs` at ABI 6.
The Worker remains a controlled-evaluation prototype. It does not yet provide
durable browser state or a safe execution deadline for untrusted scripts.

## Identify a release

Use the `worker-release-manifest.json` retained by the Worker WASM CI job.
Record the Git commit, WASM and adapter SHA-256 values, ABI version, complete
Wrangler bundle size, Cargo.lock hash, resolved fork commits and tool versions.
Deploy the adapter and WASM from the same CI run. Refuse an artifact whose ABI
does not match the adapter. Keep the previous artifact and manifest for rollback.

## Session boundaries

Create one WASM instance per unrelated user session. Serialize calls that mutate
one runtime; call `beginInvocation()` at the start of each new serialized host
call after the previous call has settled and its evaluation result has been read.
`reset()` cancels outstanding work and navigates to `about:blank`, but leaves
cookies and storage in that instance. To end a session, stop accepting calls,
cancel or finish outstanding streams, drop every reference to the runtime and
discard the instance. Never put that instance into another user's session pool.
There is no secure erase or persistent-session recovery contract yet.

If a WASM export traps, retire the instance immediately. The adapter rejects
later exports because Rust state may contain partial updates. A fresh instance
can recover only data the host previously committed; the current in-memory
storage backend has no such committed state. A page evaluation that runs forever
cannot be interrupted by a JavaScript timer around the synchronous export;
do not admit arbitrary untrusted pages until the engine interrupt gate is met.

## Observe failures

At the host boundary, associate each call with a session ID, operation ID,
release manifest hash and phase (`bootstrap`, `navigate`, `pump`, `fetch`,
`evaluate`, `screenshot`, `reset`). Record elapsed time, subrequest counts,
response sizes, pending fetch/socket counts, failures and trap retirement.
Classify failures as input rejection, resource limit, network error, timeout,
storage error or WASM trap. Log the URL origin only when needed for diagnosis;
do not log cookies, authorization headers, response bodies, page content or
evaluation source by default. The hosting service must supply authentication,
egress policy, retention, session expiry and deletion.

## Recover and roll back

On host fetch failure or client disconnect, abort readers and sockets, let the
adapter retire callbacks, and report an operation failure. If the runtime traps
or misses an agreed service deadline, end the session and create a new instance
for a later call. Do not replay a mutating call automatically without a host
idempotency key. Current sessions have no durable checkpoint, so replacement
starts with empty browser state.

Before rolling back, compare the new and previous release manifests, ABI and
storage schema. The current engine has no persistent schema; a future Durable
Object integration must provide forward/backward migration or explicitly block
rollback after a schema change. Roll back both WASM and adapter together, then
run the root, fixture and screenshot smoke routes against the deployed Worker.
Track failed requests, startup time, CPU, isolate memory and session recreation
after the switch. A local workerd pass does not establish remote account limits.
