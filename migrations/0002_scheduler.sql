ALTER TABLE notifications ADD COLUMN next_fire_at TEXT;
ALTER TABLE notifications ADD COLUMN fired_at TEXT;
ALTER TABLE notifications ADD COLUMN escalation_state TEXT;
ALTER TABLE notifications ADD COLUMN escalation_fires_at TEXT;

CREATE INDEX notifications_next_fire ON notifications (next_fire_at) WHERE next_fire_at IS NOT NULL;

ALTER TABLE channels ADD COLUMN escalate_after_secs INTEGER NOT NULL DEFAULT 300;
ALTER TABLE notifications ADD COLUMN escalate_after_secs INTEGER;
