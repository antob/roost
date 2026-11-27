export class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

export async function api<T = any>(path: string, init: { method?: string; body?: unknown } = {}): Promise<T> {
  const res = await fetch(`/api/human${path}`, {
    method: init.method ?? (init.body === undefined ? "GET" : "POST"),
    headers: init.body === undefined ? {} : { "Content-Type": "application/json" },
    body: init.body === undefined ? undefined : JSON.stringify(init.body),
    credentials: "same-origin",
  });
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  let data: any = undefined;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    data = undefined;
  }
  if (!res.ok) throw new HttpError(res.status, data?.error ?? text ?? res.statusText);
  return data as T;
}

export type ActionSpec = {
  id: string;
  label: string;
  terminal?: boolean;
  custom?: boolean;
  biometric?: boolean;
  confirm?: boolean;
  style?: string;
  destructive?: boolean;
};

export type Item = {
  id: number;
  correlation_id: string;
  channel_id: number;
  channel_name: string | null;
  status: string;
  open: boolean;
  title: string;
  subtitle: string | null;
  body: string | null;
  body_markdown: string | null;
  profile: string;
  actions: string[];
  action_specs: ActionSpec[];
  url: string | null;
  copy: string | null;
  deliver_at: string | null;
  event_at: string | null;
  remind_at: string | null;
  after: string | null;
  seen_at: string | null;
  created_at: string;
  responded: boolean;
  chosen_action: { kind: string; action_id: string; label: string; text: string | null; at: string; snooze_until: string | null } | null;
  escalation: { state: string; fires_at: string | null } | null;
};
