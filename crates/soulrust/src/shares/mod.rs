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

    /// The wire `SharedFile` for a file id (full virtual path as the name, as
    /// Soulseek folder streams carry it). Attributes are deferred (empty).
    pub fn shared_file(&self, id: u32) -> SharedFile {
        let entry = &self.files[id as usize];
        SharedFile {
            name: entry.virtual_path.clone(),
            size: entry.size,
            extension: String::new(),
            attributes: Vec::new(),
        }
    }

    /// The full browsable share tree (a `SharedFileListResponse` we serve to a
    /// peer that browses us). Public shares only for now.
    pub fn browse(&self) -> SharedFileListResponse {
        let directories = self
            .folders
            .iter()
            .map(|(path, ids)| SharedDirectory {
                path: path.clone(),
                files: ids.iter().map(|&id| self.shared_file(id)).collect(),
            })
            .collect();
        SharedFileListResponse { directories, private_directories: Vec::new() }
    }

    /// The files directly within one virtual folder (for FolderContentsResponse).
    pub fn folder_contents(&self, virtual_folder: &str) -> Vec<SharedFile> {
        self.folders
            .get(virtual_folder)
            .map(|ids| ids.iter().map(|&id| self.shared_file(id)).collect())
            .unwrap_or_default()
    }
}

fn walk(dir: &Path, virtual_prefix: &str, index: &mut ShareIndex) {
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
        let virtual_path = format!("{virtual_prefix}\\{name}");
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(&entry.path(), &virtual_path, index);
        } else if file_type.is_file() {
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
    fn browse_groups_files_by_folder() {
        let music = build_share("browse");
        let index = ShareIndex::scan(&[music.clone()]);
        let listing = index.browse();

        let folders: Vec<&str> = listing.directories.iter().map(|d| d.path.as_str()).collect();
        assert!(folders.contains(&"Music"));
        assert!(folders.contains(&"Music\\Album"));

        let album = listing.directories.iter().find(|d| d.path == "Music\\Album").unwrap();
        assert_eq!(album.files.len(), 1);
        assert_eq!(album.files[0].name, "Music\\Album\\track.flac");

        // folder_contents agrees with the browse tree.
        let contents = index.folder_contents("Music\\Album");
        assert_eq!(contents, album.files);

        std::fs::remove_dir_all(music.parent().unwrap()).ok();
    }
}
