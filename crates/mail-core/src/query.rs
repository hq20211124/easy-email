//! What a search query means, as the user typed it.
//!
//! Split from `cache.rs` for two reasons: that file was already three thousand lines long, and —
//! the one that matters — "what does this string mean" and "what SQL does it become" are two
//! questions. Only the second needs a database, so the first is testable without one, and the rules
//! that decide what a user's words mean are exactly the ones that must not rot.
//!
//! The tokenisation lives in `cache.rs` (`clauses_of`, `to_tokens`) and is **not** reimplemented
//! here: the index and the query must keep using the same transformation or Chinese search stops
//! working, and one copy of that rule is the only version of it that holds.

/// Which column a bare search should cover when the user scoped it with a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryColumn {
    Subject,
    /// The FTS keeps the display name and the address apart; both are "who sent it" to a user.
    From,
    To,
    /// Body text — and the attachment names, which are indexed into the same column (round 71).
    Body,
}

impl QueryColumn {
    /// The FTS5 column filter for this column, in FTS5's own syntax.
    pub fn filter(&self) -> &'static str {
        match self {
            QueryColumn::Subject => "{subject}",
            QueryColumn::From => "{from_name from_addr}",
            QueryColumn::To => "{to_addrs}",
            QueryColumn::Body => "{body}",
        }
    }
}

/// One term of a parsed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTerm {
    /// A text term, optionally scoped to one column.
    Text {
        column: Option<QueryColumn>,
        text: String,
    },
    /// `has:attachment` — a fact about the stored row, not about indexed text.
    HasAttachment,
    /// `after:` / `before:` — a unix timestamp already resolved from what the user typed.
    Date { after: bool, ts: i64 },
    /// `is:unread`.
    Unread,
    /// `is:flagged`.
    Flagged,
    /// `in:` — a folder name as the wire spells it, matched case-insensitively.
    InFolder { folder: String },
    /// `account:` — an account id.
    Account { id: String },
}

/// Every prefix this version understands.
///
/// A function rather than a constant so the search box's hint cannot drift from the parser: "what
/// this means" and "what you can type" are two halves of the same table.
pub fn known_prefixes() -> Vec<&'static str> {
    vec![
        "subject",
        "from",
        "to",
        "body",
        "filename",
        "has:attachment",
        "after",
        "before",
        "is:unread",
        "is:flagged",
        "in",
        "account",
    ]
}

fn column_of(head: &str) -> Option<QueryColumn> {
    match head.to_ascii_lowercase().as_str() {
        "subject" => Some(QueryColumn::Subject),
        "from" | "sender" => Some(QueryColumn::From),
        "to" | "cc" => Some(QueryColumn::To),
        // Attachment names are indexed in the body column (round 71), so this maps there on purpose.
        "body" | "filename" => Some(QueryColumn::Body),
        _ => None,
    }
}

/// Parse a query string into terms.
///
/// Every whitespace-separated word is looked at for a `prefix:` head. **Anything not understood stays
/// whole**: a typo like `frm:boss` is then searched for as the literal text `frm:boss`, which is the
/// only outcome that keeps the user's words findable. That rule is also the one the hint speaks to —
/// see [`query_issues`].
pub fn parse_query(input: &str) -> Vec<QueryTerm> {
    let mut terms = Vec::new();
    for raw in input.split_whitespace() {
        let term = raw.replace(['"', '\''], "");
        // Set when this word is a known prefix whose value we cannot use: the word is skipped
        // (not searched as literal text) and `query_issues` reports it.
        let mut unusable = false;
        if term.is_empty() {
            continue;
        }
        let (head, rest) = match term.split_once(':') {
            Some(pair) => pair,
            None => {
                terms.push(QueryTerm::Text {
                    column: None,
                    text: term,
                });
                continue;
            }
        };
        if rest.trim().is_empty() {
            // `subject:` with nothing after it: a word that happens to end in a colon.
            terms.push(QueryTerm::Text {
                column: None,
                text: term,
            });
            continue;
        }

        let lower = head.to_ascii_lowercase();
        let structured = match lower.as_str() {
            "has" if rest.eq_ignore_ascii_case("attachment") => Some(QueryTerm::HasAttachment),
            "is" if rest.eq_ignore_ascii_case("unread") => Some(QueryTerm::Unread),
            "is" if rest.eq_ignore_ascii_case("flagged") => Some(QueryTerm::Flagged),
            "after" | "before" => match parse_date(rest) {
                Some(ts) => Some(QueryTerm::Date {
                    after: lower == "after",
                    ts,
                }),
                // A date we cannot read is not a date, and it is not a word either: keeping it as
                // literal text would search for a string nobody meant to type. `query_issues`
                // reports it, and the rest of the query survives.
                None => {
                    unusable = true;
                    None
                }
            },
            "in" => Some(QueryTerm::InFolder {
                folder: rest.to_string(),
            }),
            "account" => Some(QueryTerm::Account {
                id: rest.to_string(),
            }),
            _ => None,
        };
        if unusable {
            continue;
        }
        if let Some(term) = structured {
            terms.push(term);
            continue;
        }

        if let Some(column) = column_of(&lower) {
            terms.push(QueryTerm::Text {
                column: Some(column),
                text: rest.to_string(),
            });
            continue;
        }

        // Unknown prefix: the whole word, unchanged.
        terms.push(QueryTerm::Text {
            column: None,
            text: term,
        });
    }
    terms
}

