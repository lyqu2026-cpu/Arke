//! Bookmarks: named folders and files, in groups, kept in the sidebar.
//!
//! - A bookmark is a path with a name the user picked (or the item's own name). Groups hold
//!   bookmarks; deleting a group moves its items to the ungrouped list, never deletes them.
//! - Items that no longer exist stay listed as missing until removed. Existence is rechecked on
//!   a background thread (on activation, after file operations).
//! - Kept across restarts in `<exe dir>\data\bookmarks.json`, read and written on background
//!   threads only (like the shelf; see `shelf.rs` for why it isn't in `settings.json`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arke_core::Fs;
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::folder_views::folder_key;
use crate::saved_workspaces::clean_name;

/// A burst of changes becomes one write.
const SAVE_DELAY: Duration = Duration::from_millis(400);
/// Written into the file so a later format can tell old files apart.
const FILE_VERSION: u32 = 1;
/// First id handed out.
const FIRST_ID: u64 = 1;

/// One bookmark.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Bookmark {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) is_dir: bool,
    /// Gone from its folder at the last check (not stored)
    #[serde(skip)]
    pub(crate) missing: bool,
}

/// One group of bookmarks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BookmarkGroup {
    pub(crate) id: u64,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) items: Vec<Bookmark>,
    /// Hidden in the sidebar until the header is clicked again; kept in the file.
    #[serde(default)]
    pub(crate) collapsed: bool,
}

/// Why a name can't be used. Display is the localized message shown under the name field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NameProblem {
    #[error("{}", rust_i18n::t!("bookmarks.name_empty"))]
    Empty,
    #[error("{}", rust_i18n::t!("bookmarks.name_taken"))]
    Taken,
}

/// The name a new bookmark gets: the item's own name, or the full path for a drive root.
pub(crate) fn default_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The bookmarks themselves, without GPUI or files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bookmarks {
    next_id: u64,
    groups: Vec<BookmarkGroup>,
    ungrouped: Vec<Bookmark>,
}

impl Bookmarks {
    /// The groups, in order.
    pub(crate) fn groups(&self) -> &[BookmarkGroup] {
        &self.groups
    }

    /// The bookmarks in no group.
    pub(crate) fn ungrouped(&self) -> &[Bookmark] {
        &self.ungrouped
    }

    pub(crate) fn len(&self) -> usize {
        self.groups.iter().map(|g| g.items.len()).sum::<usize>() + self.ungrouped.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn take_id(&mut self) -> u64 {
        if self.next_id < FIRST_ID {
            self.next_id = FIRST_ID;
        }
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Where the bookmark with `id` lives: the group index (`None` for ungrouped) and its
    /// index inside that list.
    fn position(&self, id: u64) -> Option<(Option<usize>, usize)> {
        for (gi, group) in self.groups.iter().enumerate() {
            if let Some(ix) = group.items.iter().position(|b| b.id == id) {
                return Some((Some(gi), ix));
            }
        }
        self.ungrouped
            .iter()
            .position(|b| b.id == id)
            .map(|ix| (None, ix))
    }

    fn contains_path(&self, path: &Path) -> bool {
        let key = folder_key(path);
        self.groups
            .iter()
            .any(|g| g.items.iter().any(|b| folder_key(&b.path) == key))
            || self.ungrouped.iter().any(|b| folder_key(&b.path) == key)
    }

    /// Whether the path is bookmarked already (any group, any case or spelling).
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.contains_path(path)
    }

    /// Adds a bookmark to the ungrouped list. Already bookmarked (same path in any case or
    /// spelling) is not added again. Returns whether it was new.
    pub(crate) fn add(&mut self, path: &Path, is_dir: bool) -> bool {
        self.add_to_group(None, path, is_dir)
    }

    /// Adds a bookmark to `group` (`None` for ungrouped). Returns whether it was new.
    pub(crate) fn add_to_group(&mut self, group: Option<u64>, path: &Path, is_dir: bool) -> bool {
        if self.contains_path(path) {
            return false;
        }
        let bookmark = Bookmark {
            id: self.take_id(),
            name: default_name(path),
            path: path.to_path_buf(),
            is_dir,
            missing: false,
        };
        match group {
            Some(id) => {
                let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
                    return false;
                };
                group.items.push(bookmark);
            }
            None => self.ungrouped.push(bookmark),
        }
        true
    }

