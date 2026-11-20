/**
 * A stand-in for the Tauri runtime in a browser tab: `window.__TAURI_INTERNALS__` as
 * `@tauri-apps/api` expects it, with the commands going to `openidea-server` over a WebSocket.
 * The workbench runs unchanged on top of it: it calls `commands.*`, listens for backend
 * events and opens channels as it does in the desktop shell.
 *
 * Three kinds of command:
 * - plugin commands (events, windows, opener): answered here, the browser being the window;
 * - desktop-shell commands with a browser equivalent (`window_*`, the clipboard): answered here;
 * - everything else: the server's. One it does not have fails as `unavailable`, which the
 *   features already handle for a backend without that capability.
 */
import { METHOD_NOT_FOUND, Transport, type RpcFailure } from "./transport";
import { t } from "../i18n";
import { guessOs } from "../platformInfo";

const WINDOW_LABEL = "main";
const EVENT = "ide://event";
const PROTOCOL_VERSION = 2;

type Callback = (message: unknown) => void;
type Args = Record<string, unknown>;

interface Listener {
  eventId: number;
  event: string;
  handler: number;
}

const callbacks = new Map<number, Callback>();
const listeners = new Map<string, Listener[]>();
const channelCounters = new Map<number, number>();
let nextCallback = 1;
let nextEventId = 1;

function unavailable(message: string) {
  return { kind: "unavailable", message, data: null };
}

function dispatchLocal(event: string, payload: unknown) {
  for (const l of listeners.get(event) ?? []) callbacks.get(l.handler)?.({ event, id: l.eventId, payload });
}

function onNotification(method: string, params: unknown) {
  if (method === "event") {
    const { window, event } = params as {
      window: string | null;
      event: unknown;
    };
    if (window === null || window === WINDOW_LABEL) dispatchLocal(EVENT, event);
  } else if (method === "channel") {
    const { id, data } = params as { id: number; data: unknown };
    const index = channelCounters.get(id) ?? 0;
    channelCounters.set(id, index + 1);
    callbacks.get(id)?.({ index, message: data });
  }
}

/** What a tab shows when its server is gone: the session (documents, terminals) lived there. */
function showConnectionLost() {
  if (document.getElementById("openidea-connection-lost")) return;
  const bar = document.createElement("div");
  bar.id = "openidea-connection-lost";
  bar.setAttribute("role", "alert");
  bar.style.cssText =
    "position:fixed;inset:0 0 auto 0;z-index:2147483647;padding:8px 16px;text-align:center;font:13px system-ui;" +
    "background:var(--color-bg-elevated,#2b2b2b);color:var(--color-fg,#ddd);border-bottom:1px solid var(--color-border,#444)";
  bar.textContent = t("Connection to the OpenIDEA server lost. Reload the page to continue.");
  document.body.appendChild(bar);
}

/** Replaces each `Channel` argument with the id the server streams its values to. */
function encodeArgs(args: unknown): Args {
  const plain = JSON.parse(JSON.stringify(args ?? {})) as Args;
  for (const [key, value] of Object.entries(plain)) {
    const match = typeof value === "string" ? /^__CHANNEL__:(\d+)$/.exec(value) : null;
    if (match) plain[key] = { $channel: Number(match[1]) };
  }
  return plain;
}

function toIdeError(failure: RpcFailure, cmd: string) {
  if (failure.code === METHOD_NOT_FOUND) return unavailable(`${cmd} is not available in the browser`);
  try {
    const parsed = JSON.parse(failure.message) as { kind?: string };
    if (parsed && typeof parsed.kind === "string") return parsed;
  } catch {
    /* not an IdeError */
  }
  return { kind: "internal", message: failure.message, data: null };
}

const noop = () => null;

