import { DurableObject } from "cloudflare:workers";
import { Container, getContainer } from "@cloudflare/containers";
export { ContainerProxy } from "@cloudflare/containers";
import { isInternalPath, isPixelRequest, limiterKey, LOGIN_WINDOW_MS, recentFailures, retryAfterSeconds } from "./guard";

interface Env {
  ERASER: DurableObjectNamespace<EraserContainer>;
  STATE: R2Bucket;
  ERASER_PASSWORD: string;
  ERASER_AUTOPILOT_TOKEN: string;
  ERASER_CONFIG_KEY: string;
  LOGIN_LIMITER: DurableObjectNamespace<LoginLimiter>;
}

const STATE_KEY = "eraser-home.tar.gz";

export class EraserContainer extends Container<Env> {
  defaultPort = 8080;
  // Bulk sends run in-process for hours; sleeping would pause them.
  sleepAfter = "2h";
  enableInternet = true; // SMTP/IMAP to iCloud
  // The container's tick endpoint compares X-Autopilot-Token against this.
  // ERASER_CONFIG_KEY encrypts the SMTP/IMAP passwords in config.yaml (and so in the R2 snapshot).
  envVars = {
    ERASER_AUTOPILOT_TOKEN: this.env.ERASER_AUTOPILOT_TOKEN ?? "",
    ERASER_CONFIG_KEY: this.env.ERASER_CONFIG_KEY ?? "",
  };
}

/** Failed sign-ins of one client (one instance per IP or IPv6 /64). */
export class LoginLimiter extends DurableObject<Env> {
  private async failures(): Promise<number[]> {
    return recentFailures((await this.ctx.storage.get<number[]>("f")) ?? [], Date.now());
  }
  async retryAfter(): Promise<number> {
    return retryAfterSeconds(await this.failures(), Date.now());
  }
  async fail(): Promise<void> {
    const now = Date.now();
    await this.ctx.storage.put("f", [...(await this.failures()), now]);
    await this.ctx.storage.setAlarm(now + LOGIN_WINDOW_MS);
  }
  async clear(): Promise<void> {
    await this.ctx.storage.deleteAll();
  }
  async alarm(): Promise<void> {
    await this.ctx.storage.deleteAll(); // every failure has left the window
  }
}

function limiter(request: Request, env: Env): DurableObjectStub<LoginLimiter> {
  return env.LOGIN_LIMITER.get(env.LOGIN_LIMITER.idFromName(limiterKey(request.headers.get("CF-Connecting-IP"))));
}

// The container reads/writes its ~/.eraser snapshot via http://state.internal/
EraserContainer.outboundByHost = {
  "state.internal": async (request: Request, env: Env) => {
    if (request.method === "GET") {
      const obj = await env.STATE.get(STATE_KEY);
      return obj ? new Response(obj.body) : new Response(null, { status: 404 });
    }
    if (request.method === "PUT") {
      await env.STATE.put(STATE_KEY, request.body);
      return new Response(null, { status: 204 });
    }
    return new Response(null, { status: 405 });
  },
};

function timingSafeEqual(a: string, b: string): boolean {
  const enc = new TextEncoder();
  const x = enc.encode(a), y = enc.encode(b);
  if (x.length !== y.length) return false;
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i] ^ y[i];
  return diff === 0;
}

const COOKIE = "eraser_auth";
const SESSION_DAYS = 30;

function basicAuthOk(request: Request, password: string): boolean {
  const header = request.headers.get("Authorization") ?? "";
  if (!header.startsWith("Basic ")) return false;
  let decoded: string;
  try {
    decoded = atob(header.slice(6));
  } catch {
    return false; // malformed base64
  }
  return timingSafeEqual(decoded.slice(decoded.indexOf(":") + 1), password);
}

// Session cookie = "<expiry>.<HMAC-SHA256(expiry)>", keyed with the password,
// so rotating ERASER_PASSWORD logs out every session.
async function sign(expiry: string, password: string): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw", new TextEncoder().encode(password), { name: "HMAC", hash: "SHA-256" }, false, ["sign"],
  );
  const mac = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(expiry));
  return btoa(String.fromCharCode(...new Uint8Array(mac))).replace(/=+$/, "");
}

async function cookieOk(request: Request, password: string): Promise<boolean> {
  const match = (request.headers.get("Cookie") ?? "").match(/(?:^|;\s*)eraser_auth=([^;]+)/);
  if (!match) return false;
  const [expiry, mac] = match[1].split(".");
  if (!expiry || !mac || Number(expiry) < Date.now()) return false;
  return timingSafeEqual(mac, await sign(expiry, password));
}

function safeNext(next: string | null): string {
  return next && next.startsWith("/") && !next.startsWith("//") ? next : "/";
}