    /// Removes a bookmark. Returns whether it was there.
    pub(crate) fn remove(&mut self, id: u64) -> bool {
        let Some((group, ix)) = self.position(id) else {
            return false;
        };
        match group {
            Some(gi) => {
                self.groups[gi].items.remove(ix);
            }
            None => {
                self.ungrouped.remove(ix);
            }
        }
        true
    }

    /// Renames a bookmark. An empty name is refused.
    pub(crate) fn rename(&mut self, id: u64, name: &str) -> Result<(), NameProblem> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err(NameProblem::Empty);
        }
        let Some((group, ix)) = self.position(id) else {
            return Ok(());
        };
        match group {
            Some(gi) => self.groups[gi].items[ix].name = name,
            None => self.ungrouped[ix].name = name,
        }
        Ok(())
    }

    /// Moves a bookmark to `group` (`None` for ungrouped). Returns whether it moved.
    pub(crate) fn move_to_group(&mut self, id: u64, group: Option<u64>) -> bool {
        let target_ix = match group {
            Some(id) => {
                let Some(ix) = self.groups.iter().position(|g| g.id == id) else {
                    return false;
                };
                Some(ix)
            }
            None => None,
        };
        let Some((from, ix)) = self.position(id) else {
            return false;
        };
        if from == target_ix {
            return false;
        }
        let bookmark = match from {
            Some(gi) => self.groups[gi].items.remove(ix),
            None => self.ungrouped.remove(ix),
        };
        match group {
            Some(id) => {
                let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
                    return false;
                };
                group.items.push(bookmark);
            }
            None => self.ungrouped.push(bookmark),
        }
        true
    }

    /// Moves a bookmark one step within its own list. Returns whether it moved.
    pub(crate) fn move_within(&mut self, id: u64, up: bool) -> bool {
        let Some((group, ix)) = self.position(id) else {
            return false;
        };
        let list = match group {
            Some(gi) => &mut self.groups[gi].items,
            None => &mut self.ungrouped,
        };
        if up {
            if ix == 0 {
                return false;
            }
            list.swap(ix, ix - 1);
        } else {
            if ix + 1 >= list.len() {
                return false;
            }
            list.swap(ix, ix + 1);
        }
        true
    }

    /// Creates a group. Group names are unique (any case). Returns the new group's id.
    pub(crate) fn add_group(&mut self, name: &str) -> Result<u64, NameProblem> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err(NameProblem::Empty);
        }
        if self.groups.iter().any(|g| same_name(&g.name, &name)) {
            return Err(NameProblem::Taken);
        }
        let id = self.take_id();
        self.groups.push(BookmarkGroup {
            id,
            name,
            items: Vec::new(),
            collapsed: false,
        });
        Ok(id)
    }

    /// Renames a group. Group names are unique (any case); changing only the case is allowed.
    pub(crate) fn rename_group(&mut self, id: u64, name: &str) -> Result<(), NameProblem> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err(NameProblem::Empty);
        }
        if self
            .groups
            .iter()
            .any(|g| g.id != id && same_name(&g.name, &name))
        {
            return Err(NameProblem::Taken);
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.name = name;
        }
        Ok(())
    }

    /// Deletes a group; its items move to the ungrouped list, in order. Returns whether the
    /// group was there.
    pub(crate) fn delete_group(&mut self, id: u64) -> bool {
        let Some(ix) = self.groups.iter().position(|g| g.id == id) else {
            return false;
        };
        let group = self.groups.remove(ix);
        self.ungrouped.extend(group.items);
        true
    }

    /// Flips a group's collapsed state. Returns whether the group was there.
    pub(crate) fn toggle_group_collapsed(&mut self, id: u64) -> bool {
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return false;
        };
        group.collapsed = !group.collapsed;
        true
    }

    /// Moves a group one step in the group list. Returns whether it moved.
    pub(crate) fn move_group(&mut self, id: u64, up: bool) -> bool {
        let Some(ix) = self.groups.iter().position(|g| g.id == id) else {
            return false;
        };
        if up {
            if ix == 0 {
                return false;
            }
            self.groups.swap(ix, ix - 1);
        } else {
            if ix + 1 >= self.groups.len() {
                return false;
            }
            self.groups.swap(ix, ix + 1);
        }
        true
    }

    /// Applies an existence check: `found` holds `Some(is_dir)` for items that exist. Missing
    /// items stay listed, flagged. Returns whether anything changed that is stored in the file.
    fn apply_check(&mut self, found: &[(PathBuf, Option<bool>)]) -> bool {
        let mut stored_changed = false;
        for (path, state) in found {
            let key = folder_key(path);
            let mut item = None;
            for group in self.groups.iter_mut() {
                if let Some(found) = group.items.iter_mut().find(|b| folder_key(&b.path) == key) {
                    item = Some(found);
                    break;
                }
            }
            if item.is_none() {
                item = self
                    .ungrouped
                    .iter_mut()
                    .find(|b| folder_key(&b.path) == key);
            }
            let Some(item) = item else {
                continue;
            };
            match state {
                None => item.missing = true,
                Some(is_dir) => {
                    item.missing = false;
                    stored_changed |= item.is_dir != *is_dir;
                    item.is_dir = *is_dir;
                }
            }
        }
        stored_changed
    }
}

