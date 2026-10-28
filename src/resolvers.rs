use std::env;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use regex::Regex;
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

#[derive(Deserialize)]
struct SpotifyOEmbedResponse {
    title: Option<String>,
}

#[derive(Deserialize)]
struct ITunesLookupResponse {
    results: Vec<ITunesTrack>,
}

#[derive(Deserialize)]
struct ITunesTrack {
    #[serde(rename = "trackName")]
    track_name: Option<String>,
    #[serde(rename = "artistName")]
    artist_name: Option<String>,
}

#[derive(Deserialize)]
struct DeezerTrackResponse {
    title: String,
    artist: Option<DeezerArtist>,
}

#[derive(Deserialize)]
struct DeezerArtist {
    name: String,
}

pub fn decode_html_entities(s: &str) -> String {
    let named = s
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&ndash;", "–")
        .replace("&mdash;", "—")
        .replace("&lsquo;", "‘")
        .replace("&rsquo;", "’")
        .replace("&ldquo;", "“")
        .replace("&rdquo;", "”");

    // Decode numeric entities: &#x27; (hex) or &#39; (decimal)
    let Ok(re) = Regex::new(r"&#(?:x([0-9a-fA-F]+)|([0-9]+));") else {
        return named;
    };

    re.replace_all(&named, |caps: &regex::Captures| {
        if let Some(hex_match) = caps.get(1) {
            if let Ok(code) = u32::from_str_radix(hex_match.as_str(), 16) {
                if let Some(c) = char::from_u32(code) {
                    return c.to_string();
                }
            }
        } else if let Some(dec_match) = caps.get(2) {
            if let Ok(code) = dec_match.as_str().parse::<u32>() {
                if let Some(c) = char::from_u32(code) {
                    return c.to_string();
                }
            }
        }
        caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
    })
    .to_string()
}

