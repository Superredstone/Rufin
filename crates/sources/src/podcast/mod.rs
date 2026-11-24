//! Podcast subscriptions as a source: each RSS feed is an album, each episode a track.

mod feed;

use std::time::Duration;

use audio_processing::ResolvedStream;
use serde::{Deserialize, Serialize};

use crate::config::{decode_provider_payload, encode_provider_payload, require_payload_version};
use crate::remote_http::{BodyLimit, RemoteHttpPolicy};
use crate::{
    ConnectedSource, ImageBytes, NativeImageRef, SourceConfiguration, SourceEditResult,
    SourceError, SourceId, SourceImageRequest, SourceProgress, SourceProgressStage, SourceResult,
};

pub(crate) const PODCAST_SOURCE_ID: &str = "podcasts";
const SOURCE_CONFIG_VERSION: u32 = 1;
const FEED_LIMIT: BodyLimit = BodyLimit {
    max_bytes: 32 * 1024 * 1024,
    context: "Podcast feed",
};
const IMAGE_LIMIT: BodyLimit = BodyLimit {
    max_bytes: 16 * 1024 * 1024,
    context: "Podcast artwork",
};
const POLICY: RemoteHttpPolicy = RemoteHttpPolicy {
    service: "Podcast",
    auth_context: "Podcast feed",
    error_body: BodyLimit {
        max_bytes: 64 * 1024,
        context: "Podcast error",
    },
    redact_error_url: None,
};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PodcastSourceConfig {
    pub feeds: Vec<String>,
}

#[derive(Deserialize)]
struct PodcastSourcePayload {
    version: u32,
    #[serde(default)]
    feeds: Vec<String>,
}

impl PodcastSourceConfig {
    pub(crate) fn from_configuration(stored: &SourceConfiguration) -> SourceResult<Self> {
        crate::source::require_source_edit(stored, PODCAST_SOURCE_ID)?;
        let payload: PodcastSourcePayload = decode_provider_payload(stored)?;
        require_payload_version(payload.version, SOURCE_CONFIG_VERSION)?;
        Ok(Self {
            feeds: payload.feeds,
        })
    }

    fn into_configuration(self, source_id: SourceId, name: String) -> SourceConfiguration {
        encode_provider_payload(
            source_id,
            PODCAST_SOURCE_ID,
            name,
            serde_json::json!({ "version": SOURCE_CONFIG_VERSION, "feeds": self.feeds }),
        )
    }
}

/// Trims, validates (http/https only) and de-duplicates feed URLs, keeping their order.
fn normalized_feeds(feeds: Vec<String>) -> SourceResult<Vec<String>> {
    let mut result: Vec<String> = Vec::with_capacity(feeds.len());
    for feed in feeds {
        let feed = feed.trim();
        if feed.is_empty() {
            continue;
        }
        let url = url::Url::parse(feed)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| {
                SourceError::InvalidConfig(format!("invalid podcast feed URL {feed}"))
            })?;
        let url = url.to_string();
        if !result.contains(&url) {
            result.push(url);
        }
    }
    Ok(result)
}

pub struct PodcastSource {
    source_id: SourceId,
    feeds: Vec<String>,
    client: reqwest::Client,
}

pub(crate) fn connect(
    source_id: SourceId,
    name: String,
    feeds: Vec<String>,
) -> SourceResult<ConnectedSource> {
    let name = if name.trim().is_empty() {
        "Podcasts".to_owned()
    } else {
        name.trim().to_owned()
    };
    let configuration = PodcastSourceConfig {
        feeds: normalized_feeds(feeds)?,
    }
    .into_configuration(source_id, name);
    let source = open(&configuration)?;
    Ok(ConnectedSource::podcasts(configuration, source))
}

