/**
 * agent-bus shim for omp (oh-my-pi).
 *
 * Makes an omp session a participant on the local agent bus without the model
 * having to be told: on session start it registers, joins the room for the
 * current project, and shows it in the footer. Inbound messages are polled and
 * surfaced to both the TUI and the model. Tools let the model read and post.
 *
 * Load it per-session:
 *   omp --extension path/to/agent-bus.ts
 *
 * Or auto-discover it by dropping this file in `~/.omp/agent/hooks/pre/`.
 *
 * Everything here is async on purpose. An earlier version used execFileSync in
 * a 1s interval: it blocked omp's event loop for the whole 20s recv timeout,
 * stalling the UI and the agent loop. A trivial prompt went from 2s to over two
 * minutes purely because of that, which looked exactly like a slow model.
 * omp logs this as `ui.loop-blocked`.
 */

import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { basename, join } from "node:path";
import { promisify } from "node:util";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

const run = promisify(execFile);

/** Locate the bus CLI. Prefer an explicit override, then the standard install. */
function findBus(): string {
  const fromEnv = process.env.AGENT_BUS_CLI;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  const localAppData = process.env.LOCALAPPDATA;
  if (localAppData) {
    const win = join(localAppData, "agent-bus", "bin", "bus.exe");
    if (existsSync(win)) return win;
  }
  return "bus"; // fall back to PATH
}

async function bus(args: string[], timeout = 15_000): Promise<string> {
  const { stdout } = await run(findBus(), args, {
    encoding: "utf8",
    timeout,
    windowsHide: true,
    maxBuffer: 4 * 1024 * 1024,
  });
  return String(stdout).trim();
}

export default function agentBusShim(pi: ExtensionAPI) {
  let room = "";
  let agentId = "";
  let stopped = false;
  let polling = false; // never overlap polls; a slow recv must not stack up

  const identify = (cwd: string) => {
    const project = basename(cwd) || "workspace";
    agentId = process.env.AGENT_ID ?? `omp@${project}`;
    room = process.env.AGENT_BUS_ROOM ?? `#repo:${project}`;
  };

  /** Read room history. History rather than recv: history does not consume, so
   *  a model asking later still sees traffic the poller already drained. */
  async function listRoom(limit = 20): Promise<any[]> {
    const raw = await bus(["history", "--room", room, "--format", "json"]);
    return raw
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean)
      .slice(-limit)
      .map((l) => {
        try {
          return JSON.parse(l);
        } catch {
          return { text: l };
        }
      });
  }

  const format = (envs: any[]) =>
    envs.map(
      (e) =>
        `[${e.kind}] ${e.from?.id ?? "?"}${e.reply_to ? ` (reply to ${e.reply_to})` : ""}: ${e.text ?? ""}`,
    );

  pi.on("session_start", async (_event, ctx) => {
    identify(ctx?.cwd ?? process.cwd());

    const status = (text: string, variant: "info" | "warn" = "info") => {
      try {
        ctx?.ui?.setStatus?.("agent-bus", text, variant);
      } catch {
        /* UI is optional */
      }
    };

    // Failures are reported, never thrown: a bus outage must not stop work.
    try {
      await bus(["register", "--agent", agentId, "--harness", "omp"]);
      await bus(["join", "--agent", agentId, "--room", room]);
      status(`${room} · joined`);
    } catch (e) {
      const why = e instanceof Error ? e.message.split("\n")[0] : String(e);
      status(`offline: ${why}`, "warn");
    }

    // Poll for inbound traffic. The short recv keeps a cycle cheap; the loop
    // is async so omp stays responsive between polls.
    ctx?.setInterval?.(() => {
      if (stopped || polling) return;
      polling = true;
      void (async () => {
        try {
          // --format json is required: the default is human-readable text.
          const raw = await bus(
            ["recv", "--agent", agentId, "--room", room, "--timeout", "3", "--format", "json"],
            12_000,
          );
          if (!raw) return;
          const env = JSON.parse(raw);
          const from = env?.from?.id ?? "someone";

          // Tell the model. Without this an agent receives on the bus and never
          // knows, which is the whole failure mode this shim exists to fix.
          try {
            pi.sendMessage?.(
              `[agent-bus] ${from} (${env.kind}) in ${room}:\n${env.text ?? ""}\n` +
                `Reply with the bus_send tool if you want to answer.`,
            );
          } catch {
            /* not available in every mode */
          }
          try {
            ctx?.ui?.notify?.(
              `agent-bus · ${env.kind} from ${from}: ${(env.text ?? "").slice(0, 200)}`,
              "info",
            );
          } catch {
            /* notify is a no-op in some modes */
          }
        } catch {
          /* timeout exits non-zero by design */
        } finally {
          polling = false;
        }
      })();
    }, 1000);

    // Tell the model the room exists at all, or it has no reason to reach for it.
    pi.on("before_agent_start", async () => {
      let recent = "(room unreadable)";
      try {
        recent = format(await listRoom(5)).join("\n") || "(empty)";
      } catch {
        /* keep the fallback */
      }
      return {
        message:
          `You are on the agent bus in room ${room} as ${agentId}. ` +
          `Other agent harnesses talk here. Use bus_send to post and bus_read to read.\n` +
          `Recent traffic:\n${recent}`,
      };
    });

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
          const out = await bus(args);
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

    pi.registerTool({
      name: "bus_read",
      label: "Bus Read",
      description:
        "Read recent traffic from the agent bus room. Use it to see what other agents have said, especially to answer a question addressed to you.",
      parameters: pi.zod.object({
        limit: pi.zod.number().optional().describe("How many recent messages (default 20)"),
      }),
      approval: "read",
      async execute(_id: string, params: { limit?: number }) {
        try {
          const msgs = await listRoom(params.limit ?? 20);
          const lines = format(msgs);
          return {
            content: [
              {
                type: "text",
                text: lines.length ? lines.join("\n") : "(no messages in this room yet)",
              },
            ],
            details: { room, count: msgs.length },
          };
        } catch (e) {
          const why = e instanceof Error ? e.message : String(e);
          return {
            content: [{ type: "text", text: `bus read failed: ${why}` }],
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