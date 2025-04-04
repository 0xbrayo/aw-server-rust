use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Browser application names mapped by browser type
pub static BROWSER_APPNAMES: phf::Map<&'static str, &'static [&'static str]> = phf::phf_map! {
    "chrome" => &[
        // Chrome
        "Google Chrome",
        "Google-chrome",
        "chrome.exe",
        "google-chrome-stable",
        // Chromium
        "Chromium",
        "Chromium-browser",
        "Chromium-browser-chromium",
        "chromium.exe",
        // Pre-releases
        "Google-chrome-beta",
        "Google-chrome-unstable",
        // Brave (should this be merged with the brave entry?)
        "Brave-browser",
    ],
    "firefox" => &[
        "Firefox",
        "Firefox.exe",
        "firefox",
        "firefox.exe",
        "Firefox Developer Edition",
        "firefoxdeveloperedition",
        "Firefox-esr",
        "Firefox Beta",
        "Nightly",
        "org.mozilla.firefox",
    ],
    "opera" => &["opera.exe", "Opera"],
    "brave" => &["brave.exe"],
    "edge" => &[
        "msedge.exe",  // Windows
        "Microsoft Edge",  // macOS
    ],
    "vivaldi" => &["Vivaldi-stable", "Vivaldi-snapshot", "vivaldi.exe"],
};

pub const DEFAULT_LIMIT: u32 = 100;

/// Represents a class rule for categorization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassRule {
    pub patterns: Vec<String>,
    pub properties: HashMap<String, String>,
}

/// Base query parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryParamsBase {
    #[serde(default)]
    pub bid_browsers: Vec<String>,
    #[serde(default)]
    pub classes: Vec<ClassRule>,
    #[serde(default)]
    pub filter_classes: Vec<Vec<String>>,
    #[serde(default = "default_true")]
    pub filter_afk: bool,
    #[serde(default = "default_true")]
    pub include_audible: bool,
}

fn default_true() -> bool {
    true
}

/// Query parameters specific to desktop
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopQueryParams {
    #[serde(flatten)]
    pub base: QueryParamsBase,
    pub bid_window: String,
    pub bid_afk: String,
}

/// Query parameters specific to Android
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AndroidQueryParams {
    #[serde(flatten)]
    pub base: QueryParamsBase,
    pub bid_android: String,
}

/// Enum to represent different types of query parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QueryParams {
    Desktop(DesktopQueryParams),
    Android(AndroidQueryParams),
}

impl QueryParams {
    /// Build canonical events query string
    pub fn canonical_events(&self) -> String {
        match self {
            QueryParams::Desktop(params) => build_desktop_canonical_events(params),
            QueryParams::Android(params) => build_android_canonical_events(params),
        }
    }
}