pub(crate) fn open(configuration: &SourceConfiguration) -> SourceResult<PodcastSource> {
    let config = PodcastSourceConfig::from_configuration(configuration)?;
    // Feeds and enclosures redirect across CDNs freely and carry no credentials,
    // so the default redirect policy is used instead of the authenticated one.
    let client = reqwest::Client::builder()
        .referer(false)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("Rufin/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| crate::remote_http::map_reqwest_error(error, POLICY))?;
    Ok(PodcastSource {
        source_id: configuration.source_id.clone(),
        feeds: config.feeds,
        client,
    })
}

pub(crate) fn edit(
    current: SourceConfiguration,
    name: String,
    feeds: Vec<String>,
) -> SourceResult<SourceEditResult> {
    let saved = PodcastSourceConfig::from_configuration(&current)?;
    let name = crate::source::edited_source_name(&name, &current.name);
    let feeds = normalized_feeds(feeds)?;
    if feeds == saved.feeds && name == current.name {
        return Ok(SourceEditResult::Unchanged);
    }
    if feeds == saved.feeds {
        return Ok(SourceEditResult::ConfigurationOnly(SourceConfiguration {
            name,
            ..current
        }));
    }
    let next = PodcastSourceConfig { feeds }.into_configuration(current.source_id, name);
    let source = open(&next)?;
    Ok(SourceEditResult::Connected(Box::new(
        ConnectedSource::podcasts(next, source),
    )))
}

impl PodcastSource {
    async fn fetch_feed(&self, url: &str) -> SourceResult<feed::Feed> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| crate::remote_http::map_reqwest_error(error, POLICY))?;
        let body = crate::remote_http::bounded_response_body(response, POLICY, FEED_LIMIT).await?;
        feed::parse_feed(&String::from_utf8_lossy(&body))
    }

    /// Reads every subscribed feed. A failing feed is logged and skipped so one dead
    /// show never blocks the others; the result is `false` when any feed failed.
    pub(crate) async fn acquire_catalog(
        &self,
        observation: &mut library::Observation<'_>,
        progress: &(dyn Fn(SourceProgress) + Send + Sync),
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> SourceResult<bool> {
        let mut complete = true;
        let total = Some(self.feeds.len());
        for (index, url) in self.feeds.iter().enumerate() {
            if cancelled() {
                return Err(SourceError::Cancelled);
            }
            progress(SourceProgress {
                stage: SourceProgressStage::Albums,
                completed: index,
                total,
            });
            let feed = match self.fetch_feed(url).await {
                Ok(feed) => feed,
                Err(error) => {
                    tracing::warn!(%error, "podcast feed refresh failed");
                    complete = false;
                    continue;
                }
            };
            if cancelled() {
                return Err(SourceError::Cancelled);
            }
            observation
                .apply_catalog(&self.batch(url, &feed)?, true)
                .await?;
        }
        progress(SourceProgress {
            stage: SourceProgressStage::Albums,
            completed: self.feeds.len(),
            total,
        });
        // A feed no longer followed goes with its episodes. A followed feed keeps episodes its
        // RSS dropped, which users may still have queued or downloaded.
        let followed = self.feeds.iter().cloned().collect();
        observation.remove_unlisted_albums(&followed).await?;
        Ok(complete)
    }

    fn batch(&self, feed_url: &str, feed: &feed::Feed) -> SourceResult<library::CatalogBatch> {
        let source_id = self.source_id.as_str();
        let artwork = |url: &Option<String>| {
            url.as_ref()
                .map(|url| {
                    crate::native_artwork_binding(source_id, &NativeImageRef::new(url, None))
                })
                .transpose()
        };
        let author = feed.author.clone().unwrap_or_default();
        let album_artwork = artwork(&feed.image)?;
        let latest = feed
            .episodes
            .iter()
            .filter_map(|episode| episode.release_date.clone())
            .max();
        let album = library::AlbumObservation {
            object_id: feed_url.to_owned(),
            metadata: library::AlbumMetadata {
                title: feed.title.clone(),
                artist: author.clone(),
                sort_text: feed.title.to_lowercase(),
                release_date: latest.clone(),
                year: year(latest.as_deref()),
                ..Default::default()
            },
            artwork_binding: album_artwork.clone(),
            ..Default::default()
        };
        let tracks = feed
            .episodes
            .iter()
            .map(|episode| {
                Ok(library::TrackObservation {
                    object_id: episode_object_id(feed_url, &episode.guid),
                    album_object_id: Some(feed_url.to_owned()),
                    metadata: library::TrackMetadata {
                        title: episode.title.clone(),
                        artist: author.clone(),
                        album: feed.title.clone(),
                        album_display_artist: Some(author.clone()),
                        sort_text: episode.title.to_lowercase(),
                        duration_millis: i64::from(episode.duration_seconds.unwrap_or(0)) * 1000,
                        year: year(episode.release_date.as_deref()),
                        release_date: episode.release_date.clone(),
                        date_added: episode.release_date.clone(),
                        source_format: episode
                            .enclosure_type
                            .as_deref()
                            .and_then(|kind| kind.rsplit('/').next())
                            .map(str::to_owned),
                        comment: episode.description.clone(),
                        ..Default::default()
                    },
                    // shortcut: the enclosure URL lives in source_path because streaming reads
                    // it back as `Playable::source_path`; move it to its own column if file
                    // features ever start treating podcast source paths as files.
                    source_path: Some(episode.enclosure_url.clone()),
                    first_seen_at: episode.published,
                    artwork_binding: match artwork(&episode.image)? {
                        Some(binding) => Some(binding),
                        None => album_artwork.clone(),
                    },
                    ..Default::default()
                })
            })
            .collect::<SourceResult<Vec<_>>>()?;
        Ok(library::CatalogBatch {
            albums: vec![album],
            tracks,
            ..Default::default()
        })
    }

    pub(crate) fn stream(&self, playable: &library::Playable) -> SourceResult<ResolvedStream> {
        if playable.source_id.as_ref() != Some(&self.source_id) {
            return Err(SourceError::NotFound);
        }
        playable
            .source_path
            .clone()
            .map(ResolvedStream::new)
            .ok_or(SourceError::NotFound)
    }

    pub(crate) async fn image(&self, request: SourceImageRequest) -> SourceResult<ImageBytes> {
        // Feeds publish one image size; Rufin scales thumbnails itself.
        let SourceImageRequest::Native { image_ref, .. } = request else {
            return Err(SourceError::NotFound);
        };
        let url = url::Url::parse(&image_ref.item_id)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or(SourceError::NotFound)?;
        crate::remote_http::bytes(self.client.get(url), POLICY, IMAGE_LIMIT).await
    }
}

