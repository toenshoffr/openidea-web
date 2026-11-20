/**
 * JSON-RPC 2.0 over one WebSocket, one message per text frame: the protocol of
 * `ide_host::rpc`, spoken to the server that served the page.
 */
import { t } from "../i18n";

export interface RpcFailure {
  code: number;
  message: string;
}

export const METHOD_NOT_FOUND = -32601;

type Notification = (method: string, params: unknown) => void;

export class Transport {
  private socket: WebSocket | null = null;
  private nextId = 1;
  private readonly pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: RpcFailure) => void }>();
  private opened: Promise<void> | null = null;

  constructor(
    private readonly onNotification: Notification,
    private readonly onClosed: () => void,
  ) {}

  /** The socket address relative to the page, so a proxy prefix (`/user/lab/proxy/8080/`) is kept. */
  static address(base: string = document.baseURI): string {
    const url = new URL("ws", base);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    return url.toString();
  }

  open(): Promise<void> {
    this.opened ??= new Promise<void>((resolve, reject) => {
      const socket = new WebSocket(Transport.address());
      this.socket = socket;
      socket.onopen = () => resolve();
      socket.onerror = () => reject(new Error("cannot reach the OpenIDEA server"));
      socket.onmessage = (event) => this.receive(String(event.data));
      socket.onclose = () => {
        this.failAll(t("Connection to the OpenIDEA server closed."));
        this.onClosed();
      };
    });
    return this.opened;
  }

  request<T>(method: string, params: unknown): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const socket = this.socket;
      if (!socket || socket.readyState !== WebSocket.OPEN) {
        reject({
          code: -32000,
          message: t("Connection to the OpenIDEA server closed."),
        } satisfies RpcFailure);
        return;
      }
      const id = this.nextId++;
      this.pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
      });
      socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  private receive(text: string) {
    let message: {
      id?: number;
      method?: string;
      params?: unknown;
      result?: unknown;
      error?: RpcFailure;
    };
    try {
      message = JSON.parse(text);
    } catch {
      return;
    }
    if (typeof message.id === "number" && !message.method) {
      const waiting = this.pending.get(message.id);
      if (!waiting) return;
      this.pending.delete(message.id);
      if (message.error) waiting.reject(message.error);
      else waiting.resolve(message.result);
    } else if (message.method && message.id === undefined) {
      this.onNotification(message.method, message.params);
    }
  }

  private failAll(why: string) {
    for (const waiting of this.pending.values()) waiting.reject({ code: -32000, message: why });
    this.pending.clear();
  }
}
