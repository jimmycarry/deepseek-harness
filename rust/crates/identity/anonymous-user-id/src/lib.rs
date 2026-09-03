//! Per-harness-home anonymous user id (`$DSH_HOME/.anonymous-user-id`).
//!
//! The id is a random UUID persisted as a bare line. It is never derived from
//! hostname, network address, or git remotes. One process memos per resolved
//! path; deleting the file mid-run keeps the in-memory id until the next
//! launch. Persistence is best-effort so a read-only home still yields an id.

use dsh_home_paths::resolve_dsh_home;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

/// File inside the harness home storing the id.
pub const ANONYMOUS_USER_ID_FILE_NAME: &str = ".anonymous-user-id";

static MEMO: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

fn memo() -> &'static Mutex<HashMap<PathBuf, String>> {
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

fn read_persisted_id(file: &Path) -> Option<String> {
    let text = fs::read_to_string(file).ok()?;
    let value = text.trim();
    is_uuid(value).then(|| value.to_string())
}

/// Return the harness home's anonymous user id, creating and persisting one
/// on first use.
///
/// # Errors
/// This function does not return an error. A write failure still returns a
/// usable in-memory id for the current run.
pub fn get_or_create_anonymous_user_id() -> String {
    get_or_create_in(resolve_dsh_home(None), || Uuid::new_v4().to_string())
}

/// Same as [`get_or_create_anonymous_user_id`] against an explicit home and
/// generator. The generator runs only when the file is absent or corrupt.
pub fn get_or_create_in(home: impl AsRef<Path>, generate: impl FnOnce() -> String) -> String {
    let file = home.as_ref().join(ANONYMOUS_USER_ID_FILE_NAME);
    if let Some(cached) = memo().lock().expect("anonymous-user-id").get(&file).cloned() {
        return cached;
    }
    if let Some(existing) = read_persisted_id(&file) {
        memo()
            .lock()
            .expect("anonymous-user-id")
            .insert(file, existing.clone());
        return existing;
    }
    let created = generate();
    let id = persist_or_adopt(&file, &created);
    memo()
        .lock()
        .expect("anonymous-user-id")
        .insert(file, id.clone());
    id
}

fn persist_or_adopt(file: &Path, created: &str) -> String {
    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
    {
        Ok(mut handle) => {
            let _ = handle.write_all(format!("{created}\n").as_bytes());
            created.to_string()
        }
        Err(_) => match read_persisted_id(file) {
            Some(winner) => winner,
            None => {
                if fs::write(file, format!("{created}\n")).is_err() {
                    // Best-effort persistence: keep the fresh id in memory.
                }
                created.to_string()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static UNIQUE: AtomicUsize = AtomicUsize::new(0);

    fn temp_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-anon-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_persists_and_returns_bare_uuid_line() {
        let home = temp_home();
        let id = get_or_create_in(&home, || "01234567-89ab-4cde-8f01-23456789abcd".into());
        assert_eq!(id, "01234567-89ab-4cde-8f01-23456789abcd");
        assert_eq!(
            fs::read_to_string(home.join(ANONYMOUS_USER_ID_FILE_NAME)).unwrap(),
            "01234567-89ab-4cde-8f01-23456789abcd\n"
        );
    }

    #[test]
    fn returns_persisted_id_and_tolerates_whitespace() {
        let home = temp_home();
        let existing = "01234567-89ab-4cde-8f01-23456789abcd";
        fs::write(
            home.join(ANONYMOUS_USER_ID_FILE_NAME),
            format!("  {existing}\n\n"),
        )
        .unwrap();
        assert_eq!(get_or_create_in(&home, || "should-not-run".into()), existing);
    }

    #[test]
    fn overwrites_corrupt_file() {
        let home = temp_home();
        fs::write(home.join(ANONYMOUS_USER_ID_FILE_NAME), "not-a-uuid\n").unwrap();
        let id = get_or_create_in(&home, || "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into());
        assert_eq!(id, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
    }

    #[test]
    fn memoizes_per_home_even_after_file_delete() {
        let home = temp_home();
        let id = get_or_create_in(&home, || "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff".into());
        fs::remove_file(home.join(ANONYMOUS_USER_ID_FILE_NAME)).unwrap();
        assert_eq!(get_or_create_in(&home, || "should-not-run".into()), id);
    }
}
