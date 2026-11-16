/**
 * Poke relay — Cloudflare Worker + one Durable Object per team.
 *
 * The relay is deliberately dumb: it authenticates the team token during the
 * WebSocket upgrade, tracks who is online, and forwards sealed (end-to-end
 * encrypted) blobs between teammates. It never stores or logs message content
 * and cannot decrypt it.
 *
 * Wire protocol: see crates/poke-core/src/protocol.rs (keep both in sync).
 * Uses the WebSocket Hibernation API so idle connections cost nothing.
 */
import { DurableObject } from "cloudflare:workers";

export interface Env {
  TEAM: DurableObjectNamespace<Team>;
  /** Optional comma-separated list of team ids allowed to use this relay. Empty = any team. */
  ALLOWED_TEAMS?: string;
}

const TOKEN_RE = /^[A-Za-z0-9_-]{43}$/; // 32 bytes, base64url, no padding
const ID_RE = /^[A-Za-z0-9-]{8,64}$/;
const PID_RE = /^[A-Za-z0-9-]{1,64}$/;
const MAX_FRAME = 8 * 1024;
const MAX_PROFILE = 1024;
const MAX_BODY = 4096;
const POKES_PER_MINUTE = 12;
/** Same as POKE_COOLDOWN_SECS in crates/poke-core/src/protocol.rs. */
const COOLDOWN_MS = 60_000;
const WS_OPEN = 1;

