mod app;
mod clipboard;
mod config;
mod diff;
mod editor;
mod git;
mod settings;
mod ui;

use std::ffi::OsString;
use std::io::stdout;
use std::path::{Path, PathBuf};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyEventKind,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::style::Print;

use app::App;
use config::Config;
use git::Repo;

/// Mouse reporting, asked for by hand rather than through `EnableMouseCapture`,
/// which also turns on any-motion tracking (`?1003h`) — an event for every cell
/// the pointer crosses with no button down, and a full redraw for each one.
/// difv only needs motion during a drag, which is what button-event tracking
/// (`?1002h`) reports.
///
/// Both coordinate encodings are asked for, SGR (`?1006h`) last: urxvt built
/// without frills has only its own (`?1015h`), and where a terminal keeps one
/// encoding rather than a set, the later request wins — so SGR is what everyone
/// who has it ends up using. Without either, coordinates past column 223 are
/// unreportable.
///
/// Turning 1003 back off after the fact is not the same thing: xterm.js keeps
/// one active mouse protocol rather than a flag per mode, so a reset of any of
/// them silently disables mouse input altogether. That is a whole class of
/// terminal — VS Code's, and anything else embedding xterm.js — left with no
/// clicking and no scrolling at all.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h";

/// difv's command line is `git diff`'s, so that nothing about it has to be
/// learned twice: up to two revisions and at most one path. Parsing is kept
/// away from the filesystem and the terminal so it can be tested as the table
/// of outcomes it is — which is also why it stops at collecting words rather
/// than deciding which of them are revisions, a question only the disk and git
/// can answer.
#[derive(Debug, PartialEq, Eq)]
enum Args {
    Version,
    Help,
    Open {
        /// Positionals before `--`, each still either a revision or a path.
        words: Vec<OsString>,
        /// Positionals after `--`, which can only be paths.
        paths: Vec<OsString>,
    },
    Error(String),
}

/// Arguments are taken as `OsString`, not `String`: a path is one of the
/// things this takes, and a path is not required to be UTF-8 anywhere difv
/// runs. Refusing one is fine; panicking inside `std::env::args()` on a file
/// name the user can see in their own shell is not.
fn parse_args<I: IntoIterator<Item = OsString>>(args: I) -> Args {
    let (mut words, mut paths) = (Vec::new(), Vec::new());
    let mut separated = false;
    for arg in args {
        // A leading `-` is what separates a flag from an argument, so a file
        // whose name starts with one is reached as `./-name` or past `--`,
        // which is git's own escape hatch. A name difv cannot decode cannot be
        // a flag either, so it goes down the positional side untouched.
        if !separated
            && let Some(text) = arg.to_str()
            && text.starts_with('-')
        {
            match text {
                "--" => separated = true,
                "-V" | "--version" => return Args::Version,
                "-h" | "--help" => return Args::Help,
                other => {
                    return Args::Error(format!("unknown argument `{other}` — try `difv --help`"));
                }
            }
            continue;
        }
        if arg.is_empty() {
            return Args::Error("empty argument — try `difv --help`".to_string());
        }
        match separated {
            true => paths.push(arg),
            false => words.push(arg),
        }
    }
    Args::Open { words, paths }
}

/// Where difv starts: the arguments still to be told apart, the paths that
/// already are, and the directory to discover the repository from — which has
/// to be settled first, since a revision means nothing until there is a
/// repository to resolve it in.
struct Target {
    words: Vec<OsString>,
    paths: Vec<OsString>,
    start: PathBuf,
}

/// What difv ended up being asked for, once git has been consulted.
struct Asked {
    revs: Vec<OsString>,
    file: Option<PathBuf>,
}

