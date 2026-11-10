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
