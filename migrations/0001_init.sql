CREATE TABLE channels (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    icon TEXT,
    color TEXT,
    key_hash TEXT UNIQUE,
    claim_generation INTEGER NOT NULL DEFAULT 0,
    claimed_by_label TEXT,
    claimed_by_fingerprint TEXT,
    claimed_at TEXT,
    operating_contract TEXT NOT NULL DEFAULT '{}',
    notify_contract TEXT NOT NULL DEFAULT '{}',
    default_profile TEXT NOT NULL DEFAULT 'message',
    max_urgency TEXT NOT NULL DEFAULT 'alarm',
    created_at TEXT NOT NULL
);

CREATE TABLE claim_codes (
    code TEXT PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    exchanged_at TEXT,
    exchanged_fingerprint TEXT
);

CREATE TABLE notifications (
    id INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    correlation_id TEXT NOT NULL,
    title TEXT NOT NULL,
    subtitle TEXT,
    body TEXT,
    body_markdown TEXT,
    template_kind TEXT NOT NULL,
    profile TEXT NOT NULL,
    urgency TEXT NOT NULL,
    actions TEXT NOT NULL DEFAULT '[]',
    custom_actions TEXT NOT NULL DEFAULT '[]',
    url TEXT,
    copy TEXT,
    thread_id TEXT,
    collapse_key TEXT,
    action_data TEXT,
    reply_to TEXT,
    mirror_reply INTEGER NOT NULL DEFAULT 1,
    sent_note TEXT,
    sent_by_label TEXT,
    event_at TEXT,
    deliver_at TEXT,
    status TEXT NOT NULL,
    delivered_at TEXT,
    seen_at TEXT,
    responded_at TEXT,
    chosen_action TEXT,
    reply_delivered_at TEXT,
    reply_message_id INTEGER,
    created_at TEXT NOT NULL,
    UNIQUE (channel_id, correlation_id)
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    thread_id TEXT,
    correlation_id TEXT,
    ref TEXT,
    action_id TEXT,
    text TEXT,
    created_at TEXT NOT NULL,
    delivered_at TEXT,
    delivery_expires_at TEXT,
    lease_fingerprint TEXT,
    lease_label TEXT,
    processed_at TEXT,
    acked_by_label TEXT,
    handler_summary TEXT
);

CREATE INDEX messages_queue ON messages (channel_id, processed_at, id);
