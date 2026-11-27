use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::util::{new_claim_code, now, now_plus};

pub const CLAIM_TTL_SECS: i64 = 15 * 60;

pub struct ClaimCode {
    pub code: String,
    pub expires_at: String,
}

pub async fn create(db: &SqlitePool, name: &str) -> anyhow::Result<(i64, ClaimCode)> {
    let created = now();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO channels (name, created_at) VALUES (?, ?) RETURNING id as "id!""#,
        name,
        created
    )
    .fetch_one(db)
    .await?;
    let code = mint_claim_code(db, id).await?;
    Ok((id, code))
}

pub async fn mint_claim_code(db: &SqlitePool, channel_id: i64) -> anyhow::Result<ClaimCode> {
    let code = new_claim_code();
    let expires_at = now_plus(CLAIM_TTL_SECS);
    sqlx::query!(
        "INSERT INTO claim_codes (code, channel_id, expires_at) VALUES (?, ?, ?)",
        code,
        channel_id,
        expires_at
    )
    .execute(db)
    .await?;
    Ok(ClaimCode { code, expires_at })
}

pub struct Channel {
    pub id: i64,
    pub name: String,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub claim_generation: i64,
    pub claimed_by_label: Option<String>,
    pub claimed_by_fingerprint: Option<String>,
    pub claimed_at: Option<String>,
    pub operating_contract: String,
    pub notify_contract: String,
    pub default_profile: String,
    pub max_urgency: String,
    pub escalate_after_secs: i64,
}

/// Deletes a channel with its notifications, messages and claim codes; its key stops working at once.
pub async fn delete(db: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    Ok(sqlx::query!("DELETE FROM channels WHERE id = ?", id)
        .execute(db)
        .await?
        .rows_affected()
        == 1)
}

pub async fn load(db: &SqlitePool, id: i64) -> sqlx::Result<Option<Channel>> {
    sqlx::query_as!(
        Channel,
        r#"SELECT id as "id!", name, icon, color, claim_generation, claimed_by_label, claimed_by_fingerprint,
                  claimed_at, operating_contract, notify_contract, default_profile, max_urgency,
                  escalate_after_secs
           FROM channels WHERE id = ?"#,
        id
    )
    .fetch_optional(db)
    .await
}

impl Channel {
    pub fn brief(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "icon": self.icon, "color": self.color })
    }

    pub fn claim(&self) -> Value {
        json!({
            "claimed_by_label": self.claimed_by_label,
            "claimed_by_fingerprint": self.claimed_by_fingerprint,
            "claimed_at": self.claimed_at,
            "claim_generation": self.claim_generation,
        })
    }

    pub fn operating_contract(&self) -> Value {
        serde_json::from_str(&self.operating_contract).unwrap_or_else(|_| json!({}))
    }

    pub fn notify_contract(&self) -> Value {
        serde_json::from_str(&self.notify_contract).unwrap_or_else(|_| json!({}))
    }

    pub fn full(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "icon": self.icon,
            "color": self.color,
            "default_profile": self.default_profile,
            "max_urgency": self.max_urgency,
            "escalate_after_secs": self.escalate_after_secs,
            "operating_contract": self.operating_contract(),
            "notify_contract": self.notify_contract(),
            "claimed_by_label": self.claimed_by_label,
            "claimed_at": self.claimed_at,
            "claim_generation": self.claim_generation,
            "e2e_enabled": false,
        })
    }
}
