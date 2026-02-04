use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::fs;

#[derive(Debug, Clone)]
pub struct OpmlFeed {
    pub title: String,
    pub xml_url: String,
    pub html_url: Option<String>,
}

pub fn parse(path: &str) -> Result<Vec<OpmlFeed>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read OPML file: {}", path))?;
    parse_opml_content(&content)
}

fn parse_opml_content(content: &str) -> Result<Vec<OpmlFeed>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut feeds = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"outline" => {
                let mut xml_url = None;
                let mut html_url = None;
                let mut title = None;

                for attr in e.attributes().flatten() {
                    let decoder = reader.decoder();
                    match attr.key.as_ref() {
                        b"xmlUrl" => {
                            xml_url = Some(attr.decode_and_unescape_value(decoder)?.to_string())
                        }
                        b"htmlUrl" => {
                            html_url = Some(attr.decode_and_unescape_value(decoder)?.to_string())
                        }
                        b"title" => {
                            title = Some(attr.decode_and_unescape_value(decoder)?.to_string())
                        }
                        b"text" => {
                            if title.is_none() {
                                title = Some(attr.decode_and_unescape_value(decoder)?.to_string())
                            }
                        }
                        _ => {}
                    }
                }

                if let Some(url) = xml_url {
                    feeds.push(OpmlFeed {
                        title: title.unwrap_or_else(|| url.clone()),
                        xml_url: url,
                        html_url,
                    });
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(feeds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_opml_content() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Test Feeds</title></head>
  <body>
    <outline text="Blogs" title="Blogs">
      <outline type="rss" text="Example Blog" title="Example Blog" xmlUrl="https://example.com/feed.xml" htmlUrl="https://example.com"/>
      <outline type="rss" text="No HTML" title="No HTML" xmlUrl="https://nohtml.com/rss"/>
    </outline>
  </body>
</opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert_eq!(feeds.len(), 2);

        assert_eq!(feeds[0].title, "Example Blog");
        assert_eq!(feeds[0].xml_url, "https://example.com/feed.xml");
        assert_eq!(feeds[0].html_url, Some("https://example.com".to_string()));

        assert_eq!(feeds[1].title, "No HTML");
        assert_eq!(feeds[1].xml_url, "https://nohtml.com/rss");
        assert_eq!(feeds[1].html_url, None);
    }

    #[test]
    fn test_fallback_to_text() {
        let content = r#"<?xml version="1.0"?>
<opml version="2.0">
  <body>
    <outline type="rss" text="Text Only" xmlUrl="https://textonly.com/feed"/>
  </body>
</opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].title, "Text Only");
    }

    #[test]
    fn test_fallback_to_url() {
        let content = r#"<?xml version="1.0"?>
<opml version="2.0">
  <body>
    <outline type="rss" xmlUrl="https://notitle.com/feed"/>
  </body>
</opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].title, "https://notitle.com/feed");
    }
}
