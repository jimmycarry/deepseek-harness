//! Pure provider-independent predicates for logical sessions and event text.

use crate::documents::SessionEventSearchDocument;
use crate::{
    SessionAvailability, SessionQueryError, SessionQueryErrorCode, SessionRecord,
    SessionResultRange,
};

/// One logical-session predicate. A filter array is ANDed; `values` within a
/// clause are ORed.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionResultFilter {
    /// Match any of the listed session ids.
    Id {
        /// Session ids accepted by this clause.
        values: Vec<String>,
    },
    /// Match any of the listed cwd values; `None` is a missing cwd.
    Cwd {
        /// cwd strings or a missing cwd.
        values: Vec<Option<String>>,
    },
    /// Inclusive `createdAt` interval.
    CreatedAt(SessionResultRange),
    /// Match any of the listed parent ids; `None` is a top-level session.
    Parent {
        /// Parent session ids or a missing parent.
        values: Vec<Option<String>>,
    },
    /// Match live and/or persisted availability bits.
    Availability {
        /// Availability tokens accepted by this clause.
        values: Vec<SessionAvailability>,
    },
}

/// One event predicate. A filter array is ANDed; list-valued clauses are ORed.
/// Text is a literal, case-insensitive, whitespace-flexible semantic-text scan.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEventResultFilter {
    /// Inclusive event seq interval.
    Seq(SessionResultRange),
    /// Inclusive event time interval.
    Time(SessionResultRange),
    /// Match any of the listed event type names.
    Type {
        /// Event type discriminants.
        values: Vec<String>,
    },
    /// Match any of the listed surface placements.
    Surface {
        /// Surface tokens accepted by this clause.
        values: Vec<crate::SessionEventSurface>,
    },
    /// Literal semantic-text scan.
    Text {
        /// Caller-provided literal text.
        text: String,
    },
}

/// Copy and validate logical-session filters before an asynchronous boundary.
///
/// @param filters - caller-owned clauses to materialize.
/// @returns detached validated clauses.
pub fn materialize_session_result_filters(
    filters: &[SessionResultFilter],
) -> Result<Vec<SessionResultFilter>, SessionQueryError> {
    filters
        .iter()
        .map(|filter| match filter {
            SessionResultFilter::CreatedAt(range) => {
                validate_range("created-at", range)?;
                Ok(SessionResultFilter::CreatedAt(range.clone()))
            }
            other => Ok(other.clone()),
        })
        .collect()
}

/// Copy and validate event filters before an asynchronous boundary.
///
/// @param filters - caller-owned clauses to materialize.
/// @returns detached validated clauses.
pub fn materialize_session_event_result_filters(
    filters: &[SessionEventResultFilter],
) -> Result<Vec<SessionEventResultFilter>, SessionQueryError> {
    filters
        .iter()
        .map(|filter| match filter {
            SessionEventResultFilter::Seq(range) => {
                validate_range("seq", range)?;
                Ok(SessionEventResultFilter::Seq(range.clone()))
            }
            SessionEventResultFilter::Time(range) => {
                validate_range("time", range)?;
                Ok(SessionEventResultFilter::Time(range.clone()))
            }
            SessionEventResultFilter::Text { text } => {
                compile_session_text_filter(text)?;
                Ok(SessionEventResultFilter::Text { text: text.clone() })
            }
            other => Ok(other.clone()),
        })
        .collect()
}

/// Apply ANDed logical-session filters while preserving input order.
///
/// @param records - detached logical-session records to inspect.
/// @param filters - clauses whose list values are ORed within each clause.
/// @returns records accepted by every clause.
pub fn filter_session_results(
    records: &[SessionRecord],
    filters: &[SessionResultFilter],
) -> Result<Vec<SessionRecord>, SessionQueryError> {
    let filters = materialize_session_result_filters(filters)?;
    Ok(records
        .iter()
        .filter(|record| filters.iter().all(|filter| session_matches(record, filter)))
        .cloned()
        .collect())
}

/// Apply ANDed event filters to extracted semantic documents.
///
/// @param documents - semantic documents produced by [`crate::build_session_event_search_documents`].
/// @param filters - metadata and literal-text predicates.
/// @returns documents accepted by every clause, in input order.
pub fn filter_session_event_documents(
    documents: &[SessionEventSearchDocument],
    filters: &[SessionEventResultFilter],
) -> Result<Vec<SessionEventSearchDocument>, SessionQueryError> {
    let filters = materialize_session_event_result_filters(filters)?;
    let compiled: Result<Vec<_>, _> = filters.iter().map(event_predicate).collect();
    let predicates = compiled?;
    Ok(documents
        .iter()
        .filter(|document| predicates.iter().all(|predicate| predicate(document)))
        .cloned()
        .collect())
}

