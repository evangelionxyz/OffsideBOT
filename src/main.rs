mod cache;
mod config;
mod handler;
mod models;
mod playback;
mod search;
mod spotify;
mod state;
mod stream;

use std::{path::Path, sync::Arc, time::Duration};

use serenity::{model::gateway::GatewayIntents, prelude::*};
use songbird::SerenityInit;

use crate::{
    config::{detect_dev_instance, load_discord_token, load_env_file},
    handler::Handler,
    state::BotState,
};

#[tokio::main]
async fn main() {
    let path = Path::new(".env");

    if path.exists() {
        if let Err(error) = load_env_file(path) {
            eprintln!(
                "Failed to load environment file {}: {:?}",
                path.display(),
                error
            );
        }
    }

    let token = load_discord_token();
    let is_dev_instance = detect_dev_instance();

    let state = Arc::new(BotState::new(is_dev_instance));
    state.cache.enforce_limits().await;

    // Periodic background task to prune expired (>7 days) or >5GB cache entries every hour
    {
        let cache = state.cache.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                cache.enforce_limits().await;
            }
        });
    }

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES;

    let handler = Handler { state };

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird()
        .await
        .expect("Error creating client");

    if let Err(why) = client.start().await {
        eprintln!("Client error: {why:?}");
    }
}
