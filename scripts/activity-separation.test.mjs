import assert from 'node:assert/strict';
import { build } from 'esbuild';
import { createRequire } from 'node:module';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

const require = createRequire(import.meta.url);
let fixtureRuns = [];
let seeded = false;
const result = await build({ entryPoints: ['src/RunsPanel.tsx'], bundle: true, platform: 'node', format: 'cjs', write: false, packages: 'external', jsx: 'automatic' });
const mod = { exports: {} };
const testRequire = name => name === 'react' ? { ...React, useState(initial) {
  // Supply persisted runs while retaining React's real rendering and other state.
  if (!seeded && Array.isArray(initial)) { seeded = true; return React.useState(fixtureRuns); }
  return React.useState(initial);
} } : require(name);
new Function('require', 'module', 'exports', result.outputFiles[0].text)(testRequire, mod, mod.exports);
const { RunsPanel } = mod.exports;
globalThis.window = {};
const project = { id: 'owned', name: 'Owned project', path: 'C:/Projects/Owned', detectedStacks: [], workflowPrerequisites: [], git: { remotes: [] } };
const run = { id: 'old-run', cwd: 'C:/Projects/Removed', state: 'running', executable: 'tool.exe', events: [{ sequence: 1, kind: 'stdout', text: 'Exact run output' }], startedAtMs: 1, finishedAtMs: null, exitCode: null, latencyMs: null, processId: 10, workflow: null, handoff: null };
function render(mode, runs, selectedProject) {
  fixtureRuns = runs; seeded = false;
  return renderToStaticMarkup(React.createElement(RunsPanel, { mode, project: selectedProject, defaultCwd: selectedProject?.path, issue: null }));
}
const activity = render('activity', [run]);
assert.match(activity, /Running &amp; completed work/);
assert.match(activity, /Exact run output/);
assert.match(activity, /Cancel run/);
assert.match(activity, /C:\/Projects\/Removed/);
assert.doesNotMatch(activity, /Start selected action|Start Codex|Start run|Project actions|Retry build safely|Prepare a manual provider handoff/);
assert.equal((activity.match(/class="run-list-item/g) ?? []).length, 1);
const completed = render('activity', [{ ...run, state: 'completed', finishedAtMs: 2, exitCode: 0, workflow: { action: 'dev-build', intent: 'Build', requestedEffects: [], scopePaths: [], buildArtifact: { verified: true, profile: 'release', path: 'release.exe', fileVersion: '1.0.0', productVersion: '1.0.0', verificationMessage: 'Verified artifact' } } }]);
assert.match(completed, /Artifact verified/);
assert.match(completed, /Exit code 0/);
assert.doesNotMatch(completed, /Cancel run|Retry build safely/);
const needsInput = render('activity', [{ ...run, state: 'needsInput' }]);
assert.match(needsInput, /Send process input/);
const actions = render('actions', [run], project);
assert.match(actions, /Project actions/);
assert.match(actions, /Start Codex/);
assert.doesNotMatch(actions, /Running &amp; completed work|Exact run output|Continue previous work/);
const previousWork = render('actions', [{ ...run, cwd: project.path, state: 'failed', finishedAtMs: 2, processId: null, workflow: { action: 'dev-build' } }], project);
assert.match(previousWork, /Retry build safely/);
assert.match(previousWork, /Prepare a manual provider handoff/);
const empty = render('activity', []);
assert.match(empty, /Work started from your projects appears here/);
console.log('Activity separation passed: global history, output, results, active controls and project-only launch forms.');

// Exercise selection and repeat-click scrolling through the real component handlers.
let hookIndex = 0;
const hooks = [];
const effects = [];
const interactionReact = { ...React,
  useState(initial) {
    const index = hookIndex++;
    if (!(index in hooks)) hooks[index] = Array.isArray(initial) && !hooks.some(Array.isArray) ? [run, { ...run, id: 'older', startedAtMs: 0, events: [{ sequence: 1, kind: 'stdout', text: 'Older output' }] }] : initial;
    return [hooks[index], value => { hooks[index] = typeof value === 'function' ? value(hooks[index]) : value; }];
  },
  useRef(initial) { const index = hookIndex++; return hooks[index] ??= { current: initial }; },
  useMemo(callback) { hookIndex++; return callback(); },
  useEffect(callback, dependencies) { hookIndex++; effects.push({ callback, dependencies }); },
};
const interactionModule = { exports: {} };
new Function('require', 'module', 'exports', result.outputFiles[0].text)(name => name === 'react' ? interactionReact : require(name), interactionModule, interactionModule.exports);
const detailNode = { scrollTop: 240 };
const outputNode = { scrollTop: 120 };
function walk(element, predicate) {
  if (!element || typeof element !== 'object') return null;
  if (predicate(element)) return element;
  for (const child of React.Children.toArray(element.props?.children)) {
    const found = walk(child, predicate);
    if (found) return found;
  }
  return null;
}
function interactionRender() {
  hookIndex = 0; effects.length = 0;
  const tree = interactionModule.exports.RunsPanel({ mode: 'activity', issue: null });
  const output = walk(tree, element => element.type === 'pre');
  output.props.ref.current = outputNode;
  walk(tree, element => element.props?.className === "panel run-detail-panel").props.ref.current = detailNode;
  // Only execute the output navigation effect; IPC subscriptions are outside this test.
  effects.find(effect => effect.dependencies?.length === 2 && (effect.dependencies[0] === null || effect.dependencies[0]?.runId))?.callback();
  return tree;
}
let tree = interactionRender();
const olderButton = walk(tree, element => element.type === 'button' && element.props.className === 'run-list-item');
olderButton.props.onClick();
tree = interactionRender();
assert.equal(walk(tree, element => element.type === 'pre').props.children, '[1] stdout: Older output');
assert.equal(detailNode.scrollTop, 0);
assert.equal(outputNode.scrollTop, 0);
detailNode.scrollTop = 180;
outputNode.scrollTop = 80;
walk(tree, element => element.type === 'button' && element.props.className === 'run-list-item selected').props.onClick();
interactionRender();
assert.equal(detailNode.scrollTop, 0);
assert.equal(outputNode.scrollTop, 0);
detailNode.scrollTop = 160;
interactionRender();
assert.equal(detailNode.scrollTop, 160, 'rerenders must preserve the reader position');
console.log('Run navigation passed: exact selection, top-of-details reset, repeated clicks and preserved reading position.');
