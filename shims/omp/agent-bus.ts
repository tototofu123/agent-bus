/**
 * agent-bus shim for omp (oh-my-pi).
 *
 * Makes an omp session a participant on the local agent bus without the model
 * having to be told: on session start it registers, joins the room for the
 * current project, and shows it in the footer. Inbound messages are polled and
 * surfaced in the TUI. A `bus_send` tool is registered so the model can post.
 *
 * Load it per-session:
 *   omp --extension path/to/agent-bus.ts
 *
 * Or auto-discover it by dropping this file in `~/.omp/agent/hooks/pre/`.
 *
 * Notes from omp's own docs, learned the hard way:
 *  - `ctx.ui.setStatus` lands in the footer and is reliably visible.
 *    `ctx.ui.notify` from `session_start` can be painted-then-cleared during
 *    startup, so it is not trusted for the "I joined" signal.
 *  - In `-p`/print mode the UI context is a no-op, so status writes vanish.
 *    The bus work still happens; only the visible feedback is lost.
 */

import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { basename, join } from "node:path";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

const PORT = process.env.AGENT_BUS_PORT ?? "47113";

/** Locate the bus CLI. Prefer PATH, fall back to the standard install. */
function findBus(): string | null {
  const fromEnv = process.env.AGENT_BUS_CLI;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  const localAppData = process.env.LOCALAPPDATA;
  if (localAppData) {
    const win = join(localAppData, "agent-bus", "bin", "bus.exe");
    if (existsSync(win)) return win;
  }
  const home = process.env.HOME ?? process.env.USERPROFILE;
  if (home) {
    const nix = join(home, ".local", "bin", "bus");
    if (existsSync(nix)) return nix;
  }
  return null; // let execFileSync fall back to PATH lookup
}

function bus(args: string[], timeout = 15_000): string {
  const exe = findBus();
  return execFileSync(exe ?? "bus", args, {
    encoding: "utf8",
    timeout,
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

export default function agentBusShim(pi: ExtensionAPI) {
  let room = "";
  let agentId = "";
  let stopped = false;

  const identify = (cwd: string) => {
    const project = basename(cwd) || "workspace";
    agentId = process.env.AGENT_ID ?? `omp@${project}`;
    room = process.env.AGENT_BUS_ROOM ?? `#repo:${project}`;
  };

  /** Register + join. Failures are reported, never thrown, so a bus outage
   *  cannot stop someone from working. */
  const attach = (ctx: any) => {
    const status = (text: string, variant: "info" | "warn" = "info") => {
      try {
        ctx?.ui?.setStatus?.("agent-bus", text, variant);
      } catch {
        /* UI is optional */
      }
    };
    try {
      bus(["register", "--agent", agentId, "--harness", "omp"]);
      bus(["join", "--agent", agentId, "--room", room]);
      status(`${room} · joined`);
    } catch (e) {
      const why = e instanceof Error ? e.message.split("\n")[0] : String(e);
      status(`offline: ${why}`, "warn");
    }
  };

  pi.on("session_start", (_event, ctx) => {
    identify(ctx?.cwd ?? process.cwd());
    attach(ctx);

    // Poll for inbound traffic. setInterval is session-owned: omp clears it on
    // shutdown, so nothing leaks if the session dies.
    ctx?.setInterval?.(() => {
      if (stopped) return;
      try {
        // --format json is required: the default is human-readable text.
        const raw = bus([
          "recv",
          "--agent",
          agentId,
          "--room",
          room,
          "--timeout",
          "20",
          "--format",
          "json",
        ]);
        if (!raw) return;
        const env = JSON.parse(raw);
        const from = env?.from?.id ?? "someone";
        const text = (env?.text ?? "").slice(0, 200);
        try {
          ctx.ui?.notify?.(`agent-bus · ${env.kind} from ${from}: ${text}`, "info");
        } catch {
          /* notify is a no-op in some modes; the poll still drained it */
        }
      } catch {
        /* timeout exits non-zero by design; nothing to report */
      }
    }, 1000);

    // Let the model post to the bus without shelling out itself.
    pi.registerTool({
      name: "bus_send",
      label: "Bus Send",
      description:
        "Post a message to the agent bus room. Use it to talk to other agent harnesses sharing this project (omp, opencode, claude, codex).",
      parameters: pi.zod.object({
        text: pi.zod.string().describe("The message body"),
        kind: pi.zod
          .string()
          .optional()
          .describe("Message kind: chat, question, answer, status (default chat)"),
        replyTo: pi.zod.string().optional().describe("Message id being replied to"),
      }),
      approval: "read",
      async execute(_id: string, params: { text: string; kind?: string; replyTo?: string }) {
        try {
          const args = [
            "send",
            "--agent",
            agentId,
            "--room",
            room,
            "--kind",
            params.kind ?? "chat",
            "--text",
            params.text,
          ];
          if (params.replyTo) args.push("--reply-to", params.replyTo);
          const out = bus(args);
          return {
            content: [{ type: "text", text: `sent to ${room}: ${out}` }],
            details: { room, kind: params.kind ?? "chat" },
          };
        } catch (e) {
          const why = e instanceof Error ? e.message : String(e);
          return {
            content: [{ type: "text", text: `bus send failed: ${why}` }],
            details: { error: why },
            isError: true,
          };
        }
      },
    });
  });

  pi.on("session_shutdown", () => {
    stopped = true;
  });
}