import assert from 'node:assert/strict';
import test from 'node:test';
import { latestBuildStep } from '../src/buildProgress.ts';

test('build progress follows the latest phase while compiler output continues', () => {
  const run = { workflow: { action: 'dev-build' }, events: [
    { text: 'BUILD_STEP:1/6:Preparing version' },
    { text: 'BUILD_STEP:2/6:Building release artifacts' },
    { text: 'Compiling crate 103 of 400' },
  ] };
  assert.equal(latestBuildStep(run), 'Step 2 of 6 · Building release artifacts');
  run.events.push({ text: 'BUILD_STEP:6/6:Build completed and verified' });
  assert.equal(latestBuildStep(run), 'Step 6 of 6 · Build completed and verified');
});

test('old logs and unrelated actions do not fabricate progress', () => {
  assert.equal(latestBuildStep({ workflow: { action: 'dev-build' }, events: [{ text: 'Compiling…' }, { text: 'BUILD_STEP:99/6:invalid' }] }), null);
  assert.equal(latestBuildStep({ workflow: { action: 'dev-push' }, events: [{ text: 'BUILD_STEP:1/6:Preparing version' }] }), null);
});
