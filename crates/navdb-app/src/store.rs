//! Mac App Store services: the NavDB Studio Pro subscription (StoreKit) and
//! security-scoped bookmarks that keep sandbox access to files the user picked.
//!
//! Builds without the `appstore` feature (GitHub releases) have everything
//! unlocked and no sandbox, so these functions are no-ops there.

use serde::Deserialize;
use std::path::Path;

/// Saved connections allowed without a Pro subscription.
pub const FREE_CONNECTIONS: usize = 2;

pub const TERMS_URL: &str = "https://www.apple.com/legal/internet-services/itunes/dev/stdeula/";
pub const PRIVACY_URL: &str = "https://alicankuklaci.github.io/navdb-studio/privacy.html";
pub const MANAGE_URL: &str = "https://apps.apple.com/account/subscriptions";

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub description: String,
    pub price: String,
    /// "month", "year", ...
    pub period: String,
    /// Length of the free trial, empty when there is none.
    pub trial: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Status {
    pub pro: bool,
    pub busy: bool,
    pub message: String,
    pub products: Vec<Product>,
}

/// True when this build sells Pro (the App Store build).
pub const ENABLED: bool = cfg!(feature = "appstore");

#[cfg(feature = "appstore")]
mod ffi {
    use std::ffi::{c_char, CStr, CString};

    extern "C" {
        fn navdb_free(p: *mut c_char);
        fn navdb_store_start();
        fn navdb_store_is_pro() -> bool;
        fn navdb_store_status() -> *mut c_char;
        fn navdb_store_purchase(id: *const c_char);
        fn navdb_store_restore();
        fn navdb_bookmark_create(path: *const c_char) -> *mut c_char;
        fn navdb_bookmark_open(b64: *const c_char) -> *mut c_char;
    }

    fn take(p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: the bridge returns NUL-terminated strings from strdup
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { navdb_free(p) };
        Some(s)
    }

    fn cstr(s: &str) -> CString {
        CString::new(s.replace('\0', "")).unwrap_or_default()
    }

    pub fn start() {
        unsafe { navdb_store_start() }
    }
    pub fn is_pro() -> bool {
        unsafe { navdb_store_is_pro() }
    }
    pub fn status() -> Option<String> {
        take(unsafe { navdb_store_status() })
    }
    pub fn purchase(id: &str) {
        let id = cstr(id);
        unsafe { navdb_store_purchase(id.as_ptr()) }
    }
    pub fn restore() {
        unsafe { navdb_store_restore() }
    }
    pub fn bookmark_create(path: &str) -> Option<String> {
        let p = cstr(path);
        take(unsafe { navdb_bookmark_create(p.as_ptr()) })
    }
    pub fn bookmark_open(b64: &str) -> Option<String> {
        let b = cstr(b64);
        take(unsafe { navdb_bookmark_open(b.as_ptr()) })
    }
}

/// Load products and entitlements; call once at startup.
pub fn start() {
    #[cfg(feature = "appstore")]
    {
        ffi::start();
        restore_access();
    }
}

pub fn is_pro() -> bool {
    #[cfg(feature = "appstore")]
    return ffi::is_pro();
    #[cfg(not(feature = "appstore"))]
    true
}

pub fn status() -> Status {
    #[cfg(feature = "appstore")]
    return ffi::status().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    #[cfg(not(feature = "appstore"))]
    Status { pro: true, ..Default::default() }
}

pub fn purchase(_id: &str) {
    #[cfg(feature = "appstore")]
    ffi::purchase(_id);
}

pub fn restore() {
    #[cfg(feature = "appstore")]
    ffi::restore();
}

#[cfg(feature = "appstore")]
fn bookmarks_path() -> std::path::PathBuf {
    navdb_core::config::data_dir().join("bookmarks.json")
}

#[cfg(feature = "appstore")]
fn load_bookmarks() -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(bookmarks_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

#[cfg(feature = "appstore")]
fn save_bookmarks(map: &std::collections::BTreeMap<String, String>) {
    if let Ok(s) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(bookmarks_path(), s);
    }
}

/// Keep sandbox access to a file or folder the user picked in a dialog, so
/// saved connections and automation jobs still reach it after a restart.
pub fn remember(_path: &Path) {
    #[cfg(feature = "appstore")]
    {
        let key = _path.display().to_string();
        let Some(b) = ffi::bookmark_create(&key) else { return };
        let mut map = load_bookmarks();
        map.insert(key, b);
        save_bookmarks(&map);
    }
}

/// [`remember`] for a file the app keeps writing to (a database, a scheduled
/// export). The file is created when missing so it can be bookmarked. With
/// `folder`, the user is also asked for its folder: SQLite and DuckDB create
/// journal/WAL files next to the database, and `{date}` exports add new files.
pub fn keep_access(_path: &Path, _folder: bool) {
    #[cfg(feature = "appstore")]
    {
        if !_path.exists() {
            let _ = std::fs::File::create(_path);
        }
        remember(_path);
        if let Some(dir) = _folder.then(|| _path.parent()).flatten() {
            if !dir.as_os_str().is_empty() {
                let picked = rfd::FileDialog::new()
                    .set_title("Allow NavDB Studio to use this folder (needed for journal and backup files)")
                    .set_directory(dir)
                    .pick_folder();
                if let Some(d) = picked {
                    remember(&d);
                }
            }
        }
    }
}

/// Re-open every remembered bookmark; drops the ones that no longer resolve.
#[cfg(feature = "appstore")]
fn restore_access() {
    let map = load_bookmarks();
    let mut kept = std::collections::BTreeMap::new();
    for (path, b) in map {
        if let Some(now) = ffi::bookmark_open(&b) {
            kept.insert(if now.is_empty() { path } else { now }, b);
        }
    }
    save_bookmarks(&kept);
}

/// Returns true when Pro features are available; otherwise opens the
/// subscription window and returns false.
pub fn require_pro(st: &mut crate::state::AppState, feature: &str) -> bool {
    if is_pro() {
        return true;
    }
    st.open_dialog(crate::dialogs::pro::ProDialog::new(Some(feature)));
    false
}
