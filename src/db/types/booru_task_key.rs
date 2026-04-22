use crate::db::types::BooruFilter;
pub use booru_client::PopularScale;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BooruRankingMode {
    Orderby(OrderbyKind),
    Popular(PopularScale),
    Interval(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderbyKind {
    Score,
    Fav,
    Random,
}

impl OrderbyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderbyKind::Score => "score",
            OrderbyKind::Fav => "fav",
            OrderbyKind::Random => "random",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "score" => Some(OrderbyKind::Score),
            "fav" | "favcount" => Some(OrderbyKind::Fav),
            "random" => Some(OrderbyKind::Random),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BooruTaskKey {
    pub site: String,
    pub tags: String,
    pub filter_sig: String,
    pub ranking: Option<BooruRankingMode>,
}

impl BooruTaskKey {
    pub fn new_tag(site: &str, tags: &str, filter: &BooruFilter) -> Self {
        Self {
            site: site.to_lowercase(),
            tags: tags.to_string(),
            filter_sig: filter.task_value_signature(),
            ranking: None,
        }
    }

    pub fn new_ranking(
        site: &str,
        tags: &str,
        mode: BooruRankingMode,
        filter: &BooruFilter,
    ) -> Self {
        Self {
            site: site.to_lowercase(),
            tags: tags.to_string(),
            filter_sig: filter.task_value_signature(),
            ranking: Some(mode),
        }
    }

    pub fn to_task_value(&self) -> String {
        let mut s = format!("{}:{}", self.site, self.tags);
        match &self.ranking {
            None => {}
            Some(BooruRankingMode::Orderby(mode)) => {
                s.push_str("|o=");
                s.push_str(mode.as_str());
            }
            Some(BooruRankingMode::Popular(scale)) => {
                s.push_str("|r=");
                s.push_str(scale.as_str());
            }
            Some(BooruRankingMode::Interval(iso)) => {
                s.push_str("|i=");
                s.push_str(iso);
            }
        }
        if !self.filter_sig.is_empty() {
            s.push_str("|f=");
            s.push_str(&self.filter_sig);
        }
        s
    }

    pub fn parse(value: &str) -> Option<Self> {
        let (head, rest) = value.split_once('|').unwrap_or((value, ""));
        let (site, tags) = head.split_once(':')?;

        let mut filter_sig = String::new();
        let mut ranking: Option<BooruRankingMode> = None;

        if !rest.is_empty() {
            for segment in rest.split('|') {
                if let Some(sig) = segment.strip_prefix("f=") {
                    filter_sig = sig.to_string();
                } else if let Some(mode) = segment.strip_prefix("o=") {
                    let kind = OrderbyKind::from_str(mode)?;
                    ranking = Some(BooruRankingMode::Orderby(kind));
                } else if let Some(scale) = segment.strip_prefix("r=") {
                    let s = PopularScale::from_str(scale)?;
                    ranking = Some(BooruRankingMode::Popular(s));
                } else if let Some(iso) = segment.strip_prefix("i=") {
                    ranking = Some(BooruRankingMode::Interval(iso.to_string()));
                } else {
                    return None;
                }
            }
        }

        Some(Self {
            site: site.to_string(),
            tags: tags.to_string(),
            filter_sig,
            ranking,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use booru_client::BooruRating;

    fn filter_score() -> BooruFilter {
        BooruFilter::new(Some(10), None, vec![])
    }

    fn filter_all() -> BooruFilter {
        BooruFilter::new(Some(10), Some(5), vec![BooruRating::Safe])
    }

    #[test]
    fn tag_no_filter_plain_format() {
        let k = BooruTaskKey::new_tag("Konachan", "cat", &BooruFilter::default());
        assert_eq!(k.to_task_value(), "konachan:cat");
    }

    #[test]
    fn tag_with_score_filter_adds_sig() {
        let k = BooruTaskKey::new_tag("konachan", "cat", &filter_score());
        assert_eq!(k.to_task_value(), "konachan:cat|f=s");
    }

    #[test]
    fn tag_with_all_filters_sorted_sig() {
        let k = BooruTaskKey::new_tag("konachan", "cat", &filter_all());
        assert_eq!(k.to_task_value(), "konachan:cat|f=sfr");
    }

    #[test]
    fn same_filter_keys_different_values_produce_same_task_value() {
        let f1 = BooruFilter::new(Some(10), None, vec![]);
        let f2 = BooruFilter::new(Some(50), None, vec![]);
        let k1 = BooruTaskKey::new_tag("konachan", "cat", &f1);
        let k2 = BooruTaskKey::new_tag("konachan", "cat", &f2);
        assert_eq!(k1.to_task_value(), k2.to_task_value());
    }

    #[test]
    fn different_filter_keys_produce_different_task_values() {
        let f_score = BooruFilter::new(Some(10), None, vec![]);
        let f_fav = BooruFilter::new(None, Some(10), vec![]);
        let k1 = BooruTaskKey::new_tag("konachan", "cat", &f_score);
        let k2 = BooruTaskKey::new_tag("konachan", "cat", &f_fav);
        assert_ne!(k1.to_task_value(), k2.to_task_value());
    }

    #[test]
    fn ranking_orderby_score_format() {
        let k = BooruTaskKey::new_ranking(
            "konachan",
            "cat",
            BooruRankingMode::Orderby(OrderbyKind::Score),
            &BooruFilter::default(),
        );
        assert_eq!(k.to_task_value(), "konachan:cat|o=score");
    }

    #[test]
    fn ranking_popular_day_format() {
        let k = BooruTaskKey::new_ranking(
            "konachan",
            "",
            BooruRankingMode::Popular(PopularScale::Day),
            &BooruFilter::default(),
        );
        assert_eq!(k.to_task_value(), "konachan:|r=day");
    }

    #[test]
    fn ranking_interval_format() {
        let k = BooruTaskKey::new_ranking(
            "konachan",
            "landscape",
            BooruRankingMode::Interval("PT1H".into()),
            &BooruFilter::default(),
        );
        assert_eq!(k.to_task_value(), "konachan:landscape|i=PT1H");
    }

    #[test]
    fn ranking_with_filter_combines() {
        let k = BooruTaskKey::new_ranking(
            "konachan",
            "cat",
            BooruRankingMode::Orderby(OrderbyKind::Score),
            &filter_score(),
        );
        assert_eq!(k.to_task_value(), "konachan:cat|o=score|f=s");
    }

    #[test]
    fn parse_plain_tag() {
        let k = BooruTaskKey::parse("konachan:cat").unwrap();
        assert_eq!(k.site, "konachan");
        assert_eq!(k.tags, "cat");
        assert_eq!(k.filter_sig, "");
        assert!(k.ranking.is_none());
    }

    #[test]
    fn parse_tag_with_filter() {
        let k = BooruTaskKey::parse("konachan:cat|f=sfr").unwrap();
        assert_eq!(k.filter_sig, "sfr");
        assert!(k.ranking.is_none());
    }

    #[test]
    fn parse_ranking_orderby_with_filter() {
        let k = BooruTaskKey::parse("konachan:cat|o=score|f=s").unwrap();
        assert_eq!(k.tags, "cat");
        assert_eq!(k.filter_sig, "s");
        matches!(
            k.ranking,
            Some(BooruRankingMode::Orderby(OrderbyKind::Score))
        );
    }

    #[test]
    fn parse_ranking_interval() {
        let k = BooruTaskKey::parse("konachan:|i=PT30M").unwrap();
        assert!(matches!(k.ranking, Some(BooruRankingMode::Interval(ref s)) if s == "PT30M"));
    }

    #[test]
    fn parse_roundtrip_preserves_all_fields() {
        let original = BooruTaskKey::new_ranking(
            "danbooru",
            "1girl",
            BooruRankingMode::Popular(PopularScale::Week),
            &filter_all(),
        );
        let parsed = BooruTaskKey::parse(&original.to_task_value()).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_invalid_format_returns_none() {
        assert!(BooruTaskKey::parse("nocolon").is_none());
        assert!(BooruTaskKey::parse("site:tags|unknown=value").is_none());
        assert!(BooruTaskKey::parse("site:tags|o=invalidmode").is_none());
    }
}
