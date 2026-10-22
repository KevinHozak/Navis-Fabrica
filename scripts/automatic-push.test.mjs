import assert from 'node:assert/strict';
import test from 'node:test';
import { automaticallyPush } from '../src/automaticPush.ts';

const context = { mode: 'commitPush', snapshotFingerprint: 'snapshot', paths: ['app.ts'] };
test('drafts before starting with the prepared snapshot and generated text', async () => {
  const steps = [];
  const result = await automaticallyPush({
    prepare: async () => context,
    draft: async fingerprint => { assert.equal(fingerprint, 'snapshot'); return { ...context, subject: ' fix: change ', body: ' description ' }; },
    start: async (prepared, subject, body) => {
      assert.equal(prepared, context); assert.equal(subject, 'fix: change'); assert.equal(body, 'description');
      return { id: 'run' };
    },
  }, step => steps.push(step));
  assert.equal(result.id, 'run');
  assert.deepEqual(steps, ['Preparing changes…', 'Drafting message and description…', 'Starting commit and push…']);
});
test('push-only preserves existing messages without invoking drafting', async () => {
  await automaticallyPush({
    prepare: async () => ({ ...context, mode: 'pushOnly' }),
    draft: async () => assert.fail('must not draft'),
    start: async (_, subject, body) => { assert.equal(subject, ''); assert.equal(body, ''); return {}; },
  }, () => {});
});
test('draft errors, empty messages and changed snapshots never start a run', async () => {
  for (const draft of [async () => { throw new Error('API unavailable'); }, async () => ({ ...context, subject: ' ', body: '' }), async () => ({ ...context, snapshotFingerprint: 'changed', subject: 'fix: change', body: '' })]) {
    await assert.rejects(automaticallyPush({ prepare: async () => context, draft, start: async () => assert.fail('must not start') }, () => {}));
  }
});

test('refreshes a stale draft once and starts only the latest snapshot', async () => {
  let preparations = 0, drafts = 0, starts = 0;
  await automaticallyPush({
    prepare: async () => ({...context, snapshotFingerprint: String(++preparations)}),
    draft: async fingerprint => {
      drafts += 1;
      if (drafts === 1) throw 'Repository changes shifted. Refresh the draft before continuing.';
      return {...context, snapshotFingerprint: fingerprint, subject: 'fix: latest change', body: ''};
    },
    start: async prepared => { starts += 1; assert.equal(prepared.snapshotFingerprint, '2'); return {}; },
  }, () => {});
  assert.equal(preparations, 2); assert.equal(drafts, 2); assert.equal(starts, 1);
});
test('continuous edits stop after one refresh and start failures are never retried', async () => {
  let preparations = 0;
  await assert.rejects(automaticallyPush({
    prepare: async () => { preparations += 1; return context; },
    draft: async () => { throw new Error('Repository changes shifted while drafting.'); },
    start: async () => assert.fail('must not start'),
  }, () => {}), /Files keep changing/);
  assert.equal(preparations, 2);
  let starts = 0;
  await assert.rejects(automaticallyPush({
    prepare: async () => ({...context, mode: 'pushOnly'}),
    draft: async () => assert.fail('must not draft'),
    start: async () => { starts += 1; throw new Error('Repository changes shifted during preflight'); },
  }, () => {}), /preflight/);
  assert.equal(starts, 1);
});
