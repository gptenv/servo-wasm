import { execFileSync } from 'node:child_process';

const metadata = JSON.parse(execFileSync('cargo', [
  'metadata', '--locked', '--format-version', '1',
  '--filter-platform', 'wasm32-unknown-unknown',
], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }));
const root = metadata.packages.find((pkg) =>
  pkg.name === 'servo-js-wasm' && metadata.workspace_members.includes(pkg.id));
if (!root) throw new Error('servo-js-wasm is missing from locked Cargo metadata');
const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
const packages = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
const reachable = new Set();
const pending = [root.id];
while (pending.length) {
  const id = pending.pop();
  if (reachable.has(id)) continue;
  reachable.add(id);
  for (const dependency of nodes.get(id)?.deps ?? []) pending.push(dependency.pkg);
}
const entries = [...reachable].map((id) => {
  const pkg = packages.get(id);
  if (!pkg) throw new Error(`Resolved package metadata is missing: ${id}`);
  return {
    name: pkg.name,
    version: pkg.version,
    license: pkg.license,
    source: pkg.source ?? 'workspace',
    repository: pkg.repository ?? null,
  };
}).sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
console.log(JSON.stringify({
  schema: 1,
  target: 'wasm32-unknown-unknown',
  root: 'servo-js-wasm',
  packageCount: entries.length,
  packages: entries,
}, null, 2));