/** Window commands: the tab is the window. */
const WINDOW: Record<string, (args: Args) => unknown> = {
  get_all_windows: () => [],
  is_focused: () => document.hasFocus(),
  is_visible: () => true,
  is_fullscreen: () => document.fullscreenElement !== null,
  is_maximized: () => false,
  is_minimized: () => false,
  is_decorated: () => true,
  inner_size: () => ({ width: innerWidth, height: innerHeight }),
  outer_size: () => ({ width: innerWidth, height: innerHeight }),
  inner_position: () => ({ x: 0, y: 0 }),
  outer_position: () => ({ x: 0, y: 0 }),
  scale_factor: () => devicePixelRatio,
  theme: () => (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"),
  title: () => document.title,
  set_title: (a) => {
    document.title = String(a.value ?? "");
    return null;
  },
};

const OPENER: Record<string, (args: Args) => unknown> = {
  open_url: (a) => {
    window.open(String(a.url), "_blank", "noopener,noreferrer");
    return null;
  },
};

/** Desktop-shell commands a browser can serve itself. */
const SHELL: Record<string, (args: Args) => unknown> = {
  window_complete_launch: noop,
  menu_configure: noop,
  menu_refresh: noop,
  menu_set_disabled: noop,
  window_group_current: () => [],
  window_group_restore: noop,
  window_is_fullscreen: () => document.fullscreenElement !== null,
  web_browsers: () => [],
  window_toggle_zoom: noop,
  window_start_drag: noop,
  window_flash_taskbar: noop,
  window_set_titlebar_height: noop,
  window_set_smooth_scrolling: noop,
  window_toggle_fullscreen: async () => {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await document.documentElement.requestFullscreen?.();
    return null;
  },
  window_new: () => {
    window.open(location.href, "_blank");
    return null;
  },
  clipboard_text: async () => (await navigator.clipboard?.readText?.().catch(() => "")) ?? "",
  clipboard_write_text: async (a) => {
    await navigator.clipboard?.writeText?.(String(a.text ?? ""));
    return null;
  },
};

async function pluginCommand(cmd: string, args: Args): Promise<unknown> {
  const [plugin = "", name = ""] = cmd.slice("plugin:".length).split("|");
  switch (plugin) {
    case "event":
      return eventCommand(name, args);
    case "window":
    case "webview":
      return (WINDOW[name] ?? noop)(args);
    case "opener":
      return (OPENER[name] ?? noop)(args);
    default:
      // Dialogs, shell and the like have no browser counterpart: nothing was chosen.
      return null;
  }
}

function eventCommand(name: string, args: Args): unknown {
  switch (name) {
    case "listen": {
      const listener: Listener = {
        eventId: nextEventId++,
        event: String(args.event),
        handler: Number(args.handler),
      };
      const list = listeners.get(listener.event) ?? [];
      list.push(listener);
      listeners.set(listener.event, list);
      return listener.eventId;
    }
    case "unlisten":
      unregisterListener(String(args.event), Number(args.eventId));
      return null;
    case "emit":
    case "emit_to":
      dispatchLocal(String(args.event), args.payload);
      return null;
    default:
      return null;
  }
}

function unregisterListener(event: string, eventId: number) {
  const list = listeners.get(event);
  if (list)
    listeners.set(
      event,
      list.filter((l) => l.eventId !== eventId),
    );
}

export function installWebHost() {
  const transport = new Transport(onNotification, showConnectionLost);

  /** The connection to the server. */
  const ready = (async () => {
    await transport.open();
    await transport.request("initialize", { version: PROTOCOL_VERSION });
  })();
  // Reported where the commands fail; the page itself shows the cause on the first failed call.
  ready.catch((e) => console.error("OpenIDEA server unreachable", e));

  async function send(cmd: string, args: Args): Promise<unknown> {
    try {
      return await transport.request("invoke", {
        window: WINDOW_LABEL,
        cmd,
        args: encodeArgs(args),
      });
    } catch (failure) {
      throw toIdeError(failure as RpcFailure, cmd);
    }
  }

  let launched = false;
  /** What this window was opened for: the folder the server was started with, once. */
  async function takeLaunch(): Promise<unknown> {
    if (launched) return null;
    launched = true;
    await ready;
    const { project } = (await send("web_launch", {})) as { project: string | null };
    if (!project) return null;
    return {
      project,
      files: [],
      remote: null,
      settingsPage: null,
      newProjects: false,
      positions: [],
      comparison: null,
      diffWindow: null,
      diff: null,
      diffBeforeFiles: false,
      deployment: null,
      editor: null,
    };
  }

  async function invoke(cmd: string, args: Args = {}): Promise<unknown> {
    if (cmd.startsWith("plugin:")) return pluginCommand(cmd, args);
    const local = SHELL[cmd];
    if (local) return local(args);
    if (cmd === "window_take_launch") return takeLaunch();
    await ready.catch(() => {
      throw unavailable("the OpenIDEA server is unreachable");
    });
    const result = await send(cmd, args);
    if (cmd === "app_info" && result && typeof result === "object") {
      // The keymap and platform conventions are the browser's; paths and processes stay the host's.
      return { ...(result as object), platform: guessOs() };
    }
    return result;
  }

  Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", {
    configurable: true,
    value: { unregisterListener },
  });
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    configurable: true,
    value: {
      metadata: {
        currentWindow: { label: WINDOW_LABEL },
        currentWebview: { label: WINDOW_LABEL, windowLabel: WINDOW_LABEL },
      },
      transformCallback(callback: Callback, once = false) {
        const id = nextCallback++;
        callbacks.set(id, once ? (m) => (callbacks.delete(id), callback(m)) : callback);
        return id;
      },
      unregisterCallback(id: number) {
        callbacks.delete(id);
        channelCounters.delete(id);
      },
      // Local files are not served to the browser yet: the path stays a path.
      convertFileSrc: (path: string) => path,
      invoke,
    },
  });
}
