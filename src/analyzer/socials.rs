use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::models::token::SocialLinks;

/// Intermediate structure for parsing the off-chain metadata JSON
/// that Metaplex tokens typically link to via the `uri` field.
#[derive(Debug, Deserialize)]
struct MetadataJson {
    #[serde(default)]
    external_url: Option<String>,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    twitter: Option<String>,
    #[serde(default)]
    telegram: Option<String>,
    /// Some tokens store links inside an `extensions` map.
    #[serde(default)]
    extensions: Option<MetadataExtensions>,
}

#[derive(Debug, Deserialize)]
struct MetadataExtensions {
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    twitter: Option<String>,
    #[serde(default)]
    telegram: Option<String>,
}

/// Fetch the metadata JSON from the token's URI and extract social links.
///
/// Returns `SocialLinks` with any website, twitter, or telegram URLs found.
/// If `metadata_uri` is `None`, returns empty links.
pub async fn check_socials(metadata_uri: Option<&str>) -> Result<SocialLinks> {
    let uri = match metadata_uri {
        Some(u) if !u.is_empty() => u,
        _ => {
            info!("No metadata URI provided, returning empty social links");
            return Ok(SocialLinks::default());
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;

    let response = client
        .get(uri)
        .send()
        .await
        .context("Failed to fetch metadata JSON")?;

    if !response.status().is_success() {
        warn!(uri = %uri, status = %response.status(), "Metadata URI returned non-success");
        return Ok(SocialLinks::default());
    }

    let metadata: MetadataJson = response
        .json()
        .await
        .context("Failed to parse metadata JSON")?;

    // Merge top-level and extension fields, preferring top-level.
    let website = metadata
        .website
        .or(metadata.external_url)
        .or_else(|| metadata.extensions.as_ref().and_then(|e| e.website.clone()))
        .filter(|s| !s.is_empty());

    let twitter = metadata
        .twitter
        .or_else(|| metadata.extensions.as_ref().and_then(|e| e.twitter.clone()))
        .filter(|s| !s.is_empty());

    let telegram = metadata
        .telegram
        .or_else(|| metadata.extensions.as_ref().and_then(|e| e.telegram.clone()))
        .filter(|s| !s.is_empty());

    let links = SocialLinks {
        website,
        twitter,
        telegram,
    };

    info!(
        count = links.count(),
        "Social links extracted from metadata"
    );

    Ok(links)
}
