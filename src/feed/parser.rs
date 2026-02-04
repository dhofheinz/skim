use anyhow::Result;
use feed_rs::parser;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct ParsedArticle {
    pub guid: String,
    pub title: String,
    pub url: Option<String>,
    pub published: Option<i64>,
    pub summary: Option<String>,
}

pub fn parse_feed(bytes: &[u8]) -> Result<Vec<ParsedArticle>> {
    let feed = parser::parse(bytes)?;

    let articles: Vec<ParsedArticle> = feed
        .entries
        .into_iter()
        .map(|entry| {
            let url = entry.links.first().map(|l| l.href.clone());
            let published = entry.published.or(entry.updated).map(|dt| dt.timestamp());
            let summary = entry
                .summary
                .map(|s| s.content)
                .or_else(|| entry.content.and_then(|c| c.body));
            let title = entry
                .title
                .map(|t| t.content)
                .unwrap_or_else(|| "Untitled".to_string());

            let existing_id = if entry.id.is_empty() {
                None
            } else {
                Some(entry.id.as_str())
            };
            let guid = generate_guid(existing_id, url.as_deref(), &title, published);

            ParsedArticle {
                guid,
                title,
                url,
                published,
                summary,
            }
        })
        .collect();

    Ok(articles)
}

fn generate_guid(
    existing: Option<&str>,
    url: Option<&str>,
    title: &str,
    published: Option<i64>,
) -> String {
    if let Some(guid) = existing {
        let trimmed = guid.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let input = format!(
        "{}|{}|{}",
        url.unwrap_or(""),
        title,
        published.map(|p| p.to_string()).unwrap_or_default()
    );
    let hash = Sha256::digest(input.as_bytes());
    format!("{:x}", hash)
}
