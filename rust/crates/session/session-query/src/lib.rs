//! In-log session search.

use dsh_session::Session;

/// Return seqs whose logged JSON contains `needle`.
///
/// An empty needle matches nothing.
pub fn search(session: &Session, needle: &str) -> Vec<u64> {
    if needle.is_empty() {
        return Vec::new();
    }
    session
        .events()
        .into_iter()
        .filter_map(|event| {
            let json = serde_json::to_string(&event).ok()?;
            json.contains(needle).then_some(event.seq)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionEventData};

    #[test]
    fn search_returns_matching_seqs() {
        let session = Session::new(session_id("s"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(SessionEventData::StepStart { turn: 1, step: 1 }, None)
            .unwrap();
        assert_eq!(search(&session, "turn/start"), vec![0]);
        assert!(search(&session, "missing").is_empty());
        assert!(search(&session, "").is_empty());
    }
}
