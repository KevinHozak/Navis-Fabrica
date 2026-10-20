import assert from 'node:assert/strict';
import test from 'node:test';
import { mergeRunSnapshot } from '../src/workflowRunState.ts';
const run = (changes = {}) => ({ id: 'build-1', state: 'running', finishedAtMs: null, nextSequence: 3,
  events: [{ sequence: 1, text: 'Prepare' }, { sequence: 2, text: 'Build' }], ...changes });
test('late snapshots preserve streamed output and do not regress run state', () => {
  const previous = run({ nextSequence: 4, events: [...run().events, { sequence: 3, text: 'Compile' }] });
  const result = mergeRunSnapshot(previous, run({ state: 'queued', nextSequence: 2, events: [{ sequence: 1, text: 'Prepare' }] }));
  assert.equal(result.state, 'running');
  assert.equal(result.nextSequence, 4);
  assert.deepEqual(result.events.map(e => e.sequence), [1, 2, 3]);
});
test('completion is retained across delayed updates and duplicate output', () => {
  const completed = mergeRunSnapshot(run(), run({ state: 'completed', finishedAtMs: 100, nextSequence: 4,
    events: [{ sequence: 2, text: 'Build' }, { sequence: 3, text: 'Verified' }] }));
  assert.deepEqual(completed.events.map(e => e.sequence), [1, 2, 3]);
  assert.equal(mergeRunSnapshot(completed, run()).state, 'completed');
});
test('verbose compiler logs remain bounded', () => {
  const result = mergeRunSnapshot(run(), run({ nextSequence: 3001, events: Array.from({length: 3000}, (_, i) => ({sequence: i + 1, text: 'Compiling'})) }));
  assert.equal(result.events.length, 2000);
  assert.equal(result.events.at(-1).sequence, 3000);
});
