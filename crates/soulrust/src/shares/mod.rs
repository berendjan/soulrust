//! In-memory share index: scans configured folders into a file list, a word
//! index (token → file ids), and a virtual-folder map. Mirrors Nicotine+'s
//! `shares.py` structure (file_path_index + word_index + per-folder streams)
//! minus the on-disk pickle databases — we rebuild on scan. Pure given a
//! filesystem, so it's unit-testable against a temp directory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use soulseek_proto::peer_message::{SharedDirectory, SharedFile, SharedFileListResponse};

/// One shared file. `virtual_path` is the backslash-separated path advertised to
/// peers (e.g. `Music\\Album\\song.mp3`), rooted at the share folder's basename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFileEntry {
    pub real_path: PathBuf,
    pub virtual_path: String,
    pub size: u64,
}

/// Substituted for a literal backslash in a real filename. The Soulseek network
/// uses `\` as the path separator, so a backslash in a basename (valid on
/// non-Windows filesystems) would corrupt the advertised path. Matches
/// Nicotine+'s `Shares.BACKSLASH_SENTINEL` (shares.py).
const BACKSLASH_SENTINEL: &str = "@@BACKSLASH@@";

/// The scanned shares, indexed for search and browse.
#[derive(Debug, Default)]
pub struct ShareIndex {
    /// File entries, indexed by file id (the index into this vec).
    pub files: Vec<SharedFileEntry>,
    /// Lowercased word → the ids of files whose path contains that word.
    pub word_index: HashMap<String, Vec<u32>>,
    /// Virtual folder path → the ids of files directly in it.
    pub folders: BTreeMap<String, Vec<u32>>,
}

/// Splits a path/filename into lowercased alphanumeric tokens — Nicotine+'s
/// `TRANSLATE_PUNCTUATION` (punctuation → space) then split + lowercase.
pub fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

impl ShareIndex {
    /// Scans `folders` (public shares) into a fresh index. Hidden files and
    /// folders (names beginning with `.`) are skipped, as in `test_shares.py`.
    pub fn scan(folders: &[PathBuf]) -> ShareIndex {
        let mut index = ShareIndex::default();
        for folder in folders {
            let root = folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "share".into());
            walk(folder, &root, &mut index);
        }
        index
    }

    fn add_file(&mut self, real_path: PathBuf, virtual_path: String, size: u64) {
        let id = self.files.len() as u32;

        let mut seen = HashSet::new();
        for token in tokenize(&virtual_path) {
            if seen.insert(token.clone()) {
                self.word_index.entry(token).or_default().push(id);
            }
        }

        let folder = virtual_path
            .rsplit_once('\\')
            .map(|(dir, _)| dir.to_owned())
            .unwrap_or_default();
        self.folders.entry(folder).or_default().push(id);

        self.files.push(SharedFileEntry { real_path, virtual_path, size });
    }

    pub fn num_files(&self) -> usize {
        self.files.len()
    }

    /// The wire `SharedFile` for a file id as carried in a *search* response:
    /// the name is the full virtual path, since the requester has no enclosing
    /// directory context. Matches Nicotine+ `search.py:_create_file_info_list`,
    /// which serves `file_path_index` entries whose name is `virtual_file_path`.
    /// Attributes are deferred (empty).
    pub fn shared_file(&self, id: u32) -> SharedFile {
        let entry = &self.files[id as usize];
        SharedFile {
            name: entry.virtual_path.clone(),
            size: entry.size,
            extension: String::new(),
            attributes: Vec::new(),
        }
    }

    /// The wire `SharedFile` as carried inside a *folder stream* (browse and
    /// FolderContentsResponse): the name is the **basename** only, since the
    /// enclosing directory is already named by the stream entry. Matches
    /// Nicotine+ `shares.py:scan_shared_folder`, which stores
    /// `basename_file_data[0] = basename_escaped` in each folder stream.
    fn folder_file(&self, id: u32) -> SharedFile {
        let entry = &self.files[id as usize];
        let basename = entry.virtual_path.rsplit('\\').next().unwrap_or(&entry.virtual_path);
        SharedFile {
            name: basename.to_owned(),
            size: entry.size,
            extension: String::new(),
            attributes: Vec::new(),
        }
    }

    /// The full browsable share tree (a `SharedFileListResponse` we serve to a
    /// peer that browses us). Public shares only for now. Every scanned folder
    /// appears — including empty and intermediate ones — as Nicotine+ stores a
    /// (possibly empty) stream for each folder it visits.
    pub fn browse(&self) -> SharedFileListResponse {
        let directories = self
            .folders
            .iter()
            .map(|(path, ids)| SharedDirectory {
                path: path.clone(),
                files: ids.iter().map(|&id| self.folder_file(id)).collect(),
            })
            .collect();
        SharedFileListResponse { directories, private_directories: Vec::new() }
    }

    /// The files directly within one virtual folder (for FolderContentsResponse).
    pub fn folder_contents(&self, virtual_folder: &str) -> Vec<SharedFile> {
        self.folders
            .get(virtual_folder)
            .map(|ids| ids.iter().map(|&id| self.folder_file(id)).collect())
            .unwrap_or_default()
    }
}

