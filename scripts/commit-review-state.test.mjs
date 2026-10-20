import assert from 'node:assert/strict';
import { build } from 'esbuild';
const bundled = await build({ entryPoints: ['src/commitReviewState.ts'], bundle: true, platform: 'node', format: 'esm', write: false });
const { commitReviewReducer: reduce, initialCommitReview: initial, loadCommitReview } = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString('base64')}`);
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const context = { mode: 'commitPush', paths: ['file.txt', 'package.json'], snapshotFingerprint: 'reviewed' };
const draft = { subject: 'AI message', body: 'AI description' };
const tick = () => new Promise(resolve => setImmediate(resolve));

let state = initial;
const dispatch = action => { state = reduce(state, action); };
const preparation = deferred(), inference = deferred();
let draftCalls = 0;
const pending = loadCommitReview({
  paths: async () => ['file.txt'], prepare: () => preparation.promise,
  draft: fingerprint => { assert.equal(fingerprint, 'reviewed'); draftCalls++; return inference.promise; },
}, dispatch, () => true);
await tick();
assert.deepEqual(state.paths, ['file.txt']);
assert.equal(state.context, null);
assert.equal(state.phase, 'preparing');
assert.equal(draftCalls, 0);
dispatch({ type: 'subject', value: 'My message' });
dispatch({ type: 'body', value: '' }); // An intentional empty description is an edit.
preparation.resolve(context);
await tick();
assert.deepEqual(state.paths, context.paths);
assert.equal(state.phase, 'drafting');
inference.resolve(draft);
await pending;
assert.equal(state.subject, 'My message');
assert.equal(state.body, '');
assert.equal(state.phase, 'ready');

state = reduce(initial, { type: 'prepared', context });
dispatch({ type: 'subject', value: 'Keep manual message' });
dispatch({ type: 'draft', draft });
assert.equal(state.subject, 'Keep manual message');
assert.equal(state.body, draft.body);
dispatch({ type: 'freeze' });
const frozen = state;
dispatch({ type: 'draft', draft: { subject: 'late', body: 'late' } });
dispatch({ type: 'error', error: 'late failure' });
assert.equal(state, frozen);
dispatch({ type: 'thaw' });
dispatch({ type: 'subject', value: 'Retry after start failure' });
assert.equal(state.subject, 'Retry after start failure');

state = initial;
await loadCommitReview({ paths: async () => [], prepare: async () => ({ ...context, mode: 'pushOnly', paths: [] }), draft: () => { throw Error('must not draft push-only'); } }, dispatch, () => true);
assert.equal(state.phase, 'ready');
assert.equal(state.error, '');
state = initial;
await loadCommitReview({ paths: async () => ['file.txt'], prepare: async () => context, draft: async () => { throw Error('Codex unavailable'); } }, dispatch, () => true);
assert.equal(state.context, context); // A failed draft still permits manual entry.
assert.equal(state.error, 'Codex unavailable');
state = initial;
await loadCommitReview({ paths: async () => ['file.txt'], prepare: async () => { throw Error('stale or unsafe repository'); }, draft: async () => draft }, dispatch, () => true);
assert.equal(state.context, null); // Preparation failure cannot enable commit.
assert.deepEqual(state.paths, ['file.txt']);
let active = true;
const latePaths = deferred();
state = initial;
const disposed = loadCommitReview({ paths: () => latePaths.promise, prepare: async () => context, draft: async () => draft }, dispatch, () => active);
active = false;
latePaths.resolve(['late.txt']);
await disposed;
assert.equal(state, initial);
console.log('Commit review: immediate paths, version refresh, delayed drafts, preserved edits, push-only, failures and disposed dialogs passed.');
