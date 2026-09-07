/**
 * KnotProvider — y-protocol v1 WebSocket client.
 *
 * Wire format mirrors the server's `crates/knot-server/src/protocol.rs`:
 *   <msg_type:u8> [<sync_subtype:u8>] <varuint length> <payload bytes>
 */

import * as Y from "yjs";
import { Awareness, encodeAwarenessUpdate, applyAwarenessUpdate } from "y-protocols/awareness";

const MSG_SYNC = 0;
const MSG_AWARENESS = 1;
const MSG_COMMENTS = 5;
const SYNC_STEP_1 = 0;
const SYNC_STEP_2 = 1;
const SYNC_UPDATE = 2;

export type ProviderStatus =
  | "connecting"
  | "connected"
  | "offline"
  | "unauthorised"
  | "conflict";

export type CommentChangeMsg = { doc_id: string };

export type ProviderEvents = {
  status: (s: ProviderStatus) => void;
  comments: (msg: CommentChangeMsg) => void;
};

type Listeners = { [K in keyof ProviderEvents]: Array<ProviderEvents[K]> };

export class KnotProvider {
  readonly doc: Y.Doc;
  readonly awareness: Awareness;
  readonly url: string;
  status: ProviderStatus = "connecting";
  private ws: WebSocket | null = null;
  private destroyed = false;
  private listeners: Listeners = { status: [], comments: [] };
  private reconnectAttempt = 0;
  private reconnectTimer: number | null = null;

  constructor(opts: { url: string; doc: Y.Doc; awareness?: Awareness }) {
    this.url = opts.url;
    this.doc = opts.doc;
    this.awareness = opts.awareness ?? new Awareness(opts.doc);
    this.connect();
    this.doc.on("update", this.handleDocUpdate);
    this.awareness.on("update", this.handleAwarenessUpdate);
  }

  on<K extends keyof ProviderEvents>(k: K, fn: ProviderEvents[K]) {
    this.listeners[k].push(fn);
  }
  off<K extends keyof ProviderEvents>(k: K, fn: ProviderEvents[K]) {
    this.listeners[k] = this.listeners[k].filter((f) => f !== fn) as Listeners[K];
  }

  /** Bytes the WebSocket has accepted but not yet pushed onto the wire.
   *  Drops to 0 once the OS socket has drained — a reasonable proxy for
   *  "all local edits have reached the server" given the lack of a
   *  per-update ACK in y-protocol v1. Returns 0 when the socket is closed
   *  because there's nothing useful to report. */
  pendingBytes(): number {
    return this.ws?.bufferedAmount ?? 0;
  }

  destroy() {
    this.destroyed = true;
    this.doc.off("update", this.handleDocUpdate);
    this.awareness.off("update", this.handleAwarenessUpdate);
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ws?.close();
    this.ws = null;
  }

  private setStatus(s: ProviderStatus) {
    this.status = s;
    this.listeners.status.forEach((fn) => fn(s));
  }

  private connect() {
    if (this.destroyed) return;
    this.setStatus("connecting");
    const ws = new WebSocket(this.url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.reconnectAttempt = 0;
      this.setStatus("connected");
      const sv = Y.encodeStateVector(this.doc);
      ws.send(encodeSync(SYNC_STEP_1, sv));
      const clients = [this.awareness.clientID];
      const ar = encodeAwarenessUpdate(this.awareness, clients);
      ws.send(encodeAwareness(ar));
    };
    ws.onmessage = (e) => this.handleFrame(new Uint8Array(e.data as ArrayBuffer));
    ws.onclose = (e) => {
      this.ws = null;
      if (this.destroyed) return;
      if (e.code === 4403) {
        this.setStatus("unauthorised");
        return;
      }
      if (e.code === 4408 || e.code === 4500) {
        this.setStatus("conflict");
        return;
      }
      this.setStatus("offline");
      this.scheduleReconnect();
    };
    ws.onerror = () => {
      // onclose fires next; let it do the work.
    };
  }

