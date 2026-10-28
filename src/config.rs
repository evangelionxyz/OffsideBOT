use std::{
    env, fs,
    io::Cursor,
    path::Path,
};

pub fn load_env_file(path: &Path) -> Result<(), String> {
    let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut normalized = String::with_capacity(content.len());

    for line in content.lines() {
        normalized.push_str(line);
        normalized.push('\n');
    }

    dotenvy::from_read(Cursor::new(normalized)).map_err(|error| error.to_string())
}

pub fn load_discord_token() -> String {
    if let Ok(token) = env::var("DISCORD_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if let Ok(token) = fs::read_to_string("token.txt") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    panic!("Expected DISCORD_TOKEN in environment/.env or a non-empty token.txt file!");
}

pub fn detect_dev_instance() -> bool {
    if let Ok(val) = env::var("BOT_ENV") {
        return !matches!(
            val.trim().to_lowercase().as_str(),
            "prod" | "production" | "release"
        );
    }
    cfg!(debug_assertions)
}

