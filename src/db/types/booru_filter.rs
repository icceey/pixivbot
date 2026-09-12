use booru_client::{BooruEngineType, BooruRating};
use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
pub struct BooruFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_min: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fav_count_min: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ratings: Vec<BooruRating>,
}

impl BooruFilter {
    pub fn new(
        score_min: Option<i32>,
        fav_count_min: Option<i32>,
        allowed_ratings: Vec<BooruRating>,
    ) -> Self {
        Self {
            score_min,
            fav_count_min,
            allowed_ratings,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.score_min.is_none() && self.fav_count_min.is_none() && self.allowed_ratings.is_empty()
    }

    /// Task-value filter-key signature: a fixed-order subset of `s`,`f`,`r`
    /// (in that order) marking which keys are SET. Used to dedup
    /// subscriptions by filter-KEY set (not filter-VALUES): `score>=10` and
    /// `score>=50` share one task; `score>=10` and `fav>=5` are separate
    /// tasks. Returns `""` when no filter is set.
    ///
    /// The order is fixed (not alphabetical) and persisted in `task_value`,
    /// so existing rows depend on it. Do not reorder.
    pub fn task_value_signature(&self) -> String {
        let mut sig = String::with_capacity(3);
        if self.score_min.is_some() {
            sig.push('s');
        }
        if self.fav_count_min.is_some() {
            sig.push('f');
        }
        if !self.allowed_ratings.is_empty() {
            sig.push('r');
        }
        sig
    }

    pub fn matches_for_engine(
        &self,
        score: i32,
        fav_count: i32,
        rating: &BooruRating,
        engine_type: BooruEngineType,
    ) -> bool {
        if let Some(min) = self.score_min {
            if score < min {
                return false;
            }
        }
        if let Some(min) = self.fav_count_min {
            if engine_type.supports_fav_count() && fav_count < min {
                return false;
            }
        }
        if !self.allowed_ratings.is_empty() && !self.allowed_ratings.contains(rating) {
            return false;
        }
        true
    }

    pub fn format_for_display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(score) = self.score_min {
            parts.push(format!("score≥{}", score));
        }
        if let Some(fav) = self.fav_count_min {
            parts.push(format!("fav≥{}", fav));
        }
        if !self.allowed_ratings.is_empty() {
            let ratings: Vec<&str> = self
                .allowed_ratings
                .iter()
                .map(|r| r.as_short_str())
                .collect();
            parts.push(format!("rating={}", ratings.join(",")));
        }
        parts.join(" ")
    }

    pub fn aggregate(filters: &[Option<&BooruFilter>]) -> BooruFilter {
        if filters.is_empty() || filters.iter().any(|f| f.is_none()) {
            return BooruFilter::default();
        }

        let filters: Vec<&BooruFilter> = filters.iter().filter_map(|f| *f).collect();

        let score_min = if filters.iter().all(|f| f.score_min.is_some()) {
            filters.iter().filter_map(|f| f.score_min).min()
        } else {
            None
        };

        let fav_count_min = if filters.iter().all(|f| f.fav_count_min.is_some()) {
            filters.iter().filter_map(|f| f.fav_count_min).min()
        } else {
            None
        };

        let allowed_ratings = if filters.iter().any(|f| f.allowed_ratings.is_empty()) {
            Vec::new()
        } else {
            let mut union: Vec<BooruRating> = Vec::new();
            for f in &filters {
                for r in &f.allowed_ratings {
                    if !union.contains(r) {
                        union.push(*r);
                    }
                }
            }
            union
        };

        BooruFilter {
            score_min,
            fav_count_min,
            allowed_ratings,
        }
    }

    pub fn to_api_tags(&self, engine_type: BooruEngineType) -> Vec<String> {
        let mut tags = Vec::new();

        if let Some(score) = self.score_min {
            tags.push(format!("score:>={}", score));
        }

        if let Some(fav) = self.fav_count_min {
            if engine_type.supports_fav_count() {
                tags.push(format!("favcount:>={}", fav));
            }
        }

        if self.allowed_ratings.len() == 1 {
            let rating = &self.allowed_ratings[0];
            tags.push(format!("rating:{}", rating.as_api_str(engine_type)));
        } else if !self.allowed_ratings.is_empty() {
            // For multi-rating: send exclusion tags for ratings NOT in the allowed set.
            // Compare by API string (not enum variant) since Safe and General may map to same value.
            let all_ratings = [
                BooruRating::General,
                BooruRating::Safe,
                BooruRating::Sensitive,
                BooruRating::Questionable,
                BooruRating::Explicit,
            ];
            let allowed_api_strs: Vec<&str> = self
                .allowed_ratings
                .iter()
                .map(|r| r.as_api_str(engine_type))
                .collect();
            let mut excluded_api_strs: Vec<&str> = Vec::new();
            for r in &all_ratings {
                let api_str = r.as_api_str(engine_type);
                if !allowed_api_strs.contains(&api_str) && !excluded_api_strs.contains(&api_str) {
                    excluded_api_strs.push(api_str);
                }
            }
            // Only send exclusions when there are few enough to be practical
            // (avoids consuming too many tag slots on sites with tag limits)
            if !excluded_api_strs.is_empty() && excluded_api_strs.len() <= 2 {
                for api_str in excluded_api_strs {
                    tags.push(format!("-rating:{}", api_str));
                }
            }
        }

        tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_for_engine_ignores_only_unsupported_fav_count() {
        let filter = BooruFilter {
            score_min: Some(10),
            fav_count_min: Some(5),
            allowed_ratings: vec![BooruRating::Safe],
        };

        assert!(filter.matches_for_engine(10, 0, &BooruRating::Safe, BooruEngineType::Moebooru));
        assert!(!filter.matches_for_engine(10, 0, &BooruRating::Safe, BooruEngineType::Danbooru));
        assert!(!filter.matches_for_engine(5, 100, &BooruRating::Safe, BooruEngineType::Moebooru));
        assert!(!filter.matches_for_engine(
            10,
            100,
            &BooruRating::Explicit,
            BooruEngineType::Moebooru
        ));
    }

    #[test]
    fn test_to_api_tags_multi_rating_exclusion() {
        // Safe + Questionable on Danbooru: Safe→"g", Questionable→"q"
        // Excluded: Sensitive→"s", Explicit→"e" (2 exclusions, sent as -rating:)
        let filter = BooruFilter::new(
            None,
            None,
            vec![BooruRating::Safe, BooruRating::Questionable],
        );
        let tags = filter.to_api_tags(BooruEngineType::Danbooru);
        assert!(tags.contains(&"-rating:s".to_string()));
        assert!(tags.contains(&"-rating:e".to_string()));
    }

    #[test]
    fn test_to_api_tags_multi_rating_too_many_exclusions_skipped() {
        // Only Safe on Danbooru means excluded = [s, q, e] (3 exclusions, too many)
        // But wait, Safe alone is len()==1, handled by the single-rating path.
        // Test with General + Safe (both map to "g"), excluded = [s, q, e] = 3, skipped
        let filter = BooruFilter::new(None, None, vec![BooruRating::General, BooruRating::Safe]);
        let tags = filter.to_api_tags(BooruEngineType::Danbooru);
        assert!(!tags.iter().any(|t| t.contains("rating")));
    }
}
