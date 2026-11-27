import "./styles.css";
import { api, HttpError, type ActionSpec, type Item } from "./api.ts";
import { CHANNEL_GLYPHS, icon } from "./icons.ts";
import { render as markdown } from "./markdown.ts";
import { assertPasskey, b64urlToUint8, createPasskey } from "./webauthn.ts";

type Session = { authenticated: boolean; enrolled: boolean; passkeys_available: boolean; vapid_public_key: string };
type Channel = {
  id: number;
  name: string;
  listening?: boolean;
  last_seen_at?: string | null;
  icon: string | null;
  color: string | null;
  claim: { claimed_by_label: string | null; claimed_at: string | null } | null;
  last: { title: string; created_at: string } | null;
  typing_until?: string | null;
  max_urgency?: string;
  escalate_after_secs?: number;
};
type Child = Node | string | null | undefined | false;

const app = document.querySelector<HTMLElement>("#app")!;
let session: Session;
let channels = new Map<number, Channel>();
let inAppDepth = 0;
/// How the current screen re-renders itself on a live event, and which channel it shows (null = any).
let live: { refresh: () => Promise<void>; channel: number | null; stickToBottom?: boolean } | null = null;
let pendingRefresh = false;
let events: EventSource | null = null;
let composerCache: { key: string; el: HTMLElement } | null = null;

// ---------- DOM helpers ----------

function h<K extends keyof HTMLElementTagNameMap>(tag: K, props: Record<string, any> = {}, ...children: Child[]): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v);
    else if (k === "html") el.innerHTML = v;
    else if (k === "style" && typeof v === "object") Object.assign(el.style, v);
    else if (v === true) el.setAttribute(k, "");
    else if (v !== false && v != null) el.setAttribute(k, String(v));
  }
  for (const c of children) if (c) el.append(c);
  return el;
}

function nav(path: string, replace = false) {
  if (location.pathname + location.search !== path) {
    if (replace) history.replaceState(null, "", path);
    else {
      history.pushState(null, "", path);
      inAppDepth++;
    }
  }
  route();
}

function back(fallback: string) {
  if (inAppDepth > 0) history.back();
  else nav(fallback, true);
}

function roundButton(name: string, onclick: (e: Event) => void, label: string, cls = "") {
  return h("button", { class: `round ${cls}`, "aria-label": label, onclick: (e: Event) => (e.stopPropagation(), onclick(e)) }, icon(name, 22));
}

function toast(msg: string, bad = false) {
  document.querySelector(".toast")?.remove();
  const t = h("div", { class: `toast${bad ? " bad" : ""}` }, msg);
  document.body.append(t);
  setTimeout(() => t.remove(), 3000);
}

async function guard<T>(fn: () => Promise<T>): Promise<T | undefined> {
  try {
    return await fn();
  } catch (e) {
    if (e instanceof HttpError && e.status === 401) {
      session.authenticated = false;
      nav("/login", true);
      return undefined;
    }
    if (e instanceof DOMException && e.name === "NotAllowedError") return undefined;
    toast(e instanceof Error ? e.message : String(e), true);
    return undefined;
  }
}

// ---------- formatting ----------

const hm = (d: Date) => d.toLocaleTimeString("sv-SE", { hour: "2-digit", minute: "2-digit" });

function when(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  const now = new Date();
  const days = Math.round((new Date(now.toDateString()).getTime() - new Date(d.toDateString()).getTime()) / 86400000);
  if (days === 0) return hm(d);
  if (days === 1) return `yesterday ${hm(d)}`;
  if (days === -1) return `tomorrow ${hm(d)}`;
  if (Math.abs(days) < 7) return `${d.toLocaleDateString("en-GB", { weekday: "short" })}, ${hm(d)}`;
  return d.toLocaleDateString("en-GB", { day: "numeric", month: "short" });
}

