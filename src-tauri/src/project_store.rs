//! Small, independent project history store.
//!
//! Projects are directories, not tasks. Keeping this state separate means a
//! directory can be opened before the first conversation exists.

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const PROJECTS_FILE: &str = "projects.json";
const RECENTLY_CLOSED_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ProjectRecord {
    pub(crate) directory: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ProjectStore {
    #[serde(default)]
    pub(crate) open: Vec<ProjectRecord>,
    #[serde(default)]
    pub(crate) recently_closed: Vec<ProjectRecord>,
}

impl ProjectStore {
    pub(crate) fn load(path: &Path) -> Option<Self> {
        let mut store = fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str::<Self>(&contents).ok())?;
        store.normalize();
        Some(store)
    }

    pub(crate) fn open(&mut self, directory: &Path, timestamp: i64) -> bool {
        let key = path_key(directory);
        let directory = display_directory(directory);
        let changed = if let Some(index) = self
            .open
            .iter()
            .position(|item| path_key(Path::new(&item.directory)) == key)
        {
            let mut item = self.open.remove(index);
            let timestamp_changed = timestamp > item.updated_at;
            let directory_changed = item.directory != directory;
            if directory_changed {
                item.directory = directory.clone();
            }
            item.updated_at = item.updated_at.max(timestamp);
            self.open.insert(0, item);
            index != 0 || timestamp_changed || directory_changed
        } else {
            self.open.insert(
                0,
                ProjectRecord {
                    directory: directory.clone(),
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            );
            true
        };
        let before = self.recently_closed.len();
        self.recently_closed
            .retain(|item| path_key(Path::new(&item.directory)) != key);
        changed || before != self.recently_closed.len()
    }

    pub(crate) fn touch(&mut self, directory: &Path, timestamp: i64) -> bool {
        let key = path_key(directory);
        let Some(index) = self
            .open
            .iter()
            .position(|item| path_key(Path::new(&item.directory)) == key)
        else {
            return false;
        };
        let mut item = self.open.remove(index);
        let timestamp_changed = timestamp > item.updated_at;
        let directory = display_directory(directory);
        let directory_changed = item.directory != directory;
        if directory_changed {
            item.directory = directory;
        }
        item.updated_at = item.updated_at.max(timestamp);
        self.open.insert(0, item);
        index != 0 || timestamp_changed || directory_changed
    }

    pub(crate) fn close(&mut self, directory: &Path, timestamp: i64) -> bool {
        let key = path_key(directory);
        let Some(index) = self
            .open
            .iter()
            .position(|item| path_key(Path::new(&item.directory)) == key)
        else {
            return false;
        };
        let mut item = self.open.remove(index);
        item.updated_at = item.updated_at.max(timestamp);
        self.recently_closed
            .retain(|entry| path_key(Path::new(&entry.directory)) != key);
        self.recently_closed.insert(0, item);
        self.recently_closed.truncate(RECENTLY_CLOSED_LIMIT);
        true
    }

    fn normalize(&mut self) {
        let mut open_keys = HashSet::with_capacity(self.open.len());
        self.open.retain(|record| {
            let key = path_key(Path::new(&record.directory));
            !key.is_empty() && open_keys.insert(key)
        });

        let mut closed_keys = HashSet::with_capacity(self.recently_closed.len());
        self.recently_closed.retain(|record| {
            let key = path_key(Path::new(&record.directory));
            !key.is_empty() && !open_keys.contains(&key) && closed_keys.insert(key)
        });
        self.recently_closed.truncate(RECENTLY_CLOSED_LIMIT);
    }
}

pub(crate) fn normalize_directory(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Project path cannot be empty.".to_string());
    }
    let path = PathBuf::from(raw);
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Unable to open project path {}: {error}", path.display()))?;
    let directory = if metadata.is_file() {
        path.parent()
            .ok_or_else(|| "The selected file has no containing directory.".to_string())?
            .to_path_buf()
    } else if metadata.is_dir() {
        path
    } else {
        return Err("The selected project path is neither a file nor a directory.".to_string());
    };
    directory.canonicalize().map_err(|error| {
        format!(
            "Unable to resolve project directory {}: {error}",
            directory.display()
        )
    })
}

pub(crate) fn path_key(path: &Path) -> String {
    let mut value = display_directory(path);
    #[cfg(windows)]
    value.make_ascii_lowercase();
    if value.len() > 1 {
        while value.ends_with('/') && !value.ends_with(":/") {
            value.pop();
        }
    }
    value
}

