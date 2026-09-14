//! One-shot event-relationship tracing over a detached raw log.

use crate::{
    SessionEventRecord, SessionEventSurface, SessionEventTrace, SessionQueryError,
    SessionQueryErrorCode,
};
use dsh_session::SessionId;
use dsh_session::{event_type_name, fold_surface, SessionError, SessionEvent, SurfaceFoldResult};
use std::collections::{HashMap, HashSet};

pub(crate) struct EventLogAnalysis {
    pub records: Vec<SessionEventRecord>,
    pub replaced_by: HashMap<u64, u64>,
    pub replaced_event_seqs: HashMap<u64, Vec<u64>>,
    pub current_seqs: Vec<u64>,
}

/// Classify a raw event log with one canonical surface fold.
///
/// @param session_id - owner of the event log.
/// @param events - detached raw event log.
/// @returns lightweight records in ascending log order.
pub fn event_records(
    session_id: &SessionId,
    events: &[SessionEvent],
) -> Result<Vec<SessionEventRecord>, SessionQueryError> {
    Ok(analyze_event_log(session_id, events)?.records)
}

/// Fold and return the current model surface after validating the whole log.
///
/// @param session_id - owner used in query diagnostics.
/// @param events - detached raw event log from one corpus observation.
/// @returns detached current surface events in folded order.
pub fn current_surface_events(
    session_id: &SessionId,
    events: &[SessionEvent],
) -> Result<Vec<SessionEvent>, SessionQueryError> {
    let analysis = analyze_event_log(session_id, events)?;
    analysis
        .current_seqs
        .into_iter()
        .map(|seq| {
            let event = events.get(seq as usize).ok_or_else(|| {
                SessionQueryError::new(
                    format!("invalid session surface: current node {seq} is not a surface event"),
                    SessionQueryErrorCode::InvalidSurface,
                )
            })?;
            if event.seq != seq || !event.data.is_surface() {
                return Err(SessionQueryError::new(
                    format!("invalid session surface: current node {seq} is not a surface event"),
                    SessionQueryErrorCode::InvalidSurface,
                ));
            }
            Ok(event.clone())
        })
        .collect()
}

/// Trace one target after one canonical surface fold and whole-log validation.
///
/// Target existence is checked before the fold so a missing seq does not
/// surface as `SESSION_QUERY_INVALID_SURFACE`.
///
/// @param session_id - owner of the event log.
/// @param events - detached raw event log.
/// @param seq - target event seq.
/// @returns direct surface replacements and relationships to cited source events.
pub fn trace_event(
    session_id: &SessionId,
    events: &[SessionEvent],
    seq: u64,
) -> Result<SessionEventTrace, SessionQueryError> {
    let target = events.get(seq as usize);
    if target.is_none() || target.is_some_and(|event| event.seq != seq) {
        return Err(SessionQueryError::new(
            format!(
                "session \"{}\" has no event at seq {seq}",
                session_id.as_str()
            ),
            SessionQueryErrorCode::EventNotFound,
        ));
    }
    let target = target.expect("existence checked above");

    let analysis = analyze_event_log(session_id, events)?;

    let mut replacement_chain = Vec::new();
    let mut replacement = analysis.replaced_by.get(&seq).copied();
    while let Some(next) = replacement {
        replacement_chain.push(next);
        replacement = analysis.replaced_by.get(&next).copied();
    }

    let mut derived_event_seqs = Vec::new();
    for event in events {
        if event.seq <= seq {
            continue;
        }
        if event_sources(event).contains(&seq) {
            derived_event_seqs.push(event.seq);
        }
    }

    let target_record = analysis
        .records
        .get(seq as usize)
        .cloned()
        .expect("analyze_event_log validated contiguous seqs");
    Ok(SessionEventTrace {
        target: target_record,
        replaced_by: analysis.replaced_by.get(&seq).copied(),
        replacement_chain,
        replaced_event_seqs: analysis
            .replaced_event_seqs
            .get(&seq)
            .cloned()
            .unwrap_or_default(),
        source_event_seqs: event_sources(target),
        derived_event_seqs,
    })
}

/// Surface placement for every seq that the fold classified.
///
/// @param events - detached raw event log.
/// @returns current and shadowed seqs; unclassified seqs are log-only at the caller.
pub fn surface_by_seq(
    events: &[SessionEvent],
) -> Result<HashMap<u64, SessionEventSurface>, SessionQueryError> {
    let folded = fold_session_events(events)?;
    let mut result = HashMap::new();
    for seq in &folded.nodes {
        result.insert(*seq, SessionEventSurface::Current);
    }
    for replacement in &folded.replacements {
        for seq in &replacement.shadowed_seqs {
            result.insert(*seq, SessionEventSurface::Shadowed);
        }
    }
    Ok(result)
}

pub(crate) fn analyze_event_log(
    session_id: &SessionId,
    events: &[SessionEvent],
) -> Result<EventLogAnalysis, SessionQueryError> {
    let folded = fold_session_events(events)?;
    let current: HashSet<u64> = folded.nodes.iter().copied().collect();
    let mut replaced_by = HashMap::new();
    let mut replaced_event_seqs = HashMap::new();
    for replacement in &folded.replacements {
        let removed = replacement.shadowed_seqs.clone();
        replaced_event_seqs.insert(replacement.seq, removed.clone());
        for removed_seq in removed {
            replaced_by.insert(removed_seq, replacement.seq);
        }
    }
    Ok(EventLogAnalysis {
        records: events
            .iter()
            .map(|event| SessionEventRecord {
                session_id: session_id.clone(),
                seq: event.seq,
                type_name: event_type_name(&event.data).to_string(),
                time: event.time,
                surface: if current.contains(&event.seq) {
                    SessionEventSurface::Current
                } else if replaced_by.contains_key(&event.seq) {
                    SessionEventSurface::Shadowed
                } else {
                    SessionEventSurface::LogOnly
                },
            })
            .collect(),
        replaced_by,
        replaced_event_seqs,
        current_seqs: folded.nodes,
    })
}

fn fold_session_events(events: &[SessionEvent]) -> Result<SurfaceFoldResult, SessionQueryError> {
    fold_surface(events).map_err(invalid_surface)
}

fn invalid_surface(error: SessionError) -> SessionQueryError {
    SessionQueryError::new(
        format!("invalid session surface: {error}"),
        SessionQueryErrorCode::InvalidSurface,
    )
}

fn event_sources(event: &SessionEvent) -> Vec<u64> {
    event.source_event_seqs.clone().unwrap_or_default()
}
