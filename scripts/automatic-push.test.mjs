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
