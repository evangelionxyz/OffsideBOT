use std::env;

use anyhow::{Context as _, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;

#[derive(Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct SpotifyTrackResponse {
    name: String,
    artists: Vec<SpotifyArtist>,
}

#[derive(Deserialize)]
struct SpotifyArtist {
    name: String,
}

pub async fn fetch_spotify_track(
    http_client: &reqwest::Client,
    track_id: &str,
) -> Result<(String, String)> {
    let client_id = env::var("SPOTIPY_CLIENT_ID")
        .or_else(|_| env::var("SPOTIFY_CLIENT_ID"))
        .context("SPOTIPY_CLIENT_ID not set in environment")?;
    let client_secret = env::var("SPOTIPY_CLIENT_SECRET")
        .or_else(|_| env::var("SPOTIFY_CLIENT_SECRET"))
        .context("SPOTIPY_CLIENT_SECRET not set in environment")?;

    let auth = BASE64.encode(format!("{client_id}:{client_secret}"));

    let token_res: SpotifyTokenResponse = http_client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", format!("Basic {auth}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("grant_type=client_credentials")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let track_res: SpotifyTrackResponse = http_client
        .get(format!("https://api.spotify.com/v1/tracks/{track_id}"))
        .header("Authorization", format!("Bearer {}", token_res.access_token))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let artist = track_res
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();

    Ok((track_res.name, artist))
}

