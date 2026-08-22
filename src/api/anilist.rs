//! AniList query helpers and request scaffolding.

use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};
use crate::media::{MediaProperties, MediaType};

const DEFAULT_ENDPOINT: &str = "https://graphql.anilist.co";
const ANILIST_MEDIA_QUERY: &str = r#"
query ($search: String!, $isAdult: Boolean) {
  Media(search: $search, type: ANIME, isAdult: $isAdult) {
    id
    idMal
    siteUrl
    title {
      romaji
      english
      native
    }
    description(asHtml: false)
    startDate {
      year
      month
      day
    }
    endDate {
      year
      month
      day
    }
    season
    seasonYear
    episodes
    duration
    status
    format
    source
    countryOfOrigin
    hashtag
    genres
    synonyms
    averageScore
    meanScore
    popularity
    favourites
    coverImage {
      medium
      large
      extraLarge
      color
    }
    bannerImage
    trailer {
      id
      site
      thumbnail
    }
    tags {
      name
      rank
      category
      isGeneralSpoiler
      isMediaSpoiler
    }
    studios {
      nodes {
        id
        name
        isAnimationStudio
        siteUrl
      }
    }
    relations {
      edges {
        relationType
        node {
          id
          type
          format
          siteUrl
          title {
            romaji
            english
            native
          }
          coverImage {
            medium
            large
            extraLarge
          }
        }
      }
    }
    characters(page: 1, perPage: 12) {
      edges {
        role
        node {
          id
          name {
            full
            native
          }
          image {
            medium
            large
          }
        }
        voiceActors(language: JAPANESE) {
          id
          name {
            full
            native
          }
          languageV2
          image {
            medium
            large
          }
        }
      }
    }
    staff(page: 1, perPage: 12) {
      edges {
        role
        node {
          id
          name {
            full
            native
          }
          image {
            medium
            large
          }
        }
      }
    }
    reviews(page: 1, perPage: 5) {
      nodes {
        id
        summary
        rating
        ratingAmount
        siteUrl
        user {
          name
        }
      }
    }
    isAdult
  }
}
"#;

/// Compact query for light-novel lookups (type MANGA, format NOVEL).
const ANILIST_NOVEL_QUERY: &str = r#"
query ($search: String!) {
  Page(page: 1, perPage: 1) {
    media(search: $search, type: MANGA, format_in: [NOVEL], sort: SEARCH_MATCH) {
      id
      siteUrl
      title {
        romaji
        english
        native
      }
      description(asHtml: false)
      status
      genres
      averageScore
      tags {
        name
      }
      coverImage {
        extraLarge
        large
      }
    }
  }
}
"#;

const ANILIST_MEDIA_SEARCH_QUERY: &str = r#"
query ($search: String!, $isAdult: Boolean, $page: Int, $perPage: Int) {
  Page(page: $page, perPage: $perPage) {
    media(search: $search, type: ANIME, isAdult: $isAdult, sort: SEARCH_MATCH) {
      id
      idMal
      siteUrl
      title {
        romaji
        english
        native
      }
      description(asHtml: false)
      startDate {
        year
        month
        day
      }
      endDate {
        year
        month
        day
      }
      season
      seasonYear
      episodes
      duration
      status
      format
      source
      countryOfOrigin
      hashtag
      genres
      synonyms
      averageScore
      meanScore
      popularity
      favourites
      coverImage {
        medium
        large
        extraLarge
        color
      }
      bannerImage
      trailer {
        id
        site
        thumbnail
      }
      tags {
        name
        rank
        category
        isGeneralSpoiler
        isMediaSpoiler
      }
      studios {
        nodes {
          id
          name
          isAnimationStudio
          siteUrl
        }
      }
      relations {
        edges {
          relationType
          node {
            id
            type
            format
            siteUrl
            title {
              romaji
              english
              native
            }
            coverImage {
              medium
              large
              extraLarge
            }
          }
        }
      }
      characters(page: 1, perPage: 12) {
        edges {
          role
          node {
            id
            name {
              full
              native
            }
            image {
              medium
              large
            }
          }
          voiceActors(language: JAPANESE) {
            id
            name {
              full
              native
            }
            languageV2
            image {
              medium
              large
            }
          }
        }
      }
      staff(page: 1, perPage: 12) {
        edges {
          role
          node {
            id
            name {
              full
              native
            }
            image {
              medium
              large
            }
          }
        }
      }
      reviews(page: 1, perPage: 5) {
        nodes {
          id
          summary
          rating
          ratingAmount
          siteUrl
          user {
            name
          }
        }
      }
      isAdult
    }
  }
}
"#;