fn walk(dir: &Path, virtual_prefix: &str, index: &mut ShareIndex) {
    // Every scanned folder gets a (possibly empty) entry, so intermediate and
    // empty folders still appear in the browse tree — Nicotine+ stores a stream
    // for each folder it visits (shares.py:scan_shared_folder).
    index.folders.entry(virtual_prefix.to_owned()).or_default();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Sort for deterministic ids/output.
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(std::fs::DirEntry::file_name);

    for entry in items {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue; // hidden file or folder
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // Folder names keep raw backslashes (Nicotine+'s real2virtual does
            // not escape them); the file basename below is what gets escaped.
            let virtual_path = format!("{virtual_prefix}\\{name}");
            walk(&entry.path(), &virtual_path, index);
        } else if file_type.is_file() {
            // A literal backslash in a basename would read as a path separator
            // on the wire, so substitute the sentinel — Nicotine+ does the same
            // before building `virtual_file_path` (shares.py:scan_shared_folder).
            let basename = name.replace('\\', BACKSLASH_SENTINEL);
            let virtual_path = format!("{virtual_prefix}\\{basename}");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            index.add_file(entry.path(), virtual_path, size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a temp share tree (unique per `tag`, since tests run in parallel)
    /// and returns the path to the `Music` share folder.
    fn build_share(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("soulrust-shares-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let music = root.join("Music");
        std::fs::create_dir_all(music.join("Album")).unwrap();
        std::fs::create_dir_all(music.join(".hidden")).unwrap();
        std::fs::write(music.join("song one.mp3"), b"aaaa").unwrap();
        std::fs::write(music.join("Album").join("track.flac"), b"bbbbbb").unwrap();
        std::fs::write(music.join(".secret.mp3"), b"xxxx").unwrap();
        std::fs::write(music.join(".hidden").join("nope.mp3"), b"yyyy").unwrap();
        music
    }

    #[test]
    fn scans_files_skips_hidden_and_builds_virtual_paths() {
        let music = build_share("scan");
        let index = ShareIndex::scan(&[music.clone()]);

        let paths: Vec<&str> = index.files.iter().map(|f| f.virtual_path.as_str()).collect();
        assert!(paths.contains(&"Music\\song one.mp3"));
        assert!(paths.contains(&"Music\\Album\\track.flac"));
        // Hidden file and the file under a hidden folder are excluded.
        assert_eq!(index.num_files(), 2, "two visible files");
        assert!(!paths.iter().any(|p| p.contains("secret") || p.contains("nope")));

        // Sizes come from the filesystem.
        let song = index.files.iter().find(|f| f.virtual_path.ends_with("song one.mp3")).unwrap();
        assert_eq!(song.size, 4);

        std::fs::remove_dir_all(music.parent().unwrap()).ok();
    }

    #[test]
    fn word_index_tokenizes_folder_and_filename() {
        let music = build_share("words");
        let index = ShareIndex::scan(&[music.clone()]);

        // Every token is lowercased and punctuation-split.
        for word in ["music", "song", "one", "mp3", "album", "track", "flac"] {
            assert!(index.word_index.contains_key(word), "missing token: {word}");
        }
        // "mp3" appears only on the song; "music" on both files.
        assert_eq!(index.word_index["music"].len(), 2);
        assert_eq!(index.word_index["flac"].len(), 1);

        std::fs::remove_dir_all(music.parent().unwrap()).ok();
    }

    #[test]
    fn word_index_dedupes_repeated_tokens_per_file() {
        // Nicotine+ indexes the union `set(folder_words + basename_words)` once
        // per file (shares.py:scan_shared_folder), so a token repeated across
        // the folder path and filename appends the file id only once.
        let root = std::env::temp_dir()
            .join(format!("soulrust-shares-{}-dedup", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        // Share root "demo" and a "demo" subfolder: "demo" appears in the root,
        // the subfolder, and the filename.
        let share = root.join("demo");
        std::fs::create_dir_all(share.join("demo")).unwrap();
        std::fs::write(share.join("demo").join("demo.mp3"), b"q").unwrap();

        let index = ShareIndex::scan(&[share.clone()]);
        assert_eq!(index.num_files(), 1);
        // Even though "demo" occurs three times in "demo\\demo\\demo.mp3", the
        // single file id is recorded just once.
        assert_eq!(index.word_index["demo"], vec![0]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn browse_groups_files_by_folder() {
        let music = build_share("browse");
        let index = ShareIndex::scan(&[music.clone()]);
        let listing = index.browse();

        let folders: Vec<&str> = listing.directories.iter().map(|d| d.path.as_str()).collect();
        assert!(folders.contains(&"Music"));
        assert!(folders.contains(&"Music\\Album"));

        let album = listing.directories.iter().find(|d| d.path == "Music\\Album").unwrap();
        assert_eq!(album.files.len(), 1);
        // Folder streams carry the basename only, not the full virtual path:
        // Nicotine+ stores `basename_file_data[0] = basename_escaped` per folder
        // (shares.py:scan_shared_folder). The full path is reserved for search
        // responses (search.py:_create_file_info_list).
        assert_eq!(album.files[0].name, "track.flac");

        // folder_contents agrees with the browse tree.
        let contents = index.folder_contents("Music\\Album");
        assert_eq!(contents, album.files);

        std::fs::remove_dir_all(music.parent().unwrap()).ok();
    }

    #[test]
    fn browse_includes_empty_and_intermediate_folders() {
        // Nicotine+ stores a (possibly empty) stream for every folder it scans;
        // test_shares.py asserts `public_streams["Shares\\folder2"]` is an empty
        // stream (b"\x00\x00\x00\x00") even though folder2 holds only subfolders.
        let root = std::env::temp_dir()
            .join(format!("soulrust-shares-{}-emptyfolders", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let share = root.join("Shares");
        // folder2 has no direct files, only the subfolder `test` which holds one.
        std::fs::create_dir_all(share.join("folder2").join("test")).unwrap();
        std::fs::create_dir_all(share.join("empty")).unwrap();
        std::fs::write(share.join("folder2").join("test").join("nothing"), b"x").unwrap();

        let index = ShareIndex::scan(&[share.clone()]);
        let listing = index.browse();
        let folders: Vec<&str> = listing.directories.iter().map(|d| d.path.as_str()).collect();

        // The root, an intermediate folder with no direct files, a leaf-only
        // empty folder, and the populated subfolder all appear.
        assert!(folders.contains(&"Shares"));
        assert!(folders.contains(&"Shares\\folder2"));
        assert!(folders.contains(&"Shares\\empty"));
        assert!(folders.contains(&"Shares\\folder2\\test"));

        // The intermediate/empty folders carry zero files (the wire stream
        // would be a uint32 count of 0).
        let folder2 = listing.directories.iter().find(|d| d.path == "Shares\\folder2").unwrap();
        assert!(folder2.files.is_empty());
        let empty = listing.directories.iter().find(|d| d.path == "Shares\\empty").unwrap();
        assert!(empty.files.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn backslash_in_filename_is_replaced_with_sentinel() {
        // A backslash is a valid character in a basename on non-Windows
        // filesystems, but the Soulseek path separator on the wire. Nicotine+
        // substitutes `Shares.BACKSLASH_SENTINEL` ("@@BACKSLASH@@") for it
        // before building the virtual path (shares.py:scan_shared_folder), so
        // the file stays one file rather than reading as a nested folder.
        let root = std::env::temp_dir()
            .join(format!("soulrust-shares-{}-backslash", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let share = root.join("Music");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(share.join("AC\\DC.mp3"), b"zz").unwrap();

        let index = ShareIndex::scan(&[share.clone()]);
        assert_eq!(index.num_files(), 1);

        // Full virtual path (search response) keeps the sentinel, so the file
        // sits directly in `Music`, not in a phantom `AC` subfolder.
        assert_eq!(index.files[0].virtual_path, "Music\\AC@@BACKSLASH@@DC.mp3");

        // Folder stream (browse) carries the escaped basename.
        let listing = index.browse();
        let music = listing.directories.iter().find(|d| d.path == "Music").unwrap();
        assert_eq!(music.files.len(), 1);
        assert_eq!(music.files[0].name, "AC@@BACKSLASH@@DC.mp3");
        // No phantom `Music\AC` folder was created.
        assert!(!listing.directories.iter().any(|d| d.path == "Music\\AC"));

        std::fs::remove_dir_all(&root).ok();
    }
}
