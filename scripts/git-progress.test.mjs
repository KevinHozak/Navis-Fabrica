import assert from 'node:assert/strict';
import test from 'node:test';
import { latestGitStep, gitOutputText, pullRequestMergeStatus, localSyncStatus } from '../src/gitProgress.ts';
test('conflicts remain visible and receipts stay out of the user log', () => {
  const run = {workflow: {action: 'dev-push'}, events: [
    {text: 'GIT_STEP:fetch:Fetching the remote branch'},
    {text: 'GIT_STEP:conflict:Rebase stopped: resolve conflicts before pushing'},
    {text: 'Conflicted files: file.txt'},
    {text: 'NAVIS_SIDE_EFFECTS:{"commitSha":"saved"}'},
  ]};
  assert.equal(latestGitStep(run), 'Rebase stopped: resolve conflicts before pushing');
  assert.match(gitOutputText(run), /Conflicted files: file.txt/);
  assert.doesNotMatch(gitOutputText(run), /NAVIS_SIDE_EFFECTS|GIT_STEP:/);
});
test('unrelated output does not fabricate Git progress', () => {
  assert.equal(latestGitStep({workflow: {action: 'dev-build'}, events: [{text: 'GIT_STEP:push:Push'}]}), null);
});

test('queued auto-merge is distinct from a verified merge', () => {
  const run = {workflow: {action: 'dev-push'}, events: [
    {text: 'NAVIS_PR_MERGE:{"state":"queued","url":"https://github.com/owner/repo/pull/3"}'},
  ]};
  assert.match(pullRequestMergeStatus(run), /^Auto-merge enabled/);
  assert.doesNotMatch(gitOutputText(run), /NAVIS_PR_MERGE/);
  run.events.push({text: 'NAVIS_PR_MERGE:' + JSON.stringify({state: 'merged', mergeCommit: '7'.repeat(40)})});
  assert.equal(pullRequestMergeStatus(run), 'PR merged · 77777777');
});
test('unverified and malformed merge messages never claim success', () => {
  assert.equal(pullRequestMergeStatus({workflow: {action: 'dev-push'}, events: [{text: 'NAVIS_PR_MERGE:{"state":"merged"}'}]}), null);
  assert.equal(pullRequestMergeStatus({workflow: {action: 'dev-push'}, events: [{text: 'NAVIS_PR_MERGE:invalid'}]}), null);
});

test('merged PR and local sync remain separate verified outcomes', () => {
  const run = {workflow: {action: 'dev-push'}, events: [
    {text: 'NAVIS_PR_MERGE:' + JSON.stringify({state: 'merged', mergeCommit: '7'.repeat(40)})},
    {text: 'NAVIS_LOCAL_SYNC:' + JSON.stringify({state: 'skipped', message: 'Local changes preserved'})},
  ]};
  assert.match(pullRequestMergeStatus(run), /^PR merged/);
  assert.equal(localSyncStatus(run), 'Local changes preserved');
  run.events.push({text: 'NAVIS_LOCAL_SYNC:' + JSON.stringify({state: 'synced', branch: 'main', commit: '8'.repeat(40)})});
  assert.equal(localSyncStatus(run), 'Local main synced · 88888888');
  assert.doesNotMatch(gitOutputText(run), /NAVIS_LOCAL_SYNC/);
});
test('local sync success requires a verified SHA', () => {
  assert.equal(localSyncStatus({workflow: {action: 'dev-push'}, events: [{text: 'NAVIS_LOCAL_SYNC:{"state":"synced","branch":"main"}'}]}), null);
});