/// Errors reading or writing the bookmarks file. Logged, never shown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BookmarksError {
    /// The file exists but could not be read.
    #[error("could not read {}: {source}", .path.display())]
    Read {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a bookmarks file.
    #[error("{} is not a bookmarks file: {source}", .path.display())]
    Parse {
        /// The file being read.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
    /// The bookmarks could not be serialized.
    #[error("could not encode the bookmarks: {0}")]
    Encode(#[source] serde_json::Error),
    /// The file could not be written.
    #[error("could not write {}: {source}", .path.display())]
    Write {
        /// The file being written.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// The file as written.
#[derive(Serialize)]
struct StoredFile<'a> {
    version: u32,
    next_id: u64,
    groups: &'a [BookmarkGroup],
    ungrouped: &'a [Bookmark],
}

/// Parses the file; unreadable items (a mangled hand edit) are skipped one by one.
fn parse(bytes: &[u8], path: &Path) -> Result<Bookmarks, BookmarksError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| BookmarksError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let mut bookmarks = Bookmarks::default();
    bookmarks.next_id = value
        .get("next_id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let mut max_id = 0;
    if let Some(list) = value.get("groups").and_then(serde_json::Value::as_array) {
        for v in list {
            // A group with one bad item still loads: read the group fields and
            // each item on its own instead of failing the whole group.
            let Some(obj) = v.as_object() else {
                continue;
            };
            let (Some(id), Some(name)) = (
                obj.get("id").and_then(serde_json::Value::as_u64),
                obj.get("name").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let mut group = BookmarkGroup {
                id,
                name: name.to_owned(),
                items: Vec::new(),
                collapsed: obj
                    .get("collapsed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            };
            if let Some(items) = obj.get("items").and_then(serde_json::Value::as_array) {
                for iv in items {
                    let Ok(item) = serde_json::from_value::<Bookmark>(iv.clone()) else {
                        continue;
                    };
                    group.items.push(item);
                }
            }
            // Drop bad or repeated items inside the group, keeping the rest.
            let mut kept = Vec::new();
            for item in group.items.drain(..) {
                if item.name.is_empty() || bookmarks.contains_path(&item.path) {
                    continue;
                }
                if kept
                    .iter()
                    .any(|b: &Bookmark| folder_key(&b.path) == folder_key(&item.path))
                {
                    continue;
                }
                max_id = max_id.max(item.id);
                kept.push(item);
            }
            group.items = kept;
            max_id = max_id.max(group.id);
            bookmarks.groups.push(group);
        }
    }
    if let Some(list) = value.get("ungrouped").and_then(serde_json::Value::as_array) {
        for v in list {
            let Ok(item) = serde_json::from_value::<Bookmark>(v.clone()) else {
                continue;
            };
            if item.name.is_empty() || bookmarks.contains_path(&item.path) {
                continue;
            }
            max_id = max_id.max(item.id);
            bookmarks.ungrouped.push(item);
        }
    }
    // Never hand out an id twice, even when the file's counter is stale.
    bookmarks.next_id = bookmarks.next_id.max(max_id + 1).max(FIRST_ID);
    Ok(bookmarks)
}

/// Reads the file; a missing file means no bookmarks yet.
pub(crate) fn load(path: &Path) -> Result<Bookmarks, BookmarksError> {
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes, path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Bookmarks::default()),
        Err(source) => Err(BookmarksError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Writes through a temporary file and a rename.
fn save(path: &Path, bookmarks: &Bookmarks) -> Result<(), BookmarksError> {
    let write_err = |source| BookmarksError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(write_err)?;
    }
    let file = StoredFile {
        version: FILE_VERSION,
        next_id: bookmarks.next_id,
        groups: &bookmarks.groups,
        ungrouped: &bookmarks.ungrouped,
    };
    let json = serde_json::to_vec_pretty(&file).map_err(BookmarksError::Encode)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

/// `<exe dir>\data\bookmarks.json`, next to the settings.
pub(crate) fn default_path() -> Option<PathBuf> {
    crate::settings::default_path().map(|p| p.with_file_name("bookmarks.json"))
}

/// The app-wide bookmarks, shared by every window and tab.
pub(crate) struct BookmarkStore {
    bookmarks: Bookmarks,
    path: Option<PathBuf>,
    /// The file has been read
    loaded: bool,
    /// Removed or renamed before the file arrived: those changes win over the file
    edited: bool,
    writer: Option<flume::Sender<Bookmarks>>,
    save_task: Option<Task<()>>,
    check_task: Option<Task<()>>,
}

struct GlobalBookmarks(Entity<BookmarkStore>);

impl Global for GlobalBookmarks {}

impl BookmarkStore {
    /// Creates the bookmarks and reads the file in the background. `None` keeps them in memory
    /// only (tests, benchmarks).
    pub(crate) fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            bookmarks: Bookmarks::default(),
            path: path.clone(),
            loaded: path.is_none(),
            edited: false,
            writer: None,
            save_task: None,
            check_task: None,
        });
        cx.set_global(GlobalBookmarks(store.clone()));
        let Some(path) = path else {
            return store;
        };
        let (tx, rx) = flume::bounded::<Bookmarks>(1);
        crate::explorer::spawn_thread("bookmarks-load", move || {
            let bookmarks = load(&path).unwrap_or_else(|err| {
                log::warn!("{err}");
                Bookmarks::default()
            });
            let _ = tx.send(bookmarks);
        });
        let weak = store.downgrade();
        cx.spawn(async move |cx| {
            let bookmarks = rx.recv_async().await.unwrap_or_default();
            let _ = weak.update(cx, |store, cx| store.finish_loading(bookmarks, cx));
        })
        .detach();
        store
    }

    /// The app-wide bookmarks; in memory only if nothing set it up (GPUI tests).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalBookmarks>() {
            Some(g) => g.0.clone(),
            None => Self::init(None, cx),
        }
    }

    /// The app-wide bookmarks if they exist, for read-only use.
    pub(crate) fn global_ref(cx: &App) -> Option<&Self> {
        cx.try_global::<GlobalBookmarks>().map(|g| g.0.read(cx))
    }

    /// Items added before the file arrived go after the loaded ones; a removal or rename in the
    /// meantime drops the file's items. Groups made in the meantime are remade by name (a group
    /// whose name is taken lands its items in the ungrouped list instead).
    fn finish_loading(&mut self, mut loaded: Bookmarks, cx: &mut Context<Self>) {
        if !self.edited {
            let pending = std::mem::take(&mut self.bookmarks);
            for group in pending.groups {
                match loaded.add_group(&group.name) {
                    Ok(id) => {
                        for item in group.items {
                            loaded.add_to_group(Some(id), &item.path, item.is_dir);
                        }
                    }
                    Err(_) => {
                        for item in group.items {
                            loaded.add(&item.path, item.is_dir);
                        }
                    }
                }
            }
            for item in pending.ungrouped {
                loaded.add(&item.path, item.is_dir);
            }
            self.bookmarks = loaded;
        }
        self.loaded = true;
        cx.notify();
    }

    /// The bookmarks themselves.
    pub(crate) const fn bookmarks(&self) -> &Bookmarks {
        &self.bookmarks
    }

    /// Adds a bookmark to the ungrouped list. Returns whether it was new.
    pub(crate) fn add(&mut self, path: &Path, is_dir: bool, cx: &mut Context<Self>) -> bool {
        let added = self.bookmarks.add(path, is_dir);
        if added {
            self.schedule_save(cx);
            cx.notify();
        }
        added
    }

    /// Removes a bookmark.
    pub(crate) fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.bookmarks.remove(id) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Renames a bookmark.
    pub(crate) fn rename(
        &mut self,
        id: u64,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), NameProblem> {
        let result = self.bookmarks.rename(id, name);
        if result.is_ok() {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
        result
    }

    /// Moves a bookmark to a group (`None` for ungrouped).
    pub(crate) fn move_to_group(&mut self, id: u64, group: Option<u64>, cx: &mut Context<Self>) {
        if self.bookmarks.move_to_group(id, group) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Moves a bookmark one step within its own list.
    pub(crate) fn move_within(&mut self, id: u64, up: bool, cx: &mut Context<Self>) {
        if self.bookmarks.move_within(id, up) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Creates a group. Returns the new group's id.
    pub(crate) fn add_group(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<u64, NameProblem> {
        let result = self.bookmarks.add_group(name);
        if result.is_ok() {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
        result
    }

    /// Renames a group.
    pub(crate) fn rename_group(
        &mut self,
        id: u64,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), NameProblem> {
        let result = self.bookmarks.rename_group(id, name);
        if result.is_ok() {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
        result
    }

    /// Deletes a group; its items move to the ungrouped list.
    pub(crate) fn delete_group(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.bookmarks.delete_group(id) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Moves a group one step in the group list.
    pub(crate) fn move_group(&mut self, id: u64, up: bool, cx: &mut Context<Self>) {
        if self.bookmarks.move_group(id, up) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Flips a group's collapsed state; the state is saved with the bookmarks.
    pub(crate) fn toggle_group_collapsed(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.bookmarks.toggle_group_collapsed(id) {
            self.edited = true;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Checks on a background thread which bookmarks still exist.
    pub(crate) fn recheck(&mut self, fs: Arc<dyn Fs>, cx: &mut Context<Self>) {
        if self.bookmarks.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = self
            .bookmarks
            .groups
            .iter()
            .flat_map(|g| g.items.iter().map(|b| b.path.clone()))
            .chain(self.bookmarks.ungrouped.iter().map(|b| b.path.clone()))
            .collect();
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("bookmarks-check", move || {
            let found: Vec<(PathBuf, Option<bool>)> = paths
                .into_iter()
                .map(|p| {
                    let state = fs.stat(&p).ok().map(|e| e.is_dir_like());
                    (p, state)
                })
                .collect();
            let _ = tx.send(found);
        });
        self.check_task = Some(cx.spawn(async move |this, cx| {
            let Ok(found) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |store, cx| {
                let before = store.bookmarks.clone();
                if store.bookmarks.apply_check(&found) {
                    store.edited = true;
                    store.schedule_save(cx);
                }
                if store.bookmarks != before {
                    cx.notify();
                }
            });
        }));
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.path.is_none() {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |store, _| store.send_to_writer());
        }));
    }

    fn send_to_writer(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let bookmarks = self.bookmarks.clone();
        let writer = self.writer.get_or_insert_with(|| {
            let (tx, rx) = flume::unbounded::<Bookmarks>();
            crate::explorer::spawn_thread("bookmarks-save", move || {
                while let Ok(mut bookmarks) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        bookmarks = newer;
                    }
                    if let Err(err) = save(&path, &bookmarks) {
                        log::warn!("{err}");
                    }
                }
            });
            tx
        });
        if writer.send(bookmarks).is_err() {
            self.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn adds_once_with_default_names() {
        let mut bookmarks = Bookmarks::default();
        assert!(bookmarks.add(Path::new(r"C:\docs\report.docx"), false));
        assert!(bookmarks.add(Path::new(r"D:\photos"), true));
        // Same path in another case: not added again.
        assert!(!bookmarks.add(Path::new(r"c:\DOCS\REPORT.docx"), false));
        assert_eq!(bookmarks.len(), 2);
        let first_id = bookmarks.ungrouped()[0].id;
        let first_name = bookmarks.ungrouped()[0].name.clone();
        let first_is_dir = bookmarks.ungrouped()[0].is_dir;
        assert_eq!(first_name, "report.docx");
        assert_eq!(first_id, 1);
        assert!(!first_is_dir);
        assert!(bookmarks.ungrouped()[1].is_dir);
        assert!(bookmarks.contains(Path::new(r"C:\docs\report.docx")));
        assert!(!bookmarks.contains(Path::new(r"C:\docs\other.docx")));
        assert!(bookmarks.remove(first_id));
        assert!(!bookmarks.remove(first_id));
        assert_eq!(bookmarks.len(), 1);
    }

    #[test]
    fn groups_keep_their_items_on_delete() {
        let mut bookmarks = Bookmarks::default();
        let work = bookmarks.add_group("Work").unwrap();
        let personal = bookmarks.add_group("Personal").unwrap();
        assert!(bookmarks.add_to_group(Some(work), Path::new(r"C:\a"), true));
        assert!(bookmarks.add_to_group(Some(work), Path::new(r"C:\b"), true));
        assert!(bookmarks.add_to_group(Some(personal), Path::new(r"C:\c"), true));
        assert_eq!(bookmarks.groups()[0].items.len(), 2);
        // Moving between groups and within a group.
        let moved = bookmarks.groups()[0].items[0].id;
        assert!(bookmarks.move_to_group(moved, Some(personal)));
        assert_eq!(bookmarks.groups()[0].items.len(), 1);
        assert_eq!(bookmarks.groups()[1].items.len(), 2);
        assert!(bookmarks.move_within(moved, true));
        assert_eq!(bookmarks.groups()[1].items[0].id, moved);
        assert!(!bookmarks.move_within(moved, true));
        // Deleting a group moves its items to ungrouped, in order.
        assert!(bookmarks.delete_group(personal));
        assert_eq!(bookmarks.groups().len(), 1);
        assert_eq!(bookmarks.ungrouped().len(), 2);
        assert_eq!(bookmarks.ungrouped()[0].path, p(r"C:\a"));
        assert_eq!(bookmarks.ungrouped()[1].path, p(r"C:\c"));
        // Groups reorder, and names stay unique.
        let other = bookmarks.add_group("Other").unwrap();
        assert!(bookmarks.move_group(other, true));
        assert_eq!(bookmarks.groups()[0].id, other);
        assert!(!bookmarks.move_group(other, true));
        assert_eq!(bookmarks.add_group("other"), Err(NameProblem::Taken));
        assert_eq!(bookmarks.add_group("  "), Err(NameProblem::Empty));
        assert_eq!(
            bookmarks.rename_group(work, "Other"),
            Err(NameProblem::Taken)
        );
        assert!(bookmarks.rename_group(work, "Jobs").is_ok());
        assert_eq!(bookmarks.groups()[1].name, "Jobs");
        // Renaming a bookmark refuses an empty name.
        let id = bookmarks.ungrouped()[0].id;
        assert_eq!(bookmarks.rename(id, "  "), Err(NameProblem::Empty));
        assert!(bookmarks.rename(id, "Photos").is_ok());
        assert_eq!(bookmarks.ungrouped()[0].name, "Photos");
    }

    #[test]
    fn groups_collapse_and_persist() {
        let dir = std::env::temp_dir()
            .join(format!("arke-bookmarks-collapse-{}", std::process::id()));
        let file = dir.join("bookmarks.json");
        let mut bookmarks = Bookmarks::default();
        let work = bookmarks.add_group("Work").unwrap();
        bookmarks.add_to_group(Some(work), Path::new(r"C:\a"), true);
        assert!(!bookmarks.groups()[0].collapsed);
        assert!(bookmarks.toggle_group_collapsed(work));
        assert!(bookmarks.groups()[0].collapsed);
        assert!(!bookmarks.toggle_group_collapsed(999));
        save(&file, &bookmarks).unwrap();
        let loaded = load(&file).unwrap();
        assert!(loaded.groups()[0].collapsed, "collapsed survives a restart");
        assert_eq!(loaded.groups()[0].items.len(), 1);
        // A file from before this feature (no "collapsed" key) loads expanded.
        std::fs::write(
            &file,
            br#"{"version":1,"next_id":9,"groups":[{"id":3,"name":"Work","items":[]}],"ungrouped":[]}"#,
        )
        .unwrap();
        assert!(!load(&file).unwrap().groups()[0].collapsed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_items_stay_flagged() {
        let mut bookmarks = Bookmarks::default();
        bookmarks.add(Path::new(r"C:\kept"), false);
        bookmarks.add(Path::new(r"C:\gone"), false);
        bookmarks.add(Path::new(r"C:\now_dir"), false);
        let changed = bookmarks.apply_check(&[
            (p(r"C:\kept"), Some(false)),
            (p(r"C:\gone"), None),
            (p(r"C:\now_dir"), Some(true)),
        ]);
        assert!(changed);
        let state: Vec<_> = bookmarks
            .ungrouped()
            .iter()
            .map(|b| (b.path.clone(), b.missing, b.is_dir))
            .collect();
        assert_eq!(
            state,
            [
                (p(r"C:\kept"), false, false),
                (p(r"C:\gone"), true, false),
                (p(r"C:\now_dir"), false, true),
            ]
        );
        // Back again: no longer missing.
        bookmarks.apply_check(&[(p(r"C:\gone"), Some(false))]);
        assert!(!bookmarks.ungrouped()[1].missing);
    }

    #[test]
    fn file_round_trip_skips_bad_items() {
        let dir = std::env::temp_dir().join(format!("arke-bookmarks-test-{}", std::process::id()));
        let file = dir.join("bookmarks.json");
        let mut bookmarks = Bookmarks::default();
        let group = bookmarks.add_group("Work").unwrap();
        bookmarks.add_to_group(Some(group), Path::new(r"C:\a b\c.txt"), false);
        bookmarks.add(Path::new(r"\\nas\share\d"), true);
        save(&file, &bookmarks).unwrap();
        assert_eq!(load(&file).unwrap(), bookmarks);
        std::fs::write(
            &file,
            br#"{"version":1,"next_id":9,"groups":[{"id":3,"name":"Work","items":[{"id":4,"name":"ok","path":"C:\\ok"},{"nope":1},{"id":4,"name":"dup","path":"C:\\ok"}]}],"ungrouped":[{"id":5,"name":"","path":"C:\\noname"}]}"#,
        )
        .unwrap();
        let loaded = load(&file).unwrap();
        assert_eq!(loaded.groups().len(), 1);
        assert_eq!(
            loaded.groups()[0].items.len(),
            1,
            "bad and repeated items are skipped"
        );
        assert!(loaded.ungrouped().is_empty(), "nameless items are skipped");
        // The next id never goes backwards, so a later add gets a fresh id.
        let mut loaded = loaded;
        assert!(loaded.add(Path::new(r"C:\new"), true));
        assert_eq!(loaded.ungrouped()[0].id, 9);
        assert!(load(&dir.join("absent.json")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
