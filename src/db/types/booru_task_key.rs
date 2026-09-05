use crate::db::types::BooruFilter;
use crate::utils::duration::parse_duration_key;
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
            Some(BooruRankingMode::Interval(interval_key)) => {
                s.push_str("|i=");
                s.push_str(interval_key);
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
                    if !filter_sig.is_empty() {
                        return None;
                    }
                    filter_sig = sig.to_string();
                } else if let Some(mode) = segment.strip_prefix("o=") {
                    if ranking.is_some() {
                        return None;
                    }
                    let kind = OrderbyKind::from_str(mode)?;
                    ranking = Some(BooruRankingMode::Orderby(kind));
                } else if let Some(scale) = segment.strip_prefix("r=") {
                    if ranking.is_some() {
                        return None;
                    }
                    let s = PopularScale::from_str(scale)?;
                    ranking = Some(BooruRankingMode::Popular(s));
                } else if let Some(interval) = segment.strip_prefix("i=") {
                    if ranking.is_some() {
                        return None;
                    }
                    parse_duration_key(interval)?;
                    ranking = Some(BooruRankingMode::Interval(interval.to_string()));
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

    #[test]
    fn task_keys_encode_and_roundtrip_tag_and_ranking_modes() {
        let score = BooruFilter::new(Some(10), None, vec![]);
        let all = BooruFilter::new(Some(10), Some(5), vec![BooruRating::Safe]);
        for (key, expected) in [
            (
                BooruTaskKey::new_tag("Konachan", "cat", &BooruFilter::default()),
                "konachan:cat",
            ),
            (
                BooruTaskKey::new_tag("konachan", "cat", &score),
                "konachan:cat|f=s",
            ),
            (
                BooruTaskKey::new_tag("konachan", "cat", &all),
                "konachan:cat|f=sfr",
            ),
            (
                BooruTaskKey::new_ranking(
                    "konachan",
                    "cat",
                    BooruRankingMode::Orderby(OrderbyKind::Score),
                    &BooruFilter::default(),
                ),
                "konachan:cat|o=score",
            ),
            (
                BooruTaskKey::new_ranking(
                    "konachan",
                    "cat",
                    BooruRankingMode::Orderby(OrderbyKind::Score),
                    &score,
                ),
                "konachan:cat|o=score|f=s",
            ),
            (
                BooruTaskKey::new_ranking(
                    "konachan",
                    "",
                    BooruRankingMode::Popular(PopularScale::Day),
                    &BooruFilter::default(),
                ),
                "konachan:|r=day",
            ),
            (
                BooruTaskKey::new_ranking(
                    "danbooru",
                    "1girl",
                    BooruRankingMode::Popular(PopularScale::Week),
                    &all,
                ),
                "danbooru:1girl|r=week|f=sfr",
            ),
            (
                BooruTaskKey::new_ranking(
                    "konachan",
                    "landscape",
                    BooruRankingMode::Interval("3600s".into()),
                    &BooruFilter::default(),
                ),
                "konachan:landscape|i=3600s",
            ),
        ] {
            assert_eq!(key.to_task_value(), expected);
            assert_eq!(BooruTaskKey::parse(expected), Some(key), "{expected}");
        }
    }

    #[test]
    fn task_sharing_depends_on_filter_kinds_not_thresholds() {
        for (filter, expected) in [
            (BooruFilter::new(Some(10), None, vec![]), "konachan:cat|f=s"),
            (BooruFilter::new(Some(50), None, vec![]), "konachan:cat|f=s"),
            (BooruFilter::new(None, Some(10), vec![]), "konachan:cat|f=f"),
        ] {
            assert_eq!(
                BooruTaskKey::new_tag("konachan", "cat", &filter).to_task_value(),
                expected
            );
        }
    }

    #[test]
    fn parse_rejects_malformed_or_ambiguous_task_keys() {
        for value in [
            "nocolon",
            "site:tags|unknown=value",
            "site:tags|o=invalidmode",
            "site:|i=abc",
            "site:|i=0s",
            "site:|i=1h",
            "site:|i=60m",
            "site:|o=score|r=day",
            "site:|o=score|o=fav",
            "site:|i=3600s|r=day",
            "site:|r=day|i=3600s",
            "site:tags|f=s|f=r",
        ] {
            assert!(BooruTaskKey::parse(value).is_none(), "{value}");
        }
    }
}
