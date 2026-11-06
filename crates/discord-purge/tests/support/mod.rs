//! Shared setup for tests that run the engine.
use discord_purge_core::api::DiscordClient;
use discord_purge_core::engine::ChannelJob;
use discord_purge_core::{ConfirmedRun, PreparedRun};

/// Take jobs through the same confirm step as a real run. The mock must answer
/// `GET /users/@me`.
pub async fn confirmed(client: &DiscordClient, jobs: Vec<ChannelJob>) -> ConfirmedRun {
    PreparedRun {
        account_id: "unknown".into(),
        account_label: String::new(),
        total: jobs.iter().map(|job| job.items.len()).sum(),
        jobs,
        complete: true,
    }
    .confirm(client)
    .await
    .unwrap()
}