const plain = (s: string | null | undefined) => (s ?? "").replace(/```[\s\S]*?```/g, "").replace(/[*_#>`]+/g, "").trim();

// ---------- channel identity ----------

const PALETTE = ["#f9cf4a", "#6ee7b7", "#f9a8a0", "#f5a3cf", "#7dd3fc", "#a78bfa", "#93c5fd", "#fdba74"];

const GLYPH_HINTS: [RegExp, string][] = [
  [/home|house|door|light|power|energy/i, "bolt"],
  [/deploy|ci|build|server|ops|shell|terminal|infra/i, "terminal"],
  [/claude|code|agent|dev|bot|ai/i, "chip"],
  [/calendar|meeting|event|schedule/i, "calendar"],
  [/support|chat|triage|mail|inbox/i, "chat"],
  [/finance|money|bank|pay|budget/i, "card"],
  [/research|news|feed|monitor|watch/i, "antenna"],
  [/garden|plant|health/i, "leaf"],
];

function look(id: number) {
  const ch = channels.get(id);
  const color = ch?.color && /^#[0-9a-f]{6}$/i.test(ch.color) ? ch.color : PALETTE[id % PALETTE.length]!;
  const hinted = GLYPH_HINTS.find(([re]) => ch && re.test(ch.name))?.[1];
  const glyph = ch?.icon && CHANNEL_GLYPHS.includes(ch.icon) ? ch.icon : hinted ?? CHANNEL_GLYPHS[id % CHANNEL_GLYPHS.length]!;
  return { color, glyph, name: ch?.name ?? "" };
}

function tile(channelId: number, unread = false) {
  const { color, glyph } = look(channelId);
  return h("span", { class: "tile", style: { color, background: `${color}22` } }, icon(glyph, 22), unread ? h("span", { class: "dot unread" }) : null);
}

function avatar(c: Channel) {
  const { color, glyph } = look(c.id);
  return h("span", { class: "avatar", style: { background: color } }, icon(glyph, 30), h("span", { class: `dot ${c.listening ? "on" : "off"}` }));
}

async function loadChannels() {
  const data = await api<{ channels: Channel[] }>("/channels");
  channels = new Map(data.channels.map((c) => [c.id, c]));
  return data.channels;
}

// ---------- page chrome ----------

type Tab = "home" | "channels" | "settings";

function largePage(opts: { title: string; subtitle?: Child; left?: Child; right?: Child; tab: Tab }, ...body: Child[]) {
  const top = h("div", { class: "topbar" }, opts.left ?? h("span"), opts.right ?? h("span"));
  app.replaceChildren(
    h("div", { class: "page has-tabs" }, top, h("h1", { class: "large" }, opts.title), opts.subtitle ? h("p", { class: "lead" }, opts.subtitle) : null, ...body),
    tabbar(opts.tab),
  );
}

function detailPage(opts: { title: string; subtitle?: string; subtitleClass?: string; backTo: string; right?: Child; footer?: Child }, ...body: Child[]) {
  const top = h(
    "div",
    { class: "topbar centered" },
    roundButton("back", () => back(opts.backTo), "Back"),
    h("div", { class: "center-title" }, h("div", { class: "t" }, opts.title), opts.subtitle ? h("div", { class: `s ${opts.subtitleClass ?? ""}` }, opts.subtitle) : null),
    opts.right ?? h("span", { class: "round-spacer" }),
  );
  const page = h("div", { class: `page${opts.footer ? " has-composer" : ""}` }, top, ...body);
  const footer = opts.footer instanceof Node ? opts.footer : null;
  // Re-inserting a focused textarea would blur it and drop the iOS keyboard.
  if (footer && footer.parentNode === app && app.firstElementChild && app.firstElementChild !== footer) app.firstElementChild.replaceWith(page);
  else app.replaceChildren(page, ...(footer ? [footer] : []));
}

function tabbar(active: Tab) {
  const tab = (id: Tab, name: string, label: string, path: string) =>
    h("button", { class: `tab${id === active ? " active" : ""}`, onclick: () => nav(path) }, icon(name, 26), h("span", {}, label));
  return h("nav", { class: "tabbar" }, tab("home", "home", "Home", "/"), tab("channels", "channels", "Channels", "/channels"), tab("settings", "settings", "Settings", "/settings"));
}

function composer(key: string, placeholder: string, onsend: (text: string) => Promise<boolean | undefined>) {
  if (composerCache?.key === key) return composerCache.el;
  const input = h("textarea", { rows: 1, placeholder, enterkeyhint: "send" });
  const send = h("button", { class: "send", "aria-label": "Send", disabled: true }, icon("send", 22));
  const grow = () => {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 140)}px`;
    send.toggleAttribute("disabled", !input.value.trim());
  };
  input.addEventListener("input", grow);
  const go = async () => {
    const text = input.value.trim();
    if (!text) return;
    send.setAttribute("disabled", "");
    if (await onsend(text)) {
      input.value = "";
      grow();
    } else send.removeAttribute("disabled");
  };
  send.addEventListener("click", go);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      go();
    }
  });
  const el = h("div", { class: "composer" }, input, send);
  composerCache = { key, el };
  return el;
}

function sheet(title: string, ...body: Child[]) {
  const close = () => {
    overlay.remove();
    if (pendingRefresh) liveRefresh();
  };
  const overlay = h(
    "div",
    { class: "overlay", onclick: (e: Event) => e.target === overlay && close() },
    h("div", { class: "sheet" }, h("div", { class: "grabber" }), h("h2", {}, title), ...body),
  );
  document.body.append(overlay);
  return close;
}

// ---------- answering ----------

function primarySpecs(item: Item): ActionSpec[] {
  return item.action_specs.filter((a) => a.id !== "snooze" && a.id !== "reply");
}

const offersSnooze = (item: Item) => item.action_specs.some((a) => a.id === "snooze") || ["important", "urgent", "event"].includes(item.profile);
const isClosed = (item: Item) => (item.responded && item.chosen_action?.kind !== "snoozed") || item.status === "cancelled";
const isDestructive = (s: ActionSpec) => s.style === "destructive" || !!s.destructive;
/// Built-in labels like "Done ✓" carry their own glyph; drop it wherever an icon is drawn too.
const cleanLabel = (label: string) => label.replace(/\s*[✓✔✅☑]\uFE0F?\s*$/u, "") || label;
const NEGATIVE = /^(no|reject|decline|deny|hold|cancel|stop|abort)$/i;

function actionIcon(s: ActionSpec): string {
  if (s.biometric) return "lock";
  if (isDestructive(s) || NEGATIVE.test(s.id)) return "x";
  if (s.id === "later" || s.id === "reschedule") return "snooze";
  if (s.id === "mute") return "bell";
  return "check";
}

async function answer(item: Item, spec: { id: string; label: string; biometric?: boolean; confirm?: boolean }, extra: Record<string, unknown> = {}) {
  if (spec.confirm && !spec.biometric && !confirm(`${spec.label}?`)) return false;
  const body: Record<string, unknown> = { action_id: spec.id, ...extra };
  const ok = await guard(async () => {
    if (spec.biometric) {
      const start = await api(`/notifications/${item.id}/gate`, { body: { action_id: spec.id } });
      body.gate = { challenge_id: start.challenge_id, credential: await assertPasskey(start.options) };
    }
    await api(`/notifications/${item.id}/respond`, { body });
    return true;
  });
  if (ok) toast(spec.id === "snooze" ? "Snoozed" : `Sent · ${spec.label}`);
  return !!ok;
}

function snoozeSheet(item: Item, after: () => void) {
  const tomorrow9 = new Date();
  tomorrow9.setDate(tomorrow9.getDate() + 1);
  tomorrow9.setHours(9, 0, 0, 0);
  const options: [string, number][] = [
    ["10 minutes", 10],
    ["30 minutes", 30],
    ["1 hour", 60],
    ["4 hours", 240],
    ["Tomorrow 09:00", Math.round((tomorrow9.getTime() - Date.now()) / 60000)],
  ];
  const close = sheet(
    "Snooze until…",
    h(
      "div",
      { class: "list-group" },
      ...options.map(([label, minutes]) =>
        h("button", {
          class: "list-row",
          onclick: async () => {
            close();
            if (await answer(item, { id: "snooze", label: "Snooze" }, { snooze_minutes: minutes })) after();
          },
        }, h("span", { class: "row-icon" }, icon("snooze", 20)), h("span", { class: "row-label" }, label)),
      ),
    ),
  );
}

/// The compact inline actions of a feed row or conversation card.
function inlineActions(item: Item, after: () => void) {
  if (isClosed(item) || item.status === "scheduled" || item.status === "queued") return null;
  const specs = primarySpecs(item);
  const gated = specs.some((s) => s.biometric);
  const { color } = look(item.channel_id);
  const buttons: Node[] = [];
  if (!gated && item.profile !== "message" && !specs.some((s) => s.id === "done")) buttons.push(roundButton("check-circle", async () => (await answer(item, { id: "done", label: "Done" })) && after(), "Done"));
  if (offersSnooze(item)) buttons.push(roundButton("snooze", () => snoozeSheet(item, after), "Snooze"));
  if (specs.length > 0) buttons.push(roundButton("x", async () => (await answer(item, { id: "dismiss", label: "Dismiss" })) && after(), "Dismiss"));
  const first = specs[0];
  if (first) {
    buttons.push(
      h("button", {
        class: "pill",
        style: { color, background: `${color}26` },
        onclick: async (e: Event) => {
          e.stopPropagation();
          if (specs.length > 2) return nav(`/n/${item.id}`);
          if (await answer(item, first)) after();
        },
      }, icon(actionIcon(first), 18), h("span", {}, specs.length > 2 ? `${cleanLabel(first.label)}…` : cleanLabel(first.label))),
    );
  }
  return buttons.length ? h("div", { class: "inline-actions" }, ...buttons) : null;
}

function stateLine(item: Item): string | null {
  if (item.status === "scheduled") return `Scheduled · ${when(item.deliver_at)}`;
  if (item.status === "queued") return "Queued behind an earlier question";
  if (item.status === "snoozed") return `Snoozed until ${when(item.chosen_action?.snooze_until)}`;
  if (item.status === "cancelled") return "Cancelled";
  if (item.responded && item.chosen_action) return `Answered · ${cleanLabel(item.chosen_action.label)}`;
  return null;
}

// ---------- auth screens ----------

function hero(...children: Child[]) {
  app.replaceChildren(h("div", { class: "hero" }, h("img", { src: "/icon-192.png", alt: "", class: "app-icon" }), ...children));
}

function enrollView(code: string) {
  const label = h("input", { class: "field", placeholder: "Device name", value: /iPhone/.test(navigator.userAgent) ? "iPhone" : "" });
  const go = h("button", { class: "primary big" }, icon("key", 20), h("span", {}, "Create passkey"));
  go.addEventListener("click", async () => {
    go.setAttribute("disabled", "");
    const done = await guard(async () => {
      const start = await api("/enroll/start", { body: { code } });
      const credential = await createPasskey(start.options);
      await api("/enroll/finish", { body: { challenge_id: start.challenge_id, credential, label: label.value } });
      return true;
    });
    go.removeAttribute("disabled");
    if (done) {
      session.authenticated = session.enrolled = true;
      toast("Passkey saved");
      nav("/settings", true);
    }
  });
  hero(h("h1", {}, "Welcome to roost"), h("p", {}, "Create a passkey for this device. Face ID signs you in and confirms sensitive approvals."), label, go);
}

function loginView() {
  const go = h("button", { class: "primary big" }, icon("key", 20), h("span", {}, "Sign in with passkey"));
  go.addEventListener("click", async () => {
    const ok = await guard(async () => {
      const start = await api("/login/start", { body: {} });
      const credential = await assertPasskey(start.options);
      await api("/login/finish", { body: { challenge_id: start.challenge_id, credential } });
      return true;
    });
    if (ok) {
      session.authenticated = true;
      nav("/", true);
    }
  });
  hero(
    h("h1", {}, "roost"),
    session.enrolled ? h("p", {}, "Your agents are waiting.") : h("p", {}, "No passkey yet. Run ", h("code", {}, "roost passkey enroll"), " on the server and open its link here."),
    session.enrolled ? go : null,
  );
}

// ---------- home ----------

function feedRow(n: Item, refresh: () => void) {
  const { color, name } = look(n.channel_id);
  const state = stateLine(n);
  return h(
    "li",
    { class: `feed-row${n.open ? "" : " dim"}`, onclick: () => nav(`/n/${n.id}`) },
    h("div", { class: "row-head" }, tile(n.channel_id, n.open && !n.seen_at), h("span", { class: "ch", style: { color } }, n.channel_name ?? name), h("span", { class: "time" }, when(n.created_at))),
    h("div", { class: "row-title" }, n.title),
    n.subtitle || n.body ? h("div", { class: "row-body" }, plain(n.subtitle ?? n.body)) : null,
    state ? h("div", { class: "row-state" }, state) : null,
    n.profile === "urgent" && n.open ? h("div", { class: "row-state urgent" }, "Urgent") : null,
    inlineActions(n, refresh),
  );
}

async function homeView(params: URLSearchParams) {
  const earlier = params.has("all");
  const filter = Number(params.get("c")) || null;
  const data = await guard(async () => {
    const [feed] = await Promise.all([api<{ notifications: Item[] }>(`/feed${earlier ? "?all=true" : ""}`), loadChannels()]);
    return feed;
  });
  if (!data) return;
  const items = data.notifications.filter((n) => !filter || n.channel_id === filter);
  const open = items.filter((n) => n.open);
  const rest = items.filter((n) => !n.open).slice(0, 40);
  const waiting = data.notifications.filter((n) => n.open).length;
  const withItems = [...new Set(data.notifications.map((n) => n.channel_id))];
  const qs = (c: number | null, all = earlier) => {
    const p: string[] = [];
    if (c) p.push(`c=${c}`);
    if (all) p.push("all");
    return p.length ? `/?${p.join("&")}` : "/";
  };
  const chip = (label: string, c: number | null) =>
    h("button", { class: `chip${filter === c ? " active" : ""}`, onclick: () => nav(qs(c), true) }, c ? icon(look(c).glyph, 16) : null, h("span", {}, label));
  const refresh = () => homeView(new URLSearchParams(location.search));

  largePage(
    {
      title: "Home",
      subtitle: waiting ? `${waiting} waiting for you` : "Nothing waiting for you",
      tab: "home",
      right: h("button", { class: `round-text${earlier ? " active" : ""}`, onclick: () => nav(qs(filter, !earlier), true) }, earlier ? "Open only" : "Earlier"),
    },
    withItems.length > 1 ? h("div", { class: "chips" }, chip("All", null), ...withItems.map((c) => chip(look(c).name, c))) : null,
    open.length ? h("h3", { class: "section" }, "Waiting for you") : null,
    open.length ? h("ul", { class: "feed" }, ...open.map((n) => feedRow(n, refresh))) : h("div", { class: "empty" }, icon("check-circle", 44), h("p", {}, "You're all caught up.")),
    earlier && rest.length ? h("h3", { class: "section" }, "Earlier") : null,
    earlier && rest.length ? h("ul", { class: "feed" }, ...rest.map((n) => feedRow(n, refresh))) : null,
  );
  live = { refresh, channel: null };
  reportViewing(filter ?? "home");
}

// ---------- item ----------

async function itemView(id: number) {
  const [item] = (await guard(() => Promise.all([api<Item>(`/notifications/${id}`), loadChannels()]))) ?? [];
  if (!item) return;
  if (!item.seen_at) api(`/notifications/${id}/seen`, { method: "POST" }).catch(() => {});
  const { color, name } = look(item.channel_id);
  const closed = isClosed(item);
  const specs = primarySpecs(item);
  const reload = () => itemView(id);
  const btn = (spec: ActionSpec, cls: string) =>
    h("button", {
      class: `${cls}${isDestructive(spec) && !cls.includes("primary") ? " destructive" : ""}`,
      onclick: async () => (await answer(item, spec)) && reload(),
    }, icon(actionIcon(spec), 20), h("span", {}, cleanLabel(spec.label)));

  const status = closed
    ? null
    : item.status === "scheduled"
      ? `scheduled for ${when(item.deliver_at)}`
      : item.status === "queued"
        ? "queued behind an earlier question"
      : item.status === "snoozed"
        ? `snoozed until ${when(item.chosen_action?.snooze_until)}`
        : specs.length
          ? "waiting for your reply"
          : null;
  const starts = item.event_at && !Number.isNaN(Date.parse(item.event_at)) ? `starts ${when(item.event_at)} ${hm(new Date(item.event_at))}` : null;
  const meta = [when(item.created_at), item.profile[0]!.toUpperCase() + item.profile.slice(1), starts, status].filter(Boolean).join(" · ");

  // The consequential action leads; a destructive one never takes the primary slot.
  const negative = (s: ActionSpec) => isDestructive(s) || NEGATIVE.test(s.id);
  const ordered = [...specs.filter((s) => !negative(s)), ...specs.filter(negative)];
  const [first, ...others] = ordered;
  const grid: Node[] = others.map((s) => btn(s, "secondary"));
  if (offersSnooze(item)) grid.push(h("button", { class: "secondary", onclick: () => snoozeSheet(item, reload) }, icon("snooze", 20), h("span", {}, "Snooze")));
  const showDone = !specs.some((s) => s.id === "done" || s.biometric);
  const actions = closed
    ? null
    : h(
        "div",
        { class: "actions" },
        first ? btn(first, "primary big") : null,
        grid.length ? h("div", { class: `grid${grid.length === 1 ? " one" : ""}` }, ...grid) : null,
        showDone
          ? h("button", { class: first ? "secondary wide" : "primary big", onclick: async () => (await answer(item, { id: "done", label: "Done" })) && reload() }, icon("check-circle", 20), h("span", {}, "Done"))
          : null,
      );

  const answered =
    item.responded && item.chosen_action && item.chosen_action.kind !== "snoozed"
      ? h(
          "div",
          { class: "answer-card" },
          icon("check-circle", 24),
          h("div", {}, h("div", { class: "a-label" }, cleanLabel(item.chosen_action.label)), item.chosen_action.text ? h("div", { class: "a-text" }, `“${item.chosen_action.text}”`) : null, h("div", { class: "a-time" }, `answered ${when(item.chosen_action.at)}`)),
        )
      : null;

  const share =
    "share" in navigator ? roundButton("share", () => navigator.share({ title: item.title, text: plain(item.body ?? item.subtitle) }).catch(() => {}), "Share") : undefined;

  detailPage(
    {
      title: item.channel_name ?? name,
      backTo: "/",
      right: share,
      footer: closed
        ? undefined
        : composer(`reply-${id}`, "Reply…", async (text) => {
            const ok = await answer(item, { id: "reply", label: "Reply" }, { text });
            if (ok) reload();
            return ok;
          }),
    },
    h(
      "div",
      { class: "item-channel" },
      tile(item.channel_id),
      h("span", { class: "ch", style: { color } }, item.channel_name ?? name),
      h("button", { class: "chat-link", onclick: () => nav(`/channels/${item.channel_id}`) }, icon("chat", 16), h("span", {}, "chat"), icon("chevron", 14)),
    ),
    h("h1", { class: "item-title" }, item.title),
    h("div", { class: "item-meta" }, meta),
    h("hr"),
    item.subtitle ? h("p", { class: "item-subtitle" }, item.subtitle) : null,
    item.body_markdown || item.body ? h("div", { class: "md", html: markdown(item.body_markdown ?? item.body ?? "") }) : null,
    item.url ? h("a", { class: "link-chip", href: item.url, target: "_blank", rel: "noopener" }, icon("link", 16), h("span", {}, item.url.replace(/^https?:\/\//, ""))) : null,
    item.copy ? h("button", { class: "link-chip", onclick: () => navigator.clipboard.writeText(item.copy!).then(() => toast("Copied")) }, icon("copy", 16), h("span", {}, item.copy)) : null,
    item.escalation && !closed && item.escalation.state !== "defused" ? h("p", { class: "hint" }, `Escalation ${item.escalation.state}${item.escalation.fires_at ? ` · ${when(item.escalation.fires_at)}` : ""}`) : null,
    item.status === "cancelled" ? h("p", { class: "hint" }, "The agent cancelled this.") : null,
    answered,
    actions,
  );
  live = { refresh: reload, channel: item.channel_id };
  reportViewing(item.channel_id);
}

// ---------- channels ----------

function claimBlock(code: string) {
  const cmd = `roost setup --claim ${code} --url ${location.origin}`;
  return h(
    "div",
    { class: "claim" },
    h("p", { class: "hint" }, "Run this where the agent lives. It works once, for 15 minutes."),
    h("pre", {}, cmd),
    h("button", { class: "primary big", onclick: () => navigator.clipboard.writeText(cmd).then(() => toast("Copied")) }, icon("copy", 20), h("span", {}, "Copy command")),
  );
}

function newChannelSheet() {
  const name = h("input", { class: "field", placeholder: "Name, e.g. Laptop agent" });
  const body = h("div", { class: "sheet-body" });
  const create = h("button", { class: "primary big" }, icon("plus", 20), h("span", {}, "Create channel"));
  create.addEventListener("click", async () => {
    if (!name.value.trim()) return;
    const res = await guard(() => api("/channels", { body: { name: name.value.trim() } }));
    if (res) {
      body.replaceChildren(claimBlock(res.claim_code));
      if (location.pathname === "/channels") channelsView();
    }
  });
  body.append(name, create);
  sheet("New channel", body);
  name.focus();
}

async function channelsView() {
  const [list, feed] = (await guard(() => Promise.all([loadChannels(), api<{ notifications: Item[] }>("/feed")]))) ?? [];
  if (!list || !feed) return;
  const openCount = new Map<number, number>();
  for (const n of feed.notifications) openCount.set(n.channel_id, (openCount.get(n.channel_id) ?? 0) + 1);
  const search = h("input", { class: "search", placeholder: "Search channels", type: "search" });
  const ul = h("ul", { class: "channel-list" });
  const draw = () => {
    const q = search.value.trim().toLowerCase();
    ul.replaceChildren(
      ...list
        .filter((c) => !q || c.name.toLowerCase().includes(q))
        .sort((a, b) => (b.last?.created_at ?? "").localeCompare(a.last?.created_at ?? ""))
        .map((c) => {
          const count = openCount.get(c.id) ?? 0;
          return h(
            "li",
            { class: "channel-row", onclick: () => nav(`/channels/${c.id}`) },
            avatar(c),
            h(
              "div",
              { class: "cr-main" },
              h("div", { class: "cr-name" }, c.name),
              h("div", { class: `cr-last${typingNow(c) ? " live" : ""}` }, typingNow(c) ? "typing…" : c.last ? `“${c.last.title}”` : c.claim?.claimed_by_label ? `claimed by ${c.claim.claimed_by_label}` : "not claimed yet"),
            ),
            h("div", { class: "cr-side" }, h("span", { class: `time${count ? " fresh" : ""}` }, when(c.last?.created_at)), count ? h("span", { class: "count" }, String(count)) : null),
          );
        }),
    );
  };
  search.addEventListener("input", draw);
  draw();
  largePage(
    { title: "Channels", tab: "channels", right: roundButton("plus", newChannelSheet, "New channel", "accent") },
    h("label", { class: "search-wrap" }, icon("search", 20), search),
    list.length ? ul : h("div", { class: "empty" }, icon("channels", 44), h("p", {}, "No channels yet. Tap + to create one.")),
  );
  live = { refresh: channelsView, channel: null };
}

// ---------- conversation ----------

type Msg = { id: number; kind: string; body: string; created_at: string; delivered_at: string | null; processed_at: string | null; acked_by_label: string | null; handler_summary: string | null };
type ThreadItem = Item & { sent_by_label?: string | null };

let typingTimer: number | undefined;
const typingNow = (c: Channel) => !!c.typing_until && Date.parse(c.typing_until) > Date.now();

const URGENCY = [
  { id: "normal", label: "Quiet", hint: "Every send is an ordinary notification: nothing stays on screen or re-alerts." },
  { id: "persistent", label: "Alert", hint: "Urgent sends stay on screen until you handle them, but never re-alert." },
  { id: "alarm", label: "Re-alert", hint: "Urgent sends alert again if you leave them unanswered." },
];
const REALERT_MINUTES = [1, 2, 5, 10, 15, 30, 60];

/// Name, look, urgency ceiling and re-alert delay for one channel.
function channelSettings(ch: Channel, close: () => void) {
  const cur = look(ch.id);
  let color = cur.color;
  let glyph = cur.glyph;
  let urgency = ch.max_urgency ?? "alarm";
  const name = h("input", { class: "field", value: ch.name, placeholder: "Name" });
  const swatches = h("div", { class: "swatches" });
  const glyphs = h("div", { class: "swatches" });
  const drawLook = () => {
    swatches.replaceChildren(
      ...PALETTE.map((c) => h("button", { class: `swatch${c === color ? " active" : ""}`, style: { background: c }, "aria-label": c, onclick: () => ((color = c), drawLook()) })),
    );
    glyphs.replaceChildren(
      ...CHANNEL_GLYPHS.map((g) =>
        h("button", { class: `glyph${g === glyph ? " active" : ""}`, style: g === glyph ? { color, background: `${color}22` } : {}, "aria-label": g, onclick: () => ((glyph = g), drawLook()) }, icon(g, 22)),
      ),
    );
  };
  drawLook();
  const urgencyHint = h("p", { class: "hint" });
  const urgencyChips = h("div", { class: "chips" });
  const realert = h("select", { class: "field" }, ...REALERT_MINUTES.map((m) => h("option", { value: String(m * 60) }, m === 1 ? "1 minute" : `${m} minutes`)));
  realert.value = String(ch.escalate_after_secs ?? 300);
  if (!realert.value) realert.append(h("option", { value: String(ch.escalate_after_secs), selected: true }, `${Math.round((ch.escalate_after_secs ?? 300) / 60)} minutes`));
  const realertBlock = h("div", { class: "sheet-body" }, h("div", { class: "label" }, "Re-alert after"), realert);
  const drawUrgency = () => {
    urgencyChips.replaceChildren(...URGENCY.map((u) => h("button", { class: `chip${u.id === urgency ? " active" : ""}`, onclick: () => ((urgency = u.id), drawUrgency()) }, u.label)));
    urgencyHint.textContent = URGENCY.find((u) => u.id === urgency)?.hint ?? "";
    realertBlock.hidden = urgency !== "alarm";
  };
  drawUrgency();
  const save = h("button", { class: "primary big" }, icon("check-circle", 20), h("span", {}, "Save"));
  save.addEventListener("click", async () => {
    if (!name.value.trim()) return toast("The name can't be empty", true);
    const ok = await guard(() =>
      api(`/channels/${ch.id}`, {
        method: "PATCH",
        body: { name: name.value.trim(), color, icon: glyph, max_urgency: urgency, escalate_after_secs: Number(realert.value) },
      }),
    );
    if (!ok) return;
    close();
    toast("Saved");
    liveRefresh();
  });
  return h(
    "div",
    { class: "sheet-body" },
    h("div", { class: "label" }, "Name"),
    name,
    h("div", { class: "label" }, "Colour"),
    swatches,
    h("div", { class: "label" }, "Icon"),
    glyphs,
    h("div", { class: "label" }, "Loudest alert"),
    urgencyChips,
    urgencyHint,
    realertBlock,
    save,
  );
}

async function threadView(id: number, refreshing = false) {
  const data = await guard(async () => {
    try {
      const [t] = await Promise.all([api<{ channel: Channel; notifications: ThreadItem[]; messages: Msg[] }>(`/channels/${id}/messages`), loadChannels()]);
      return t;
    } catch (e) {
      if (e instanceof HttpError && e.status === 404) return null;
      throw e;
    }
  });
  if (data === null) return nav("/channels", true);
  if (!data) return;
  const reload = () => threadView(id, true);
  const entries: { at: string; nodes: Node[] }[] = [];
  for (const n of data.notifications) {
    const footText = isClosed(n) ? "" : n.status === "scheduled" ? `scheduled · ${when(n.deliver_at)}` : n.status === "queued" ? "queued" : n.status === "snoozed" ? `snoozed until ${when(n.chosen_action?.snooze_until)}` : primarySpecs(n).length ? "waiting for you" : "";
    const card = h(
      "div",
      { class: `card${n.open && !isClosed(n) && n.profile !== "message" ? " live" : ""}`, onclick: () => nav(`/n/${n.id}`) },
      h("div", { class: "card-title" }, n.title),
      n.body || n.subtitle ? h("div", { class: "card-body" }, plain(n.subtitle ?? n.body).slice(0, 220)) : null,
      inlineActions(n, reload),
      h("div", { class: "card-foot" }, h("span", { class: "waiting" }, footText), h("span", { class: "time" }, when(n.created_at))),
      n.sent_by_label ? h("div", { class: "agent" }, h("span", { class: "dot on" }), n.sent_by_label) : null,
    );
    const nodes: Node[] = [card];
    if (n.responded && n.chosen_action && n.chosen_action.kind !== "snoozed") {
      const label = cleanLabel(n.chosen_action.label);
      nodes.push(h("div", { class: "marker" }, icon("check-circle", 18), h("span", {}, n.chosen_action.text ? `${label} · “${n.chosen_action.text}”` : label)));
    }
    entries.push({ at: n.created_at, nodes });
  }
  for (const m of data.messages.filter((m) => m.kind === "message")) {
    const ticks = m.processed_at ? h("span", { class: "ticks read" }, "✓✓") : m.delivered_at ? h("span", { class: "ticks" }, "✓✓") : h("span", { class: "ticks" }, "✓");
    const receipt = m.processed_at
      ? h("div", { class: "handled", hidden: true }, `Handled by ${m.acked_by_label ?? "the agent"} · ${when(m.processed_at)}${m.handler_summary ? ` — ${m.handler_summary}` : ""}`)
      : null;
    const bubble = h("div", { class: `bubble${receipt ? " tappable" : ""}`, onclick: () => receipt?.toggleAttribute("hidden") }, h("span", {}, m.body), ticks);
    const nodes: Node[] = receipt ? [bubble, receipt] : [bubble];
    entries.push({ at: m.created_at, nodes });
  }
  entries.sort((a, b) => a.at.localeCompare(b.at));
  const ch = data.channel;
  const typing = typingNow(ch);
  if (typing) entries.push({ at: "~", nodes: [h("div", { class: "typing", "aria-label": `${ch.name} is typing` }, h("span"), h("span"), h("span"))] });
  clearTimeout(typingTimer);
  if (typing) typingTimer = window.setTimeout(() => live?.channel === id && liveRefresh(), Date.parse(ch.typing_until!) - Date.now() + 250);
  detailPage(
    {
      title: ch.name,
      subtitle: typing ? "typing…" : ch.listening ? "listening now" : ch.last_seen_at ? `offline · seen ${when(ch.last_seen_at)}` : ch.claim?.claimed_by_label ? "offline" : "not claimed yet",
      subtitleClass: typing || ch.listening ? "live" : undefined,
      backTo: "/channels",
      right: roundButton("settings", () => {
        const body = h("div", { class: "sheet-body" });
        const mint = h("button", { class: "primary big" }, icon("key", 20), h("span", {}, "New claim code"));
        mint.addEventListener("click", async () => {
          const res = await guard(() => api(`/channels/${id}/claim`, { body: {} }));
          if (res) body.replaceChildren(claimBlock(res.code));
        });
        const remove = h("button", { class: "secondary destructive" }, icon("x", 20), h("span", {}, "Delete channel"));
        remove.addEventListener("click", async () => {
          if (!confirm(`Delete “${ch.name}”? Its notifications and messages are removed, and the agent's key stops working.`)) return;
          const ok = await guard(async () => {
            await api(`/channels/${id}`, { method: "DELETE" });
            return true;
          });
          if (!ok) return;
          close();
          toast(`Deleted ${ch.name}`);
          nav("/channels", true);
        });
        body.append(channelSettings(ch, () => close()), h("p", { class: "hint" }, ch.claim?.claimed_by_label ? `Claimed by ${ch.claim.claimed_by_label} · ${when(ch.claim.claimed_at)}` : "No agent has claimed this channel yet."), mint, remove);
        const close = sheet(ch.name, body);
      }, "Channel settings"),
      footer: composer(`thread-${id}`, `Message ${ch.name}…`, async (text) => {
        const ok = await guard(() => api(`/channels/${id}/messages`, { body: { body: text } }));
        if (ok) reload();
        return !!ok;
      }),
    },
    entries.length ? h("div", { class: "thread" }, ...entries.flatMap((e) => e.nodes)) : h("div", { class: "empty" }, icon("chat", 44), h("p", {}, "Nothing here yet.")),
  );
  if (!refreshing) requestAnimationFrame(() => window.scrollTo(0, document.body.scrollHeight));
  live = { refresh: reload, channel: id, stickToBottom: true };
  reportViewing(id);
}

