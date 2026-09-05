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
    fn task_keys_encode_and_roundtrip_queries_and_filters() {
        for (query, categories, filter, expected) in [
            ("female:elf", 0, EhFilter::new(), "eh:female:elf"),
            ("artist:wlop", 3, EhFilter::new(), "eh:artist:wlop|c=3"),
            (
                "female:elf",
                0,
                EhFilter {
                    max_pages: Some(500),
                    ..Default::default()
                },
                "eh:female:elf|f=P500",
            ),
            (
                "female:elf",
                0,
                EhFilter {
                    min_rating: Some(4),
                    ..Default::default()
                },
                "eh:female:elf|f=r4",
            ),
            (
                "parody:touhou",
                3,
                EhFilter {
                    min_rating: Some(4),
                    min_pages: Some(20),
                    telegraph: true,
                    ..Default::default()
                },
                "eh:parody:touhou|c=3|f=r4p20",
            ),
            (
                "female:elf cat:2",
                7,
                EhFilter {
                    min_rating: Some(3),
                    min_pages: Some(10),
                    max_pages: Some(200),
                    ..Default::default()
                },
                "eh:female:elf cat:2|c=7|f=r3p10P200",
            ),
            (
                "foo|f=r3 100% ~bar",
                2,
                EhFilter {
                    min_rating: Some(5),
                    ..Default::default()
                },
                "ehq:foo%7Cf=r3 100%25 ~bar|c=2|f=r5",
            ),
        ] {
            let key = EhTaskKey::new(query, categories, &filter);
            assert_eq!(key.to_task_value(), expected);
            assert_eq!(EhTaskKey::parse(expected), Some(key), "{expected}");
        }
    }

    #[test]
    fn parse_preserves_unescaped_persisted_queries() {
        for query in ["female:elf", "foo%7Cbar", "~foo", "~foo%7Cbar"] {
            let parsed = EhTaskKey::parse(&format!("eh:{query}|c=3|f=r4")).unwrap();
            assert_eq!(
                parsed,
                EhTaskKey {
                    query: query.into(),
                    category_bitmask: 3,
                    filter_sig: "r4".into()
                }
            );
        }
    }

    #[test]
    fn parse_rejects_malformed_or_ambiguous_task_keys() {
        for value in [
            "booru:konachan:cat",
            "no_colon",
            "eh:female:elf|c=1|c=2",
            "eh:female:elf|x=1",
        ] {
            assert!(EhTaskKey::parse(value).is_none(), "{value}");
        }
    }
}