/// GUIDs are only unique inside one feed.
fn episode_object_id(feed_url: &str, guid: &str) -> String {
    format!("{feed_url}#{guid}")
}

fn year(date: Option<&str>) -> Option<i64> {
    date?.get(..4)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds_are_validated_and_deduplicated() {
        assert_eq!(
            normalized_feeds(vec![
                " https://example.com/feed ".into(),
                String::new(),
                "https://example.com/feed".into(),
                "http://b.example/rss".into(),
            ])
            .unwrap(),
            vec!["https://example.com/feed", "http://b.example/rss"]
        );
        assert!(normalized_feeds(vec!["file:///etc/passwd".into()]).is_err());
        assert!(normalized_feeds(vec!["not a url".into()]).is_err());
    }

    #[test]
    fn configuration_round_trips() {
        let connected = connect(
            SourceId::new("p"),
            " ".into(),
            vec!["https://e.com/rss".into()],
        )
        .unwrap();
        let (configuration, _, credential) = connected.into_parts();
        assert_eq!(configuration.kind, PODCAST_SOURCE_ID);
        assert_eq!(configuration.name, "Podcasts");
        assert!(credential.is_none());
        assert_eq!(
            PodcastSourceConfig::from_configuration(&configuration)
                .unwrap()
                .feeds,
            vec!["https://e.com/rss"]
        );
        assert!(matches!(
            edit(
                configuration.clone(),
                "Podcasts".into(),
                vec!["https://e.com/rss".into()]
            )
            .unwrap(),
            SourceEditResult::Unchanged
        ));
        assert!(matches!(
            edit(
                configuration,
                "Shows".into(),
                vec!["https://e.com/rss".into()]
            )
            .unwrap(),
            SourceEditResult::ConfigurationOnly(_)
        ));
    }

    #[test]
    fn batch_maps_feed_to_album_and_episodes_to_tracks() {
        let source = open(
            &PodcastSourceConfig {
                feeds: vec!["https://e.com/rss".into()],
            }
            .into_configuration(SourceId::new("p"), "Podcasts".into()),
        )
        .unwrap();
        let feed = feed::Feed {
            title: "Show".into(),
            author: Some("Host".into()),
            image: Some("https://e.com/show.jpg".into()),
            episodes: vec![feed::Episode {
                guid: "g1".into(),
                title: "Ep".into(),
                release_date: Some("2026-11-10".into()),
                duration_seconds: Some(90),
                enclosure_url: "https://cdn.e.com/1.mp3".into(),
                enclosure_type: Some("audio/mpeg".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let batch = source.batch("https://e.com/rss", &feed).unwrap();
        assert_eq!(batch.albums[0].object_id, "https://e.com/rss");
        assert_eq!(batch.albums[0].metadata.year, Some(2026));
        let track = &batch.tracks[0];
        assert_eq!(track.object_id, "https://e.com/rss#g1");
        assert_eq!(track.album_object_id.as_deref(), Some("https://e.com/rss"));
        assert_eq!(track.metadata.duration_millis, 90_000);
        assert_eq!(track.metadata.source_format.as_deref(), Some("mpeg"));
        assert_eq!(
            track.source_path.as_deref(),
            Some("https://cdn.e.com/1.mp3")
        );
        assert_eq!(track.artwork_binding, batch.albums[0].artwork_binding);
    }
}