// ---------- settings ----------

async function currentSubscription(): Promise<PushSubscription | null> {
  if (!("serviceWorker" in navigator)) return null;
  return (await navigator.serviceWorker.ready).pushManager.getSubscription();
}

function row(iconName: string, label: Child, opts: { detail?: Child; onclick?: () => void; cls?: string } = {}) {
  return h(
    opts.onclick ? "button" : "div",
    { class: `list-row ${opts.cls ?? ""}`, onclick: opts.onclick },
    h("span", { class: "row-icon" }, icon(iconName, 20)),
    h("span", { class: "row-label" }, label),
    opts.detail ? h("span", { class: "row-detail" }, opts.detail) : null,
  );
}

async function settingsView() {
  const supported = "serviceWorker" in navigator && "PushManager" in window;
  const sub = supported ? await currentSubscription() : null;
  const enable = async () => {
    await guard(async () => {
      if ((await Notification.requestPermission()) !== "granted") throw new Error("Notification permission was denied");
      const reg = await navigator.serviceWorker.ready;
      const s =
        (await reg.pushManager.getSubscription()) ??
        (await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64urlToUint8(session.vapid_public_key) as BufferSource }));
      await api("/push/subscriptions", { body: s.toJSON() });
      toast("Notifications on");
      settingsView();
    });
  };
  const disable = async () => {
    await guard(async () => {
      const s = await currentSubscription();
      if (s) {
        await api("/push/subscriptions", { method: "DELETE", body: { endpoint: s.endpoint } });
        await s.unsubscribe();
      }
      settingsView();
    });
  };
  const test = async () => {
    const r = await guard(() => api("/push/test", { body: {} }));
    if (r) toast(`Delivered to ${r.accepted} of ${r.devices} device(s)`);
  };
  const notifications = !supported
    ? [row("bell", "Add roost to your Home Screen", { detail: "≡ → Share" })]
    : sub
      ? [row("bell", "Notifications", { detail: h("span", { class: "on" }, "On") }), row("send", "Send test push", { onclick: test }), row("x", "Turn off on this device", { onclick: disable })]
      : [row("bell", "Turn on notifications", { onclick: enable, cls: "accent" })];
  largePage(
    { title: "Settings", tab: "settings" },
    h("h3", { class: "section" }, "Notifications"),
    h("div", { class: "list-group" }, ...notifications),
    h("h3", { class: "section" }, "Account"),
    h(
      "div",
      { class: "list-group" },
      row("logout", "Sign out", {
        cls: "destructive",
        onclick: async () => {
          await guard(() => api("/logout", { body: {} }));
          session.authenticated = false;
          nav("/login", true);
        },
      }),
    ),
  );
}

