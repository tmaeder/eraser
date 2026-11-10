import { test } from "node:test";
import assert from "node:assert/strict";
import {
  isInternalPath,
  isPixelRequest,
  limiterKey,
  LOGIN_MAX_FAILURES,
  LOGIN_WINDOW_MS,
  recentFailures,
  retryAfterSeconds,
} from "../src/guard.ts";

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

test("login limiter blocks after 10 failures within 15 minutes", () => {
  const now = 1_000_000_000;
  const nine = Array.from({ length: 9 }, (_, i) => now - i * 1000);
  assert.equal(retryAfterSeconds(nine, now), 0);
  const ten = [...nine, now - 60_000];
  // oldest counted failure (60 s ago) leaves the window in 14 minutes
  assert.equal(retryAfterSeconds(ten, now), 14 * 60);
  // failures older than the window no longer count
  assert.equal(retryAfterSeconds(ten, now + LOGIN_WINDOW_MS), 0);
  assert.deepEqual(recentFailures([now - LOGIN_WINDOW_MS, now - 1], now), [now - 1]);
  assert.equal(LOGIN_MAX_FAILURES, 10);
});

test("limiter key groups IPv6 by /64", () => {
  assert.equal(limiterKey("203.0.113.7"), "203.0.113.7");
  assert.equal(limiterKey(null), "unknown");
  assert.equal(limiterKey("2001:db8::1"), "2001:db8:0:0::/64");
  assert.equal(limiterKey("2001:0db8:0000:0000:ffff::2"), "2001:db8:0:0::/64");
  assert.equal(limiterKey("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::/64");
  assert.equal(limiterKey("::1"), "0:0:0:0::/64");
});

test("only exact pixel paths bypass the login", () => {
  const token = "AAAAAAAAAAAAAAAAAAAAAA";
  for (const p of [`/p/${token}`, `/p/${token}.gif`, "/p/abcdefghijklmnop", `/p/${"a".repeat(64)}.gif`, "/p/a-b_c-d_e-f_g-h_"]) {
    assert.equal(isPixelRequest("GET", p), true, p);
  }
  assert.equal(isPixelRequest("HEAD", `/p/${token}.gif`), true);
  for (const m of ["POST", "PUT", "DELETE", "OPTIONS", "get"]) {
    assert.equal(isPixelRequest(m, `/p/${token}.gif`), false, m);
  }
  for (const p of [
    "/p/",
    "/p/short",
    `/p/${"a".repeat(65)}`,
    `/p/${token}.png`,
    `/p/${token}/x`,
    `/p/${token}.gif/`,
    `//p/${token}`,
    `/P/${token}`,
    `/p/${token}%2F..`,
    `/p/${token}.gif?x=1`,
    `/x/p/${token}`,
    "/p/../settings",
    `/p/${token}.gifx`,
    `/p/${token}\n`,
  ]) {
    assert.equal(isPixelRequest("GET", p), false, p);
  }
});