/// What is wrong with this query, in words the user can act on.
///
/// `design/09` §2 is explicit: a syntax mistake must produce a hint, not an empty result. Two things
/// are worth saying out loud, and everything else stays quiet:
///   * a word that **looks like a prefix** (letters then a colon) but is not one — the user almost
///     certainly meant a field, so the search also says which prefixes exist;
///   * a date that could not be read, because silently dropping a date turns "after 2026-01-01"
///     into "everything", which is the opposite of what was asked for.
pub fn query_issues(input: &str) -> Vec<String> {
    let mut issues = Vec::new();
    for raw in input.split_whitespace() {
        let term = raw.replace(['"', '\''], "");
        if let Some((head, rest)) = term.split_once(':') {
            if rest.trim().is_empty() {
                continue;
            }
            let lower = head.to_ascii_lowercase();
            let known = matches!(
                lower.as_str(),
                "subject"
                    | "from"
                    | "sender"
                    | "to"
                    | "cc"
                    | "body"
                    | "filename"
                    | "has"
                    | "is"
                    | "after"
                    | "before"
                    | "in"
                    | "account"
            );
            if !known {
                issues.push(format!(
                    "「{head}:」不是已知前缀，已按字面搜索。已知前缀：{}。",
                    known_prefixes().join("、")
                ));
                continue;
            }
            if (lower == "after" || lower == "before") && parse_date(rest).is_none() {
                issues.push(format!(
                    "日期「{rest}」看不懂，这个条件没有生效。支持 YYYY-MM-DD 或 YYYY-MM-DDTHH:MM。"
                ));
            }
        }
    }
    issues
}

/// Parse a date the way a user types one, into a unix timestamp.
///
/// `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM` and a bare number of epoch seconds are all accepted — the search
/// box is the only place a user can type, and a second accepted format costs three lines.
///
/// **Interpreted as UTC, on purpose.** The alternative is a timezone database, and this crate has no
/// business pulling one in for a date filter; the cost is that a user eight hours ahead of UTC gets a
/// boundary that is local-evening rather than local-midnight. That is documented here rather than
/// discovered by a user wondering why a mail is missing from `after:昨天`.
pub fn parse_date(input: &str) -> Option<i64> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(ts) = text.parse::<i64>() {
        return Some(ts);
    }
    let (date_part, hour, minute) = match text.split_once('T').or_else(|| text.split_once(' ')) {
        Some((date, time)) => {
            let mut parts = time.split(':');
            let hour: u32 = parts.next()?.parse().ok()?;
            let minute: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            (date, hour, minute)
        }
        None => (text, 0, 0),
    };
    let mut date = date_part.split('-');
    let year: i32 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // And the day must exist in that month — 2023-02-29 parsed happily before this check, which is
    // how a date filter can quietly point at March. Leap years are the interesting case.
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let longest = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if day > longest {
        return None;
    }

    // Days since 1970-01-01 by arithmetic rather than a date crate: three lines of calendar maths do
    // not justify another dependency in the tree. Leap years are included, which is what makes this
    // boring.
    let is_leap = |y: i32| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap(year) {
                    29
                } else {
                    28
                }
            }
            _ => return None,
        };
    }
    days += i64::from(day) - 1;

    Some(days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60)
}