// ---------- live updates ----------

let viewing: number | "home" | null = null;

/// Tells the server which channel is on screen, or Home (every channel), so it can skip pushes the owner sees arrive.
function reportViewing(channel: number | "home" | null, beacon = false) {
  if (!session?.authenticated) return;
  const changed = channel !== viewing;
  viewing = channel;
  if (!changed && channel === null && !beacon) return;
  const body = JSON.stringify(channel === "home" ? { channel_id: null, home: true } : { channel_id: channel });
  // iOS drops either one now and then while backgrounding a home-screen app, so "away" sends both.
  if (beacon && navigator.sendBeacon) navigator.sendBeacon("/api/human/viewing", new Blob([body], { type: "application/json" }));
  fetch("/api/human/viewing", { method: "POST", headers: { "Content-Type": "application/json" }, body, credentials: "same-origin", keepalive: true }).catch(() => {});
}

setInterval(() => {
  if (viewing !== null && document.visibilityState === "visible") reportViewing(viewing);
}, 5000);

let refreshDebounce: number | undefined;
let badgeDebounce: number | undefined;

/// The home-screen icon shows how many items are on Home (iOS 16.4+ for installed web apps).
function updateBadge() {
  if (!session?.authenticated || !("setAppBadge" in navigator)) return;
  clearTimeout(badgeDebounce);
  badgeDebounce = window.setTimeout(async () => {
    const r = await api<{ open: number }>("/badge").catch(() => null);
    if (!r) return;
    (r.open > 0 ? navigator.setAppBadge(r.open) : navigator.clearAppBadge()).catch(() => {});
  }, 400);
}

