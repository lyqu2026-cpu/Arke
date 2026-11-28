//! Where Arke keeps its files.
//!
//! Fork: Arke's own data lives in the portable `data` folder next to the exe
//! (`crate::settings::data_dir`), not in the user profile, so the whole install can move
//! with its settings. That is what `roaming_dir` and `local_dir` return here: every file
//! the fork already kept portable (settings, saved workspaces, the shelf, folder views,
//! search history, trusted host keys, the log, the search index cache, downloaded updates)
//! flows through them and lands in `data/`.
//!
//! Three things stay in `%LOCALAPPDATA%\Arke`, as the fork has always kept them there:
//! the thumbnail disk cache, the pinned menu commands' pictures, and the recycle bin
//! usage records. They are rebuildable caches or records that belong to this PC, and the
//! uninstaller does not expect them in `data/`.
//!
//! `%TEMP%\Arke` is unchanged: downloads from remote places. The `%APPDATA%` and
//! `%LOCALAPPDATA%` roots themselves (`roaming_root`, `local_root`) are unchanged too:
//! they point at the real profile, for things outside Arke's own folder like the Start
//! menu shortcuts.
//!
//! `data_dir` reads the exe path on every call, never cached, and does no file system
//! access (string work only), so it is safe to call from any thread. Every function
//! gives `None` when its root isn't available. Callers that must stay out of the real
//! profile in tests (`cfg!(test)`) check that themselves.

use std::path::PathBuf;

/// The folder name under each root.
pub(crate) const APP_DIR: &str = "Arke";

/// `%APPDATA%` itself (the roaming profile), for what lives outside Arke's folder, like the
/// Start menu shortcuts.
pub(crate) fn roaming_root() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

/// `%LOCALAPPDATA%` itself, for what lives outside Arke's folder, like other programs'
/// install folders.
pub(crate) fn local_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// The real `%LOCALAPPDATA%\Arke` folder, for the caches that stay in the profile.
fn profile_local_dir() -> Option<PathBuf> {
    local_root().map(|root| root.join(APP_DIR))
}

/// Fork: `<exe dir>\data`, the portable data folder. Every file Arke owns lives here.
pub(crate) fn roaming_dir() -> Option<PathBuf> {
    crate::settings::data_dir()
}

/// Fork: `<exe dir>\data`, the portable data folder (same as `roaming_dir`).
pub(crate) fn local_dir() -> Option<PathBuf> {
    crate::settings::data_dir()
}

/// `<exe dir>\data\<name>`: a file next to the settings.
pub(crate) fn roaming_file(name: &str) -> Option<PathBuf> {
    roaming_dir().map(|dir| dir.join(name))
}

/// `<exe dir>\data\<name>`.
pub(crate) fn local_file(name: &str) -> Option<PathBuf> {
    local_dir().map(|dir| dir.join(name))
}

/// `<exe dir>\data\settings.json`.
pub(crate) fn settings_file() -> Option<PathBuf> {
    roaming_file("settings.json")
}

/// `<exe dir>\data\known_hosts`: the SFTP host keys the user trusted.
pub(crate) fn known_hosts_file() -> Option<PathBuf> {
    roaming_file("known_hosts")
}

/// `<exe dir>\data\arke.log`.
pub(crate) fn log_file() -> Option<PathBuf> {
    local_file("arke.log")
}

/// `<exe dir>\data\index.bin`: the search index kept between runs.
pub(crate) fn index_cache_file() -> Option<PathBuf> {
    local_file("index.bin")
}

/// `%LOCALAPPDATA%\Arke\menu-icons.bin`: the pinned menu commands' pictures. Stays in the
/// profile: a rebuildable cache, not Arke's own data.
pub(crate) fn menu_icons_file() -> Option<PathBuf> {
    profile_local_dir().map(|dir| dir.join("menu-icons.bin"))
}

/// `%LOCALAPPDATA%\Arke\thumbs`: the thumbnail disk cache. Stays in the profile.
pub(crate) fn thumbs_dir() -> Option<PathBuf> {
    profile_local_dir().map(|dir| dir.join("thumbs"))
}

/// `<exe dir>\data\updates`: downloaded updates.
pub(crate) fn updates_dir() -> Option<PathBuf> {
    local_file("updates")
}

/// `%LOCALAPPDATA%\Arke\bin-usage`: the recycle bin usage records. Stays in the profile:
/// a record that belongs to this PC.
pub(crate) fn bin_usage_dir() -> Option<PathBuf> {
    profile_local_dir().map(|dir| dir.join("bin-usage"))
}

/// `%TEMP%\Arke\remote`: where remote files are downloaded to. Always has a value (the temp
/// folder falls back to the Windows folder).
pub(crate) fn remote_cache_dir() -> PathBuf {
    std::env::temp_dir().join(APP_DIR).join("remote")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files sit under the right root, with the names other code and the uninstaller know.
    /// Nothing is written, and the assertions hold for any value of the variables, so no test
    /// changes the process's environment.
    #[test]
    fn files_sit_under_their_roots() {
        // Fork: Arke's own data lives in the portable folder next to the exe.
        if let Some(dir) = roaming_dir() {
            assert_eq!(dir, crate::settings::data_dir().unwrap());
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("data"));
            assert_eq!(settings_file(), Some(dir.join("settings.json")));
            assert_eq!(known_hosts_file(), Some(dir.join("known_hosts")));
            assert_eq!(roaming_file("shelf.json"), Some(dir.join("shelf.json")));
        } else {
            assert_eq!(settings_file(), None);
        }
        if let Some(dir) = local_dir() {
            assert_eq!(log_file(), Some(dir.join("arke.log")));
            assert_eq!(index_cache_file(), Some(dir.join("index.bin")));
            assert_eq!(updates_dir(), Some(dir.join("updates")));
        } else {
            assert_eq!(log_file(), None);
        }
        // The caches stay in the real profile, whatever it is.
        match std::env::var_os("LOCALAPPDATA") {
            Some(root) => {
                let dir = PathBuf::from(root).join(APP_DIR);
                assert_eq!(menu_icons_file(), Some(dir.join("menu-icons.bin")));
                assert_eq!(thumbs_dir(), Some(dir.join("thumbs")));
                assert_eq!(bin_usage_dir(), Some(dir.join("bin-usage")));
            }
            None => {
                assert_eq!(menu_icons_file(), None);
                assert_eq!(thumbs_dir(), None);
                assert_eq!(bin_usage_dir(), None);
            }
        }
    }

    #[test]
    fn remote_downloads_go_to_the_temp_folder() {
        assert!(remote_cache_dir().ends_with(r"Arke\remote"));
    }
}