function loginPage(next: string, error = "", status = 401): Response {
  const esc = (s: string) => s.replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
  return new Response(
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Eraser – Sign in</title><meta name="robots" content="noindex">
<style>
:root{color-scheme:light dark}body{margin:0;min-height:100vh;display:grid;place-items:center;font:16px/1.5 system-ui,sans-serif;background:#f6f7f9;color:#111}
@media (prefers-color-scheme:dark){body{background:#111317;color:#eee}form{background:#1b1e24!important;border-color:#2b2f37!important}input{background:#111317!important;color:#eee!important;border-color:#2b2f37!important}}
form{width:min(340px,calc(100vw - 32px));padding:28px;background:#fff;border:1px solid #e3e5e8;border-radius:14px}
h1{margin:0 0 4px;font-size:22px}p{margin:0 0 18px;opacity:.7;font-size:14px}
label{display:block;font-size:14px;margin-bottom:6px}input{width:100%;box-sizing:border-box;padding:10px 12px;font-size:16px;border:1px solid #d0d4da;border-radius:8px}
button{margin-top:14px;width:100%;padding:10px;font-size:16px;border:0;border-radius:8px;background:#2563eb;color:#fff;cursor:pointer}
.err{color:#dc2626;font-size:14px;margin:10px 0 0}
</style></head><body>
<form method="post" action="/__login"><h1>Eraser</h1><p>Enter your password to continue.</p>
<label for="pw">Password</label><input id="pw" name="password" type="password" autocomplete="current-password" autofocus required>
<input type="hidden" name="next" value="${esc(next)}">
<button type="submit">Sign in</button>${error ? `<p class="err">${esc(error)}</p>` : ""}</form></body></html>`,
    { status, headers: { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store" } },
  );
}

function tooManyAttempts(next: string, retryAfter: number): Response {
  const minutes = Math.ceil(retryAfter / 60);
  const res = loginPage(
    next,
    `Too many failed attempts. Please wait ${minutes} minute${minutes === 1 ? "" : "s"} and try again.`,
    429,
  );
  res.headers.set("Retry-After", String(retryAfter));
  return res;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const password = env.ERASER_PASSWORD;
    const url = new URL(request.url);
    // Only the cron trigger (scheduled handler) talks to /internal/*.
    if (isInternalPath(url.pathname)) return new Response("Not found", { status: 404 });
    if (!password) return new Response("Not configured", { status: 503 });
    // Mail clients load the pixel without credentials; forward only the path and User-Agent
    // (no cookies, auth or client IP reach the container).
    if (isPixelRequest(request.method, url.pathname)) {
      const headers = new Headers();
      const ua = request.headers.get("User-Agent");
      if (ua) headers.set("User-Agent", ua);
      return getContainer(env.ERASER, "main").fetch(
        new Request(`http://container${url.pathname}`, { method: request.method, headers }),
      );
    }

    if (url.pathname === "/__login" && request.method === "POST") {
      const form = await request.formData();
      const next = safeNext(String(form.get("next") ?? "/"));
      const limit = limiter(request, env);
      const wait = await limit.retryAfter();
      if (wait) return tooManyAttempts(next, wait);
      // trim: pasted passwords often carry a trailing newline
      if (!timingSafeEqual(String(form.get("password") ?? "").trim(), password)) {
        await limit.fail();
        return loginPage(next, "Wrong password.");
      }
      await limit.clear();
      const expiry = String(Date.now() + SESSION_DAYS * 86400_000);
      return new Response(null, {
        status: 303,
        headers: {
          Location: next,
          "Set-Cookie": `${COOKIE}=${expiry}.${await sign(expiry, password)}; Path=/; Max-Age=${SESSION_DAYS * 86400}; HttpOnly; Secure; SameSite=Lax`,
        },
      });
    }
    if (url.pathname === "/__logout") {
      return new Response(null, {
        status: 303,
        headers: { Location: "/", "Set-Cookie": `${COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax` },
      });
    }

    // Basic auth still works for scripts (curl -u eraser:...); wrong Basic credentials count
    // towards the same limit as the sign-in form.
    const next = safeNext(url.pathname + url.search);
    if (!(await cookieOk(request, password))) {
      if (!request.headers.get("Authorization")?.startsWith("Basic ")) return loginPage(next);
      const limit = limiter(request, env);
      const wait = await limit.retryAfter();
      if (wait) return tooManyAttempts(next, wait);
      if (!basicAuthOk(request, password)) {
        await limit.fail();
        return loginPage(next);
      }
    }
    // Single-user app: one container instance holds all state.
    return getContainer(env.ERASER, "main").fetch(request);
  },

  // Cron trigger (wrangler.jsonc): one autopilot round every 6 hours. The container answers
  // 202 (started), 200 (autopilot switched off) or 409 (a round is still running).
  async scheduled(_controller: ScheduledController, env: Env): Promise<void> {
    if (!env.ERASER_AUTOPILOT_TOKEN) {
      console.warn("autopilot: ERASER_AUTOPILOT_TOKEN is not set, skipping");
      return;
    }
    const res = await getContainer(env.ERASER, "main").fetch(
      new Request("http://container/internal/autopilot/tick", {
        method: "POST",
        headers: { "X-Autopilot-Token": env.ERASER_AUTOPILOT_TOKEN },
      }),
    );
    console.log(`autopilot tick: ${res.status} ${await res.text()}`);
  },
};
