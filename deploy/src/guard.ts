/** Paths only the cron trigger may reach; the public fetch handler answers 404 for them. */
export function isInternalPath(pathname: string): boolean {
  let path = pathname;
  try {
    path = decodeURIComponent(pathname);
  } catch {
    // malformed escapes: check the raw path
  }
  return /^\/+internal(\/|$)/i.test(path);
}

/** The open-tracking pixel: public (no login), GET/HEAD and exactly /p/<token>[.gif] only. */
export function isPixelRequest(method: string, pathname: string): boolean {
  return (method === "GET" || method === "HEAD") && /^\/p\/[A-Za-z0-9_-]{16,64}(\.gif)?$/.test(pathname);
}

export const LOGIN_MAX_FAILURES = 10;
export const LOGIN_WINDOW_MS = 15 * 60_000;

/** Failure timestamps still inside the window. */
export function recentFailures(times: number[], now: number): number[] {
  return times.filter((t) => now - t < LOGIN_WINDOW_MS);
}

/** Seconds until the next attempt is allowed; 0 when not limited. */
export function retryAfterSeconds(times: number[], now: number): number {
  const recent = recentFailures(times, now).sort((a, b) => a - b);
  if (recent.length < LOGIN_MAX_FAILURES) return 0;
  const freesAt = recent[recent.length - LOGIN_MAX_FAILURES] + LOGIN_WINDOW_MS;
  return Math.max(1, Math.ceil((freesAt - now) / 1000));
}

/** Limiter bucket per client: the IPv4 address, or the /64 of an IPv6 address (one host owns a whole /64). */
export function limiterKey(ip: string | null): string {
  if (!ip) return "unknown";
  if (!ip.includes(":")) return ip;
  const [head, tail = ""] = ip.split("::");
  const h = head ? head.split(":") : [];
  const t = tail ? tail.split(":") : [];
  const groups = ip.includes("::") ? [...h, ...Array(Math.max(0, 8 - h.length - t.length)).fill("0"), ...t] : h;
  return groups.slice(0, 4).map((g) => parseInt(g || "0", 16).toString(16)).join(":") + "::/64";
}