pub(crate) fn display_directory(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    let value = value
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| value.strip_prefix(r"\\?\").map(ToString::to_string))
        .unwrap_or_else(|| value.into_owned());
    value.replace('\\', "/")
}

pub(crate) fn display_name(directory: &Path) -> String {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| directory.display().to_string())
}

pub(crate) fn persist(path: &Path, store: &ProjectStore) -> Result<(), String> {
    let contents = serde_json::to_vec(store)
        .map_err(|error| format!("Unable to encode project state: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create project state directory: {error}"))?;
    }
    let temporary =
        path.with_file_name(format!("{}.{}.tmp", PROJECTS_FILE, Uuid::new_v4().simple()));
    let write_result = (|| -> Result<(), io::Error> {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Unable to write project state: {error}"));
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Unable to commit project state: {error}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    windows_replace::replace(temporary, destination)
}

#[cfg(windows)]
mod windows_replace {
    use std::{io, os::windows::ffi::OsStrExt, path::Path};

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    pub(super) fn replace(temporary: &Path, destination: &Path) -> io::Result<()> {
        let temporary = temporary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let destination = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let moved = unsafe {
            MoveFileExW(
                temporary.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
pub(crate) fn pick_path(kind: &str) -> Result<Option<PathBuf>, String> {
    windows_picker::pick(kind)
}

#[cfg(not(windows))]
pub(crate) fn pick_path(_kind: &str) -> Result<Option<PathBuf>, String> {
    Err("The native project picker is currently available on Windows; drop a file or folder onto Projects.".to_string())
}

#[cfg(windows)]
mod windows_picker {
    use std::{ffi::c_void, mem::size_of, ptr::null_mut, thread};

    use super::*;

    const OFN_EXPLORER: u32 = 0x0008_0000;
    const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
    const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
    const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
    const BIF_RETURNONLYFSDIRS: u32 = 0x0000_0001;
    const BIF_NEWDIALOGSTYLE: u32 = 0x0000_0040;

    type Hwnd = *mut c_void;
    type Hinstance = *mut c_void;
    type Pidl = *mut c_void;

    #[repr(C)]
    struct OpenFileNameW {
        l_struct_size: u32,
        hwnd_owner: Hwnd,
        h_instance: Hinstance,
        filter: *const u16,
        custom_filter: *mut u16,
        max_custom_filter: u32,
        filter_index: u32,
        file: *mut u16,
        max_file: u32,
        file_title: *mut u16,
        max_file_title: u32,
        initial_dir: *const u16,
        title: *const u16,
        flags: u32,
        file_offset: u16,
        file_extension: u16,
        default_extension: *const u16,
        custom_data: isize,
        hook: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> usize>,
        template_name: *const u16,
        reserved: *mut c_void,
        reserved2: u32,
        flags_ex: u32,
    }

    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: Hwnd,
        pidl_root: Pidl,
        display_name: *mut u16,
        title: *const u16,
        flags: u32,
        callback: Option<unsafe extern "system" fn(Hwnd, u32, isize, isize) -> i32>,
        callback_data: isize,
        image: i32,
    }

    #[link(name = "comdlg32")]
    unsafe extern "system" {
        fn GetOpenFileNameW(file_name: *mut OpenFileNameW) -> i32;
        fn CommDlgExtendedError() -> u32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHBrowseForFolderW(info: *mut BrowseInfoW) -> Pidl;
        fn SHGetPathFromIDListW(pidl: Pidl, path: *mut u16) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
        fn CoUninitialize();
        fn CoTaskMemFree(pointer: *mut c_void);
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn pick(kind: &str) -> Result<Option<PathBuf>, String> {
        let kind = kind.to_string();
        thread::Builder::new()
            .name("rustpilot-project-picker".to_string())
            .spawn(move || pick_on_sta_thread(&kind))
            .map_err(|error| format!("Unable to start the native project picker: {error}"))?
            .join()
            .map_err(|_| "The native project picker thread exited unexpectedly.".to_string())?
    }

    fn pick_on_sta_thread(kind: &str) -> Result<Option<PathBuf>, String> {
        let initialized = unsafe { CoInitializeEx(null_mut(), 0x2) };
        if initialized < 0 {
            return Err(format!(
                "Unable to initialize the native project picker (HRESULT 0x{:08X}).",
                initialized as u32
            ));
        }
        let result = match kind {
            "file" => pick_file(),
            "folder" => pick_folder(),
            _ => Err("Unknown project picker type.".to_string()),
        };
        unsafe { CoUninitialize() };
        result
    }

    fn pick_file() -> Result<Option<PathBuf>, String> {
        let filter = wide("All files\0*.*\0\0");
        let title = wide("Open file as project");
        let mut buffer = vec![0u16; 32_768];
        let mut dialog = OpenFileNameW {
            l_struct_size: size_of::<OpenFileNameW>() as u32,
            hwnd_owner: null_mut(),
            h_instance: null_mut(),
            filter: filter.as_ptr(),
            custom_filter: null_mut(),
            max_custom_filter: 0,
            filter_index: 1,
            file: buffer.as_mut_ptr(),
            max_file: buffer.len() as u32,
            file_title: null_mut(),
            max_file_title: 0,
            initial_dir: std::ptr::null(),
            title: title.as_ptr(),
            flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
            file_offset: 0,
            file_extension: 0,
            default_extension: std::ptr::null(),
            custom_data: 0,
            hook: None,
            template_name: std::ptr::null(),
            reserved: null_mut(),
            reserved2: 0,
            flags_ex: 0,
        };
        let selected = unsafe { GetOpenFileNameW(&mut dialog) } != 0;
        if !selected {
            let error = unsafe { CommDlgExtendedError() };
            if error == 0 {
                return Ok(None);
            }
            return Err(format!(
                "Unable to open the native file picker (error 0x{error:08X})."
            ));
        }
        let length = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16(&buffer[..length])
            .map(PathBuf::from)
            .map(Some)
            .map_err(|error| format!("Unable to decode selected file path: {error}"))
    }

    fn pick_folder() -> Result<Option<PathBuf>, String> {
        let title = wide("Open project folder");
        let mut display = vec![0u16; 32_768];
        let mut info = BrowseInfoW {
            hwnd_owner: null_mut(),
            pidl_root: null_mut(),
            display_name: display.as_mut_ptr(),
            title: title.as_ptr(),
            flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            callback: None,
            callback_data: 0,
            image: 0,
        };
        let pidl = unsafe { SHBrowseForFolderW(&mut info) };
        if pidl.is_null() {
            return Ok(None);
        }
        let mut buffer = vec![0u16; 32_768];
        let selected = unsafe { SHGetPathFromIDListW(pidl, buffer.as_mut_ptr()) } != 0;
        unsafe { CoTaskMemFree(pidl) };
        if !selected {
            return Err("Unable to read the selected project folder.".to_string());
        }
        let length = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16(&buffer[..length])
            .map(PathBuf::from)
            .map(Some)
            .map_err(|error| format!("Unable to decode selected folder path: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "rustpilot-project-store-{label}-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&directory).expect("temporary project directory should be created");
        directory
    }

    #[test]
    fn file_project_resolves_to_its_parent_directory() {
        let directory = temporary_directory("file");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("project file should be written");

        let normalized = normalize_directory(&file.to_string_lossy())
            .expect("file project should resolve to its directory");
        assert_eq!(normalized, directory.canonicalize().unwrap());

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn open_close_and_reopen_keep_one_canonical_record() {
        let directory = temporary_directory("lifecycle");
        let mut store = ProjectStore::default();

        assert!(store.open(&directory, 10));
        assert!(!store.open(&directory, 10));
        assert_eq!(store.open.len(), 1);
        assert!(store.close(&directory, 20));
        assert!(store.open.is_empty());
        assert_eq!(store.recently_closed.len(), 1);
        assert!(store.open(&directory, 30));
        assert_eq!(store.open.len(), 1);
        assert!(store.recently_closed.is_empty());

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn persisted_store_round_trips_the_latest_state() {
        let directory = temporary_directory("persist");
        let state_path = directory.join(PROJECTS_FILE);
        let first = directory.join("first");
        let second = directory.join("second");
        fs::create_dir_all(&first).expect("first project should be created");
        fs::create_dir_all(&second).expect("second project should be created");

        let mut store = ProjectStore::default();
        store.open(&first, 10);
        persist(&state_path, &store).expect("first state should persist");
        store.open(&second, 20);
        persist(&state_path, &store).expect("replacement state should persist");

        let loaded = ProjectStore::load(&state_path).expect("persisted store should load");
        assert_eq!(loaded.open.len(), 2);
        assert_eq!(
            path_key(Path::new(&loaded.open[0].directory)),
            path_key(&second)
        );

        let _ = fs::remove_dir_all(directory);
    }
}