fn build_desktop_canonical_events(params: &DesktopQueryParams) -> String {
    let mut query = Vec::new();

    // Fetch window events
    query.push(format!(
        "events = flood(query_bucket(find_bucket(\"{}\")))",
        escape_doublequote(&params.bid_window)
    ));

    // Fetch not-afk events
    if params.base.filter_afk {
        query.push(format!(
            "not_afk = flood(query_bucket(find_bucket(\"{}\")));
             not_afk = filter_keyvals(not_afk, \"status\", [\"not-afk\"])",
            escape_doublequote(&params.bid_afk)
        ));
    }

    // Add browser events if any browser buckets specified
    if !params.base.bid_browsers.is_empty() {
        query.push(build_browser_events(params));

        if params.base.include_audible {
            query.push(
                "audible_events = filter_keyvals(browser_events, \"audible\", [true]);
                 not_afk = period_union(not_afk, audible_events)".to_string(),
            );
        }
    }

    // Filter out window events when user was AFK
    if params.base.filter_afk {
        query.push("events = filter_period_intersect(events, not_afk)".to_string());
    }

    // Add categorization if classes specified
    if !params.base.classes.is_empty() {
        query.push(format!(
            "events = categorize(events, {})",
            serde_json::to_string(&params.base.classes).unwrap()
        ));
    }

    // Filter categories if specified
    if !params.base.filter_classes.is_empty() {
        query.push(format!(
            "events = filter_keyvals(events, \"$category\", {})",
            serde_json::to_string(&params.base.filter_classes).unwrap()
        ));
    }

    query.join(";\n")
}

fn build_android_canonical_events(params: &AndroidQueryParams) -> String {
    let mut query = Vec::new();

    // Fetch app events
    query.push(format!(
        "events = flood(query_bucket(find_bucket(\"{}\")))",
        escape_doublequote(&params.bid_android)
    ));

    // Merge events by app
    query.push("events = merge_events_by_keys(events, [\"app\"])".to_string());

    // Add categorization if classes specified
    if !params.base.classes.is_empty() {
        query.push(format!(
            "events = categorize(events, {})",
            serde_json::to_string(&params.base.classes).unwrap()
        ));
    }

    // Filter categories if specified
    if !params.base.filter_classes.is_empty() {
        query.push(format!(
            "events = filter_keyvals(events, \"$category\", {})",
            serde_json::to_string(&params.base.filter_classes).unwrap()
        ));
    }

    query.join(";\n")
}

fn build_browser_events(params: &DesktopQueryParams) -> String {
    let mut query = String::from("browser_events = [];\n");

    for browser_bucket in &params.base.bid_browsers {
        for (browser_name, app_names) in BROWSER_APPNAMES.entries() {
            if browser_bucket.contains(browser_name) {
                query.push_str(&format!(
                    "events_{0} = flood(query_bucket(\"{1}\"));
                     window_{0} = filter_keyvals(events, \"app\", {2});
                     events_{0} = filter_period_intersect(events_{0}, window_{0});
                     events_{0} = split_url_events(events_{0});
                     browser_events = concat(browser_events, events_{0});
                     browser_events = sort_by_timestamp(browser_events);\n",
                    browser_name,
                    escape_doublequote(browser_bucket),
                    serde_json::to_string(app_names).unwrap()
                ));
            }
        }
    }

    query
}

/// Build a full desktop query
pub fn full_desktop_query(params: &DesktopQueryParams) -> String {
    let mut query = QueryParams::Desktop(params.clone()).canonical_events();

    // Add basic event aggregations
    query.push_str(&format!(
        "
        title_events = sort_by_duration(merge_events_by_keys(events, [\"app\", \"title\"]));
        app_events = sort_by_duration(merge_events_by_keys(title_events, [\"app\"]));
        cat_events = sort_by_duration(merge_events_by_keys(events, [\"$category\"]));
        app_events = limit_events(app_events, {});
        title_events = limit_events(title_events, {});
        duration = sum_durations(events);
        ",
        DEFAULT_LIMIT, DEFAULT_LIMIT
    ));

    // Add browser-specific query parts if browser buckets exist
    if !params.base.bid_browsers.is_empty() {
        query.push_str(&format!(
            "
            browser_events = split_url_events(browser_events);
            browser_urls = merge_events_by_keys(browser_events, [\"url\"]);
            browser_urls = sort_by_duration(browser_urls);
            browser_urls = limit_events(browser_urls, {});
            browser_domains = merge_events_by_keys(browser_events, [\"$domain\"]);
            browser_domains = sort_by_duration(browser_domains);
            browser_domains = limit_events(browser_domains, {});
            browser_duration = sum_durations(browser_events);
            ",
            DEFAULT_LIMIT, DEFAULT_LIMIT
        ));
    } else {
        query.push_str(
            "
            browser_events = [];
            browser_urls = [];
            browser_domains = [];
            browser_duration = 0;
            ",
        );
    }

    // Add return statement
    query.push_str(
        "
        RETURN = {
            \"events\": events,
            \"window\": {
                \"app_events\": app_events,
                \"title_events\": title_events,
                \"cat_events\": cat_events,
                \"active_events\": not_afk,
                \"duration\": duration
            },
            \"browser\": {
                \"domains\": browser_domains,
                \"urls\": browser_urls,
                \"duration\": browser_duration
            }
        };
        ",
    );

    query
}

fn escape_doublequote(s: &str) -> String {
    s.replace('\"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_query_generation() {
        let params = DesktopQueryParams {
            base: QueryParamsBase {
                bid_browsers: vec![],
                classes: vec![],
                filter_classes: vec![],
                filter_afk: true,
                include_audible: true,
            },
            bid_window: "aw-watcher-window_".to_string(),
            bid_afk: "aw-watcher-afk_".to_string(),
        };

        let query = full_desktop_query(&params);
        assert!(!query.is_empty());
        assert!(query.contains("events = flood"));
    }
}
