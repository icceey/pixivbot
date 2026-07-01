use super::eh_filter::EhFilter;

/// Task key encoding for e-hentai subscriptions.
///
/// Format: `eh:{query}|c={bitmask}|f={filter_sig}` for legacy/raw queries or
/// `ehq:{encoded_query}|c={bitmask}|f={filter_sig}` when query escaping is
/// needed.
/// - encoded queries escape `%` and `|` as `%25` and `%7C` so user input cannot
///   be reinterpreted as task metadata segments while legacy unescaped task
///   values remain stable
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
        let encoded_query = encode_query(&self.query);
        let prefix = if encoded_query == self.query {
            "eh"
        } else {
            "ehq"
        };
        let mut value = format!("{prefix}:{encoded_query}");
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
        if prefix != "eh" && prefix != "ehq" {
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

        let query = if prefix == "ehq" {
            decode_query(query)
        } else {
            query.to_string()
        };

        Some(Self {
            query,
            category_bitmask: category_bitmask.unwrap_or(0),
            filter_sig: filter_sig.unwrap_or_default(),
        })
    }
}

fn encode_query(query: &str) -> String {
    if query.contains(['%', '|', '~']) {
        query.replace('%', "%25").replace('|', "%7C")
    } else {
        query.to_string()
    }
}

fn decode_query(query: &str) -> String {
    let mut decoded = String::with_capacity(query.len());
    let mut chars = query.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let first = chars.peek().copied();
            let second = chars.clone().nth(1);
            match (first, second) {
                (Some('7'), Some('C' | 'c')) => {
                    chars.next();
                    chars.next();
                    decoded.push('|');
                }
                (Some('2'), Some('5')) => {
                    chars.next();
                    chars.next();
                    decoded.push('%');
                }
                _ => decoded.push(ch),
            }
        } else {
            decoded.push(ch);
        }
    }
    decoded
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
    fn test_query_delimiters_are_encoded() {
        let filter = EhFilter {
            min_rating: Some(5),
            ..Default::default()
        };
        let key = EhTaskKey::new("foo|f=r3 100% ~bar", 2, &filter);
        let value = key.to_task_value();
        assert_eq!(value, "ehq:foo%7Cf=r3 100%25 ~bar|c=2|f=r5");

        let parsed = EhTaskKey::parse(&value).unwrap();
        assert_eq!(parsed.query, "foo|f=r3 100% ~bar");
        assert_eq!(parsed.category_bitmask, 2);
        assert_eq!(parsed.filter_sig, "r5");
    }

    #[test]
    fn test_parse_legacy_unescaped_query_still_works() {
        let parsed = EhTaskKey::parse("eh:female:elf|c=3|f=r4").unwrap();
        assert_eq!(parsed.query, "female:elf");
        assert_eq!(parsed.category_bitmask, 3);
        assert_eq!(parsed.filter_sig, "r4");
    }

    #[test]
    fn test_parse_legacy_percent_text_is_not_decoded() {
        let parsed = EhTaskKey::parse("eh:foo%7Cbar|f=r4").unwrap();
        assert_eq!(parsed.query, "foo%7Cbar");
        assert_eq!(parsed.filter_sig, "r4");
    }

    #[test]
    fn test_parse_legacy_tilde_queries_are_not_decoded() {
        let parsed = EhTaskKey::parse("eh:~foo|f=r4").unwrap();
        assert_eq!(parsed.query, "~foo");
        assert_eq!(parsed.filter_sig, "r4");

        let parsed = EhTaskKey::parse("eh:~foo%7Cbar|f=r4").unwrap();
        assert_eq!(parsed.query, "~foo%7Cbar");
        assert_eq!(parsed.filter_sig, "r4");
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
