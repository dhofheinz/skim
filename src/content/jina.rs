use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContentError {
    #[error("Request timed out after 10s")]
    Timeout,
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("HTTP error: status {0}")]
    HttpStatus(u16),
}

pub async fn fetch_content(client: &reqwest::Client, url: &str) -> Result<String, ContentError> {
    let jina_url = format!("https://r.jina.ai/{}", url);

    let mut request = client.get(&jina_url);
    if let Ok(key) = std::env::var("JINA_API_KEY") {
        request = request.header("Authorization", format!("Bearer {}", key));
    }

    let response = tokio::time::timeout(Duration::from_secs(10), request.send())
        .await
        .map_err(|_| ContentError::Timeout)?
        .map_err(ContentError::Network)?;

    if !response.status().is_success() {
        return Err(ContentError::HttpStatus(response.status().as_u16()));
    }

    response.text().await.map_err(ContentError::Network)
}
