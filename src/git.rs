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
    pub fn discover() -> Result<Self> {
        let out = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !out.status.success() {
            bail!("not a git repository");
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
