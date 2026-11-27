ALTER TABLE notifications ADD COLUMN after_cid TEXT;
ALTER TABLE notifications ADD COLUMN remind_at TEXT;

CREATE INDEX notifications_after ON notifications (channel_id, after_cid) WHERE after_cid IS NOT NULL;
