use furball::{Credentials, DEMO_URL, PRODUCTION_EXTERNAL_URL, PRODUCTION_URL};

pub const DEMO_URL_ENV: &str = "KALSHI_DEMO_URL";

pub fn refuse_production(url: &str) -> Result<&str, String> {
    let trimmed = url.trim_end_matches('/');
    let is_production = [PRODUCTION_URL, PRODUCTION_EXTERNAL_URL]
        .iter()
        .any(|production| trimmed.eq_ignore_ascii_case(production.trim_end_matches('/')));
    if is_production || !url.contains("demo") {
        return Err(format!(
            "refusing to run against {url}: demo tests only run against a demo URL"
        ));
    }
    Ok(url)
}

pub fn demo_url() -> String {
    let url = std::env::var(DEMO_URL_ENV).unwrap_or_else(|_| DEMO_URL.to_owned());
    refuse_production(&url).unwrap_or_else(|message| panic!("{message}"));
    url
}

pub fn demo_credentials() -> Option<Credentials> {
    match Credentials::from_env() {
        Ok(credentials) => Some(credentials),
        Err(error) => {
            eprintln!(
                "SKIPPED: demo credentials unavailable ({error}); set KALSHI_API_KEY_ID and KALSHI_PRIVATE_KEY_PATH"
            );
            None
        }
    }
}