/// Minimal AniList client configuration used by the foundation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AniListClient {
    /// GraphQL endpoint.
    pub endpoint: String,
    /// Optional access token.
    pub access_token: Option<String>,
}

impl Default for AniListClient {
    fn default() -> Self {
        Self::new(DEFAULT_ENDPOINT)
    }
}

impl AniListClient {
    /// Creates a new AniList client configuration.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            access_token: None,
        }
    }

    /// Sets the access token used for authenticated requests.
    pub fn with_access_token(mut self, access_token: impl Into<String>) -> Self {
        self.access_token = Some(access_token.into());
        self
    }

    /// Returns the media type specific adult flag used by AniList queries.
    pub fn adult_flag_for(media_type: MediaType) -> bool {
        media_type.is_adult()
    }

    /// Builds a JSON request body for an AniList anime search.
    pub fn build_search_query(search: &str, adult: bool) -> String {
        serde_json::json!({
            "query": ANILIST_MEDIA_QUERY,
            "variables": {
                "search": search,
                "isAdult": adult,
            }
        })
        .to_string()
    }

    /// Builds a JSON request body for AniList anime search suggestions.
    pub fn build_search_results_query(
        search: &str,
        adult: bool,
        page: u32,
        per_page: u32,
    ) -> String {
        serde_json::json!({
            "query": ANILIST_MEDIA_SEARCH_QUERY,
            "variables": {
                "search": search,
                "isAdult": adult,
                "page": page,
                "perPage": per_page,
            }
        })
        .to_string()
    }

    /// Searches AniList for a light novel and returns the best match.
    ///
    /// Uses `type: MANGA, format_in: [NOVEL]` — AniList files light novels
    /// under the manga type with a dedicated NOVEL format.
    pub fn search_novel(&self, search: &str) -> Result<Option<AniListNovelMetadata>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(12))
            .build()
            .map_err(|error| VaultError::ExternalApi(error.to_string()))?;

        let mut request = client
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json");
        if let Some(access_token) = self.access_token.as_ref() {
            request = request.header(AUTHORIZATION, format!("Bearer {access_token}"));
        }

        let body = serde_json::json!({
            "query": ANILIST_NOVEL_QUERY,
            "variables": { "search": search }
        })
        .to_string();

        let response = request
            .body(body)
            .send()
            .map_err(|error| VaultError::ExternalApi(error.to_string()))?;
        if !response.status().is_success() {
            return Err(VaultError::ExternalApi(format!(
                "http {} from AniList",
                response.status()
            )));
        }

        let payload: AniListNovelResponse = response
            .json()
            .map_err(|error| VaultError::ExternalApi(error.to_string()))?;
        let media = payload
            .data
            .and_then(|data| data.page)
            .map(|page| page.media)
            .unwrap_or_default()
            .into_iter()
            .next();

        Ok(media.map(|raw| AniListNovelMetadata {
            anilist_id: raw.id,
            anilist_url: raw.site_url,
            title: raw
                .title
                .as_ref()
                .and_then(|title| title.english.clone().or_else(|| title.romaji.clone())),
            description: raw.description,
            status: raw.status,
            genres: raw.genres.unwrap_or_default(),
            tags: raw
                .tags
                .unwrap_or_default()
                .into_iter()
                .map(|tag| tag.name)
                .collect(),
            average_score: raw.average_score,
            cover_url: raw
                .cover_image
                .and_then(|cover| cover.extra_large.or(cover.large)),
        }))
    }

    /// Searches AniList for an anime title and returns the best match.
    pub fn search_anime(&self, search: &str, adult: bool) -> Result<Option<AniListAnimeMetadata>> {
        Ok(self
            .search_anime_candidates(search, adult, 1)?
            .into_iter()
            .next())
    }

    /// Searches AniList for anime title suggestions.
    pub fn search_anime_candidates(
        &self,
        search: &str,
        adult: bool,
        limit: usize,
    ) -> Result<Vec<AniListAnimeMetadata>> {
        let per_page = limit.clamp(1, 10) as u32;
        let client = Client::builder()
            .timeout(Duration::from_secs(12))
            .build()
            .map_err(|error| VaultError::ExternalApi(error.to_string()))?;

        let mut request = client
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json");

        if let Some(access_token) = self.access_token.as_ref() {
            request = request.header(AUTHORIZATION, format!("Bearer {access_token}"));
        }

        let response = request
            .body(Self::build_search_results_query(search, adult, 1, per_page))
            .send()
            .map_err(|error| VaultError::ExternalApi(error.to_string()))?;

        if !response.status().is_success() {
            return Err(VaultError::ExternalApi(format!(
                "http {} from AniList",
                response.status()
            )));
        }

        let payload: AniListGraphQlSearchResponse = response
            .json()
            .map_err(|error| VaultError::ExternalApi(error.to_string()))?;

        if let Some(errors) = payload.errors {
            let message = errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(VaultError::ExternalApi(message));
        }

        Ok(payload
            .data
            .map(|data| {
                data.page
                    .media
                    .into_iter()
                    .map(AniListAnimeMetadata::from)
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// A normalized AniList anime result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AniListAnimeMetadata {
    /// AniList media ID.
    pub anilist_id: u32,
    /// AniList detail URL.
    pub anilist_url: Option<String>,
    /// MyAnimeList identifier when available.
    pub mal_id: Option<u32>,
    /// Romaji title.
    pub title_romaji: Option<String>,
    /// English title.
    pub title_english: Option<String>,
    /// Native title.
    pub title_native: Option<String>,
    /// Description text.
    pub description: Option<String>,
    /// Aired season label.
    pub season: Option<String>,
    /// Aired season year.
    pub season_year: Option<u16>,
    /// Start date.
    pub start_date: Option<AniListDate>,
    /// End date.
    pub end_date: Option<AniListDate>,
    /// Episode count.
    pub episodes: Option<u16>,
    /// Episode duration in minutes.
    pub duration: Option<u16>,
    /// AniList status string.
    pub status: Option<String>,
    /// AniList format string.
    pub format: Option<String>,
    /// Source material.
    pub source: Option<String>,
    /// Country of origin.
    pub country_of_origin: Option<String>,
    /// Official hashtag.
    pub hashtag: Option<String>,
    /// Genres.
    pub genres: Vec<String>,
    /// Alternative titles.
    pub synonyms: Vec<String>,
    /// AniList community score.
    pub average_score: Option<f32>,
    /// AniList mean score.
    pub mean_score: Option<f32>,
    /// AniList popularity.
    pub popularity: Option<u32>,
    /// AniList favourites.
    pub favourites: Option<u32>,
    /// Medium cover image.
    pub cover_image_medium: Option<String>,
    /// Large cover image.
    pub cover_image_large: Option<String>,
    /// Extra large cover image.
    pub cover_image_extra_large: Option<String>,
    /// Dominant cover color.
    pub cover_color: Option<String>,
    /// Banner image.
    pub banner_image: Option<String>,
    /// Trailer metadata.
    pub trailer: Option<AniListTrailer>,
    /// Provider tags.
    pub tags: Vec<AniListTag>,
    /// Animation and production studios.
    pub studios: Vec<AniListStudio>,
    /// Related media.
    pub relations: Vec<AniListRelation>,
    /// Character and voice actor credits.
    pub characters: Vec<AniListCharacterCredit>,
    /// Staff credits.
    pub staff: Vec<AniListStaffCredit>,
    /// Community review summaries.
    pub reviews: Vec<AniListReview>,
    /// Whether the title is adult-oriented.
    pub is_adult: bool,
}

impl AniListAnimeMetadata {
    /// Returns the best title for display and search results.
    pub fn display_title(&self) -> Option<&str> {
        self.title_english
            .as_deref()
            .or(self.title_romaji.as_deref())
            .or(self.title_native.as_deref())
    }

    /// Applies the fetched AniList data to portable media properties.
    pub fn apply_to_properties(&self, properties: &mut MediaProperties) {
        properties.anilist_id = Some(self.anilist_id);
        properties.anilist_url = self.anilist_url.clone();
        properties.title = self.display_title().map(|value| value.to_string());
        properties.title_original = self.title_native.clone();
        properties.description = self.description.clone();
        properties.year = self.season_year;
        properties.genres = self.genres.clone();
        properties.tags = self.tags.iter().map(|tag| tag.name.clone()).collect();
        properties.categories = self
            .format
            .as_ref()
            .map(|value| vec![value.clone()])
            .unwrap_or_default();
        properties.rating_external = self.average_score;
        properties.series_title = self.display_title().map(|value| value.to_string());
        properties.episode_count = self.episodes;
        properties.runtime_minutes = self.duration;
        properties.average_score = self.average_score;
        properties.format = self.format.clone();
        properties.airing_season = self.season.clone();
    }
}

/// Date value returned by AniList.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListDate {
    /// Year component.
    pub year: Option<u16>,
    /// Month component.
    pub month: Option<u8>,
    /// Day component.
    pub day: Option<u8>,
}

/// Trailer metadata returned by AniList.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListTrailer {
    /// Provider-local trailer id.
    pub id: Option<String>,
    /// Trailer provider.
    pub site: Option<String>,
    /// Trailer thumbnail URL.
    pub thumbnail: Option<String>,
}

/// AniList tag metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListTag {
    /// Tag name.
    pub name: String,
    /// Provider rank.
    pub rank: Option<u16>,
    /// Provider category.
    pub category: Option<String>,
    /// Whether this tag may spoil general information.
    pub is_general_spoiler: bool,
    /// Whether this tag may spoil this media.
    pub is_media_spoiler: bool,
}