/// Compile a literal case-insensitive, whitespace-flexible semantic-text match.
///
/// @param text - caller-provided literal text.
/// @returns token matcher safe from regex injection.
pub fn compile_session_text_filter(text: &str) -> Result<SessionTextFilter, SessionQueryError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(SessionQueryError::new(
            "session text filter must contain non-whitespace text",
            SessionQueryErrorCode::InvalidFilter,
        ));
    }
    Ok(SessionTextFilter {
        tokens: trimmed.split_whitespace().map(str::to_lowercase).collect(),
    })
}

/// TypeScript `session unknown filter kind "{kind}"` sentence.
///
/// @param kind - unrecognized filter discriminant.
/// @returns typed invalid-filter failure.
pub fn unknown_filter_kind(kind: &str) -> SessionQueryError {
    invalid_filter(format!("unknown filter kind \"{kind}\""))
}

/// Literal token matcher for one text filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTextFilter {
    tokens: Vec<String>,
}

impl SessionTextFilter {
    /// Whether `haystack` contains the compiled tokens in order.
    ///
    /// @param haystack - extracted semantic text.
    /// @returns true when the literal, whitespace-flexible pattern matches.
    pub fn is_match(&self, haystack: &str) -> bool {
        let hay: Vec<char> = haystack.to_lowercase().chars().collect();
        let tokens: Vec<Vec<char>> = self
            .tokens
            .iter()
            .map(|token| token.chars().collect())
            .collect();
        let Some(first) = tokens.first() else {
            return false;
        };
        if tokens.len() == 1 {
            return contains_subslice(&hay, first);
        }
        let mut from = 0;
        while let Some(start) = find_subslice_from(&hay, first, from) {
            if rest_matches(&hay, start + first.len(), &tokens[1..]) {
                return true;
            }
            from = start + 1;
        }
        false
    }
}

fn session_matches(record: &SessionRecord, filter: &SessionResultFilter) -> bool {
    match filter {
        SessionResultFilter::Id { values } => {
            values.iter().any(|id| id == record.header.id.as_str())
        }
        SessionResultFilter::Cwd { values } => values
            .iter()
            .any(|cwd| cwd.as_deref() == record.header.cwd.as_deref()),
        SessionResultFilter::CreatedAt(range) => {
            matches_range(record.header.created_at as f64, range)
        }
        SessionResultFilter::Parent { values } => values.iter().any(|parent| {
            parent.as_deref() == record.header.parent_session.as_ref().map(|id| id.as_str())
        }),
        SessionResultFilter::Availability { values } => values.iter().any(|value| match value {
            SessionAvailability::Live => record.live,
            SessionAvailability::Persisted => record.persisted,
        }),
    }
}

fn event_predicate(
    filter: &SessionEventResultFilter,
) -> Result<Box<dyn Fn(&SessionEventSearchDocument) -> bool>, SessionQueryError> {
    match filter {
        SessionEventResultFilter::Seq(range) => {
            let range = range.clone();
            Ok(Box::new(move |document| {
                matches_range(document.seq as f64, &range)
            }))
        }
        SessionEventResultFilter::Time(range) => {
            let range = range.clone();
            Ok(Box::new(move |document| {
                matches_range(document.time as f64, &range)
            }))
        }
        SessionEventResultFilter::Type { values } => {
            let values = values.clone();
            Ok(Box::new(move |document| {
                values.iter().any(|value| value == &document.type_name)
            }))
        }
        SessionEventResultFilter::Surface { values } => {
            let values = values.clone();
            Ok(Box::new(move |document| values.contains(&document.surface)))
        }
        SessionEventResultFilter::Text { text } => {
            let pattern = compile_session_text_filter(text)?;
            Ok(Box::new(move |document| pattern.is_match(&document.text)))
        }
    }
}

fn validate_range(
    name: &str,
    range: &SessionResultRange,
) -> Result<SessionResultRange, SessionQueryError> {
    if let Some(from) = range.from {
        if !from.is_finite() {
            return Err(invalid_range(name, "from must be finite"));
        }
    }
    if let Some(to) = range.to {
        if !to.is_finite() {
            return Err(invalid_range(name, "to must be finite"));
        }
    }
    if let (Some(from), Some(to)) = (range.from, range.to) {
        if from > to {
            return Err(invalid_range(name, "from must be less than or equal to to"));
        }
    }
    Ok(range.clone())
}

fn matches_range(value: f64, range: &SessionResultRange) -> bool {
    range.from.is_none_or(|from| value >= from) && range.to.is_none_or(|to| value <= to)
}

fn invalid_range(name: &str, detail: &str) -> SessionQueryError {
    invalid_filter(format!("{name} filter {detail}"))
}

fn invalid_filter(detail: impl Into<String>) -> SessionQueryError {
    SessionQueryError::new(
        format!("session {}", detail.into()),
        SessionQueryErrorCode::InvalidFilter,
    )
}

fn contains_subslice(hay: &[char], needle: &[char]) -> bool {
    find_subslice_from(hay, needle, 0).is_some()
}

