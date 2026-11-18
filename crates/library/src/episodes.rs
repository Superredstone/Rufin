//! Where a podcast episode was left, and whether it was heard. Only tracks of a podcasts
//! source keep this; every other track is left alone.

use crate::{Catalog, LibraryResult, TrackKey};

/// An episode counts as played once this share of it, in thousandths, was heard.
const PLAYED_PERMILLE: u64 = 950;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, sqlx::FromRow)]
pub struct EpisodeProgress {
    /// Where playback resumes; 0 once the episode was played.
    pub position_millis: i64,
    pub played: bool,
}

/// Whether stopping at `position_millis` of `duration_millis` means the episode was heard.
pub fn episode_finished(position_millis: u64, duration_millis: u64, completed: bool) -> bool {
    completed
        || duration_millis > 0
            && position_millis.saturating_mul(1000)
                >= duration_millis.saturating_mul(PLAYED_PERMILLE)
}

impl Catalog {
    pub async fn episode_progress(
        &self,
        media_uri: &str,
    ) -> LibraryResult<Option<EpisodeProgress>> {
        let mut connection = self.acquire_reader().await?;
        Ok(
            sqlx::query_as(
                "SELECT position_millis,played FROM episode_progress WHERE media_uri=?1",
            )
            .bind(media_uri)
            .fetch_optional(&mut *connection)
            .await?,
        )
    }

    /// Saves where `track` stopped, when it is a podcast episode. A finished episode is
    /// marked played and resumes from its start; a played one stays played.
    pub async fn save_episode_progress(
        &self,
        track: TrackKey,
        position_millis: u64,
        duration_millis: u64,
        completed: bool,
    ) -> LibraryResult<()> {
        let finished = episode_finished(position_millis, duration_millis, completed);
        let position = if finished { 0 } else { position_millis };
        let mut write = self.write().await?;
        sqlx::query(
            "INSERT INTO episode_progress(media_uri,position_millis,played,changed_at)
             SELECT track.media_uri,?2,?3,unixepoch() FROM tracks track JOIN sources USING(source_key)
             WHERE track.track_key=?1 AND sources.kind=?4
             ON CONFLICT(media_uri) DO UPDATE SET position_millis=excluded.position_millis,
                played=max(played,excluded.played),changed_at=excluded.changed_at",
        )
        .bind(track)
        .bind(position.min(i64::MAX as u64) as i64)
        .bind(finished)
        .bind(crate::SourceKind::Podcasts.name())
        .execute(write.connection().await?)
        .await?;
        write.commit().await?;
        Ok(())
    }

    /// Marks the podcast episodes among `media_uris` played or unplayed; either resumes them
    /// from their start.
    pub async fn set_episodes_played(
        &self,
        media_uris: &[String],
        played: bool,
    ) -> LibraryResult<()> {
        let mut write = self.write().await?;
        sqlx::query(
            "INSERT INTO episode_progress(media_uri,position_millis,played,changed_at)
             SELECT track.media_uri,0,?2,unixepoch() FROM tracks track JOIN sources USING(source_key)
             WHERE track.media_uri IN(SELECT value FROM json_each(?1)) AND sources.kind=?3
             ON CONFLICT(media_uri) DO UPDATE SET position_millis=0,played=excluded.played,
                changed_at=excluded.changed_at",
        )
        .bind(serde_json::to_string(media_uris)?)
        .bind(played)
        .bind(crate::SourceKind::Podcasts.name())
        .execute(write.connection().await?)
        .await?;
        write.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_at_ninety_five_percent_or_on_completion() {
        assert!(!episode_finished(0, 0, false));
        assert!(!episode_finished(949, 1000, false));
        assert!(episode_finished(950, 1000, false));
        assert!(episode_finished(10, 1000, true));
        assert!(
            !episode_finished(u64::MAX / 2, 0, false),
            "unknown length never finishes"
        );
    }

    #[tokio::test]
    async fn progress_is_kept_for_podcast_episodes_only() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(directory.path().join("catalog"))
            .await
            .unwrap();
        {
            let mut writer = catalog.writer().await.unwrap();
            sqlx::raw_sql(
                "INSERT INTO sources(source_key,object_id,kind) VALUES (1,'p','podcasts'),(2,'m','local');
                 INSERT INTO tracks(track_key,source_key,object_id,media_uri,title,sort_text,duration_millis,indexed,native_metadata)
                 VALUES (1,1,'ep','rufin:ep',  'Ep','ep',1000,1,1),
                        (2,2,'song','file:///song','Song','song',1000,1,1);",
            )
            .execute(&mut *writer)
            .await
            .unwrap();
        }
        catalog.require_reload();
        let progress = |uri: &'static str| catalog.episode_progress(uri);

        catalog
            .save_episode_progress(TrackKey::from_raw(2), 400, 1000, false)
            .await
            .unwrap();
        assert_eq!(
            progress("file:///song").await.unwrap(),
            None,
            "music keeps nothing"
        );

        catalog
            .save_episode_progress(TrackKey::from_raw(1), 400, 1000, false)
            .await
            .unwrap();
        let saved = EpisodeProgress {
            position_millis: 400,
            played: false,
        };
        assert_eq!(progress("rufin:ep").await.unwrap(), Some(saved));

        // Heard to 95%: played, and the next play starts over.
        catalog
            .save_episode_progress(TrackKey::from_raw(1), 960, 1000, false)
            .await
            .unwrap();
        let played = EpisodeProgress {
            position_millis: 0,
            played: true,
        };
        assert_eq!(progress("rufin:ep").await.unwrap(), Some(played));

        // Listening again keeps it played.
        catalog
            .save_episode_progress(TrackKey::from_raw(1), 100, 1000, false)
            .await
            .unwrap();
        let replaying = EpisodeProgress {
            position_millis: 100,
            played: true,
        };
        assert_eq!(progress("rufin:ep").await.unwrap(), Some(replaying));

        let uris = ["rufin:ep".to_owned(), "file:///song".to_owned()];
        catalog.set_episodes_played(&uris, false).await.unwrap();
        assert_eq!(
            progress("rufin:ep").await.unwrap(),
            Some(EpisodeProgress::default())
        );
        assert_eq!(progress("file:///song").await.unwrap(), None);
    }
}
