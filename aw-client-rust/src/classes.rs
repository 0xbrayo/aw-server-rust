//! Default classes
//!
//! Taken from default classes in aw-webui

use log::warn;
use rand::Rng;
use serde::{Deserialize, Serialize};

use super::blocking::AwClient as ActivityWatchClient;

pub type CategoryId = Vec<String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategorySpec {
    #[serde(rename = "type")]
    pub spec_type: String,
    pub regex: String,
    #[serde(default)]
    pub ignore_case: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassSetting {
    pub name: Vec<String>,
    pub rule: CategorySpec,
}

/// Returns the default categorization classes
pub fn default_classes() -> Vec<(CategoryId, CategorySpec)> {
    vec![
        (
            vec!["Work".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Google Docs|libreoffice|ReText".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Work".to_string(), "Programming".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "GitHub|Stack Overflow|BitBucket|Gitlab|vim|Spyder|kate|Ghidra|Scite"
                    .to_string(),
                ignore_case: false,
            },
        ),
        (
            vec![
                "Work".to_string(),
                "Programming".to_string(),
                "ActivityWatch".to_string(),
            ],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "ActivityWatch|aw-".to_string(),
                ignore_case: true,
            },
        ),
        (
            vec!["Work".to_string(), "Image".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Gimp|Inkscape".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Work".to_string(), "Video".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Kdenlive".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Work".to_string(), "Audio".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Audacity".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Work".to_string(), "3D".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Blender".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Media".to_string(), "Games".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Minecraft|RimWorld".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Media".to_string(), "Video".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "YouTube|Plex|VLC".to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Media".to_string(), "Social Media".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "reddit|Facebook|Twitter|Instagram|devRant".to_string(),
                ignore_case: true,
            },
        ),
        (
            vec!["Media".to_string(), "Music".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Spotify|Deezer".to_string(),
                ignore_case: true,
            },
        ),
        (
            vec!["Comms".to_string(), "IM".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Messenger|Telegram|Signal|WhatsApp|Rambox|Slack|Riot|Discord|Nheko"
                    .to_string(),
                ignore_case: false,
            },
        ),
        (
            vec!["Comms".to_string(), "Email".to_string()],
            CategorySpec {
                spec_type: "regex".to_string(),
                regex: "Gmail|Thunderbird|mutt|alpine".to_string(),
                ignore_case: false,
            },
        ),
    ]
}

/// Get classes from server-side settings.
/// Might throw an error if not set yet, in which case we use the default classes as a fallback.
pub fn get_classes() -> Vec<(CategoryId, CategorySpec)> {
    let mut rng = rand::rng();
    let random_int = rng.random_range(0..10001);
    let client_id = format!("get-setting-{}", random_int);

    // Create a client with a random ID, similar to the Python implementation
    let awc = ActivityWatchClient::new("localhost", 5600, &client_id)
        .expect("Failed to create ActivityWatch client");

    awc.get_setting("classes")
        .map(|setting| {
            // Deserialize the setting into a Vec<(CategoryId, CategorySpec)>
            serde_json::from_value(
                serde_json::to_value(setting).expect("Failed to convert Settings to Value"),
            )
            .unwrap_or_else(|_| default_classes())
        })
        .unwrap_or_else(|_| {
            warn!("Failed to get classes from server, using default classes");
            default_classes()
        })
}
