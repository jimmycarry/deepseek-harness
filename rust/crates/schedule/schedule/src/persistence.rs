//! Schedule-owned use of the shared session durability barrier.

use dsh_cordis::Context;
use dsh_session::Session;
use dsh_session_persistence::PersistenceRuntime;
use thiserror::Error;

/// Failure to prove that the current live prefix reached a persistence listener.
#[derive(Debug, Error)]
#[error("Schedule persistence did not complete.")]
pub struct SchedulePersistenceError;

/// Require one successful shared persistence checkpoint.
pub async fn flush_schedule_persistence(
    ctx: &Context,
    session: &Session,
) -> Result<(), SchedulePersistenceError> {
    let Some(persistence) = ctx.get::<PersistenceRuntime>() else {
        return Err(SchedulePersistenceError);
    };
    persistence
        .flush(session)
        .await
        .map_err(|_| SchedulePersistenceError)
}
