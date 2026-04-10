use booru_client::BooruRating;
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

    pub fn matches(&self, score: i32, fav_count: i32, rating: &BooruRating) -> bool {
        if let Some(min) = self.score_min {
            if score < min {
                return false;
            }
        }
        if let Some(min) = self.fav_count_min {
            if fav_count < min {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_filter_matches_all() {
        let filter = BooruFilter::default();
        assert!(filter.is_empty());
        assert!(filter.matches(0, 0, &BooruRating::Explicit));
    }

    #[test]
    fn test_score_filter() {
        let filter = BooruFilter {
            score_min: Some(10),
            ..Default::default()
        };
        assert!(!filter.matches(5, 0, &BooruRating::Safe));
        assert!(filter.matches(10, 0, &BooruRating::Safe));
        assert!(filter.matches(20, 0, &BooruRating::Safe));
    }

    #[test]
    fn test_fav_count_filter() {
        let filter = BooruFilter {
            fav_count_min: Some(5),
            ..Default::default()
        };
        assert!(!filter.matches(0, 3, &BooruRating::Safe));
        assert!(filter.matches(0, 5, &BooruRating::Safe));
    }

    #[test]
    fn test_rating_filter() {
        let filter = BooruFilter {
            allowed_ratings: vec![BooruRating::Safe, BooruRating::General],
            ..Default::default()
        };
        assert!(filter.matches(0, 0, &BooruRating::Safe));
        assert!(filter.matches(0, 0, &BooruRating::General));
        assert!(!filter.matches(0, 0, &BooruRating::Explicit));
    }

    #[test]
    fn test_combined_filter() {
        let filter = BooruFilter {
            score_min: Some(10),
            fav_count_min: Some(5),
            allowed_ratings: vec![BooruRating::Safe],
        };
        assert!(!filter.matches(5, 10, &BooruRating::Safe));
        assert!(!filter.matches(10, 3, &BooruRating::Safe));
        assert!(!filter.matches(10, 5, &BooruRating::Explicit));
        assert!(filter.matches(10, 5, &BooruRating::Safe));
    }

    #[test]
    fn test_serde_roundtrip() {
        let filter = BooruFilter {
            score_min: Some(10),
            fav_count_min: None,
            allowed_ratings: vec![BooruRating::Safe],
        };
        let json = serde_json::to_string(&filter).unwrap();
        let restored: BooruFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(filter, restored);
    }
}