/// Resolves a Spotify track ID using the official Spotify Web API if client credentials are set.
async fn fetch_spotify_track_api(
    http_client: &reqwest::Client,
    track_id: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<(String, String)> {
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

    Ok((decode_html_entities(&track_res.name), decode_html_entities(&artist)))
}

/// Free, zero-API-key fallback that resolves Spotify track metadata from public OpenGraph tags.
pub async fn fetch_spotify_track_public(
    http_client: &reqwest::Client,
    track_id: &str,
) -> Result<(String, String)> {
    let url = format!("https://open.spotify.com/track/{track_id}");
    let response = http_client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?
        .error_for_status()?;

    let html = response.text().await?;

    let title_regex = Regex::new(r#"<meta\s+property="og:title"\s+content="([^"]+)""#)
        .map_err(|e| anyhow!("{e}"))?;
    let title_alt_regex = Regex::new(r#"<meta\s+name="twitter:title"\s+content="([^"]+)""#)
        .map_err(|e| anyhow!("{e}"))?;

    let artist_regex = Regex::new(r#"<meta\s+name="music:musician_description"\s+content="([^"]+)""#)
        .map_err(|e| anyhow!("{e}"))?;
    let desc_artist_regex = Regex::new(r#"<meta\s+property="og:description"\s+content="([^"·]+)"#)
        .map_err(|e| anyhow!("{e}"))?;

    let title = title_regex
        .captures(&html)
        .or_else(|| title_alt_regex.captures(&html))
        .and_then(|c| c.get(1))
        .map(|m| decode_html_entities(m.as_str().trim()));

    let artist = artist_regex
        .captures(&html)
        .or_else(|| desc_artist_regex.captures(&html))
        .and_then(|c| c.get(1))
        .map(|m| decode_html_entities(m.as_str().trim()))
        .unwrap_or_default();

    if let Some(title) = title {
        if !title.is_empty() && !title.starts_with("Spotify -") && title != "Page not found" {
            return Ok((title, artist));
        }
    }

    // Secondary fallback: Spotify oEmbed endpoint
    let oembed_url = format!(
        "https://open.spotify.com/oembed?url=https://open.spotify.com/track/{track_id}"
    );
    let oembed_res: SpotifyOEmbedResponse = http_client
        .get(&oembed_url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let Some(t) = oembed_res.title {
        if !t.is_empty() {
            return Ok((decode_html_entities(&t), artist));
        }
    }

    Err(anyhow!("Could not extract Spotify track metadata"))
}

/// Resolves a Spotify track. Uses official API if credentials exist, otherwise falls back
/// seamlessly to the zero-key public OpenGraph resolver.
pub async fn fetch_spotify_track(
    http_client: &reqwest::Client,
    track_id: &str,
) -> Result<(String, String)> {
    let client_id = env::var("SPOTIPY_CLIENT_ID").or_else(|_| env::var("SPOTIFY_CLIENT_ID"));
    let client_secret =
        env::var("SPOTIPY_CLIENT_SECRET").or_else(|_| env::var("SPOTIFY_CLIENT_SECRET"));

    if let (Ok(cid), Ok(csec)) = (client_id, client_secret) {
        if let Ok(res) = fetch_spotify_track_api(http_client, track_id, &cid, &csec).await {
            return Ok(res);
        }
    }

    fetch_spotify_track_public(http_client, track_id).await
}

/// Free, zero-API-key resolver for Apple Music tracks via the official iTunes Search API.
pub async fn fetch_apple_music_track(
    http_client: &reqwest::Client,
    track_id: &str,
) -> Result<(String, String)> {
    let url = format!("https://itunes.apple.com/lookup?id={track_id}&entity=song");
    let res: ITunesLookupResponse = http_client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let track = res
        .results
        .into_iter()
        .find(|t| t.track_name.is_some())
        .ok_or_else(|| anyhow!("Track not found in Apple Music / iTunes directory"))?;

    let title = track.track_name.unwrap_or_default();
    let artist = track.artist_name.unwrap_or_default();

    Ok((title, artist))
}

/// Free, zero-API-key resolver for Deezer tracks via Deezer's public REST API.
pub async fn fetch_deezer_track(
    http_client: &reqwest::Client,
    track_id: &str,
) -> Result<(String, String)> {
    let url = format!("https://api.deezer.com/track/{track_id}");
    let res: DeezerTrackResponse = http_client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let artist = res.artist.map(|a| a.name).unwrap_or_default();
    Ok((res.title, artist))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spotify_public_resolver() {
        let client = reqwest::Client::new();
        // Rick Astley - Never Gonna Give You Up
        let (title, artist) = fetch_spotify_track_public(&client, "4cOdK2wGLETKBW3PvgPWqT")
            .await
            .unwrap();
        println!("Spotify public resolved: {title} by {artist}");
        assert_eq!(title, "Never Gonna Give You Up");
        assert_eq!(artist, "Rick Astley");
    }

    #[tokio::test]
    async fn test_spotify_peabo_bryson_html_entities() {
        let client = reqwest::Client::new();
        // Peabo Bryson - If Ever You're in My Arms Again (contains &#x27;)
        let (title, artist) = fetch_spotify_track_public(&client, "6zWSB11sqCRMTAuWYkccCt")
            .await
            .unwrap();
        println!("Spotify public resolved: {title} by {artist}");
        assert_eq!(title, "If Ever You're in My Arms Again");
        assert_eq!(artist, "Peabo Bryson");
    }

    #[tokio::test]
    async fn test_spotify_white_chorus_user_reported() {
        let client = reqwest::Client::new();
        // White Chorus - Ingatlah Aku Di Sini
        let (title, artist) = fetch_spotify_track_public(&client, "4LyUgOxEfMBEWz4jZlcr0K")
            .await
            .unwrap();
        println!("Spotify public resolved: {title} by {artist}");
        assert_eq!(title, "Ingatlah Aku Di Sini");
        assert_eq!(artist, "White Chorus");
    }

    #[tokio::test]
    async fn test_apple_music_resolver() {
        let client = reqwest::Client::new();
        // Rick Astley - Never Gonna Give You Up
        let (title, artist) = fetch_apple_music_track(&client, "1559885421")
            .await
            .unwrap();
        println!("Apple Music resolved: {title} by {artist}");
        assert_eq!(title, "Never Gonna Give You Up");
        assert_eq!(artist, "Rick Astley");
    }

    #[tokio::test]
    async fn test_deezer_resolver() {
        let client = reqwest::Client::new();
        // Rick Astley - Never Gonna Give You Up
        let (title, artist) = fetch_deezer_track(&client, "14408104")
            .await
            .unwrap();
        println!("Deezer resolved: {title} by {artist}");
        assert_eq!(title, "Never Gonna Give You Up");
        assert_eq!(artist, "Rick Astley");
    }
}
