use anyhow::{Result, bail};
use std::ffi::{OsStr, OsString};
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
    /// The blob on the Before side, `None` where there is none — a file added
    /// between the two sides. Reading content by id rather than by
    /// `<rev>:<path>` is one object lookup instead of a tree walk, and needs no
    /// arithmetic about which path a rename had on which side.
    pub old_blob: Option<String>,
    /// The blob on the Current side. `None` for a deletion, and meaningless
    /// when the Current side is the working tree — `git diff` reports either
    /// zeroes or the index's blob there, neither of which is what is on disk.
    pub new_blob: Option<String>,
}

/// What difv compares, and where. The revisions are held as they were typed:
/// difv never parses revision syntax — `a..b`, `a...b`, `a^!`, `@{u}` and
/// everything else `gitrevisions` allows work because git resolves them, and
/// the pane titles say what the user wrote rather than a resolved id.
pub struct Repo {
    root: PathBuf,
    /// The revision arguments as typed. Empty means difv's own default of
    /// `HEAD` against the working tree.
    revs: Vec<OsString>,
    /// What `git diff` is actually given, which differs from `revs` only in a
    /// repository whose `HEAD` has no commit yet.
    diff_revs: Vec<OsString>,
    /// Whether the Current side is the working tree, which is what makes it
    /// editable, polled and able to hold untracked files.
    worktree: bool,
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
            revs: Vec::new(),
            diff_revs: Vec::new(),
            worktree: true,
        })
    }

    /// The revisions to compare, in the repository already discovered — a
    /// revision means nothing until there is a repository to resolve it in.
    ///
    /// Which side the working tree is on is git's own rule, not a reading of
    /// the syntax: `git diff` compares against the working tree when the
    /// arguments name exactly one commit, and one commit is what
    /// `rev-parse --revs-only` prints one line for. That covers the forms difv
    /// would otherwise have to know about — `a..b` and `a^!` are one argument
    /// and two lines, `a b` is two of each — including a root commit's `^!`,
    /// which has no parent to exclude and so really is a comparison against
    /// the working tree.
    pub fn with_revs(mut self, revs: Vec<OsString>) -> Result<Self> {
        let mut args: Vec<&OsStr> = vec![OsStr::new("rev-parse"), OsStr::new("--revs-only")];
        args.extend(revs.iter().map(OsString::as_os_str));
        args.push(OsStr::new("--"));
        let out = self.run(&args)?;
        if !out.status.success() {
            // Git's own wording, which is the wording the user knows, minus
            // the prefix difv's own `difv: ` replaces.
            let message = String::from_utf8_lossy(&out.stderr);
            let message = message.trim();
            bail!(
                "{}",
                message.strip_prefix("fatal: ").unwrap_or(message).trim()
            );
        }
        self.worktree = String::from_utf8_lossy(&out.stdout).lines().count() <= 1;
        self.diff_revs = if revs.is_empty() {
            vec![self.default_base()?]
        } else {
            revs.clone()
        };
        self.revs = revs;
        Ok(self)
    }

    /// What difv compares against with no revision given. `HEAD` — except in a
    /// repository whose first commit has not been made, where `git diff HEAD`
    /// refuses and the empty tree is what makes every file an addition, which
    /// is what difv showed there before it used `git diff` at all. The id is
    /// asked for rather than written down because it depends on the
    /// repository's hash algorithm.
    fn default_base(&self) -> Result<OsString> {
        if self
            .git(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok()
        {
            return Ok(OsString::from("HEAD"));
        }
        let empty = self.git(&["hash-object", "-t", "tree", "/dev/null"])?;
        Ok(OsString::from(String::from_utf8(empty)?.trim().to_string()))
    }

    pub fn worktree(&self) -> bool {
        self.worktree
    }

    /// What the two panes are looking at, for their titles. Derived from the
    /// text the user typed: the name they think in is the name they should
    /// read back, and the merge base `a...b` compares against has no short
    /// name to offer anyway.
    pub fn labels(&self) -> (String, String) {
        let typed: Vec<String> = self
            .revs
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        let (old, new) = match typed.as_slice() {
            [] => ("HEAD".to_string(), WORKING_TREE.to_string()),
            [one] => split_revision(one),
            [a, b] => (a.clone(), b.clone()),
            _ => (typed.join(" "), "Current".to_string()),
        };
        // One argument can still name one commit — a plain revision always
        // does, a root commit's `^!` does too — and then the Current side is
        // the working tree whatever the syntax suggested.
        match self.worktree {
            true => (old, WORKING_TREE.to_string()),
            false => (old, new),
        }
    }

    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        Self {
            root,
            revs: Vec::new(),
            diff_revs: vec![OsString::from("HEAD")],
            worktree: true,
        }
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

    /// The command as run, whether or not it succeeded — for the callers that
    /// have something to say about a failure themselves.
    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<std::process::Output> {
        Ok(Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()?)
    }

    fn git<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Vec<u8>> {
        let out = self.run(args)?;
        if !out.status.success() {
            let shown: Vec<String> = args
                .iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned())
                .collect();
            bail!(
                "git {:?}: {}",
                shown,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Every file that differs between the two sides. `git diff --raw` is what
    /// can name arbitrary sides — `git status` only ever knew one pair — and
    /// it reports the blob on each side, which is what the content below is
    /// read by. Untracked files are difv's own addition: `git diff` does not
    /// list them, difv always has, and they only exist when the working tree
    /// is a side.
    pub fn changed_files(&self) -> Result<Vec<ChangedFile>> {
        let mut args: Vec<OsString> = ["diff", "--raw", "-z", "-M", "--abbrev=40"]
            .iter()
            .map(OsString::from)
            .collect();
        args.extend(self.diff_revs.iter().cloned());
        args.push(OsString::from("--"));
        let mut files = parse_raw_z(&self.git(&args)?);
        if self.worktree {
            let out = self.git(&["ls-files", "--others", "--exclude-standard", "-z"])?;
            files.extend(
                out.split(|b| *b == 0)
                    .filter(|r| !r.is_empty())
                    .map(|r| ChangedFile {
                        path: bytes_to_path(r),
                        old_path: None,
                        status: Status::Added,
                        old_blob: None,
                        new_blob: None,
                    }),
            );
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// The Before side is only ever displayed, so undecodable bytes are shown
    /// lossily rather than refused.
    pub fn old_content(&self, f: &ChangedFile) -> String {
        let Some(id) = &f.old_blob else {
            return String::new();
        };
        match self.git(&["cat-file", "blob", id]).map(FileContent::decode) {
            Ok(FileContent::Text(text)) => text.body,
            _ => String::new(),
        }
    }

    /// The Current side: the file on disk when the working tree is what is
    /// being compared, and the blob otherwise. The blob is never read for the
    /// working tree even when `git diff` reported one, because what it reports
    /// there is the index's copy, not what the editor would be editing.
    pub fn new_content(&self, f: &ChangedFile) -> FileContent {
        if f.status == Status::Deleted {
            return FileContent::Missing;
        }
        if !self.worktree {
            let Some(id) = &f.new_blob else {
                return FileContent::Missing;
            };
            return match self.git(&["cat-file", "blob", id]) {
                Ok(bytes) => FileContent::decode(bytes),
                Err(_) => FileContent::Missing,
            };
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

/// The Current pane's label when it is the working tree rather than a
/// revision.
const WORKING_TREE: &str = "Working Tree";

/// The two sides one revision argument names, for the pane titles. The order
/// matters: `...` has to be tried before `..`, which is a prefix of it. An
/// omitted end of a range is `HEAD`, as it is to git.
fn split_revision(rev: &str) -> (String, String) {
    let end = |text: &str| match text.is_empty() {
        true => "HEAD".to_string(),
        false => text.to_string(),
    };
    if let Some((a, b)) = rev.split_once("...") {
        return (end(a), end(b));
    }
    if let Some((a, b)) = rev.split_once("..") {
        return (end(a), end(b));
    }
    if let Some(base) = rev.strip_suffix("^!") {
        return (format!("{base}^"), base.to_string());
    }
    (rev.to_string(), WORKING_TREE.to_string())
}

/// `git diff --raw -z` writes one `:<old mode> <new mode> <old id> <new id>
/// <status>\0` record per file, followed by its path — or, for a rename or a
/// copy, by the old path and then the new one. An id of all zeroes means
/// there is no blob on that side.
fn parse_raw_z(buf: &[u8]) -> Vec<ChangedFile> {
    let records: Vec<&[u8]> = buf.split(|b| *b == 0).filter(|r| !r.is_empty()).collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let head = records[i];
        i += 1;
        let Some(rest) = head.strip_prefix(b":") else {
            continue;
        };
        let text = String::from_utf8_lossy(rest);
        let mut fields = text.split(' ').skip(2);
        let (Some(old_id), Some(new_id), Some(letter)) = (
            fields.next(),
            fields.next(),
            fields.next().and_then(|f| f.chars().next()),
        ) else {
            continue;
        };
        let renamed = letter == 'R' || letter == 'C';
        // A rename's two path records are its old path and then its new one;
        // every other status has just the one.
        let Some(first) = records.get(i) else {
            break;
        };
        i += 1;
        let (old_path, path) = match renamed {
            true => {
                let Some(second) = records.get(i) else {
                    break;
                };
                i += 1;
                (Some(bytes_to_path(first)), bytes_to_path(second))
            }
            false => (None, bytes_to_path(first)),
        };
        files.push(ChangedFile {
            path,
            old_path,
            status: match letter {
                'A' => Status::Added,
                'D' => Status::Deleted,
                'R' | 'C' => Status::Renamed,
                _ => Status::Modified,
            },
            old_blob: blob_id(old_id),
            new_blob: blob_id(new_id),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// An id of all zeroes is git's way of saying there is no blob on that side —
/// a file added, deleted, or (as `git diff` reports it) not in the index.
fn blob_id(id: &str) -> Option<String> {
    (!id.is_empty() && !id.chars().all(|c| c == '0')).then(|| id.to_string())
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

    /// One `--raw` record per file, with the rename's two paths and the score
    /// on its status letter, and an all-zero id meaning no blob on that side.
    #[test]
    fn parses_statuses_renames_and_blobs() {
        let raw = b":100644 100644 aaa1 bbb1 M\0src/auth.ts\0\
                    :000000 100644 0000 ccc1 A\0src/utils.ts\0\
                    :100644 000000 ddd1 0000 D\0src/old.ts\0\
                    :100644 100644 eee1 eee1 R100\0old.ts\0new.ts\0\
                    :100644 100644 fff1 0000000 M\0src/my file.ts\0";
        let got: Vec<String> = parse_raw_z(raw)
            .iter()
            .map(|f| {
                format!(
                    "{} {} {:?} {:?} {:?}",
                    f.status.letter(),
                    f.path.display(),
                    f.old_path.as_ref().map(|p| p.display().to_string()),
                    f.old_blob,
                    f.new_blob,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                r#"R new.ts Some("old.ts") Some("eee1") Some("eee1")"#,
                r#"M src/auth.ts None Some("aaa1") Some("bbb1")"#,
                // A path with a space is one record like any other, and the
                // zero id on the right is what the working tree reports.
                r#"M src/my file.ts None Some("fff1") None"#,
                r#"D src/old.ts None Some("ddd1") None"#,
                r#"A src/utils.ts None None Some("ccc1")"#,
            ]
        );
    }

    /// The pane titles say what was typed. `...` has to be tried before `..`,
    /// and an omitted end of a range is `HEAD`, as it is to git.
    #[test]
    fn revision_labels_name_both_sides() {
        let cases = [
            ("main", ("main", WORKING_TREE)),
            ("main..feature", ("main", "feature")),
            ("main...feature", ("main", "feature")),
            ("abc123^!", ("abc123^", "abc123")),
            ("..feature", ("HEAD", "feature")),
            ("main..", ("main", "HEAD")),
        ];
        for (rev, want) in cases {
            let (old, new) = split_revision(rev);
            assert_eq!((old.as_str(), new.as_str()), want, "{rev}");
        }
    }

    /// The pane labels through a `Repo`, where one argument naming one commit
    /// means the working tree is the Current side whatever the syntax said.
    #[test]
    fn labels_follow_which_side_the_working_tree_is_on() {
        let fixture = crate::app::tests::Fixture::new("labels", "a\n", "b\n");
        let head = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();
        let repo = |revs: &[&str]| {
            Repo::discover(fixture.dir())
                .unwrap()
                .with_revs(revs.iter().map(OsString::from).collect())
                .unwrap()
        };

        assert_eq!(
            repo(&[]).labels(),
            ("HEAD".to_string(), WORKING_TREE.to_string())
        );
        assert_eq!(
            repo(&["HEAD"]).labels(),
            ("HEAD".to_string(), WORKING_TREE.to_string())
        );
        // The one commit there is has no parent, so `^!` names one commit and
        // git diffs it against the working tree — the title has to agree.
        assert_eq!(repo(&[&format!("{head}^!")]).labels().1, WORKING_TREE);
    }

    /// The whole point of the revision arguments: what each form compares,
    /// which side the working tree is on, and where the content comes from.
    #[test]
    fn revisions_choose_what_is_compared() {
        let fixture = crate::app::tests::Fixture::new("revisions", "one\n", "one\n");
        fixture.write("two\n");
        fixture.write_file("added.txt", "added\n");
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-qm", "second"]);
        let second = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();
        let first = fixture.git(&["rev-parse", "HEAD~1"]).trim().to_string();
        fixture.write("three\n");
        fixture.write_file("untracked.txt", "loose\n");

        let open = |revs: &[&str]| {
            Repo::discover(fixture.dir())
                .unwrap()
                .with_revs(revs.iter().map(OsString::from).collect())
                .unwrap()
        };
        let names = |repo: &Repo| {
            repo.changed_files()
                .unwrap()
                .iter()
                .map(|f| f.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        // No revision, and one revision, are both against the working tree —
        // which is what puts the untracked file in the list.
        let now = open(&[]);
        assert!(now.worktree());
        assert_eq!(names(&now), ["file.txt", "untracked.txt"]);
        let since_first = open(&[&first]);
        assert!(since_first.worktree());
        assert_eq!(
            names(&since_first),
            ["added.txt", "file.txt", "untracked.txt"]
        );

        // Two revisions, in every form, compare two commits: no untracked
        // file, and the Current side is a blob rather than the disk.
        for revs in [
            vec![format!("{second}^!")],
            vec![format!("{first}..{second}")],
            vec![first.clone(), second.clone()],
        ] {
            let borrowed: Vec<&str> = revs.iter().map(String::as_str).collect();
            let repo = open(&borrowed);
            assert!(!repo.worktree(), "{revs:?}");
            assert_eq!(names(&repo), ["added.txt", "file.txt"], "{revs:?}");
            let file = repo
                .changed_files()
                .unwrap()
                .into_iter()
                .find(|f| f.path == Path::new("file.txt"))
                .unwrap();
            assert_eq!(repo.old_content(&file), "one\n", "{revs:?}");
            assert_eq!(repo.new_content(&file).text(), "two\n", "{revs:?}");
        }

        // The same file against the working tree reads what is on disk, not
        // the blob `git diff` reported for it.
        let file = now
            .changed_files()
            .unwrap()
            .into_iter()
            .find(|f| f.path == Path::new("file.txt"))
            .unwrap();
        assert_eq!(now.old_content(&file), "two\n");
        assert_eq!(now.new_content(&file).text(), "three\n");
    }

    /// A revision git cannot resolve is refused in git's own words, with the
    /// prefix difv's own `difv: ` replaces taken off.
    #[test]
    fn a_bad_revision_is_refused_in_gits_words() {
        let fixture = crate::app::tests::Fixture::new("bad-rev", "a\n", "b\n");
        let repo = Repo::discover(fixture.dir()).unwrap();

        let Err(err) = repo.with_revs(vec![OsString::from("nope")]) else {
            panic!("a revision git cannot resolve is refused");
        };
        let err = err.to_string();
        assert!(err.contains("nope"), "{err}");
        assert!(!err.starts_with("fatal: "), "{err}");
    }

    /// `git diff HEAD` refuses before the first commit, where difv has always
    /// shown every file as an addition. The empty tree is what keeps it doing
    /// that.
    #[test]
    fn a_repository_with_no_commits_lists_its_files_as_added() {
        let dir = std::env::temp_dir().join(format!("difv-nocommit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("staged.txt"), "a\n").unwrap();
        std::fs::write(dir.join("loose.txt"), "b\n").unwrap();
        git(&["add", "staged.txt"]);

        let repo = Repo::discover(&dir).unwrap().with_revs(Vec::new()).unwrap();
        let files = repo.changed_files().unwrap();
        let got: Vec<(String, Status)> = files
            .iter()
            .map(|f| (f.path.to_string_lossy().into_owned(), f.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("loose.txt".to_string(), Status::Added),
                ("staged.txt".to_string(), Status::Added),
            ]
        );
        assert_eq!(repo.labels().0, "HEAD", "the title still says HEAD");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