/** Must match `team_id_for_token` in crates/poke-core/src/crypto.rs. */
export async function teamIdForToken(token: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(`poke-team-id/v1:${token}`));
  return [...new Uint8Array(digest).slice(0, 16)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export default {
  async fetch(req, env): Promise<Response> {
    const url = new URL(req.url);
    if (url.pathname === "/") {
      return new Response("Poke relay is running.\n", { headers: { "content-type": "text/plain" } });
    }
    if (url.pathname === "/health") {
      return Response.json({ ok: true, protocol: 1 });
    }
    if (url.pathname !== "/ws") {
      return new Response("Not found\n", { status: 404 });
    }
    if (req.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("Expected a WebSocket upgrade\n", { status: 426 });
    }

    const auth = req.headers.get("Authorization") ?? "";
    const token = auth.startsWith("Bearer ") ? auth.slice(7).trim() : "";
    const clientId = req.headers.get("X-Poke-Client") ?? "";
    if (!TOKEN_RE.test(token)) {
      return new Response("missing or malformed team token", { status: 401 });
    }
    if (!ID_RE.test(clientId)) {
      return new Response("missing or malformed client id", { status: 400 });
    }

    const teamId = await teamIdForToken(token);
    const allowed = (env.ALLOWED_TEAMS ?? "").split(",").map((s) => s.trim()).filter(Boolean);
    if (allowed.length > 0 && !allowed.includes(teamId)) {
      return new Response(`team ${teamId} is not allowed on this relay`, { status: 403 });
    }

    const stub = env.TEAM.get(env.TEAM.idFromName(teamId));
    return stub.fetch(req);
  },
} satisfies ExportedHandler<Env>;

/** Per-connection state; survives hibernation. Must stay under 2 KB. */
interface Attachment {
  id: string;
  /** Sealed profile; null until the client said hello. */
  profile: string | null;
  /** Timestamps (ms) of recent pokes, for rate limiting. */
  pokes: number[];
  /** Recipient id -> time (ms) we last delivered a poke to them (pruned after a minute). */
  last?: Record<string, number>;
  /** Time (ms) of our last "poke everyone". */
  all?: number;
}

type ClientFrame =
  | { t: "hello"; v: number; profile: string }
  | { t: "profile"; profile: string }
  | { t: "poke"; to: string; pid: string; body: string }
  | { t: "poke_all"; pid: string; body: string }
  | { t: "reply"; to: string; pid: string; body: string };

export class Team extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // Answered by the runtime without waking the object: keep-alives are free.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(req: Request): Promise<Response> {
    const id = req.headers.get("X-Poke-Client") ?? "";
    if (!ID_RE.test(id)) return new Response("bad client id", { status: 400 });

    // One live connection per client id: the newest wins.
    for (const old of this.ctx.getWebSockets(id)) {
      try {
        old.close(4001, "replaced by a newer connection");
      } catch {
        /* already closed */
      }
    }

    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    this.ctx.acceptWebSocket(server, [id]);
    server.serializeAttachment({ id, profile: null, pokes: [] } satisfies Attachment);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    if (typeof message !== "string" || message.length > MAX_FRAME) {
      return this.error(ws, "bad_frame", "expected a JSON text frame under 8 KB");
    }
    let frame: ClientFrame;
    try {
      frame = JSON.parse(message);
    } catch {
      return this.error(ws, "bad_json", "frame is not valid JSON");
    }
    const me = ws.deserializeAttachment() as Attachment;

    switch (frame.t) {
      case "hello":
      case "profile": {
        if (!isStr(frame.profile, MAX_PROFILE)) return this.error(ws, "bad_profile", "invalid profile");
        const first = me.profile === null;
        me.profile = frame.profile;
        ws.serializeAttachment(me);
        if (frame.t === "hello" || first) {
          const peers = this.online().filter((p) => p.id !== me.id);
          send(ws, { t: "welcome", you: me.id, peers });
        }
        this.broadcast({ t: "join", id: me.id, profile: me.profile }, me.id);
        return;
      }

      case "poke": {
        if (me.profile === null) return this.error(ws, "not_ready", "send hello first");
        if (!ID_RE.test(frame.to) || !PID_RE.test(frame.pid) || !isStr(frame.body, MAX_BODY)) {
          return this.error(ws, "bad_poke", "invalid poke");
        }
        const now = prune(me);
        if (me.pokes.length >= POKES_PER_MINUTE) {
          return send(ws, { t: "ack", pid: frame.pid, to: frame.to, status: "rate_limited" });
        }
        if (now - Math.max(me.last![frame.to] ?? 0, me.all ?? 0) < COOLDOWN_MS) {
          return send(ws, { t: "ack", pid: frame.pid, to: frame.to, status: "too_soon" });
        }
        const delivered = this.forward(frame.to, { t: "poke", from: me.id, pid: frame.pid, body: frame.body });
        if (delivered) {
          me.pokes.push(now);
          me.last![frame.to] = now;
          ws.serializeAttachment(me);
        }
        return send(ws, { t: "ack", pid: frame.pid, to: frame.to, status: delivered ? "delivered" : "offline" });
      }

      case "poke_all": {
        if (me.profile === null) return this.error(ws, "not_ready", "send hello first");
        if (!PID_RE.test(frame.pid) || !isStr(frame.body, MAX_BODY)) {
          return this.error(ws, "bad_poke", "invalid poke_all");
        }
        const now = prune(me);
        if (me.pokes.length >= POKES_PER_MINUTE) {
          return send(ws, { t: "ack", pid: frame.pid, to: "*", status: "rate_limited" });
        }
        if (now - (me.all ?? 0) < COOLDOWN_MS) {
          return send(ws, { t: "ack", pid: frame.pid, to: "*", status: "too_soon" });
        }
        const msg = { t: "poke", from: me.id, pid: frame.pid, body: frame.body, all: true };
        let reached = 0;
        for (const peer of this.online()) {
          if (peer.id === me.id) continue;
          // Someone we poked individually moments ago is skipped, not poked twice.
          if (now - (me.last![peer.id] ?? 0) < COOLDOWN_MS) {
            send(ws, { t: "ack", pid: frame.pid, to: peer.id, status: "too_soon" });
            continue;
          }
          if (this.forward(peer.id, msg)) {
            reached++;
            send(ws, { t: "ack", pid: frame.pid, to: peer.id, status: "delivered" });
          }
        }
        if (reached === 0) {
          return send(ws, { t: "ack", pid: frame.pid, to: "*", status: "offline" });
        }
        // `last` is not filled per recipient on purpose (it must stay < 2 KB);
        // `all` covers everyone for the next minute instead.
        me.pokes.push(now);
        me.all = now;
        ws.serializeAttachment(me);
        return;
      }

      case "reply": {
        if (me.profile === null) return this.error(ws, "not_ready", "send hello first");
        if (!ID_RE.test(frame.to) || !PID_RE.test(frame.pid) || !isStr(frame.body, MAX_BODY)) {
          return this.error(ws, "bad_reply", "invalid reply");
        }
        this.forward(frame.to, { t: "reply", from: me.id, pid: frame.pid, body: frame.body });
        return;
      }

      default:
        return this.error(ws, "unknown_type", "unknown frame type");
    }
  }

  async webSocketClose(ws: WebSocket, code: number): Promise<void> {
    this.gone(ws, code);
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.gone(ws, 1011);
  }

  private gone(ws: WebSocket, code: number) {
    try {
      // Reserved codes (1005/1006/1015) cannot be sent back.
      ws.close(code >= 3000 || code === 1000 || code === 1001 ? code : 1000, "bye");
    } catch {
      /* already closed */
    }
    const me = ws.deserializeAttachment() as Attachment | null;
    if (!me?.profile) return;
    const stillHere = this.ctx
      .getWebSockets(me.id)
      .some((o) => o !== ws && o.readyState === WS_OPEN && (o.deserializeAttachment() as Attachment)?.profile);
    if (!stillHere) this.broadcast({ t: "leave", id: me.id }, me.id);
  }

  /** Everyone who completed hello, deduplicated by client id. */
  private online(): { id: string; profile: string }[] {
    const byId = new Map<string, string>();
    for (const ws of this.ctx.getWebSockets()) {
      if (ws.readyState !== WS_OPEN) continue;
      const a = ws.deserializeAttachment() as Attachment | null;
      if (a?.profile) byId.set(a.id, a.profile);
    }
    return [...byId].map(([id, profile]) => ({ id, profile }));
  }

  private forward(to: string, msg: object): boolean {
    let delivered = false;
    const json = JSON.stringify(msg);
    for (const ws of this.ctx.getWebSockets(to)) {
      const a = ws.deserializeAttachment() as Attachment | null;
      if (ws.readyState !== WS_OPEN || !a?.profile) continue;
      try {
        ws.send(json);
        delivered = true;
      } catch {
        /* socket died between check and send */
      }
    }
    return delivered;
  }

  private broadcast(msg: object, exceptId: string) {
    const json = JSON.stringify(msg);
    for (const ws of this.ctx.getWebSockets()) {
      const a = ws.deserializeAttachment() as Attachment | null;
      if (ws.readyState !== WS_OPEN || !a?.profile || a.id === exceptId) continue;
      try {
        ws.send(json);
      } catch {
        /* ignore */
      }
    }
  }

  private error(ws: WebSocket, code: string, msg: string) {
    send(ws, { t: "error", code, msg });
  }
}

/** Drop entries older than the rate-limit windows; returns now. */
function prune(me: Attachment): number {
  const now = Date.now();
  me.pokes = me.pokes.filter((t) => now - t < 60_000);
  me.last = Object.fromEntries(Object.entries(me.last ?? {}).filter(([, t]) => now - t < COOLDOWN_MS));
  return now;
}

function send(ws: WebSocket, msg: object) {
  try {
    ws.send(JSON.stringify(msg));
  } catch {
    /* ignore */
  }
}

function isStr(v: unknown, max: number): v is string {
  return typeof v === "string" && v.length > 0 && v.length <= max;
}
