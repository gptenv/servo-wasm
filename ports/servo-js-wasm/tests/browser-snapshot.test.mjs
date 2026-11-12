import assert from 'node:assert/strict';
import test from 'node:test';
import { selectResumeStateForOrigin } from '../worker-adapter.mjs';

const first = 'https://a.example/app';
const other = 'https://b.example/dashboard';
const original = {
  version: 1, url: first, origin: 'https://a.example',
  fields: [{ path: [0], kind: 'value', value: 'user input' }],
  scrollX: 42, scrollY: 91, cookies: 'sid=private',
  localStorage: [['old', 'value']], sessionStorage: [],
  indexedDB: [],
  localStorageByOrigin: {
    'https://a.example': [['a', '1']],
    'https://b.example': [['b', '2']],
  },
  sessionStorageByOrigin: { 'https://b.example': [['tab', 'yes']] },
  indexedDBByOrigin: { 'https://b.example': [{ name: 'data', version: 1, stores: [] }] },
  originAssets: { 'https://a.example': 'asset-1' },
};

test('same document preserves fields, scroll, cookies, and origin storage', () => {
  const state = selectResumeStateForOrigin(original, first);
  assert.deepEqual(state.fields, original.fields);
  assert.equal(state.scrollX, 42);
  assert.equal(state.cookies, 'sid=private');
  assert.deepEqual(state.localStorage, [['a', '1']]);
  assert.equal(state.originAssets, undefined);
  assert.equal(state.indexedDBByOrigin, undefined);
});

test('cross-origin restoration cannot leak document fields or cookies', () => {
  const state = selectResumeStateForOrigin(original, other);
  assert.deepEqual(state.fields, []);
  assert.equal(state.scrollX, 0);
  assert.equal(state.scrollY, 0);
  assert.equal(state.cookies, '');
  assert.deepEqual(state.localStorage, [['b', '2']]);
  assert.deepEqual(state.sessionStorage, [['tab', 'yes']]);
  assert.equal(state.indexedDB[0].name, 'data');
  assert.equal(state.indexedDBStatePresent, true);
  assert.deepEqual(original.fields, [{ path: [0], kind: 'value', value: 'user input' }]);
});

test('unrecorded origin receives no stored state', () => {
  const state = selectResumeStateForOrigin(original, 'https://c.example/');
  assert.deepEqual(state.localStorage, []);
  assert.deepEqual(state.sessionStorage, []);
  assert.deepEqual(state.indexedDB, []);
  assert.equal(state.indexedDBStatePresent, false);
  assert.equal(state.cookies, '');
});

test('invalid snapshot rejects before evaluating in the browser', () => {
  assert.throws(() => selectResumeStateForOrigin(null, first), TypeError);
  assert.throws(() => selectResumeStateForOrigin({}, first), TypeError);
});
