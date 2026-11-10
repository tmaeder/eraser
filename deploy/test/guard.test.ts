import { test } from "node:test";
import assert from "node:assert/strict";
import { isInternalPath } from "../src/guard.ts";

test("internal paths are blocked from the internet", () => {
  for (const p of [
    "/internal",
    "/internal/",
    "/internal/autopilot/tick",
    "//internal/autopilot/tick",
    "/INTERNAL/autopilot/tick",
    "/%69nternal/autopilot/tick",
  ]) {
    assert.equal(isInternalPath(p), true, p);
  }
  for (const p of ["/", "/internals", "/settings", "/api/internal", "/%E0%A4%A"]) {
    assert.equal(isInternalPath(p), false, p);
  }
});