/// Studio metadata returned by AniList.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListStudio {
    /// AniList studio id.
    pub id: u32,
    /// Studio name.
    pub name: String,
    /// Whether the studio is an animation studio.
    pub is_animation_studio: bool,
    /// AniList studio URL.
    pub site_url: Option<String>,
}

/// Related media metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListRelation {
    /// Relation type.
    pub relation_type: Option<String>,
    /// Related media id.
    pub id: u32,
    /// Related media type.
    pub media_type: Option<String>,
    /// Related media format.
    pub format: Option<String>,
    /// Related media URL.
    pub site_url: Option<String>,
    /// Related media title.
    pub title: Option<String>,
    /// Preferred related media cover.
    pub cover_image_medium: Option<String>,
    /// Large related media cover.
    pub cover_image_large: Option<String>,
    /// Extra large related media cover.
    pub cover_image_extra_large: Option<String>,
}

/// Character and Japanese voice actor credit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListCharacterCredit {
    /// Character role.
    pub role: Option<String>,
    /// Character id.
    pub character_id: u32,
    /// Character name.
    pub character_name: Option<String>,
    /// Character image URL.
    pub character_image: Option<String>,
    /// Japanese voice actors.
    pub voice_actors: Vec<AniListPerson>,
}

/// Staff credit metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListStaffCredit {
    /// Staff role.
    pub role: Option<String>,
    /// Staff person.
    pub person: AniListPerson,
}

