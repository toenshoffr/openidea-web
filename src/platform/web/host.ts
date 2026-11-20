/**
 * Web mode: `openidea-server --listen` serves this page and answers its commands over a
 * WebSocket, so there is no Tauri runtime. The server marks the page it serves
 * (`<meta name="openidea-web">`); a dev server or a plain static host does not.
 */
export function isWebHost(): boolean {
  return typeof document !== "undefined" && document.querySelector('meta[name="openidea-web"]') !== null;
}
