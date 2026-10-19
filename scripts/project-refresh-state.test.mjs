import assert from "node:assert/strict";
import test from "node:test";
import { mergeRefreshPaths, settleRefreshPaths } from "../src/projectRefreshState.ts";

test("refresh path sets merge slash and case variants without duplicating repositories", () => {
  assert.deepEqual(
    mergeRefreshPaths(["C:/Repos/Navis", "C:/Repos/Other"], ["c:\\repos\\navis\\"]),
    ["C:/Repos/Navis", "C:/Repos/Other"],
  );
});

test("completing a scan keeps queued watcher paths active until their follow-up scan", () => {
  assert.deepEqual(
    settleRefreshPaths(["C:/Repos/Active", "C:/Repos/Queued", "C:/Repos/Queued"], ["c:\\repos\\active"], ["c:\\repos\\queued\\"], true),
    ["C:/Repos/Queued"],
  );
});

test("cancelled scan cleanup drops active and queued paths but preserves unrelated work", () => {
  assert.deepEqual(
    settleRefreshPaths(["C:/Repos/Active", "C:/Repos/Queued", "C:/Repos/Other"], ["C:/Repos/Active", "C:/Repos/Queued"], ["C:/Repos/Queued"], false),
    ["C:/Repos/Other"],
  );
});
