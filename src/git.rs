use anyhow::{Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Eol, detect_eol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl Status {
    pub fn letter(self) -> char {
        match self {
            Status::Modified => 'M',
            Status::Added => 'A',
            Status::Deleted => 'D',
            Status::Renamed => 'R',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub status: Status,
}

pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// `start` is where the search begins, not the root: git walks up from it,
    /// which is what lets difv be pointed at a repository the shell is not
    /// standing in. Git is left to do the walking because it is the only thing
    /// that knows about `.git` files, linked worktrees and ceiling directories.
    pub fn discover(start: &Path) -> Result<Self> {
        let out = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !out.status.success() {
            bail!("not a git repository: {}", start.display());
        }
        let root = String::from_utf8(out.stdout)?.trim().to_string();
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A path as the command line gave it, as a repository-relative one — the
    /// form everything else in difv holds paths in. What is canonicalised is
    /// the nearest directory that exists rather than the path itself: a file
    /// git reports as deleted is not on disk to resolve, and neither is the
    /// directory that held it when the deletion took that too. Canonicalising
    /// both sides is what makes the strip meet the physical path
    /// `--show-toplevel` reports. `None` when the path is outside this
    /// repository.
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        let (dir, rest) = nearest_existing_dir(path)?;
        let base = std::fs::canonicalize(dir).ok()?;
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        Some(base.join(rest).strip_prefix(root).ok()?.to_path_buf())
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()?;
        if !out.status.success() {
            bail!(
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Working tree compared against HEAD, including untracked files.
    pub fn changed_files(&self) -> Result<Vec<ChangedFile>> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        Ok(parse_porcelain_z(&out))
    }

    /// HEAD is only ever displayed, so undecodable bytes are shown lossily
    /// rather than refused.
    pub fn head_content(&self, f: &ChangedFile) -> String {
        if f.status == Status::Added {
            return String::new();
        }
        let p = f.old_path.as_ref().unwrap_or(&f.path);
        let spec = format!("HEAD:{}", p.display());
        match self.git(&["show", &spec]).map(FileContent::decode) {
            Ok(FileContent::Text(text)) => text.body,
            _ => String::new(),
        }
    }

    pub fn worktree_content(&self, f: &ChangedFile) -> FileContent {
        if f.status == Status::Deleted {
            return FileContent::Missing;
        }
        match std::fs::read(self.abs(&f.path)) {
            Ok(bytes) => FileContent::decode(bytes),
            Err(_) => FileContent::Missing,
        }
    }

    pub fn abs(&self, path: &Path) -> PathBuf {
        self.root.join(path)
    }

    /// Write the buffer to the working tree, restoring the file's own line
    /// ending and trailing-newline state so lines the user never touched come
    /// back byte for byte. Never touches the index.
    pub fn write_file(
        &self,
        path: &Path,
        lines: &[String],
        eol: Eol,
        trailing: bool,
    ) -> Result<()> {
        let mut body = lines.join(eol.as_str());
        if trailing {
            body.push_str(eol.as_str());
        }
        write_atomic(&self.abs(path), body.as_bytes())
    }
}

/// Working-tree content, carrying what saving needs to put it back unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Text(TextFile),
    /// Binary or not valid UTF-8: displayable as a note, never editable, because
    /// a lossy decode would corrupt the file on save.
    Undecodable,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    pub body: String,
    pub eol: Option<Eol>,
    pub trailing_newline: bool,
}

impl FileContent {
    fn decode(bytes: Vec<u8>) -> Self {
        if bytes.iter().take(8000).any(|b| *b == 0) {
            return Self::Undecodable;
        }
        let Ok(body) = String::from_utf8(bytes) else {
            return Self::Undecodable;
        };
        Self::Text(TextFile {
            eol: detect_eol(&body),
            trailing_newline: body.ends_with('\n'),
            body,
        })
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Text(file) => &file.body,
            Self::Undecodable => "<binary file>",
            Self::Missing => "",
        }
    }

    pub fn editable(&self) -> Option<&TextFile> {
        match self {
            Self::Text(file) => Some(file),
            _ => None,
        }
    }
}

/// The nearest directory above `path` that is on disk, and what is left of
/// `path` below it. Neither the path nor its parent is something to count on:
/// `git rm -r src` leaves `src/auth.ts` a change worth opening with both of
/// them gone. The repository above them is still there, which is what this
/// finds. A bare file name has an empty parent, which is the current directory
/// rather than nothing.
pub fn nearest_existing_dir(path: &Path) -> Option<(&Path, &Path)> {
    path.ancestors().skip(1).find_map(|ancestor| {
        let dir = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        dir.is_dir()
            .then(|| Some((dir, path.strip_prefix(ancestor).ok()?)))?
    })
}

/// Write through a sibling temporary file and rename over the target, so an
/// interrupted save leaves either the old content or the new one, never a
/// truncated file. Used for the user's source files and for difv's own config,
/// which the settings panel rewrites on every keypress. The path is canonicalised first so a symlink is followed
/// rather than replaced.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.difv-tmp"));

    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Git writes `XY PATH\0` per entry; renames and copies add a second
/// `ORIG_PATH\0` record straight after the entry that names them.
fn parse_porcelain_z(buf: &[u8]) -> Vec<ChangedFile> {
    let records: Vec<&[u8]> = buf.split(|b| *b == 0).filter(|r| !r.is_empty()).collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let rec = records[i];
        i += 1;
        if rec.len() < 4 {
            continue;
        }
        let (x, y) = (rec[0] as char, rec[1] as char);
        let path = bytes_to_path(&rec[3..]);
        let mut old_path = None;
        if (x == 'R' || x == 'C')
            && let Some(orig) = records.get(i)
        {
            old_path = Some(bytes_to_path(orig));
            i += 1;
        }
        let status = match (x, y) {
            ('?', _) | ('A', _) => Status::Added,
            ('D', _) | (_, 'D') => Status::Deleted,
            ('R', _) | ('C', _) => Status::Renamed,
            _ => Status::Modified,
        };
        files.push(ChangedFile {
            path,
            old_path,
            status,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery is given a place to start, not a root: pointing difv at a
    /// subdirectory has to open the repository above it, the same as standing
    /// in that subdirectory would.
    #[test]
    fn discovery_walks_up_from_the_path_it_is_given() {
        let fixture = crate::app::tests::Fixture::new("discover", "a\n", "b\n");
        let deep = fixture.dir().join("sub/deeper");
        std::fs::create_dir_all(&deep).unwrap();

        let repo = Repo::discover(&deep).unwrap();
        assert_eq!(
            std::fs::canonicalize(repo.root()).unwrap(),
            std::fs::canonicalize(fixture.dir()).unwrap()
        );
        assert!(!repo.changed_files().unwrap().is_empty());
        // A path under the repository comes back relative to its root, whatever
        // directory it was typed from.
        assert_eq!(
            repo.relative(&deep.join("x.txt")),
            Some(PathBuf::from("sub/deeper/x.txt"))
        );
    }

    #[test]
    fn discovery_outside_a_repository_names_the_path() {
        let dir = std::env::temp_dir().join(format!("difv-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let Err(err) = Repo::discover(&dir) else {
            panic!("a directory outside a repository is refused");
        };
        let err = err.to_string();
        assert!(err.contains("not a git repository"), "{err}");
        assert!(err.contains(&dir.display().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The relative form has to survive both halves of a deletion: the file,
    /// and the directory that held it.
    #[test]
    fn a_relative_form_survives_a_deleted_directory() {
        let fixture = crate::app::tests::Fixture::new("relative-deleted", "a\n", "b\n");
        let repo = Repo::discover(fixture.dir()).unwrap();

        assert_eq!(
            repo.relative(&fixture.dir().join("src/deep/auth.ts")),
            Some(PathBuf::from("src/deep/auth.ts")),
            "neither `src` nor `src/deep` is on disk"
        );
        let gone = fixture.dir().join("src/auth.ts");
        let (dir, rest) = nearest_existing_dir(&gone).unwrap();
        assert_eq!(dir, fixture.dir());
        assert_eq!(rest, Path::new("src/auth.ts"));
        // A bare name has an empty parent, which is the current directory.
        let (dir, rest) = nearest_existing_dir(Path::new("x.txt")).unwrap();
        assert_eq!((dir, rest), (Path::new("."), Path::new("x.txt")));
        assert_eq!(nearest_existing_dir(Path::new("")), None);
    }

    #[test]
    fn a_path_outside_the_repository_has_no_relative_form() {
        let fixture = crate::app::tests::Fixture::new("relative", "a\n", "b\n");
        let repo = Repo::discover(fixture.dir()).unwrap();
        assert_eq!(repo.relative(Path::new("/nowhere/at/all/x.txt")), None);
        assert_eq!(repo.relative(&std::env::temp_dir().join("x.txt")), None);
    }

    #[test]
    fn decode_reports_line_ending_and_trailing_newline() {
        let crlf = FileContent::decode(b"a\r\nb\r\n".to_vec());
        let file = crlf.editable().unwrap();
        assert_eq!(file.eol, Some(Eol::Crlf));
        assert!(file.trailing_newline);

        let bare = FileContent::decode(b"a\nb".to_vec());
        let file = bare.editable().unwrap();
        assert_eq!(file.eol, Some(Eol::Lf));
        assert!(!file.trailing_newline);
    }

    #[test]
    fn undecodable_content_is_never_editable() {
        assert_eq!(
            FileContent::decode(vec![0x00, 0x01]),
            FileContent::Undecodable
        );
        assert_eq!(
            FileContent::decode(vec![0xff, 0xfe]),
            FileContent::Undecodable
        );
        assert!(FileContent::decode(vec![0xff]).editable().is_none());
    }

    #[test]
    fn atomic_write_preserves_permissions_and_follows_symlinks() {
        let dir = std::env::temp_dir().join(format!("difv-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        std::fs::write(&target, "old\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            write_atomic(&link, b"new\n").unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o640);
        }

        assert!(std::fs::read_dir(&dir).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".difv-tmp")
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_statuses_and_renames() {
        let raw = b"M  src/auth.ts\0 M src/user.ts\0A  src/utils.ts\0 D src/old.ts\0R  new.ts\0old.ts\0?? untracked.ts\0";
        let files = parse_porcelain_z(raw);
        let got: Vec<(&str, Status, Option<&str>)> = files
            .iter()
            .map(|f| {
                (
                    f.path.to_str().unwrap(),
                    f.status,
                    f.old_path.as_deref().and_then(|p| p.to_str()),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("new.ts", Status::Renamed, Some("old.ts")),
                ("src/auth.ts", Status::Modified, None),
                ("src/old.ts", Status::Deleted, None),
                ("src/user.ts", Status::Modified, None),
                ("src/utils.ts", Status::Added, None),
                ("untracked.ts", Status::Added, None),
            ]
        );
    }

    #[test]
    fn handles_paths_with_spaces() {
        let files = parse_porcelain_z(b"M  src/my file.ts\0");
        assert_eq!(files[0].path.to_str().unwrap(), "src/my file.ts");
    }
}