/// The FTS5 MATCH expression for the text terms of a query.
///
/// Column scoping is expressed with FTS5's own filter syntax (`{subject}: "账 单"`), so the
/// tokenisation stays exactly what it was — the index and the query must keep using the same
/// transformation, and `the_index_and_the_query_use_the_same_transformation` in `cache.rs` is what
/// stops this function from "simplifying" the CJK handling away.
pub fn match_expression(text_terms: &[QueryTerm]) -> String {
    let mut clauses: Vec<String> = Vec::new();
    for term in text_terms {
        let QueryTerm::Text { column, text } = term else {
            continue;
        };
        let head = column
            .map(|c| format!("{}: ", c.filter()))
            .unwrap_or_default();
        for clause in crate::cache::clauses_of(text) {
            match clause {
                crate::cache::Clause::Word(word) => {
                    clauses.push(format!("{head}\"{word}\""));
                }
                crate::cache::Clause::Phrase(chars) => {
                    let phrase: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
                    clauses.push(format!("{head}\"{}\"", phrase.join(" ")));
                }
            }
        }
    }
    if clauses.is_empty() {
        // Nothing searchable. A bare `*` cannot match anything, so this is a safe no-op query.
        return String::new();
    }
    clauses.join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_prefixes_become_predicates() {
        assert_eq!(
            parse_query("after:2026-01-01"),
            vec![QueryTerm::Date {
                after: true,
                ts: 1_767_225_600
            }]
        );
        assert_eq!(
            parse_query("before:2026-01-01"),
            vec![QueryTerm::Date {
                after: false,
                ts: 1_767_225_600
            }]
        );
        assert_eq!(parse_query("is:unread"), vec![QueryTerm::Unread]);
        assert_eq!(parse_query("is:flagged"), vec![QueryTerm::Flagged]);
        assert_eq!(
            parse_query("in:INBOX"),
            vec![QueryTerm::InFolder {
                folder: "INBOX".to_string()
            }]
        );
        assert_eq!(
            parse_query("account:acc-1"),
            vec![QueryTerm::Account {
                id: "acc-1".to_string()
            }]
        );
    }

    #[test]
    fn dates_come_in_three_shapes_and_nothing_else() {
        assert_eq!(parse_date("2026-01-01"), Some(1_767_225_600));
        assert_eq!(parse_date("2026-01-01T08:30"), Some(1_767_256_200)); // 08:30 is 30600 seconds, not an hour
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("1700000000"), Some(1_700_000_000));
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("昨天"), None);
        assert_eq!(
            parse_date("2026-13-01"),
            None,
            "month thirteen is not a date"
        );
        assert_eq!(
            parse_date("2026-02-30"),
            None,
            "and neither is the thirtieth of February"
        );
        assert_eq!(parse_date("2026-01-01T99:00"), None);
        // Leap days exist, which is why the arithmetic is worth the four lines.
        assert_eq!(parse_date("2024-02-29"), Some(1_709_164_800));
        assert_eq!(parse_date("2023-02-29"), None);
    }

    #[test]
    fn an_unknown_prefix_is_a_literal_and_says_so() {
        // Compared as a term, not as text: the point is that the whole word survives.
        assert_eq!(
            parse_query("frm:boss"),
            vec![QueryTerm::Text {
                column: None,
                text: "frm:boss".to_string()
            }]
        );
        let issues = query_issues("frm:boss");
        assert_eq!(
            issues.len(),
            1,
            "the user must be told, not left with nothing"
        );
        assert!(issues[0].contains("不是已知前缀"));
        assert!(issues[0].contains("from"));

        // A word with no colon is never a mistake.
        assert!(query_issues("boss").is_empty());
        // A trailing colon with nothing after it is a word.
        assert!(query_issues("subject:").is_empty());
    }

    #[test]
    fn an_unreadable_date_is_reported_rather_than_dropped_quietly() {
        let terms = parse_query("after:昨天");
        assert!(
            terms.is_empty(),
            "an unreadable date is not a date — but it must not be a word either"
        );
        let issues = query_issues("after:昨天");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("看不懂"));
    }

    #[test]
    fn a_query_mixes_text_scopes_and_predicates() {
        let terms = parse_query("from:shop has:attachment after:2026-01-01 发票");
        assert_eq!(terms.len(), 4);
        assert!(matches!(
            terms[0],
            QueryTerm::Text {
                column: Some(QueryColumn::From),
                ..
            }
        ));
        assert!(matches!(terms[1], QueryTerm::HasAttachment));
        assert!(matches!(terms[2], QueryTerm::Date { after: true, .. }));
        assert!(matches!(terms[3], QueryTerm::Text { column: None, .. }));
        assert!(query_issues("from:shop has:attachment after:2026-01-01 发票").is_empty());
    }
}
