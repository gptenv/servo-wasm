import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createWriteStream } from 'node:fs';
import { copyFile, mkdir, mkdtemp, rename, rm } from 'node:fs/promises';
import { readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Readable, Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';

const OWNER = 'gptenv';
const REPOSITORY = 'servo-wasm';
const WORKFLOW = 'worker-wasm.yml';
const API = `https://api.github.com/repos/${OWNER}/${REPOSITORY}`;
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const TARGET = resolve(process.env.SERVO_WASM_STAGE_DIR ?? join(ROOT, 'src/vendor/servo-worker'));
const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024;

function requireToken() {
  const token = process.env.SERVO_WASM_GITHUB_TOKEN?.trim();
  if (!token) {
    throw new Error('SERVO_WASM_GITHUB_TOKEN is required. Copy .env.example to .env and set a fine-grained token with Actions: Read on gptenv/servo-wasm.');
  }
  return token;
}

async function apiGet(url, token) {
  const response = await fetch(url, {
    headers: {
      Accept: 'application/vnd.github+json',
      Authorization: `Bearer ${token}`,
      'X-GitHub-Api-Version': '2022-11-28',
      'User-Agent': 'servo-wasm-artifact-fetcher',
    },
  });
  if (!response.ok) {
    const detail = (await response.text()).slice(0, 500);
    throw new Error(`GitHub API request failed (${response.status} ${response.statusText}): ${detail}`);
  }
  return response.json();
}

function verifyFiles(extractDir, run) {
  const manifestPath = join(extractDir, 'release-manifest.json');
  const wasmPath = join(extractDir, 'servo_js_wasm.wasm');
  const adapterPath = join(extractDir, 'worker-adapter.mjs');
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const wasm = readFileSync(wasmPath);
  const adapter = readFileSync(adapterPath);
  const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

  if (manifest.schema !== 1) throw new Error(`Unsupported Worker release manifest schema: ${manifest.schema}`);
  if (manifest.gitCommit?.toLowerCase() !== run.head_sha.toLowerCase()) {
    throw new Error(`Artifact commit ${manifest.gitCommit} does not match successful run commit ${run.head_sha}.`);
  }
  if (sha256(wasm) !== manifest.wasmSha256) throw new Error('Servo WASM SHA-256 does not match release-manifest.json.');
  if (sha256(adapter) !== manifest.adapterSha256) throw new Error('Worker adapter SHA-256 does not match release-manifest.json.');
  if (!Number.isInteger(manifest.workerAbiVersion) || manifest.workerAbiVersion < 1) {
    throw new Error('Release manifest is missing a valid workerAbiVersion.');
  }
  return { manifest, wasmBytes: wasm.byteLength };
}

async function writeLimitedResponse(response, path) {
  if (!response.body) throw new Error('GitHub artifact download returned an empty response body.');
  const headerLength = Number(response.headers.get('content-length'));
  if (Number.isFinite(headerLength) && headerLength > MAX_ARCHIVE_BYTES) {
    throw new Error(`Artifact archive exceeds the ${MAX_ARCHIVE_BYTES}-byte safety limit.`);
  }
  let received = 0;
  const limit = new Transform({
    transform(chunk, _encoding, callback) {
      received += chunk.length;
      if (received > MAX_ARCHIVE_BYTES) callback(new Error(`Artifact archive exceeds the ${MAX_ARCHIVE_BYTES}-byte safety limit.`));
      else callback(null, chunk);
    },
  });
  await pipeline(Readable.fromWeb(response.body), limit, createWriteStream(path, { flags: 'wx' }));
}

async function main() {
  const token = requireToken();
  const runUrl = `${API}/actions/workflows/${WORKFLOW}/runs?branch=main&status=completed&per_page=20`;
  const { workflow_runs: runs = [] } = await apiGet(runUrl, token);
  const successfulMainRuns = runs.filter((run) =>
    run.head_branch === 'main' && run.status === 'completed' && run.conclusion === 'success' && /^[a-f0-9]{40}$/i.test(run.head_sha),
  );
  if (successfulMainRuns.length === 0) throw new Error(`No successful ${WORKFLOW} workflow runs were found on ${OWNER}/${REPOSITORY}:main.`);

  let selected;
  for (const run of successfulMainRuns) {
    const { artifacts = [] } = await apiGet(`${API}/actions/runs/${run.id}/artifacts?per_page=100`, token);
    const artifact = artifacts.find((item) => item.name === 'servo-worker-main')
      ?? artifacts.find((item) => item.name === `servo-worker-${run.head_sha}`);
    if (artifact && !artifact.expired) {
      selected = { run, artifact };
      break;
    }
  }
  if (!selected) throw new Error('No unexpired deployable servo-worker-main artifact was found in the latest successful main runs.');

  const archiveResponse = await fetch(`${API}/actions/artifacts/${selected.artifact.id}/zip`, {
    headers: {
      Accept: 'application/vnd.github+json',
      Authorization: `Bearer ${token}`,
      'X-GitHub-Api-Version': '2022-11-28',
      'User-Agent': 'servo-wasm-artifact-fetcher',
    },
    redirect: 'manual',
  });
  if (archiveResponse.status !== 302) {
    const detail = (await archiveResponse.text()).slice(0, 500);
    throw new Error(`GitHub did not return an artifact download redirect (${archiveResponse.status}): ${detail}`);
  }
  const downloadUrl = archiveResponse.headers.get('location');
  if (!downloadUrl) throw new Error('GitHub artifact response omitted its download URL.');
  const parsedDownloadUrl = new URL(downloadUrl);
  const host = parsedDownloadUrl.hostname;
  const allowedDownloadHost = ['release-assets.githubusercontent.com', 'github-cloud.s3.amazonaws.com', 'objects.githubusercontent.com'].includes(host)
    || host.endsWith('.blob.core.windows.net');
  if (parsedDownloadUrl.protocol !== 'https:' || !allowedDownloadHost) {
    throw new Error(`GitHub returned an unexpected artifact download host: ${parsedDownloadUrl.hostname}`);
  }

  const tempDir = await mkdtemp(join(tmpdir(), 'servo-wasm-'));
  const zipPath = join(tempDir, 'artifact.zip');
  const extractDir = join(tempDir, 'extracted');
  const stagedTemp = `${TARGET}.tmp-${process.pid}`;
  try {
    const downloadResponse = await fetch(parsedDownloadUrl);
    if (!downloadResponse.ok) throw new Error(`Artifact storage download failed (${downloadResponse.status} ${downloadResponse.statusText}).`);
    await writeLimitedResponse(downloadResponse, zipPath);
    await mkdir(extractDir);
    const entries = execFileSync('unzip', ['-Z1', zipPath], { encoding: 'utf8' }).trim().split('\n').filter(Boolean);
    const expected = new Set(['release-manifest.json', 'servo_js_wasm.wasm', 'worker-adapter.mjs']);
    if (entries.length !== expected.size || entries.some((entry) => !expected.has(entry))) {
      throw new Error(`Unexpected files in Servo Worker artifact: ${entries.join(', ')}`);
    }
    execFileSync('unzip', ['-q', zipPath, '-d', extractDir], { stdio: 'ignore' });
    const { manifest, wasmBytes } = verifyFiles(extractDir, selected.run);

    await rm(stagedTemp, { recursive: true, force: true });
    await mkdir(stagedTemp, { recursive: true });
    for (const filename of expected) await copyFile(join(extractDir, filename), join(stagedTemp, filename));
    await rm(TARGET, { recursive: true, force: true });
    await rename(stagedTemp, TARGET);
    console.log(`Staged servo-wasm ${selected.run.head_sha} (ABI ${manifest.workerAbiVersion}, ${wasmBytes.toLocaleString()} WASM bytes) at ${TARGET}`);
  } finally {
    await rm(stagedTemp, { recursive: true, force: true });
    await rm(tempDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
});