/// The directory to search for the repository from: the first argument that
/// says anything about where on disk it is. A revision says nothing — `HEAD~1`
/// has no parent directory but the current one — and neither does a path in
/// the current directory, so both leave the search where it would have started
/// anyway. The walk up to a directory that does exist is what keeps a deletion
/// that took the whole directory openable.
fn classify(words: Vec<OsString>, paths: Vec<OsString>) -> Target {
    let start = paths
        .iter()
        .chain(words.iter())
        .find_map(|arg| {
            let path = Path::new(arg);
            if path.is_dir() {
                return Some(path.to_path_buf());
            }
            git::nearest_existing_dir(path)
                .map(|(dir, _)| dir)
                .filter(|dir| *dir != Path::new("."))
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    Target {
        words,
        paths,
        start,
    }
}

/// Which of the arguments were revisions. On disk means a path, as it does to
/// git; not on disk means a revision if git can resolve it. An argument that
/// is neither is taken as a path anyway, because the changed-file list is the
/// only thing that can tell a file git reports as deleted from a typo, and
/// opening a deleted file by name is one of the things difv is for.
fn split(target: Target, repo: &Repo) -> Result<Asked, String> {
    let mut revs = Vec::new();
    let mut files: Vec<PathBuf> = target.paths.iter().map(PathBuf::from).collect();
    for word in target.words {
        let on_disk = Path::new(&word).exists();
        if on_disk && repo.is_revision(&word) {
            return Err(format!(
                "ambiguous argument `{}`: both revision and filename — use `--` to separate paths from revisions",
                word.to_string_lossy()
            ));
        }
        match !on_disk && repo.is_revision(&word) {
            true => revs.push(word),
            false => files.push(PathBuf::from(word)),
        }
    }
    if files.len() > 1 {
        return Err("at most one path — try `difv --help`".to_string());
    }
    if revs.len() > 2 {
        return Err("at most two revisions — try `difv --help`".to_string());
    }
    // A directory only says which repository, which is already known by now.
    let file = files.pop().filter(|path| !path.is_dir());
    Ok(Asked { revs, file })
}

/// The repository the command line asked for. Discovery is what fails when the
/// path is wrong, but the message has to name the argument the user typed
/// rather than whichever ancestor the walk above stopped at — and an argument
/// that is on disk nowhere is a different mistake from a path that is outside
/// a repository. Nothing here can say whether such an argument was meant as a
/// revision, so the message covers both.
fn open_repo(target: &Target) -> Result<Repo, String> {
    Repo::discover(&target.start).map_err(|_| {
        let candidates = || target.paths.iter().chain(target.words.iter());
        if let Some(existing) = candidates().find(|arg| Path::new(arg).exists()) {
            return format!("not a git repository: {}", Path::new(existing).display());
        }
        match candidates().next() {
            Some(arg) => format!("no such path or revision: {}", Path::new(arg).display()),
            None => "not a git repository".to_string(),
        }
    })
}

/// Everything difv can refuse before the terminal is touched says so the same
/// way: `difv: <what went wrong>`, and exit 2.
fn refuse(message: &str) -> ! {
    eprintln!("difv: {message}");
    std::process::exit(2);
}

fn main() -> Result<()> {
    let target = match parse_args(std::env::args_os().skip(1)) {
        Args::Version => {
            println!("difv {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Args::Help => {
            println!(
                "difv {}\n{}\n\n\
                 Usage: difv [<rev>] [<rev>] [--] [PATH]\n\n\
                 The revisions mean what they mean to `git diff`:\n\n\
                 \x20 difv               HEAD against the working tree\n\
                 \x20 difv <rev>         that revision against the working tree\n\
                 \x20 difv <rev>^!       what that commit changed\n\
                 \x20 difv <a>..<b>      one revision against another\n\n\
                 A revision on the right is read-only; the working tree is not.\n\n\
                 PATH picks the repository, and a file picks the repository and\n\
                 opens on that file. Either way the whole repository's changes are\n\
                 listed, so a subdirectory does not narrow them. `--` separates a\n\
                 path from a revision when a name could be either.\n\n\
                 \x20 -h, --help       this\n\
                 \x20 -V, --version    the version\n\n\
                 Press `?` inside for the keys, `,` for the settings.\n\n\
                 Config: $XDG_CONFIG_HOME/difv/config.toml, or ~/.config/difv/config.toml\n\
                 DIFV_HOME moves it: $DIFV_HOME/config.toml instead.",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_DESCRIPTION"),
            );
            return Ok(());
        }
        Args::Error(message) => refuse(&message),
        Args::Open { words, paths } => classify(words, paths),
    };
    let repo = match open_repo(&target) {
        Ok(repo) => repo,
        Err(message) => refuse(&message),
    };
    let asked = match split(target, &repo) {
        Ok(asked) => asked,
        Err(message) => refuse(&message),
    };
    // Git's own message for a revision it cannot resolve: the wording the user
    // already knows, and one difv could only paraphrase worse.
    let repo = match repo.with_revs(asked.revs) {
        Ok(repo) => repo,
        Err(err) => refuse(&err.to_string()),
    };
    // Both of these run before the alternate screen so their output lands on a
    // normal terminal rather than a torn-down TUI.
    let (config, warnings) = Config::load();
    for warning in &warnings {
        eprintln!("difv: {warning}");
    }
    let mut app = match App::new(config, repo, asked.file.as_deref()) {
        Ok(app) => app,
        Err(err) => refuse(&err.to_string()),
    };

    refuse_signals();
    // Ratatui's own hook restores raw mode and the alternate screen, but knows
    // nothing about the modes difv turns on. Installed first so it still runs.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave_modes();
        previous(info);
    }));

    let layout = app.weights;
    let mut terminal = ratatui::init();
    execute!(stdout(), Print(MOUSE_ON), EnableBracketedPaste)?;
    let result = run(&mut terminal, &mut app);
    leave_modes();
    ratatui::restore();

    // Only after the panes are back to a plain terminal, and only when the user
    // actually moved a divider — an untouched layout has nothing to remember.
    if app.config.remember_layout
        && app.weights != layout
        && let Err(err) = config::save_layout(app.weights)
    {
        eprintln!("difv: could not save the layout: {err}");
    }
    result
}

