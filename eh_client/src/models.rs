use serde::{Deserialize, Serialize};

/// Cookies for e-hentai/exhentai authentication.
#[derive(Debug, Clone, Default)]
pub struct EhCookies {
    pub ipb_member_id: Option<String>,
    pub ipb_pass_hash: Option<String>,
    pub igneous: Option<String>,
    pub nw: bool,
}

impl EhCookies {
    /// Build a Cookie header value string.
    pub fn to_header(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref id) = self.ipb_member_id {
            parts.push(format!("ipb_member_id={id}"));
        }
        if let Some(ref hash) = self.ipb_pass_hash {
            parts.push(format!("ipb_pass_hash={hash}"));
        }
        if let Some(ref ig) = self.igneous {
            parts.push(format!("igneous={ig}"));
        }
        if self.nw {
            parts.push("nw=1".to_string());
        }
        parts.join("; ")
    }

    /// True if this is an exhentai-capable cookie set (all three required).
    pub fn is_exhentai_capable(&self) -> bool {
        self.ipb_member_id.is_some() && self.ipb_pass_hash.is_some() && self.igneous.is_some()
    }
}

/// A gallery reference parsed from search HTML results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EhGalleryRef {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub url: String,
    pub posted_ts: i64,
}

/// Full gallery metadata from the api.php JSON endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EhGallery {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub title_jpn: Option<String>,
    pub category: String,
    pub thumb: String,
    pub uploader: String,
    pub posted: i64,
    pub filecount: u32,
    pub filesize: u64,
    pub expunged: bool,
    pub rating: f64,
    pub tags: Vec<String>,
}

/// E-hentai gallery categories with their bitmask values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EhCategory {
    Doujinshi = 1,
    Manga = 2,
    ArtistCG = 4,
    GameCG = 8,
    Western = 16,
    NonH = 32,
    ImageSet = 64,
    Cosplay = 128,
    AsianPorn = 256,
    Misc = 512,
}

impl EhCategory {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "doujinshi" => Some(Self::Doujinshi),
            "manga" => Some(Self::Manga),
            "artistcg" | "artist cg" | "artist_cg" => Some(Self::ArtistCG),
            "gamecg" | "game cg" | "game_cg" => Some(Self::GameCG),
            "western" => Some(Self::Western),
            "nonh" | "non-h" | "non_h" => Some(Self::NonH),
            "imageset" | "image set" | "image_set" => Some(Self::ImageSet),
            "cosplay" => Some(Self::Cosplay),
            "asianporn" | "asian porn" | "asian_porn" => Some(Self::AsianPorn),
            "misc" => Some(Self::Misc),
            _ => None,
        }
    }

    /// Parse a comma-separated list of category names into a bitmask.
    pub fn bitmask_from_str(s: &str) -> u32 {
        s.split(',')
            .filter_map(|c| Self::parse_str(c.trim()))
            .map(|c| c as u32)
            .sum()
    }
}

/// Raw API response structures (internal).
#[derive(Debug, Deserialize)]
pub(crate) struct RawApiResponse {
    pub gmetadata: Vec<RawGalleryMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) struct RawGalleryMeta {
    pub gid: u64,
    pub token: String,
    pub title: String,
    #[serde(default)]
    pub title_jpn: Option<String>,
    pub category: String,
    pub thumb: String,
    pub uploader: String,
    pub posted: String,
    pub filecount: String,
    pub filesize: u64,
    #[serde(default)]
    pub expunged: bool,
    pub rating: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl RawGalleryMeta {
    pub fn into_gallery(self) -> EhGallery {
        let posted = self.posted.parse::<i64>().unwrap_or(0);
        let filecount = self.filecount.parse::<u32>().unwrap_or(0);
        let rating = self.rating.parse::<f64>().unwrap_or(0.0);
        EhGallery {
            gid: self.gid,
            token: self.token,
            title: self.title,
            title_jpn: self.title_jpn,
            category: self.category,
            thumb: self.thumb,
            uploader: self.uploader,
            posted,
            filecount,
            filesize: self.filesize,
            expunged: self.expunged,
            rating,
            tags: self.tags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cookie_header() {
        let cookies = EhCookies {
            ipb_member_id: Some("12345".into()),
            ipb_pass_hash: Some("abcdef".into()),
            igneous: Some("xyz".into()),
            nw: true,
        };
        let header = cookies.to_header();
        assert!(header.contains("ipb_member_id=12345"));
        assert!(header.contains("ipb_pass_hash=abcdef"));
        assert!(header.contains("igneous=xyz"));
        assert!(header.contains("nw=1"));
    }

    #[test]
    fn test_cookie_exhentai_capable() {
        let full = EhCookies {
            ipb_member_id: Some("1".into()),
            ipb_pass_hash: Some("h".into()),
            igneous: Some("i".into()),
            nw: true,
        };
        assert!(full.is_exhentai_capable());

        let partial = EhCookies {
            ipb_member_id: Some("1".into()),
            ipb_pass_hash: None,
            igneous: None,
            nw: true,
        };
        assert!(!partial.is_exhentai_capable());
    }

    #[test]
    fn test_category_bitmask() {
        assert_eq!(EhCategory::bitmask_from_str("doujinshi,manga"), 3);
        assert_eq!(EhCategory::bitmask_from_str("doujinshi"), 1);
        assert_eq!(EhCategory::bitmask_from_str("all"), 0); // unknown = 0
    }

    #[test]
    fn test_raw_meta_into_gallery() {
        let raw = RawGalleryMeta {
            gid: 123,
            token: "abc".into(),
            title: "Test".into(),
            title_jpn: Some("テスト".into()),
            category: "Manga".into(),
            thumb: "https://ehgt.org/t.jpg".into(),
            uploader: "user".into(),
            posted: "1376143500".into(),
            filecount: "20".into(),
            filesize: 51210504,
            expunged: false,
            rating: "4.64".into(),
            tags: vec!["parody:touhou".into()],
        };
        let g = raw.into_gallery();
        assert_eq!(g.gid, 123);
        assert_eq!(g.posted, 1376143500);
        assert_eq!(g.filecount, 20);
        assert!((g.rating - 4.64).abs() < 0.001);
    }
}
