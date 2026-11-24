//! RSS 2.0 podcast feed parsing (with the common iTunes extensions).

use crate::{SourceError, SourceResult};

const ITUNES: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";
const CONTENT: &str = "http://purl.org/rss/1.0/modules/content/";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Feed {
    pub title: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub episodes: Vec<Episode>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Episode {
    pub guid: String,
    pub title: String,
    /// `YYYY-MM-DD`
    pub release_date: Option<String>,
    /// Unix seconds
    pub published: Option<i64>,
    pub duration_seconds: Option<u32>,
    pub enclosure_url: String,
    pub enclosure_type: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
}

pub(crate) fn parse_feed(xml: &str) -> SourceResult<Feed> {
    let options = roxmltree::ParsingOptions {
        // Some feeds still ship a DOCTYPE; roxmltree never fetches external entities.
        allow_dtd: true,
        ..Default::default()
    };
    let document = roxmltree::Document::parse_with_options(xml, options)
        .map_err(|error| SourceError::Other(format!("podcast feed is not valid XML: {error}")))?;
    let channel = document
        .root_element()
        .children()
        .find(|node| node.has_tag_name("channel"))
        .ok_or_else(|| SourceError::Other("podcast feed has no RSS channel".into()))?;

    let episodes = channel
        .children()
        .filter(|node| node.has_tag_name("item"))
        .filter_map(episode)
        .collect();
    Ok(Feed {
        title: child_text(channel, "", "title").unwrap_or_else(|| "Untitled Podcast".into()),
        author: child_text(channel, ITUNES, "author")
            .or_else(|| child_text(channel, "", "managingEditor")),
        description: child_text(channel, "", "description")
            .or_else(|| child_text(channel, ITUNES, "summary")),
        image: itunes_image(channel).or_else(|| {
            channel
                .children()
                .find(|node| node.has_tag_name("image"))
                .and_then(|image| child_text(image, "", "url"))
        }),
        episodes,
    })
}

fn episode(item: roxmltree::Node) -> Option<Episode> {
    let enclosure = item
        .children()
        .find(|node| node.has_tag_name("enclosure"))?;
    let enclosure_url = enclosure.attribute("url")?.trim().to_owned();
    if enclosure_url.is_empty() {
        return None;
    }
    let published = child_text(item, "", "pubDate").and_then(|value| parse_date(&value));
    Some(Episode {
        // Feeds without a GUID are identified by their media file, like most podcast apps do.
        guid: child_text(item, "", "guid").unwrap_or_else(|| enclosure_url.clone()),
        title: child_text(item, "", "title")
            .or_else(|| child_text(item, ITUNES, "title"))
            .unwrap_or_else(|| "Untitled Episode".into()),
        release_date: published.map(|date| {
            let date = date.date();
            format!(
                "{:04}-{:02}-{:02}",
                date.year(),
                u8::from(date.month()),
                date.day()
            )
        }),
        published: published.map(time::OffsetDateTime::unix_timestamp),
        duration_seconds: child_text(item, ITUNES, "duration").and_then(|v| parse_duration(&v)),
        enclosure_type: enclosure
            .attribute("type")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        enclosure_url,
        description: child_text(item, CONTENT, "encoded")
            .or_else(|| child_text(item, "", "description"))
            .or_else(|| child_text(item, ITUNES, "summary"))
            .map(|notes| plain_text(&notes))
            .filter(|notes| !notes.is_empty()),
        image: itunes_image(item),
    })
}

/// `namespace` is `""` for plain RSS elements.
fn child_text(node: roxmltree::Node, namespace: &str, name: &str) -> Option<String> {
    node.children()
        .find(|child| {
            child.is_element()
                && child.tag_name().name() == name
                && child.tag_name().namespace().unwrap_or("") == namespace
        })
        .map(|child| {
            child
                .descendants()
                .filter(|node| node.is_text())
                .filter_map(|node| node.text())
                .collect::<String>()
        })
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn itunes_image(node: roxmltree::Node) -> Option<String> {
    node.children()
        .find(|child| child.has_tag_name((ITUNES, "image")))
        .and_then(|image| image.attribute("href"))
        .map(str::trim)
        .filter(|href| !href.is_empty())
        .map(str::to_owned)
}

/// Show notes as plain text: tags dropped, line breaks kept, common entities decoded.
fn plain_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some((before, after)) = rest.split_once('<') {
        text.push_str(before);
        let Some((tag, after)) = after.split_once('>') else {
            // An unclosed `<` is text.
            text.push('<');
            rest = after;
            break;
        };
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "br" | "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4"
        ) {
            text.push('\n');
        }
        rest = after;
    }
    text.push_str(rest);
    let text = decode_entities(&text);
    // At most one blank line between paragraphs.
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        if !(line.is_empty() && lines.last().is_none_or(|last| last.is_empty())) {
            lines.push(line);
        }
    }
    lines.join("\n").trim().to_owned()
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('&') {
        decoded.push_str(before);
        let entity = after
            .split_once(';')
            .filter(|(entity, _)| entity.len() <= 10)
            .and_then(|(entity, tail)| entity_char(entity).map(|value| (value, tail)));
        match entity {
            Some((value, tail)) => {
                decoded.push(value);
                rest = tail;
            }
            None => {
                decoded.push('&');
                rest = after;
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

fn entity_char(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        numeric => numeric
            .strip_prefix("#x")
            .or_else(|| numeric.strip_prefix("#X"))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| numeric.strip_prefix('#').and_then(|dec| dec.parse().ok()))
            .and_then(char::from_u32),
    }
}

fn parse_date(value: &str) -> Option<time::OffsetDateTime> {
    use time::format_description::well_known::{Rfc2822, Rfc3339};
    time::OffsetDateTime::parse(value, &Rfc2822)
        .or_else(|_| time::OffsetDateTime::parse(value, &Rfc3339))
        .ok()
}

/// Accepts `SS`, `MM:SS`, `HH:MM:SS`, with optional fractional seconds.
fn parse_duration(value: &str) -> Option<u32> {
    let mut seconds = 0_u32;
    let parts: Vec<&str> = value.trim().split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    for part in parts {
        let whole = part.split('.').next()?.trim().parse::<u32>().ok()?;
        seconds = seconds.checked_mul(60)?.checked_add(whole)?;
    }
    Some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITUNES_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>Example Show</title>
    <itunes:author>Example Media</itunes:author>
    <description>All about examples.</description>
    <itunes:image href="https://example.com/show.jpg"/>
    <item>
      <title>Episode 2</title>
      <guid isPermaLink="false">ep-2</guid>
      <pubDate>Tue, 10 Nov 2026 08:00:00 GMT</pubDate>
      <itunes:duration>01:02:03</itunes:duration>
      <enclosure url="https://cdn.example.com/ep2.mp3" length="123" type="audio/mpeg"/>
      <content:encoded><![CDATA[<p>Show <a href="https://example.com">notes</a></p>]]></content:encoded>
      <description>Plain summary</description>
      <itunes:image href="https://example.com/ep2.jpg"/>
    </item>
    <item>
      <title>Episode 1</title>
      <pubDate>2026-11-03T08:00:00+01:00</pubDate>
      <itunes:duration>2710</itunes:duration>
      <enclosure url=" https://cdn.example.com/ep1.mp3 " type="audio/mpeg"/>
    </item>
    <item>
      <title>Trailer without audio</title>
      <guid>no-audio</guid>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_channel_and_episodes() {
        let feed = parse_feed(ITUNES_FEED).unwrap();
        assert_eq!(feed.title, "Example Show");
        assert_eq!(feed.author.as_deref(), Some("Example Media"));
        assert_eq!(feed.description.as_deref(), Some("All about examples."));
        assert_eq!(feed.image.as_deref(), Some("https://example.com/show.jpg"));
        assert_eq!(
            feed.episodes.len(),
            2,
            "items without an enclosure are skipped"
        );

        let latest = &feed.episodes[0];
        assert_eq!(latest.guid, "ep-2");
        assert_eq!(latest.release_date.as_deref(), Some("2026-11-10"));
        assert_eq!(latest.duration_seconds, Some(3723));
        assert_eq!(latest.enclosure_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(latest.description.as_deref(), Some("Show notes"));
        assert_eq!(latest.image.as_deref(), Some("https://example.com/ep2.jpg"));

        let first = &feed.episodes[1];
        assert_eq!(first.guid, "https://cdn.example.com/ep1.mp3");
        assert_eq!(first.enclosure_url, "https://cdn.example.com/ep1.mp3");
        assert_eq!(first.release_date.as_deref(), Some("2026-11-03"));
        assert_eq!(first.duration_seconds, Some(2710));
    }

    #[test]
    fn falls_back_to_rss_image_and_tolerates_bad_values() {
        let feed = parse_feed(
            r#"<!DOCTYPE rss><rss version="2.0"><channel>
                <title> Plain </title>
                <image><url>https://example.com/rss.png</url></image>
                <item><title>A</title><pubDate>not a date</pubDate>
                  <itunes:duration xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">soon</itunes:duration>
                  <enclosure url="https://example.com/a.m4a"/></item>
            </channel></rss>"#,
        )
        .unwrap();
        assert_eq!(feed.title, "Plain");
        assert_eq!(feed.image.as_deref(), Some("https://example.com/rss.png"));
        assert_eq!(feed.episodes[0].release_date, None);
        assert_eq!(feed.episodes[0].duration_seconds, None);
    }

    #[test]
    fn rejects_non_rss() {
        assert!(parse_feed("<html><body/></html>").is_err());
        assert!(parse_feed("not xml").is_err());
    }

    #[test]
    fn show_notes_become_plain_text() {
        assert_eq!(
            plain_text(
                "<p>Hello&nbsp;<b>world</b> &amp; friends</p><p>Line<br/>two &#8212; &#x41;</p>"
            ),
            "Hello world & friends\n\nLine\ntwo \u{2014} A"
        );
        assert_eq!(
            plain_text("Tom & Jerry &unknown; <3"),
            "Tom & Jerry &unknown; <3"
        );
        assert_eq!(
            plain_text("<ul><li>One</li><li>Two</li></ul>"),
            "One\n\nTwo"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("45"), Some(45));
        assert_eq!(parse_duration("12:30"), Some(750));
        assert_eq!(parse_duration("1:00:00.5"), Some(3600));
        assert_eq!(parse_duration("1:2:3:4"), None);
    }
}