  private scheduleReconnect() {
    if (this.destroyed) return;
    const backoff = Math.min(30_000, 500 * Math.pow(2, this.reconnectAttempt));
    const jitter = Math.random() * 300;
    this.reconnectAttempt += 1;
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, backoff + jitter);
  }

  private handleFrame(buf: Uint8Array) {
    // A single WS frame may carry multiple concatenated y-protocol messages.
    // Consume the whole buffer; processing only the first would silently drop
    // batched CRDT updates / awareness frames.
    let offset = 0;
    while (offset < buf.length) {
      const next = this.handleMessage(buf, offset);
      // null = malformed/truncated; no-progress = guard against an infinite
      // loop on a degenerate message. Either way, stop.
      if (next === null || next <= offset) return;
      offset = next;
    }
  }

  /** Process one message starting at `start`; return the offset of the next
   *  message, or null if the frame is malformed/truncated. */
  private handleMessage(buf: Uint8Array, start: number): number | null {
    const type = buf[start];
    if (type === MSG_SYNC) {
      if (buf.length < start + 2) return null;
      const subtype = buf[start + 1];
      const [payload, next] = readVarBytes(buf, start + 2);
      if (!payload) return null;
      switch (subtype) {
        case SYNC_STEP_1: {
          const update = Y.encodeStateAsUpdate(this.doc, payload);
          this.ws?.send(encodeSync(SYNC_STEP_2, update));
          return next;
        }
        case SYNC_STEP_2:
        case SYNC_UPDATE:
          Y.applyUpdate(this.doc, payload, this);
          return next;
        default:
          // Unknown subtype: length is known, so skip and keep going.
          return next;
      }
    } else if (type === MSG_AWARENESS) {
      const [payload, next] = readVarBytes(buf, start + 1);
      if (!payload) return null;
      applyAwarenessUpdate(this.awareness, payload, this);
      return next;
    } else if (type === MSG_COMMENTS) {
      const [payload, next] = readVarBytes(buf, start + 1);
      if (!payload) return null;
      try {
        const msg = JSON.parse(new TextDecoder().decode(payload)) as CommentChangeMsg;
        if (msg.doc_id) this.listeners.comments.forEach((fn) => fn(msg));
      } catch {
        /* malformed — ignore */
      }
      return next;
    }
    // Unknown message type: we can't know its length, so stop consuming.
    return null;
  }

  private handleDocUpdate = (update: Uint8Array, origin: unknown) => {
    if (origin === this) return;
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(encodeSync(SYNC_UPDATE, update));
    }
  };

  private handleAwarenessUpdate = (
    { added, updated, removed }: { added: number[]; updated: number[]; removed: number[] },
    origin: unknown,
  ) => {
    if (origin === this) return;
    const clients = [...added, ...updated, ...removed];
    if (this.ws?.readyState === WebSocket.OPEN && clients.length > 0) {
      const update = encodeAwarenessUpdate(this.awareness, clients);
      this.ws.send(encodeAwareness(update));
    }
  };
}

function encodeVarUint(out: number[], v: number) {
  while (v >= 0x80) {
    out.push((v & 0x7f) | 0x80);
    v >>>= 7;
  }
  out.push(v & 0x7f);
}

function readVarUint(buf: Uint8Array, offset: number): [number, number] | null {
  let v = 0;
  let shift = 0;
  let i = offset;
  while (i < buf.length) {
    const b = buf[i]!;
    v |= (b & 0x7f) << shift;
    i += 1;
    if ((b & 0x80) === 0) return [v >>> 0, i];
    shift += 7;
    if (shift > 35) return null;
  }
  return null;
}

function readVarBytes(buf: Uint8Array, offset: number): [Uint8Array | null, number] {
  const res = readVarUint(buf, offset);
  if (!res) return [null, offset];
  const [len, after] = res;
  if (after + len > buf.length) return [null, offset];
  return [buf.subarray(after, after + len), after + len];
}

function encodeSync(subtype: number, payload: Uint8Array): Uint8Array {
  const head: number[] = [MSG_SYNC, subtype];
  encodeVarUint(head, payload.length);
  const out = new Uint8Array(head.length + payload.length);
  out.set(head, 0);
  out.set(payload, head.length);
  return out;
}

function encodeAwareness(payload: Uint8Array): Uint8Array {
  const head: number[] = [MSG_AWARENESS];
  encodeVarUint(head, payload.length);
  const out = new Uint8Array(head.length + payload.length);
  out.set(head, 0);
  out.set(payload, head.length);
  return out;
}
