//! Shared event metadata and semantic-document projection.

use crate::extraction::extract_session_event_text;
use crate::tracing::surface_by_seq;
use crate::{SessionEventRecord, SessionEventSurface};
use dsh_session::{event_type_name, SessionEvent, SessionId};

/// Searchable semantic document derived from one session event.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEventSearchDocument {
    /// Session that owns the event.
    pub session_id: SessionId,
    /// Monotonic event seq within the session.
    pub seq: u64,
    /// Discriminant of the session event.
    pub type_name: String,
    /// Event timestamp in Unix epoch milliseconds.
    pub time: u64,
    /// Event placement in the folded session surface.
    pub surface: SessionEventSurface,
    /// First-party semantic text used by scan filters.
    pub text: String,
}

/// Project a raw log into lightweight surface-aware event records.
///
/// @param session_id - session that owns the log.
/// @param events - complete contiguous raw event log.
/// @returns one record per event in ascending seq order.
pub fn build_session_event_records(
    session_id: &SessionId,
    events: &[SessionEvent],
) -> Result<Vec<SessionEventRecord>, crate::SessionQueryError> {
    let surface_by_seq = surface_by_seq(events)?;
    Ok(events
        .iter()
        .map(|event| SessionEventRecord {
            session_id: session_id.clone(),
            seq: event.seq,
            type_name: event_type_name(&event.data).to_string(),
            time: event.time,
            surface: surface_by_seq
                .get(&event.seq)
                .copied()
                .unwrap_or(SessionEventSurface::LogOnly),
        })
        .collect())
}

/// Build first-party semantic documents for one complete raw event log.
///
/// @param session_id - session that owns the log.
/// @param events - complete contiguous raw event log.
/// @returns searchable documents in ascending seq order; structural events are omitted.
pub fn build_session_event_search_documents(
    session_id: &SessionId,
    events: &[SessionEvent],
) -> Result<Vec<SessionEventSearchDocument>, crate::SessionQueryError> {
    let surface_by_seq = surface_by_seq(events)?;
    let mut documents = Vec::new();
    for event in events {
        let text = extract_session_event_text(event);
        if text.is_empty() {
            continue;
        }
        documents.push(SessionEventSearchDocument {
            session_id: session_id.clone(),
            seq: event.seq,
            type_name: event_type_name(&event.data).to_string(),
            time: event.time,
            surface: surface_by_seq
                .get(&event.seq)
                .copied()
                .unwrap_or(SessionEventSurface::LogOnly),
            text,
        });
    }
    Ok(documents)
}