fn leave_modes() {
    let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture);
}

/// `Ctrl+C` is copy and `Ctrl+Z` is undo inside difv, and raw mode already
/// stops the terminal driver from turning either into a signal. Some terminals
/// send `SIGINT` or `SIGTSTP` themselves anyway, which kills or stops difv
/// before it can turn mouse reporting back off and leaves the shell being
/// written to on every mouse move. Since neither interruption nor suspension
/// is something difv offers — `q` quits, and `SIGTERM` still works — refusing
/// the signals is what makes the keys mean one thing everywhere.
fn refuse_signals() {
    // SAFETY: setting a disposition to SIG_IGN is async-signal-safe and touches
    // no state of ours.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
    }
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut dirty = true;
    while !app.quit {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            // `viewport_height` and the pane widths only become correct during
            // a draw, so a resize can only be caught up with after one — a
            // scroll clamped against stale dimensions would achieve nothing.
            // If the clamp actually moved something, the frame just drawn is
            // already the wrong one: loop straight back to redraw it rather
            // than fall through to a blocking read, where an event that sets
            // `dirty` itself (or a `Moved` mouse event that clears it) could
            // swallow the correction before it ever reaches the screen. The
            // clamp is idempotent, so the redraw's own pass through here
            // reports no movement and this falls through normally.
            if app.clamp_scroll() {
                dirty = true;
                continue;
            }
            dirty = false;
        }

        // Blocking is the normal case: an idle difv should cost nothing. A
        // timeout is armed only while something is due without input — a held
        // drag past an edge, a notice about to fade, a rebuild that ran out of
        // time and a pause would let finish.
        if let Some(timeout) = app.next_tick()
            && !event::poll(timeout)?
        {
            app.tick();
            dirty = true;
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                app.on_key(key);
                dirty = true;
            }
            Event::Mouse(mouse) => {
                app.on_mouse(mouse);
                // Terminals that report motion anyway must not cost a redraw.
                dirty = !matches!(mouse.kind, MouseEventKind::Moved);
            }
            Event::Paste(text) => {
                app.paste(&text);
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
        // One stat of one file, so another process's write shows up while the
        // user is still looking at the old content rather than at save time.
        app.poll_disk();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args(args.iter().map(OsString::from))
    }

    fn open(words: &[&str]) -> Args {
        Args::Open {
            words: words.iter().map(OsString::from).collect(),
            paths: Vec::new(),
        }
    }

    fn one(path: &Path) -> Target {
        classify(vec![OsString::from(path)], Vec::new())
    }

    /// Positionals are collected, not classified — that needs the disk and git
    /// — and everything a `-` starts still means what it did, `--` aside.
    #[test]
    fn the_command_line_collects_positionals_and_flags() {
        assert_eq!(parse(&[]), open(&[]));
        assert_eq!(parse(&["-V"]), Args::Version);
        assert_eq!(parse(&["--version"]), Args::Version);
        assert_eq!(parse(&["-h"]), Args::Help);
        assert_eq!(parse(&["--help"]), Args::Help);
        assert_eq!(parse(&["src/auth.ts"]), open(&["src/auth.ts"]));
        assert_eq!(parse(&["main..feature"]), open(&["main..feature"]));
        assert_eq!(
            parse(&["main", "src/auth.ts"]),
            open(&["main", "src/auth.ts"])
        );

        // Everything past `--` can only be a path, whatever it looks like.
        assert_eq!(
            parse(&["main", "--", "-weird-name"]),
            Args::Open {
                words: vec![OsString::from("main")],
                paths: vec![OsString::from("-weird-name")],
            }
        );

        let Args::Error(unknown) = parse(&["--frobnicate"]) else {
            panic!("an unknown flag is refused");
        };
        assert!(
            unknown.contains("--frobnicate") && unknown.contains("--help"),
            "{unknown}"
        );

        let Args::Error(empty) = parse(&[""]) else {
            panic!("an empty argument is refused");
        };
        assert!(empty.contains("empty argument"), "{empty}");
    }

    /// An argument difv cannot decode is an argument all the same — refusing
    /// it is fine, panicking inside the argument iterator is not.
    #[test]
    #[cfg(unix)]
    fn an_undecodable_argument_is_an_argument() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe, b'x']);
        assert_eq!(
            parse_args([bad.clone()]),
            Args::Open {
                words: vec![bad],
                paths: Vec::new(),
            }
        );
    }

    /// A deletion that took the directory with it leaves neither the file nor
    /// its parent to start from, and that is exactly when the file is worth
    /// opening by name. The walk has to reach the repository above them — and
    /// an argument that says nothing about the disk has to leave it alone.
    #[test]
    fn classification_walks_up_to_a_directory_that_exists() {
        let fixture = app::tests::Fixture::new("classify", "a\n", "b\n");
        let root = fixture.dir();

        assert_eq!(
            one(root).start,
            root,
            "a directory is its own starting point"
        );
        assert_eq!(
            one(&root.join("file.txt")).start,
            root,
            "a file starts from its directory"
        );
        assert_eq!(
            one(&root.join("src/deep/auth.ts")).start,
            root,
            "past two directories that are not there"
        );
        assert_eq!(
            classify(vec![OsString::from("HEAD~1")], Vec::new()).start,
            Path::new("."),
            "a revision says nothing about where on disk to look"
        );
    }

    /// Which arguments were revisions, once there is a repository to ask.
    #[test]
    fn arguments_are_told_apart_by_the_disk_and_by_git() {
        let fixture = app::tests::Fixture::new("split", "a\n", "b\n");
        let root = fixture.dir();
        let repo = Repo::discover(root).unwrap();
        let head = fixture.git(&["rev-parse", "HEAD"]).trim().to_string();
        let split_of = |words: Vec<&str>| {
            split(
                classify(words.iter().map(OsString::from).collect(), Vec::new()),
                &repo,
            )
        };

        let Ok(plain) = split_of(vec![]) else {
            panic!("no argument at all is fine")
        };
        assert!(plain.revs.is_empty() && plain.file.is_none());

        let file = root.join("file.txt");
        let Ok(both) = split_of(vec![&head, file.to_str().unwrap()]) else {
            panic!("a revision and a path are both allowed")
        };
        assert_eq!(both.revs, vec![OsString::from(&head)]);
        assert_eq!(both.file.as_deref(), Some(file.as_path()));

        // A directory only says which repository, so nothing is selected.
        let Ok(dir) = split_of(vec![root.to_str().unwrap()]) else {
            panic!("a directory is a path")
        };
        assert_eq!(dir.file, None);

        // A range is one argument and still one revision to difv.
        let Ok(range) = split_of(vec![&format!("{head}^!")]) else {
            panic!("a range is a revision")
        };
        assert_eq!(range.revs.len(), 1);

        // Neither on disk nor a revision: a path, so the changed-file list can
        // still recognise a file git reports as deleted.
        let Ok(gone) = split_of(vec!["src/deep/auth.ts"]) else {
            panic!("an unresolvable argument is taken as a path")
        };
        assert_eq!(gone.file.as_deref(), Some(Path::new("src/deep/auth.ts")));

        let Err(two) = split_of(vec![root.to_str().unwrap(), file.to_str().unwrap()]) else {
            panic!("a second path is refused");
        };
        assert!(two.contains("at most one path"), "{two}");

        let Err(three) = split_of(vec![&head, &head, &head]) else {
            panic!("a third revision is refused");
        };
        assert!(three.contains("at most two revisions"), "{three}");
    }

    /// A name that is both a branch and a file is git's own ambiguity, and
    /// difv refuses it the way git does, pointing at the same way out.
    #[test]
    fn an_argument_that_is_both_a_revision_and_a_file_is_refused() {
        let fixture = app::tests::Fixture::new("ambiguous", "a\n", "b\n");
        fixture.git(&["branch", "file.txt"]);
        let repo = Repo::discover(fixture.dir()).unwrap();
        let words = vec![OsString::from("file.txt")];

        // Run from inside the repository, so the name is on disk as well.
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(fixture.dir()).unwrap();
        let ambiguous = split(classify(words.clone(), Vec::new()), &repo);
        let separated = split(classify(Vec::new(), words), &repo);
        std::env::set_current_dir(previous).unwrap();

        let Err(message) = ambiguous else {
            panic!("a name that is both is refused");
        };
        assert!(
            message.starts_with("ambiguous argument `file.txt`"),
            "{message}"
        );
        assert!(message.contains("`--`"), "{message}");

        let Ok(asked) = separated else {
            panic!("past `--` it is only a path");
        };
        assert!(asked.revs.is_empty());
        assert_eq!(asked.file.as_deref(), Some(Path::new("file.txt")));
    }

    /// Whatever the walk had to settle for, the refusal names the argument the
    /// user typed — and says which of the two mistakes it was.
    #[test]
    fn a_refusal_names_the_argument_that_was_typed() {
        let outside = std::env::temp_dir().join(format!("difv-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();

        let missing = one(&outside.join("nope/deeper.txt"));
        let Err(message) = open_repo(&missing) else {
            panic!("a path outside a repository is refused");
        };
        assert!(
            message.starts_with("no such path or revision: "),
            "{message}"
        );
        assert!(message.ends_with("nope/deeper.txt"), "{message}");

        let there = one(&outside);
        let Err(message) = open_repo(&there) else {
            panic!("a directory outside a repository is refused");
        };
        assert_eq!(
            message,
            format!("not a git repository: {}", outside.display())
        );

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&outside).unwrap();
        let nothing = open_repo(&classify(Vec::new(), Vec::new()));
        std::env::set_current_dir(previous).unwrap();
        let Err(message) = nothing else {
            panic!("no argument outside a repository is refused");
        };
        assert_eq!(message, "not a git repository", "nothing to name");
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Enabling any-motion tracking and turning it back off cost every
    /// xterm.js-based terminal its mouse entirely, because a reset there means
    /// "no protocol at all" rather than "not that one". The mode must never be
    /// asked for in the first place.
    #[test]
    fn mouse_reporting_never_asks_for_idle_motion() {
        assert!(!MOUSE_ON.contains("1003"), "{MOUSE_ON:?}");
        assert!(MOUSE_ON.contains("\x1b[?1000h"), "clicks report");
        assert!(MOUSE_ON.contains("\x1b[?1002h"), "drags report");
        // SGR is requested after the urxvt encoding, so it wins wherever both
        // are understood.
        let (urxvt, sgr) = (
            MOUSE_ON.find("1015h").unwrap(),
            MOUSE_ON.find("1006h").unwrap(),
        );
        assert!(urxvt < sgr, "{MOUSE_ON:?}");
    }
}
