use eh_client::EhGallery;
use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};

/// Filter criteria for e-hentai subscriptions.
///
/// `telegraph` is NOT part of `task_value_signature` — it is a per-subscription
/// delivery preference, not a filter that changes which galleries are fetched.
/// This means two subscriptions with the same query + rating filter but different
/// telegraph settings share the same task (and thus the same search poll).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
pub struct EhFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u32>,
    #[serde(default)]
    pub telegraph: bool,
}

impl EhFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no filtering criteria are set (telegraph is a delivery preference,
    /// but `telegraph=true` is treated as non-empty to preserve telegraph-only preferences).
    pub fn is_empty(&self) -> bool {
        self.min_rating.is_none()
            && self.min_pages.is_none()
            && self.max_pages.is_none()
            && !self.telegraph
    }

    /// Task-value filter-key signature using value-encoding (not just presence).
    ///
    /// Format: `r{rating}p{min_pages}P{max_pages}` (fixed order).
    /// Returns `""` when no filter is set.
    ///
    /// The order is fixed and persisted in `task_value`, so existing rows depend
    /// on it. Do not reorder.
    pub fn task_value_signature(&self) -> String {
        let mut sig = String::new();
        if let Some(r) = self.min_rating {
            sig.push_str(&format!("r{r}"));
        }
        if let Some(p) = self.min_pages {
            sig.push_str(&format!("p{p}"));
        }
        if let Some(p) = self.max_pages {
            sig.push_str(&format!("P{p}"));
        }
        sig
    }

    /// True when a minimum-rating filter is set, which triggers 48h scan mode.
    pub fn has_rating_filter(&self) -> bool {
        self.min_rating.is_some()
    }

    /// Check if a gallery matches all filter criteria.
    pub fn matches(&self, gallery: &EhGallery) -> bool {
        if let Some(min_rating) = self.min_rating {
            if gallery.rating < min_rating as f64 {
                return false;
            }
        }
        if let Some(min_pages) = self.min_pages {
            if gallery.filecount < min_pages {
                return false;
            }
        }
        if let Some(max_pages) = self.max_pages {
            if gallery.filecount > max_pages {
                return false;
            }
        }
        true
    }

    /// Aggregate multiple filters into the loosest one (most permissive).
    ///
    /// Takes the minimum `min_rating`, minimum `min_pages`, maximum `max_pages`,
    /// and `telegraph = true` if ANY subscription has it enabled.
    pub fn aggregate(filters: &[Option<&EhFilter>]) -> EhFilter {
        if filters.is_empty() || filters.iter().any(|f| f.is_none()) {
            return EhFilter::default();
        }

        let filters: Vec<&EhFilter> = filters.iter().filter_map(|f| *f).collect();

        let min_rating = if filters.iter().all(|f| f.min_rating.is_some()) {
            filters.iter().filter_map(|f| f.min_rating).min()
        } else {
            None
        };

        let min_pages = if filters.iter().all(|f| f.min_pages.is_some()) {
            filters.iter().filter_map(|f| f.min_pages).min()
        } else {
            None
        };

        let max_pages = if filters.iter().all(|f| f.max_pages.is_some()) {
            filters.iter().filter_map(|f| f.max_pages).max()
        } else {
            None
        };

        let telegraph = filters.iter().any(|f| f.telegraph);

        EhFilter {
            min_rating,
            min_pages,
            max_pages,
            telegraph,
        }
    }

    pub fn format_for_display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(rating) = self.min_rating {
            parts.push(format!("rating≥{rating}"));
        }
        if let Some(pages) = self.min_pages {
            parts.push(format!("pages≥{pages}"));
        }
        if let Some(pages) = self.max_pages {
            parts.push(format!("pages≤{pages}"));
        }
        if self.telegraph {
            parts.push("telegraph=on".to_string());
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eh_filter_empty() {
        let f = EhFilter::new();
        assert!(f.is_empty());
        assert!(!f.has_rating_filter());
        assert!(f.task_value_signature().is_empty());
    }

    #[test]
    fn test_eh_filter_signature() {
        let f = EhFilter {
            min_rating: Some(4),
            min_pages: None,
            max_pages: None,
            telegraph: false,
        };
        assert_eq!(f.task_value_signature(), "r4");

        let f = EhFilter {
            min_rating: Some(4),
            min_pages: Some(20),
            max_pages: None,
            telegraph: true,
        };
        assert_eq!(f.task_value_signature(), "r4p20");

        let f = EhFilter {
            min_rating: None,
            min_pages: None,
            max_pages: Some(500),
            telegraph: false,
        };
        assert_eq!(f.task_value_signature(), "P500");

        let f = EhFilter {
            min_rating: Some(3),
            min_pages: Some(10),
            max_pages: Some(200),
            telegraph: false,
        };
        assert_eq!(f.task_value_signature(), "r3p10P200");
    }

    #[test]
    fn test_eh_filter_has_rating() {
        let f = EhFilter {
            min_rating: Some(2),
            ..Default::default()
        };
        assert!(f.has_rating_filter());

        let f = EhFilter {
            min_pages: Some(10),
            ..Default::default()
        };
        assert!(!f.has_rating_filter());
    }

    #[test]
    fn test_eh_filter_matches_rating() {
        let gallery = EhGallery {
            gid: 1,
            token: "abc".into(),
            title: "Test".into(),
            title_jpn: None,
            category: "Manga".into(),
            thumb: "".into(),
            uploader: "user".into(),
            posted: 1000,
            filecount: 20,
            filesize: 1000,
            expunged: false,
            rating: 4.5,
            tags: vec![],
        };

        let f = EhFilter {
            min_rating: Some(4),
            ..Default::default()
        };
        assert!(f.matches(&gallery));

        let f = EhFilter {
            min_rating: Some(5),
            ..Default::default()
        };
        assert!(!f.matches(&gallery));
    }

    #[test]
    fn test_eh_filter_matches_pages() {
        let gallery = EhGallery {
            gid: 1,
            token: "abc".into(),
            title: "Test".into(),
            title_jpn: None,
            category: "Manga".into(),
            thumb: "".into(),
            uploader: "user".into(),
            posted: 1000,
            filecount: 20,
            filesize: 1000,
            expunged: false,
            rating: 4.5,
            tags: vec![],
        };

        let f = EhFilter {
            min_pages: Some(10),
            ..Default::default()
        };
        assert!(f.matches(&gallery));

        let f = EhFilter {
            min_pages: Some(30),
            ..Default::default()
        };
        assert!(!f.matches(&gallery));

        let f = EhFilter {
            max_pages: Some(50),
            ..Default::default()
        };
        assert!(f.matches(&gallery));

        let f = EhFilter {
            max_pages: Some(10),
            ..Default::default()
        };
        assert!(!f.matches(&gallery));
    }

    #[test]
    fn test_eh_filter_aggregate() {
        let f1 = EhFilter {
            min_rating: Some(4),
            min_pages: Some(20),
            max_pages: Some(500),
            telegraph: false,
        };
        let f2 = EhFilter {
            min_rating: Some(3),
            min_pages: Some(10),
            max_pages: Some(1000),
            telegraph: true,
        };

        let agg = EhFilter::aggregate(&[Some(&f1), Some(&f2)]);
        assert_eq!(agg.min_rating, Some(3));
        assert_eq!(agg.min_pages, Some(10));
        assert_eq!(agg.max_pages, Some(1000));
        assert!(agg.telegraph);
    }

    #[test]
    fn test_eh_filter_aggregate_with_none() {
        let f1 = EhFilter {
            min_rating: Some(4),
            ..Default::default()
        };
        let agg = EhFilter::aggregate(&[Some(&f1), None]);
        assert!(agg.is_empty());
    }

    #[test]
    fn test_eh_filter_format_for_display() {
        let f = EhFilter {
            min_rating: Some(4),
            min_pages: Some(20),
            max_pages: None,
            telegraph: true,
        };
        let display = f.format_for_display();
        assert!(display.contains("rating≥4"));
        assert!(display.contains("pages≥20"));
        assert!(display.contains("telegraph=on"));
    }

    #[test]
    fn test_telegraph_only_filter_is_not_empty() {
        let filter = EhFilter {
            telegraph: true,
            ..Default::default()
        };
        assert!(!filter.is_empty());
    }
}
