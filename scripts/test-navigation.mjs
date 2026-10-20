import assert from 'node:assert/strict';
import { build } from 'esbuild';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const result = await build({ entryPoints: ['src/navigation.ts'], bundle: true, platform: 'node', format: 'cjs', write: false });
const module = { exports: {} };
new Function('require', 'module', 'exports', result.outputFiles[0].text)(require, module, module.exports);
const { parseRoute, routeHash, routeKey, routeTitle } = module.exports;

const aEpic = { section: 'epics-stories', projectId: 'project-a', issueType: 'epic' };
const bStory = { section: 'epics-stories', projectId: 'project-b', issueType: 'story' };
assert.deepEqual(parseRoute(routeHash(aEpic)), aEpic);
assert.deepEqual(parseRoute(routeHash(bStory)), bStory);
assert.notEqual(routeKey(aEpic), routeKey(bStory));
assert.equal(parseRoute('#runs?project=project-b').projectId, 'project-b');
assert.equal(parseRoute('#activity').section, 'runs');
assert.equal(parseRoute('#home').section, 'projects');
assert.equal(routeTitle({ section: 'changes', projectId: 'project-a' }, 'Alpha'), 'Alpha · Changes');
assert.equal(routeHash({ section: 'batches', projectId: 'project-b' }), '#batches?project=project-b');
assert.equal(parseRoute('#project/%E0%A4%A'), null);

console.log('Navigation route identity, legacy links, project filters and titles passed.');
