use std::env;

#[derive(Debug, Clone)]
pub struct RdpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: String,
    pub width: u16,
    pub height: u16,
    pub swap_alt_meta: bool,
}

impl RdpConfig {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let host = env::var("GRDP_HOST")?;
        let port = env::var("GRDP_PORT")?.parse::<u16>()?;
        let username = env::var("GRDP_USER")?;
        let password = env::var("GRDP_PASSWORD")?;
        let domain = env::var("GRDP_DOMAIN").unwrap_or_default();

        let window_size = env::var("GRDP_WINDOW_SIZE").unwrap_or_else(|_| "1280x800".to_string());
        let (width, height) = parse_window_size(&window_size)?;

        let swap_alt_meta = env::var("GRDP_SWAP_ALT_META")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);

        Ok(RdpConfig {
            host,
            port,
            username,
            password,
            domain,
            width,
            height,
            swap_alt_meta,
        })
    }
}

fn parse_window_size(size: &str) -> Result<(u16, u16), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = size.split('x').collect();
    if parts.len() != 2 {
        return Err("Window size must be in WxH format (e.g., 1280x800)".into());
    }
    let width = parts[0].parse::<u16>()?;
    let height = parts[1].parse::<u16>()?;
    Ok((width, height))
}
