import { Container, getContainer } from "@cloudflare/containers";
export { ContainerProxy } from "@cloudflare/containers";

interface Env {
  ERASER: DurableObjectNamespace<EraserContainer>;
  STATE: R2Bucket;
  ERASER_PASSWORD: string;
}

const STATE_KEY = "eraser-home.tar.gz";

export class EraserContainer extends Container<Env> {
  defaultPort = 8080;
  // Bulk sends run in-process for hours; sleeping would pause them.
  sleepAfter = "2h";
  enableInternet = true; // SMTP/IMAP to iCloud
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

function authorized(request: Request, password: string): boolean {
  const header = request.headers.get("Authorization") ?? "";
  if (!header.startsWith("Basic ")) return false;
  const decoded = atob(header.slice(6));
  const pass = decoded.slice(decoded.indexOf(":") + 1);
  return !!password && timingSafeEqual(pass, password);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (!authorized(request, env.ERASER_PASSWORD)) {
      return new Response("Authentication required", {
        status: 401,
        headers: { "WWW-Authenticate": 'Basic realm="Eraser", charset="UTF-8"' },
      });
    }
    // Single-user app: one container instance holds all state.
    return getContainer(env.ERASER, "main").fetch(request);
  },
};