function connectEvents(force = false) {
  if (events && events.readyState !== EventSource.CLOSED && !force) return;
  events?.close();
  events = new EventSource("/api/human/events");
  events.addEventListener("changed", (e) => {
    const { channel_id } = JSON.parse((e as MessageEvent).data);
    updateBadge();
    if (!live || (live.channel !== null && channel_id !== null && channel_id !== live.channel)) return;
    clearTimeout(refreshDebounce);
    refreshDebounce = window.setTimeout(liveRefresh, 250);
  });
}

async function liveRefresh() {
  if (!live) return;
  if (document.querySelector(".overlay")) {
    pendingRefresh = true;
    return;
  }
  pendingRefresh = false;
  const { refresh, stickToBottom } = live;
  const y = window.scrollY;
  const atBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 80;
  await refresh();
  window.scrollTo(0, stickToBottom && atBottom ? document.body.scrollHeight : y);
}

// ---------- routing ----------

async function route() {
  live = null;
  reportViewing(null);
  pendingRefresh = false;
  document.querySelector(".overlay")?.remove();
  const path = location.pathname;
  if (path === "/enroll" && location.hash.length > 1) return enrollView(location.hash.slice(1));
  if (!session.authenticated) return loginView();
  connectEvents();
  if (path === "/login") return nav("/", true);
  window.scrollTo(0, 0);
  let m: RegExpMatchArray | null;
  if ((m = path.match(/^\/n\/(\d+)$/))) return itemView(Number(m[1]));
  if ((m = path.match(/^\/channels\/(\d+)$/))) return threadView(Number(m[1]));
  if (path === "/channels") return channelsView();
  if (path === "/settings") return settingsView();
  return homeView(new URLSearchParams(location.search));
}

