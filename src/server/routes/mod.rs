pub mod app;
mod channel;
mod claim;
pub mod human;
mod messages;
mod notifications;
mod notify;
mod selftest;

pub use app::open_count;
pub use notifications::Notification;

use axum::Router;
use axum::routing::{get, patch, post};

use crate::server::AppState;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/claim", post(claim::claim))
        .route("/claim/ownership", post(claim::ownership))
        .route("/whoami", get(channel::whoami))
        .route("/channels", get(channel::list))
        .route("/channels/{id}", patch(channel::update).get(channel::show))
        .route("/notify", post(notify::notify))
        .route("/notifications", get(notifications::index))
        .route(
            "/notifications/{cid}",
            get(notifications::show).delete(notifications::cancel),
        )
        .route("/inbox/summary", get(notifications::summary))
        .route("/messages", get(messages::consume))
        .route("/messages/ack", post(messages::ack))
        .route("/typing", post(channel::typing))
        .route("/selftest", post(selftest::create))
        .route("/selftest/{id}", get(selftest::show));

    let human = Router::new()
        .route("/channels", post(human::create_channel))
        .route(
            "/channels/{id}",
            axum::routing::delete(human::delete_channel).patch(human::update_channel),
        )
        .route("/channels/{id}/claim", post(human::claim_code))
        .route("/channels/{id}/messages", post(human::post_message))
        .route(
            "/channels/{id}/notifications/{cid}/respond",
            post(human::respond),
        )
        .route("/channels", get(app::channels_list))
        .route("/channels/{id}/messages", get(app::channel_thread))
        .route("/session", get(app::session))
        .route("/enroll/start", post(app::enroll_start))
        .route("/enroll/finish", post(app::enroll_finish))
        .route("/login/start", post(app::login_start))
        .route("/login/finish", post(app::login_finish))
        .route("/logout", post(app::logout))
        .route(
            "/push/subscriptions",
            post(app::subscribe).delete(app::unsubscribe),
        )
        .route("/push/test", post(app::push_test))
        .route("/feed", get(app::feed))
        .route("/badge", get(app::badge))
        .route("/events", get(app::events))
        .route("/viewing", post(app::viewing))
        .route("/notifications/{id}", get(app::notification))
        .route("/notifications/{id}/seen", post(app::seen))
        .route("/notifications/{id}/delivered", post(app::delivered))
        .route("/notifications/{id}/gate", post(app::gate))
        .route("/notifications/{id}/respond", post(human::respond_by_id));

    Router::new()
        .route("/up", get(|| async { "ok" }))
        .nest("/api/v1", api)
        .nest("/api/human", human)
        .fallback(crate::server::web::serve)
        .with_state(state)
}