fn find_subslice_from(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    if from >= hay.len() || needle.len() > hay.len().saturating_sub(from) {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&index| hay[index..index + needle.len()] == needle[..])
}

fn rest_matches(hay: &[char], mut pos: usize, tokens: &[Vec<char>]) -> bool {
    for token in tokens {
        if pos >= hay.len() || !hay[pos].is_whitespace() {
            return false;
        }
        while pos < hay.len() && hay[pos].is_whitespace() {
            pos += 1;
        }
        if pos + token.len() > hay.len() || hay[pos..pos + token.len()] != token[..] {
            return false;
        }
        pos += token.len();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEventSurface;
    use dsh_session::{session_id, SessionHeader, SESSION_FORMAT_VERSION};

    fn header(id: &str, created_at: u64) -> SessionHeader {
        SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: session_id(id),
            created_at,
            cwd: None,
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: 0,
        }
    }

    #[test]
    fn applies_session_clauses_with_or_values() {
        let parent = session_id("parent");
        let records = [
            SessionRecord {
                header: {
                    let mut header = header("a", 10);
                    header.cwd = Some("/a".into());
                    header.parent_session = Some(parent.clone());
                    header
                },
                live: true,
                persisted: false,
            },
            SessionRecord {
                header: header("b", 20),
                live: false,
                persisted: true,
            },
        ];
        let filtered = filter_session_results(
            &records,
            &[
                SessionResultFilter::Id {
                    values: vec!["a".into(), "x".into()],
                },
                SessionResultFilter::Cwd {
                    values: vec![Some("/a".into()), None],
                },
                SessionResultFilter::CreatedAt(SessionResultRange {
                    from: Some(5.0),
                    to: Some(15.0),
                }),
                SessionResultFilter::Parent {
                    values: vec![Some("parent".into()), None],
                },
                SessionResultFilter::Availability {
                    values: vec![SessionAvailability::Live],
                },
            ],
        )
        .unwrap();
        assert_eq!(filtered[0].header.id.as_str(), "a");
        assert_eq!(
            filter_session_results(&records, &[SessionResultFilter::Cwd { values: vec![None] }])
                .unwrap()[0]
                .header
                .id
                .as_str(),
            "b"
        );
        assert_eq!(
            filter_session_results(
                &records,
                &[SessionResultFilter::Availability {
                    values: vec![SessionAvailability::Persisted]
                }]
            )
            .unwrap()[0]
                .header
                .id
                .as_str(),
            "b"
        );
    }

    #[test]
    fn rejects_malformed_ranges_and_empty_text() {
        let error = filter_session_results(
            &[],
            &[SessionResultFilter::CreatedAt(SessionResultRange {
                from: Some(f64::NAN),
                to: None,
            })],
        )
        .unwrap_err();
        assert_eq!(error.code, SessionQueryErrorCode::InvalidFilter);
        assert_eq!(
            error.message,
            "session created-at filter from must be finite"
        );
        let infinity = filter_session_event_documents(
            &[],
            &[SessionEventResultFilter::Seq(SessionResultRange {
                from: None,
                to: Some(f64::INFINITY),
            })],
        )
        .unwrap_err();
        assert_eq!(infinity.message, "session seq filter to must be finite");
        let inverted = filter_session_event_documents(
            &[],
            &[SessionEventResultFilter::Time(SessionResultRange {
                from: Some(2.0),
                to: Some(1.0),
            })],
        )
        .unwrap_err();
        assert_eq!(
            inverted.message,
            "session time filter from must be less than or equal to to"
        );
        let empty = compile_session_text_filter(" \n ").unwrap_err();
        assert_eq!(empty.code, SessionQueryErrorCode::InvalidFilter);
        assert_eq!(
            empty.message,
            "session text filter must contain non-whitespace text"
        );
        assert_eq!(
            unknown_filter_kind("future").message,
            "session unknown filter kind \"future\""
        );
    }

    #[test]
    fn text_filter_is_literal_case_insensitive_and_whitespace_flexible() {
        let pattern = compile_session_text_filter("hello   (ai)+").unwrap();
        assert!(pattern.is_match("Hello\n(AI)+"));
        let cafe = compile_session_text_filter("CAFÉ").unwrap();
        assert!(cafe.is_match("café"));
        let spaced = compile_session_text_filter("alpha beta").unwrap();
        assert!(spaced.is_match("Alpha\n beta"));
        assert!(!spaced.is_match("alphabet"));
    }

    #[test]
    fn empty_list_values_match_nothing() {
        let document = SessionEventSearchDocument {
            session_id: session_id("s"),
            seq: 0,
            type_name: "user/message".into(),
            time: 1,
            surface: SessionEventSurface::Current,
            text: "hello".into(),
        };
        assert!(filter_session_event_documents(
            &[document],
            &[SessionEventResultFilter::Surface { values: vec![] }]
        )
        .unwrap()
        .is_empty());
    }
}
