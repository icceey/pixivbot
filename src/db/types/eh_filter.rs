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