/// The service worker may park a tap target slightly after the page wakes, so look a few times.
async function followPendingOpen() {
  for (const delay of [0, 250, 750, 1500, 3000]) {
    await new Promise((r) => setTimeout(r, delay));
    if (await takePendingOpen()) {
      route();
      return;
    }
  }
}

/// Follows a notification tap the service worker parked in the cache (see sw.ts).
async function takePendingOpen(): Promise<boolean> {
  if (!("caches" in window)) return false;
  const cache = await caches.open("roost-open");
  const hit = await cache.match("/__open");
  if (!hit) return false;
  await cache.delete("/__open");
  const { url, at } = await hit.json().catch(() => ({}));
  if (typeof url !== "string" || Date.now() - at > 120_000) return false;
  if (location.pathname !== url) {
    history.pushState(null, "", url);
    inAppDepth++;
  }
  return true;
}

async function boot() {
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker
      .register("/sw.js", { updateViaCache: "none" })
      .then((reg) => reg.update())
      .catch((e) => console.warn("service worker", e));
    navigator.serviceWorker.addEventListener("message", (e) => {
      if (e.data?.type === "open") followPendingOpen();
      else if (e.data?.type === "push") liveRefresh();
    });
  }
  window.addEventListener("popstate", () => {
    inAppDepth = Math.max(0, inAppDepth - 1);
    route();
  });
  const away = () => {
    const was = viewing;
    reportViewing(null, true);
    viewing = was;
  };
  window.addEventListener("pagehide", away);
  document.addEventListener("visibilitychange", async () => {
    if (document.visibilityState === "hidden") return away();
    if (!session?.authenticated) return;
    if (viewing !== null) reportViewing(viewing);
    connectEvents(true);
    updateBadge();
    liveRefresh();
    followPendingOpen();
  });
  session = await api<Session>("/session");
  route();
  updateBadge();
  followPendingOpen();
}

boot();
