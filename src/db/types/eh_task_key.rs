use super::eh_filter::EhFilter;

/// Task key encoding for e-hentai subscriptions.
///
/// Format: `eh:{query}|c={bitmask}|f={filter_sig}`
/// - `c=` segment omitted when bitmask is 0
/// - `f=` segment omitted when filter_sig is empty
/// - `c=` and `f=` always in fixed order (c before f)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EhTaskKey {
    pub query: String,
    pub category_bitmask: u32,
    pub filter_sig: String,
}

impl EhTaskKey {
    pub fn new(query: &str, category_bitmask: u32, filter: &EhFilter) -> Self {
        Self {
            query: query.to_string(),
            category_bitmask,
            filter_sig: filter.task_value_signature(),
        }
    }

    pub fn to_task_value(&self) -> String {
        let mut value = format!("eh:{}", self.query);
        if self.category_bitmask != 0 {
            value.push_str(&format!("|c={}", self.category_bitmask));
        }
        if !self.filter_sig.is_empty() {
            value.push_str(&format!("|f={}", self.filter_sig));
        }
        value
    }

    pub fn parse(value: &str) -> Option<Self> {
        let (head, rest) = value.split_once('|').unwrap_or((value, ""));
        let (prefix, query) = head.split_once(':')?;
        if prefix != "eh" {
            return None;
        }

        let mut category_bitmask: Option<u32> = None;
        let mut filter_sig: Option<String> = None;

        if !rest.is_empty() {
            for segment in rest.split('|') {
                if let Some(c) = segment.strip_prefix("c=") {
                    if category_bitmask.is_some() {
                        return None; // duplicate
                    }
                    category_bitmask = Some(c.parse::<u32>().ok()?);
                } else if let Some(f) = segment.strip_prefix("f=") {
                    if filter_sig.is_some() {
                        return None; // duplicate
                    }
                    filter_sig = Some(f.to_string());
                } else {
                    return None; // unknown segment
                }
            }
        }

        Some(Self {
            query: query.to_string(),
            category_bitmask: category_bitmask.unwrap_or(0),
            filter_sig: filter_sig.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_filter() {
        let filter = EhFilter::new();
        let key = EhTaskKey::new("female:elf", 0, &filter);
        assert_eq!(key.to_task_value(), "eh:female:elf");
    }

    #[test]
    fn test_with_cats() {
        let filter = EhFilter::new();
        let key = EhTaskKey::new("artist:wlop", 3, &filter);
        assert_eq!(key.to_task_value(), "eh:artist:wlop|c=3");
    }

    #[test]
    fn test_with_filter() {
        let filter = EhFilter {
            min_rating: Some(4),
            ..Default::default()
        };
        let key = EhTaskKey::new("female:elf", 0, &filter);
        assert_eq!(key.to_task_value(), "eh:female:elf|f=r4");
    }

    #[test]
    fn test_with_cats_and_filter() {
        let filter = EhFilter {
            min_rating: Some(4),
            min_pages: Some(20),
            ..Default::default()
        };
        let key = EhTaskKey::new("parody:touhou", 3, &filter);
        assert_eq!(key.to_task_value(), "eh:parody:touhou|c=3|f=r4p20");
    }

    #[test]
    fn test_roundtrip() {
        let filter = EhFilter {
            min_rating: Some(3),
            min_pages: Some(10),
            max_pages: Some(200),
            ..Default::default()
        };
        let key = EhTaskKey::new("female:elf cat:2", 7, &filter);
        let value = key.to_task_value();
        let parsed = EhTaskKey::parse(&value).unwrap();
        assert_eq!(parsed.query, "female:elf cat:2");
        assert_eq!(parsed.category_bitmask, 7);
        assert_eq!(parsed.filter_sig, "r3p10P200");
    }

    #[test]
    fn test_parse_invalid_prefix() {
        assert!(EhTaskKey::parse("booru:konachan:cat").is_none());
        assert!(EhTaskKey::parse("no_colon").is_none());
    }

    #[test]
    fn test_parse_duplicate_c() {
        let value = "eh:female:elf|c=1|c=2";
        assert!(EhTaskKey::parse(value).is_none());
    }

    #[test]
    fn test_parse_unknown_segment() {
        let value = "eh:female:elf|x=1";
        assert!(EhTaskKey::parse(value).is_none());
    }
}