/// Person metadata used for staff and voice actors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListPerson {
    /// AniList person id.
    pub id: u32,
    /// Display name.
    pub name: Option<String>,
    /// Native name.
    pub native_name: Option<String>,
    /// Language label when available.
    pub language: Option<String>,
    /// Person image URL.
    pub image: Option<String>,
}

/// Review metadata returned by AniList.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AniListReview {
    /// AniList review id.
    pub id: u32,
    /// Review summary.
    pub summary: Option<String>,
    /// Provider rating.
    pub rating: Option<u16>,
    /// Number of ratings for the review.
    pub rating_amount: Option<u32>,
    /// AniList review URL.
    pub site_url: Option<String>,
    /// Reviewer name.
    pub user_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlSearchResponse {
    data: Option<AniListGraphQlSearchData>,
    errors: Option<Vec<AniListGraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlSearchData {
    #[serde(rename = "Page")]
    page: AniListGraphQlSearchPage,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlSearchPage {
    #[serde(default)]
    media: Vec<AniListGraphQlMedia>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlMedia {
    id: u32,
    #[serde(rename = "idMal")]
    id_mal: Option<u32>,
    #[serde(rename = "siteUrl")]
    site_url: Option<String>,
    title: AniListTitles,
    description: Option<String>,
    #[serde(rename = "startDate")]
    start_date: Option<AniListGraphQlDate>,
    #[serde(rename = "endDate")]
    end_date: Option<AniListGraphQlDate>,
    season: Option<String>,
    #[serde(rename = "seasonYear")]
    season_year: Option<u16>,
    episodes: Option<u16>,
    duration: Option<u16>,
    status: Option<String>,
    format: Option<String>,
    source: Option<String>,
    #[serde(rename = "countryOfOrigin")]
    country_of_origin: Option<String>,
    hashtag: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    synonyms: Vec<String>,
    #[serde(rename = "averageScore")]
    average_score: Option<f32>,
    #[serde(rename = "meanScore")]
    mean_score: Option<f32>,
    popularity: Option<u32>,
    favourites: Option<u32>,
    #[serde(rename = "coverImage")]
    cover_image: AniListCoverImage,
    #[serde(rename = "bannerImage")]
    banner_image: Option<String>,
    trailer: Option<AniListGraphQlTrailer>,
    #[serde(default)]
    tags: Vec<AniListGraphQlTag>,
    studios: Option<AniListGraphQlStudioConnection>,
    relations: Option<AniListGraphQlRelationConnection>,
    characters: Option<AniListGraphQlCharacterConnection>,
    staff: Option<AniListGraphQlStaffConnection>,
    reviews: Option<AniListGraphQlReviewConnection>,
    #[serde(rename = "isAdult")]
    is_adult: bool,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlDate {
    year: Option<u16>,
    month: Option<u8>,
    day: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct AniListTitles {
    romaji: Option<String>,
    english: Option<String>,
    native: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListCoverImage {
    medium: Option<String>,
    large: Option<String>,
    #[serde(rename = "extraLarge")]
    extra_large: Option<String>,
    color: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlTrailer {
    id: Option<String>,
    site: Option<String>,
    thumbnail: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlTag {
    name: String,
    rank: Option<u16>,
    category: Option<String>,
    #[serde(rename = "isGeneralSpoiler")]
    is_general_spoiler: bool,
    #[serde(rename = "isMediaSpoiler")]
    is_media_spoiler: bool,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlStudioConnection {
    #[serde(default)]
    nodes: Vec<AniListGraphQlStudio>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlStudio {
    id: u32,
    name: String,
    #[serde(rename = "isAnimationStudio")]
    is_animation_studio: bool,
    #[serde(rename = "siteUrl")]
    site_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlRelationConnection {
    #[serde(default)]
    edges: Vec<AniListGraphQlRelationEdge>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlRelationEdge {
    #[serde(rename = "relationType")]
    relation_type: Option<String>,
    node: AniListGraphQlRelatedMedia,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlRelatedMedia {
    id: u32,
    #[serde(rename = "type")]
    media_type: Option<String>,
    format: Option<String>,
    #[serde(rename = "siteUrl")]
    site_url: Option<String>,
    title: AniListTitles,
    #[serde(rename = "coverImage")]
    cover_image: Option<AniListGraphQlSimpleImage>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlCharacterConnection {
    #[serde(default)]
    edges: Vec<AniListGraphQlCharacterEdge>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlCharacterEdge {
    role: Option<String>,
    node: AniListGraphQlCharacter,
    #[serde(rename = "voiceActors")]
    #[serde(default)]
    voice_actors: Vec<AniListGraphQlPerson>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlCharacter {
    id: u32,
    name: AniListName,
    image: Option<AniListGraphQlSimpleImage>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlStaffConnection {
    #[serde(default)]
    edges: Vec<AniListGraphQlStaffEdge>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlStaffEdge {
    role: Option<String>,
    node: AniListGraphQlPerson,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlPerson {
    id: u32,
    name: AniListName,
    #[serde(rename = "languageV2")]
    language: Option<String>,
    image: Option<AniListGraphQlSimpleImage>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlSimpleImage {
    medium: Option<String>,
    large: Option<String>,
    #[serde(rename = "extraLarge")]
    extra_large: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListName {
    full: Option<String>,
    native: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlReviewConnection {
    #[serde(default)]
    nodes: Vec<AniListGraphQlReview>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlReview {
    id: u32,
    summary: Option<String>,
    rating: Option<u16>,
    #[serde(rename = "ratingAmount")]
    rating_amount: Option<u32>,
    #[serde(rename = "siteUrl")]
    site_url: Option<String>,
    user: Option<AniListGraphQlReviewUser>,
}

#[derive(Debug, Deserialize)]
struct AniListGraphQlReviewUser {
    name: Option<String>,
}

impl From<AniListGraphQlMedia> for AniListAnimeMetadata {
    fn from(media: AniListGraphQlMedia) -> Self {
        Self {
            anilist_id: media.id,
            mal_id: media.id_mal,
            anilist_url: media.site_url,
            title_romaji: media.title.romaji,
            title_english: media.title.english,
            title_native: media.title.native,
            description: media.description,
            start_date: media.start_date.map(AniListDate::from),
            end_date: media.end_date.map(AniListDate::from),
            season: media.season,
            season_year: media.season_year,
            episodes: media.episodes,
            duration: media.duration,
            status: media.status,
            format: media.format,
            source: media.source,
            country_of_origin: media.country_of_origin,
            hashtag: media.hashtag,
            genres: media.genres,
            synonyms: media.synonyms,
            average_score: media.average_score,
            mean_score: media.mean_score,
            popularity: media.popularity,
            favourites: media.favourites,
            cover_image_medium: media.cover_image.medium,
            cover_image_large: media.cover_image.large,
            cover_image_extra_large: media.cover_image.extra_large,
            cover_color: media.cover_image.color,
            banner_image: media.banner_image,
            trailer: media.trailer.map(AniListTrailer::from),
            tags: media.tags.into_iter().map(AniListTag::from).collect(),
            studios: media
                .studios
                .map(|connection| {
                    connection
                        .nodes
                        .into_iter()
                        .map(AniListStudio::from)
                        .collect()
                })
                .unwrap_or_default(),
            relations: media
                .relations
                .map(|connection| {
                    connection
                        .edges
                        .into_iter()
                        .map(AniListRelation::from)
                        .collect()
                })
                .unwrap_or_default(),
            characters: media
                .characters
                .map(|connection| {
                    connection
                        .edges
                        .into_iter()
                        .map(AniListCharacterCredit::from)
                        .collect()
                })
                .unwrap_or_default(),
            staff: media
                .staff
                .map(|connection| {
                    connection
                        .edges
                        .into_iter()
                        .map(AniListStaffCredit::from)
                        .collect()
                })
                .unwrap_or_default(),
            reviews: media
                .reviews
                .map(|connection| {
                    connection
                        .nodes
                        .into_iter()
                        .map(AniListReview::from)
                        .collect()
                })
                .unwrap_or_default(),
            is_adult: media.is_adult,
        }
    }
}

impl From<AniListGraphQlDate> for AniListDate {
    fn from(date: AniListGraphQlDate) -> Self {
        Self {
            year: date.year,
            month: date.month,
            day: date.day,
        }
    }
}

impl From<AniListGraphQlTrailer> for AniListTrailer {
    fn from(trailer: AniListGraphQlTrailer) -> Self {
        Self {
            id: trailer.id,
            site: trailer.site,
            thumbnail: trailer.thumbnail,
        }
    }
}

impl From<AniListGraphQlTag> for AniListTag {
    fn from(tag: AniListGraphQlTag) -> Self {
        Self {
            name: tag.name,
            rank: tag.rank,
            category: tag.category,
            is_general_spoiler: tag.is_general_spoiler,
            is_media_spoiler: tag.is_media_spoiler,
        }
    }
}

impl From<AniListGraphQlStudio> for AniListStudio {
    fn from(studio: AniListGraphQlStudio) -> Self {
        Self {
            id: studio.id,
            name: studio.name,
            is_animation_studio: studio.is_animation_studio,
            site_url: studio.site_url,
        }
    }
}

impl From<AniListGraphQlRelationEdge> for AniListRelation {
    fn from(edge: AniListGraphQlRelationEdge) -> Self {
        Self {
            relation_type: edge.relation_type,
            id: edge.node.id,
            media_type: edge.node.media_type,
            format: edge.node.format,
            site_url: edge.node.site_url,
            title: edge
                .node
                .title
                .display_title()
                .map(|value| value.to_string()),
            cover_image_medium: edge
                .node
                .cover_image
                .as_ref()
                .and_then(|image| image.medium.clone()),
            cover_image_large: edge
                .node
                .cover_image
                .as_ref()
                .and_then(|image| image.large.clone()),
            cover_image_extra_large: edge
                .node
                .cover_image
                .as_ref()
                .and_then(|image| image.extra_large.clone()),
        }
    }
}

impl From<AniListGraphQlCharacterEdge> for AniListCharacterCredit {
    fn from(edge: AniListGraphQlCharacterEdge) -> Self {
        Self {
            role: edge.role,
            character_id: edge.node.id,
            character_name: edge.node.name.full.or(edge.node.name.native),
            character_image: edge
                .node
                .image
                .and_then(|image| image.large.or(image.medium)),
            voice_actors: edge
                .voice_actors
                .into_iter()
                .map(AniListPerson::from)
                .collect(),
        }
    }
}

impl From<AniListGraphQlStaffEdge> for AniListStaffCredit {
    fn from(edge: AniListGraphQlStaffEdge) -> Self {
        Self {
            role: edge.role,
            person: AniListPerson::from(edge.node),
        }
    }
}

impl From<AniListGraphQlPerson> for AniListPerson {
    fn from(person: AniListGraphQlPerson) -> Self {
        Self {
            id: person.id,
            name: person.name.full,
            native_name: person.name.native,
            language: person.language,
            image: person.image.and_then(|image| image.large.or(image.medium)),
        }
    }
}

impl From<AniListGraphQlReview> for AniListReview {
    fn from(review: AniListGraphQlReview) -> Self {
        Self {
            id: review.id,
            summary: review.summary,
            rating: review.rating,
            rating_amount: review.rating_amount,
            site_url: review.site_url,
            user_name: review.user.and_then(|user| user.name),
        }
    }
}

impl AniListTitles {
    fn display_title(&self) -> Option<&str> {
        self.english
            .as_deref()
            .or(self.romaji.as_deref())
            .or(self.native.as_deref())
    }
}

// ---------------------------------------------------------------------------
// Novel lookup types
// ---------------------------------------------------------------------------

/// Light-novel metadata from AniList (compact projection for webnovels).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AniListNovelMetadata {
    /// AniList media ID.
    pub anilist_id: u32,
    /// AniList detail URL.
    pub anilist_url: Option<String>,
    /// Preferred display title (english, then romaji).
    pub title: Option<String>,
    /// Description text.
    pub description: Option<String>,
    /// AniList status string (RELEASING, FINISHED, …).
    pub status: Option<String>,
    /// Genre names.
    pub genres: Vec<String>,
    /// Tag names.
    pub tags: Vec<String>,
    /// Average community score (0-100).
    pub average_score: Option<f32>,
    /// Best available cover image URL.
    pub cover_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListNovelResponse {
    data: Option<AniListNovelData>,
}

#[derive(Debug, Deserialize)]
struct AniListNovelData {
    #[serde(rename = "Page")]
    page: Option<AniListNovelPage>,
}

#[derive(Debug, Deserialize)]
struct AniListNovelPage {
    #[serde(default)]
    media: Vec<AniListNovelRaw>,
}

#[derive(Debug, Deserialize)]
struct AniListNovelRaw {
    id: u32,
    #[serde(rename = "siteUrl")]
    site_url: Option<String>,
    title: Option<AniListNovelTitle>,
    description: Option<String>,
    status: Option<String>,
    genres: Option<Vec<String>>,
    #[serde(rename = "averageScore")]
    average_score: Option<f32>,
    tags: Option<Vec<AniListNovelTag>>,
    #[serde(rename = "coverImage")]
    cover_image: Option<AniListNovelCover>,
}

#[derive(Debug, Deserialize)]
struct AniListNovelTitle {
    romaji: Option<String>,
    english: Option<String>,
    #[allow(dead_code)]
    native: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AniListNovelTag {
    name: String,
}

#[derive(Debug, Deserialize)]
struct AniListNovelCover {
    #[serde(rename = "extraLarge")]
    extra_large: Option<String>,
    large: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adult_flag_matches_media_type() {
        assert!(AniListClient::adult_flag_for(MediaType::HentaiAnime));
        assert!(!AniListClient::adult_flag_for(MediaType::Anime));
    }

    #[test]
    fn builds_query_text() {
        let query = AniListClient::build_search_query("Naruto", false);
        assert!(query.contains("Media"));
        assert!(query.contains("Naruto"));
    }
}
