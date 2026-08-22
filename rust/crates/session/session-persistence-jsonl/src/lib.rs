//! JSONL persistence provider. Crash repair closes an open turn with `interrupted`.

use dsh_session::{Session, SessionEvent, SessionEventData, TurnEndReason};
use std::path::Path;
use tokio::fs;

/// Write the log as one JSON object per line.
pub async fn write_jsonl(path: impl AsRef<Path>, session: &Session) -> std::io::Result<()> {
    let mut body = String::new();
    for event in session.events() {
        body.push_str(&serde_json::to_string(&event).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error)
        })?);
        body.push('\n');
    }
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, body).await
}

/// Load a log, repairing a trailing open turn.
pub async fn read_jsonl(path: impl AsRef<Path>, session: &Session) -> std::io::Result<()> {
    let body = fs::read_to_string(path).await?;
    let mut events: Vec<SessionEvent> = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str(line).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error)
        })?);
    }
    repair_open_turn(&mut events);
    for event in events {
        let _ = session.append(event.data, event.surface_op);
    }
    Ok(())
}

/// Close a dangling `turn/start` with `interrupted`.
pub fn repair_open_turn(events: &mut Vec<SessionEvent>) {
    let mut open: Option<u32> = None;
    for event in events.iter() {
        match &event.data {
            SessionEventData::TurnStart { turn } => open = Some(*turn),
            SessionEventData::TurnEnd { .. } => open = None,
            _ => {}
        }
    }
    if let Some(turn) = open {
        let seq = events.len() as u64;
        events.push(SessionEvent {
            seq,
            data: SessionEventData::TurnEnd {
                turn,
                reason: TurnEndReason::Interrupted,
            },
            surface_op: None,
            ignorable: false,
        });
    }
}

/// Re-export the persistence seam so bundles can depend on one crate.
pub use dsh_session_persistence::Runtime as PersistenceSeam;

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::session_id;

    #[test]
    fn crash_repair_closes_open_turn() {
        let mut events = vec![SessionEvent {
            seq: 0,
            data: SessionEventData::TurnStart { turn: 1 },
            surface_op: None,
            ignorable: false,
        }];
        repair_open_turn(&mut events);
        assert!(matches!(
            events.last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn seam_key_is_stable() {
        assert_eq!(<PersistenceSeam as dsh_cordis::Service>::KEY, "sessionPersistence");
    }
}
