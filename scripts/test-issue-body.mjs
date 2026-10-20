import assert from 'node:assert/strict';
import { build } from 'esbuild';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const result = await build({ entryPoints: ['src/IssueBody.tsx'], bundle: true, platform: 'node', format: 'cjs', write: false, packages: 'external', jsx: 'automatic' });
const loaded = { exports: {} };
new Function('require', 'module', 'exports', result.outputFiles[0].text)(require, loaded, loaded.exports);
const { withoutDuplicateIssueHeading } = loaded.exports;

assert.equal(withoutDuplicateIssueHeading('# FAB-E001: Epic: Launch\n\nDetails remain.', 'FAB-E001', 'Epic: Launch'), 'Details remain.');
assert.equal(withoutDuplicateIssueHeading('# Epic: Launch\n\nDetails remain.', 'FAB-E001', 'Epic: Launch'), 'Details remain.');
assert.equal(withoutDuplicateIssueHeading('# Different heading\n\nDetails remain.', 'FAB-E001', 'Epic: Launch'), '# Different heading\n\nDetails remain.');
assert.equal(withoutDuplicateIssueHeading('Intro\n\n# Epic: Launch', 'FAB-E001', 'Epic: Launch'), 'Intro\n\n# Epic: Launch');

console.log('Issue reader duplicate-heading display checks passed.');
