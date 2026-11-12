mod common;

use common::demo::refuse_production;
use furball::{DEMO_URL, PRODUCTION_EXTERNAL_URL, PRODUCTION_URL};

#[test]
fn accepts_the_demo_url() {
    assert_eq!(refuse_production(DEMO_URL), Ok(DEMO_URL));
}

#[test]
fn refuses_both_production_urls() {
    assert!(refuse_production(PRODUCTION_URL).is_err());
    assert!(refuse_production(PRODUCTION_EXTERNAL_URL).is_err());
}

#[test]
fn refuses_production_with_trailing_slash_or_case_change() {
    assert!(refuse_production(&format!("{PRODUCTION_URL}/")).is_err());
    assert!(refuse_production(&PRODUCTION_URL.to_uppercase()).is_err());
}

#[test]
fn refuses_any_url_that_is_not_a_demo_host() {
    assert!(refuse_production("https://example.com/trade-api/v2").is_err());
}
