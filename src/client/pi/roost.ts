// roost-extension: written by `roost hook install --target pi`. Rerun it to update; `roost hook uninstall --target pi` removes it.
import { type ChildProcess, execFile, spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { StringEnum } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const ROOST: { command: string; env: Record<string, string>; presence: string[] } = { command: "roost", env: {}, presence: ["presence"] }; // roost-config

const WATCH_ARGS = ["online", "--follow", "--ndjson", "--timeout", "0"];

export default function (pi: ExtensionAPI) {
	let watch: ChildProcess | undefined;
	let stopping = false;
	let lastExit = "";

	const env = (extra: Record<string, string> = {}) => ({ ...process.env, ...ROOST.env, ...extra });

	const say = (content: string, wake: boolean) =>
		pi.sendMessage({ customType: "roost", content, display: true }, wake ? { triggerTurn: true, deliverAs: "followUp" } : {});

	function presence(ctx: ExtensionContext) {
		execFile(ROOST.command, ROOST.presence, { cwd: ctx.cwd, env: env(), timeout: 20_000 }, (err, stdout) => {
			const text = stdout.trim();
			if (!err && text) say(text, false);
		});
	}

	function deliver(lines: string[], end: { count?: number; max_ackable_id?: number }) {
		const ack =
			end.max_ackable_id === undefined
				? ""
				: ` Once the work is really done, run \`roost ack --up-to ${end.max_ackable_id} --summary "<what you did>"\`.`;
		say(
			`roost: ${end.count ?? lines.length} new from your human (one JSON object per line):\n${lines.join("\n")}\n\n` +
				`Handle it and answer with \`roost message\` (or another send).${ack}`,
			true,
		);
	}

	function start(ctx: ExtensionContext): string {
		if (watch) return "roost: the watch is already running; your human's messages arrive here.";
		stopping = false;
		lastExit = "";
		const child = spawn(ROOST.command, WATCH_ARGS, {
			cwd: ctx.cwd,
			env: env({ ROOST_EVENT_STREAM: "1" }),
			stdio: ["ignore", "pipe", "pipe"],
		});
		watch = child;
		let batch: string[] = [];
		createInterface({ input: child.stdout! }).on("line", (line) => {
			let event: { type?: string; count?: number; max_ackable_id?: number };
			try {
				event = JSON.parse(line);
			} catch {
				return;
			}
			if (event.type === "batch_end") {
				deliver(batch, event);
				batch = [];
			} else {
				batch.push(line);
			}
		});
		let stderr = "";
		child.stderr!.on("data", (chunk) => {
			stderr = (stderr + chunk).slice(-2000);
		});
		const ended = (why: string) => {
			if (watch !== child) return;
			watch = undefined;
			lastExit = `${why}\n${stderr.trim()}`.trim();
			if (!stopping) say(`roost: the watch stopped (${why}); your human can't reach you until it runs again.\n${stderr.trim()}`, true);
		};
		child.on("error", (e) => ended(e.message));
		child.on("exit", (code, signal) => ended(signal ? `signal ${signal}` : `exit ${code}`));
		return "roost: watching. Your human's messages and answers will arrive in this session as they come; it stops when the session ends.";
	}

	function stop(): string {
		if (!watch) return "roost: the watch isn't running.";
		stopping = true;
		watch.kill("SIGTERM");
		return "roost: watch stopped; your human can't reach you now.";
	}

	function status(): string {
		if (watch) return `roost: the watch is running (pid ${watch.pid}).`;
		return lastExit ? `roost: the watch isn't running. It last ended with: ${lastExit}` : "roost: the watch isn't running.";
	}

	const run = (action: string, ctx: ExtensionContext) =>
		action === "start" ? start(ctx) : action === "stop" ? stop() : status();

	pi.registerTool({
		name: "roost_watch",
		label: "roost watch",
		description:
			"Stay reachable by your human through roost. start: their messages and answers to your notifications arrive in this session as they come, until the session ends. stop: go offline. status: is the watch running?",
		promptSnippet: "roost_watch: start/stop the watch that delivers your human's roost messages into this session",
		parameters: Type.Object({ action: StringEnum(["start", "stop", "status"] as const) }),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			return { content: [{ type: "text", text: run(String(params.action), ctx) }], details: {} };
		},
	});

	pi.registerCommand("roost", {
		description: "roost watch: /roost start | stop | status",
		handler: async (args, ctx) => {
			ctx.ui.notify(run(args.trim() || "status", ctx), "info");
		},
	});

	pi.on("session_start", async (_event, ctx) => presence(ctx));
	pi.on("session_shutdown", async () => {
		stop();
	});
}
