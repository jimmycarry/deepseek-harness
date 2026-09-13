//! Interval polling of existing skill roots and missing-root ancestors.
//!
//! TypeScript uses Chokidar for existing roots and `fs.watchFile` for the next
//! missing segment. This crate keeps the same catalog filter, stability window,
//! project LRU, and poll interval, but both modes sample the host on an interval.
//! There is no Chokidar, `notify`, or native inotify watcher.

use super::scan::{is_potential_skill_path, roots, SkillRoot};
use super::Config;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootWatchMode {
    Root { anchor: PathBuf },
    Ancestor { next_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CatalogEntry {
    Dir {
        name: String,
        skill_md: Option<(u64, u64)>,
    },
    File {
        name: String,
        fingerprint: Option<(u64, u64)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CatalogSnapshot {
    Absent,
    Present(BTreeSet<CatalogEntry>),
}

struct RootWatchState {
    root: SkillRoot,
    owners: HashSet<String>,
    mode: Option<RootWatchMode>,
    last_stable: Option<CatalogSnapshot>,
    pending: Option<(CatalogSnapshot, Instant)>,
}

struct WatchState {
    roots: HashMap<PathBuf, RootWatchState>,
    projects: VecDeque<(PathBuf, HashSet<PathBuf>)>,
}

struct WatchShared {
    stop: AtomicBool,
    wake_tx: Mutex<Option<Sender<()>>>,
    state: Mutex<WatchState>,
    invalidate: Arc<dyn Fn() + Send + Sync>,
    poll_interval: Duration,
    stability: Duration,
    max_projects: usize,
    follow_symlinks: bool,
    enabled: bool,
}

/// Owns the poll thread and the bounded set of observed skill roots.
pub(crate) struct WatchManager {
    shared: Arc<WatchShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WatchManager {
    /// Start polling when `config.watch` is true. `invalidate` must rescan the
    /// current discovery roots and replace this provider's registrations.
    pub(crate) fn start(config: &Config, invalidate: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (wake_tx, wake_rx) = mpsc::channel();
        let shared = Arc::new(WatchShared {
            stop: AtomicBool::new(false),
            wake_tx: Mutex::new(Some(wake_tx)),
            state: Mutex::new(WatchState {
                roots: HashMap::new(),
                projects: VecDeque::new(),
            }),
            invalidate,
            poll_interval: Duration::from_millis(config.watch_poll_interval_ms),
            stability: Duration::from_millis(config.watch_stability_threshold_ms),
            max_projects: config.watch_max_projects,
            follow_symlinks: config.watch_follow_symlinks,
            enabled: config.watch,
        });
        let thread = if config.watch {
            let worker = Arc::clone(&shared);
            Some(thread::spawn(move || poll_loop(worker, wake_rx)))
        } else {
            None
        };
        Self {
            shared,
            thread: Mutex::new(thread),
        }
    }

    /// Observe the current discovery roots, evicting the oldest project when
    /// the LRU exceeds `watchMaxProjects`.
    pub(crate) fn observe_roots(&self, discovered: &[SkillRoot]) {
        if self.shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let mut evicted = false;
        {
            let mut state = self.shared.state.lock().expect("skill-filesystem watch");
            let mut project_groups: HashMap<PathBuf, Vec<SkillRoot>> = HashMap::new();
            for root in discovered {
                match &root.project_root {
                    Some(project) => project_groups
                        .entry(project.clone())
                        .or_default()
                        .push(root.clone()),
                    None => {
                        retain_root(&mut state, root, &format!("shared:{}", root.path.display()))
                    }
                }
            }
            for (project, grouped) in project_groups {
                let owner = format!("project:{}", project.display());
                let paths: HashSet<PathBuf> =
                    grouped.iter().map(|root| root.path.clone()).collect();
                state.projects.retain(|(existing, _)| existing != &project);
                state.projects.push_back((project, paths));
                for root in grouped {
                    retain_root(&mut state, &root, &owner);
                }
            }
            while state.projects.len() > self.shared.max_projects {
                if let Some((project, paths)) = state.projects.pop_front() {
                    let owner = format!("project:{}", project.display());
                    for path in paths {
                        release_root(&mut state, &path, &owner);
                    }
                    evicted = true;
                }
            }
            for root in discovered {
                prime_root(&mut state, root, self.shared.follow_symlinks);
            }
        }
        if evicted {
            (self.shared.invalidate)();
        }
        if self.shared.enabled {
            self.wake();
        }
    }

    /// Synchronously invalidate when a first-party write/edit hits a watched skill path.
    pub(crate) fn observe_host_mutation(&self, path: &Path) {
        if self.shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let state = self.shared.state.lock().expect("skill-filesystem watch");
        if !state
            .roots
            .values()
            .any(|entry| is_potential_skill_path(&entry.root, path))
        {
            return;
        }
        drop(state);
        (self.shared.invalidate)();
    }

    /// Whether `path` is a catalog-relevant entry under any current discovery root.
    pub(crate) fn is_skill_path(config: &Config, path: &Path) -> bool {
        roots(config)
            .iter()
            .any(|root| is_potential_skill_path(root, path))
    }

    /// Stop the poll thread and wait until it exits.
    pub(crate) fn dispose(&self) {
        shutdown(&self.shared, &self.thread);
    }

    fn wake(&self) {
        if let Some(tx) = self
            .shared
            .wake_tx
            .lock()
            .expect("skill-filesystem wake")
            .as_ref()
        {
            let _ = tx.send(());
        }
    }

    #[cfg(test)]
    pub(crate) fn watched_projects(&self) -> Vec<PathBuf> {
        self.shared
            .state
            .lock()
            .expect("skill-filesystem watch")
            .projects
            .iter()
            .map(|(project, _)| project.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn watched_root_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = self
            .shared
            .state
            .lock()
            .expect("skill-filesystem watch")
            .roots
            .keys()
            .cloned()
            .collect();
        paths.sort();
        paths
    }

    #[cfg(test)]
    pub(crate) fn thread_alive(&self) -> bool {
        self.thread
            .lock()
            .expect("skill-filesystem thread")
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }
}

impl Drop for WatchManager {
    fn drop(&mut self) {
        shutdown(&self.shared, &self.thread);
    }
}

fn shutdown(shared: &WatchShared, thread: &Mutex<Option<JoinHandle<()>>>) {
    shared.stop.store(true, Ordering::SeqCst);
    if let Some(tx) = shared.wake_tx.lock().expect("skill-filesystem wake").take() {
        drop(tx);
    }
    if let Some(handle) = thread.lock().expect("skill-filesystem thread").take() {
        if thread::current().id() != handle.thread().id() {
            let _ = handle.join();
        }
    }
}

fn retain_root(state: &mut WatchState, root: &SkillRoot, owner: &str) {
    let entry = state
        .roots
        .entry(root.path.clone())
        .or_insert_with(|| RootWatchState {
            root: root.clone(),
            owners: HashSet::new(),
            mode: None,
            last_stable: None,
            pending: None,
        });
    entry.root = root.clone();
    entry.owners.insert(owner.to_string());
}

fn release_root(state: &mut WatchState, path: &Path, owner: &str) {
    let Some(entry) = state.roots.get_mut(path) else {
        return;
    };
    entry.owners.remove(owner);
    if entry.owners.is_empty() {
        state.roots.remove(path);
    }
}

fn prime_root(state: &mut WatchState, root: &SkillRoot, follow: bool) {
    let Some(entry) = state.roots.get_mut(&root.path) else {
        return;
    };
    if entry.mode.is_some() {
        return;
    }
    let mode = resolve_watch_mode(&root.path, follow);
    if matches!(mode, RootWatchMode::Root { .. }) {
        if let Ok(snapshot) = read_catalog_snapshot(&entry.root, follow) {
            entry.last_stable = Some(snapshot);
        }
    }
    entry.mode = Some(mode);
}

fn poll_loop(shared: Arc<WatchShared>, wake_rx: mpsc::Receiver<()>) {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        match wake_rx.recv_timeout(shared.poll_interval) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                if poll_once(&shared) {
                    (shared.invalidate)();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn poll_once(shared: &WatchShared) -> bool {
    let mut state = shared.state.lock().expect("skill-filesystem watch");
    let now = Instant::now();
    let mut dirty = false;
    let paths: Vec<PathBuf> = state.roots.keys().cloned().collect();
    for path in paths {
        let Some(entry) = state.roots.get_mut(&path) else {
            continue;
        };
        if entry.owners.is_empty() {
            continue;
        }
        let current_mode = resolve_watch_mode(&entry.root.path, shared.follow_symlinks);
        if entry.mode.as_ref() != Some(&current_mode) {
            entry.mode = Some(current_mode.clone());
            entry.pending = None;
            entry.last_stable = None;
            dirty = true;
        }
        match current_mode {
            RootWatchMode::Ancestor { .. } => {}
            RootWatchMode::Root { .. } => {
                match read_catalog_snapshot(&entry.root, shared.follow_symlinks) {
                    Ok(snapshot) => {
                        if entry.last_stable.as_ref() == Some(&snapshot) {
                            entry.pending = None;
                            continue;
                        }
                        match &entry.pending {
                            Some((pending, since)) if pending == &snapshot => {
                                if now.duration_since(*since) >= shared.stability {
                                    entry.last_stable = Some(snapshot);
                                    entry.pending = None;
                                    dirty = true;
                                }
                            }
                            _ => {
                                entry.pending = Some((snapshot, now));
                            }
                        }
                    }
                    Err(_) => {
                        // Transient host errors are not evidence of deletion.
                    }
                }
            }
        }
    }
    dirty
}

fn resolve_watch_mode(root: &Path, follow_symlinks: bool) -> RootWatchMode {
    let mut candidate = root.to_path_buf();
    loop {
        if candidate.is_dir() {
            let preserve_link = !follow_symlinks
                && candidate == root
                && candidate
                    .symlink_metadata()
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false);
            let anchor = if preserve_link {
                candidate.clone()
            } else {
                candidate
                    .canonicalize()
                    .unwrap_or_else(|_| candidate.clone())
            };
            if candidate == root {
                return RootWatchMode::Root { anchor };
            }
            let first = root
                .strip_prefix(&candidate)
                .ok()
                .and_then(|relative| relative.components().next())
                .map(|component| component.as_os_str().to_os_string());
            if let Some(segment) = first {
                return RootWatchMode::Ancestor {
                    next_path: anchor.join(segment),
                };
            }
            return RootWatchMode::Root { anchor };
        }
        match candidate.parent() {
            Some(parent) if parent != candidate.as_path() => candidate = parent.to_path_buf(),
            _ => {
                return RootWatchMode::Ancestor {
                    next_path: root.to_path_buf(),
                };
            }
        }
    }
}

fn read_catalog_snapshot(root: &SkillRoot, follow_symlinks: bool) -> io::Result<CatalogSnapshot> {
    let read = match fs::read_dir(&root.path) {
        Ok(read) => read,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(CatalogSnapshot::Absent);
        }
        Err(error) => return Err(error),
    };
    let mut entries = BTreeSet::new();
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if root.skip_system && name == ".system" {
            continue;
        }
        let path = entry.path();
        let metadata = if follow_symlinks {
            fs::metadata(&path)
        } else {
            fs::symlink_metadata(&path)
        };
        let Ok(metadata) = metadata else {
            continue;
        };
        if metadata.is_dir() {
            entries.insert(CatalogEntry::Dir {
                name,
                skill_md: file_fingerprint(&path.join("SKILL.md")),
            });
        } else if metadata.is_file() && name.ends_with(".md") {
            entries.insert(CatalogEntry::File {
                name,
                fingerprint: file_fingerprint(&path),
            });
        }
    }
    Ok(CatalogSnapshot::Present(entries))
}

fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let bytes = fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some((bytes.len() as u64, hasher.finish()))
}
