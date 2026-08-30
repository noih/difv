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

/// difv's command line is `git diff`'s revisions, so that nothing about them
/// has to be learned twice, plus `-C` for the one thing `git diff` has no good
/// answer for. A positional is a revision and only a revision: `git diff` has
/// to take a bare path because filtering by pathspec is its everyday use, and
/// pays for it with `--` and "ambiguous argument"; difv's path picks a
/// repository and a starting file rather than filtering, so borrowing the
/// syntax would have borrowed the ambiguity to mean something else by it.
///
/// Parsing touches neither the filesystem nor the terminal, which is what
/// makes it the table of outcomes it is tested as — and, now that nothing is
/// classified by whether it exists, all of it.
#[derive(Debug, PartialEq, Eq)]
enum Args {
    Version,
    Help,
    Open {
        /// Up to two, as `git diff` takes them, in the order they were typed.
        revs: Vec<OsString>,
        /// What followed `-C`.
        path: Option<OsString>,
    },
    Error(String),
}

/// Arguments are taken as `OsString`, not `String`: a path is one of the
/// things this takes, and a path is not required to be UTF-8 anywhere difv
/// runs. Refusing one is fine; panicking inside `std::env::args()` on a file
/// name the user can see in their own shell is not.
fn parse_args<I: IntoIterator<Item = OsString>>(args: I) -> Args {
    let (mut revs, mut path) = (Vec::new(), None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        // A leading `-` is what separates a flag from a revision, whatever
        // the rest of the argument is: one difv cannot decode is still refused
        // rather than handed to git, where `--output=…` would be obeyed. What
        // follows `-C` is a path whatever it starts with, which is the escape
        // hatch for a file named like a flag.
        let text = arg.to_string_lossy();
        if text.starts_with('-') {
            match text.as_ref() {
                "-V" | "--version" => return Args::Version,
                "-h" | "--help" => return Args::Help,
                "-C" => {
                    if path.is_some() {
                        return Args::Error("at most one -C — try `difv --help`".to_string());
                    }
                    let Some(next) = args.next() else {
                        return Args::Error("-C needs a path — try `difv --help`".to_string());
                    };
                    if next.is_empty() {
                        return Args::Error("empty path — try `difv --help`".to_string());
                    }
                    path = Some(next);
                }
                other => {
                    return Args::Error(format!("unknown argument `{other}` — try `difv --help`"));
                }
            }
            continue;
        }
        if arg.is_empty() {
            return Args::Error("empty revision — try `difv --help`".to_string());
        }
        if revs.len() == 2 {
            return Args::Error("at most two revisions — try `difv --help`".to_string());
        }
        revs.push(arg);
    }
    Args::Open { revs, path }
}

/// Where difv starts: the path as `-C` gave it, kept for what difv says about
/// it; the directory to discover the repository from; and the file to select
/// once the repository is known.
struct Target {
    typed: Option<PathBuf>,
    start: PathBuf,
    file: Option<PathBuf>,
}

/// Anything `-C` names that is not a directory is taken as a file, including a
/// path that is not on disk at all: a file git reports as deleted is one of the
/// things difv is most often opened on, and it is only the changed-file list
/// that can tell that case from a typo. Whether the path exists is settled
/// there, once the list is in hand — and until then the walk up to a directory
/// that does exist is what keeps a deletion that took the whole directory
/// openable.
fn classify(path: Option<OsString>) -> Target {
    let Some(typed) = path.map(PathBuf::from) else {
        return Target {
            typed: None,
            start: PathBuf::from("."),
            file: None,
        };
    };
    if typed.is_dir() {
        return Target {
            start: typed.clone(),
            typed: Some(typed),
            file: None,
        };
    }
    let start = git::nearest_existing_dir(&typed)
        .map(|(dir, _)| dir)
        .unwrap_or(Path::new("."))
        .to_path_buf();
    Target {
        start,
        file: Some(typed.clone()),
        typed: Some(typed),
    }
}

/// The repository the command line asked for. Discovery is what fails when the
/// path is wrong, but the message has to name the path the user typed rather
/// than whichever ancestor the walk above stopped at — and a path that is not
/// there at all is a different mistake from one that is outside a repository.
fn open_repo(target: &Target) -> Result<Repo, String> {
    Repo::discover(&target.start).map_err(|_| {
        let Some(typed) = &target.typed else {
            return "not a git repository".to_string();
        };
        match typed.exists() {
            true => format!("not a git repository: {}", typed.display()),
            false => format!("no such path: {}", typed.display()),
        }
    })
}

/// A revision git could not resolve. When the argument is also on disk it is
/// almost certainly the positional path difv took before `-C` existed, so the
/// refusal says where a path goes now — one `stat`, on a command line that is
/// being refused anyway.
fn bad_revision(err: &anyhow::Error, revs: &[OsString]) -> String {
    let message = err.to_string();
    match revs.iter().any(|rev| Path::new(rev).exists()) {
        true => format!("{message} — a path goes after -C"),
        false => message,
    }
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
                 Usage: difv [<rev>] [<rev>] [-C <path>]\n\n\
                 The revisions mean what they mean to `git diff`:\n\n\
                 \x20 difv               HEAD against the working tree\n\
                 \x20 difv <rev>         that revision against the working tree\n\
                 \x20 difv <rev>^!       what that commit changed\n\
                 \x20 difv <a>..<b>      one revision against another\n\n\
                 A revision on the right is read-only; the working tree is not.\n\n\
                 \x20 -C <path>        work in that repository, as it does for git.\n\
                 \x20                  A directory picks the repository; a file picks\n\
                 \x20                  the repository and opens on that file. Neither\n\
                 \x20                  narrows the list to what is under it.\n\
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
        Args::Open { revs, path } => (classify(path), revs),
    };
    let (target, revs) = target;
    let repo = match open_repo(&target) {
        Ok(repo) => repo,
        Err(message) => refuse(&message),
    };
    // Git's own message for a revision it cannot resolve: the wording the user
    // already knows, and one difv could only paraphrase worse.
    let repo = match repo.with_revs(revs.clone()) {
        Ok(repo) => repo,
        Err(err) => refuse(&bad_revision(&err, &revs)),
    };
    // Both of these run before the alternate screen so their output lands on a
    // normal terminal rather than a torn-down TUI.
    let (config, warnings) = Config::load();
    for warning in &warnings {
        eprintln!("difv: {warning}");
    }
    let mut app = match App::new(config, repo, target.file.as_deref()) {
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

    fn open(revs: &[&str], path: Option<&str>) -> Args {
        Args::Open {
            revs: revs.iter().map(OsString::from).collect(),
            path: path.map(OsString::from),
        }
    }

    fn error(args: &[&str]) -> String {
        let Args::Error(message) = parse(args) else {
            panic!("{args:?} is refused");
        };
        assert!(message.contains("--help"), "{message}");
        message
    }

    /// Positionals are revisions and only revisions; the path is what follows
    /// `-C`, from either end of the command line.
    #[test]
    fn the_command_line_is_revisions_and_one_dash_c() {
        assert_eq!(parse(&[]), open(&[], None));
        assert_eq!(parse(&["-V"]), Args::Version);
        assert_eq!(parse(&["--version"]), Args::Version);
        assert_eq!(parse(&["-h"]), Args::Help);
        assert_eq!(parse(&["--help"]), Args::Help);

        assert_eq!(parse(&["main"]), open(&["main"], None));
        assert_eq!(parse(&["main..feature"]), open(&["main..feature"], None));
        assert_eq!(
            parse(&["main", "feature"]),
            open(&["main", "feature"], None)
        );

        // `-C` reads whatever follows it, wherever it sits, and a path is not
        // a revision however much it looks like one.
        let there = open(&["main..feature"], Some("/repo"));
        assert_eq!(parse(&["-C", "/repo", "main..feature"]), there);
        assert_eq!(parse(&["main..feature", "-C", "/repo"]), there);
        assert_eq!(
            parse(&["-C", "-weird-name"]),
            open(&[], Some("-weird-name"))
        );

        assert!(error(&["--frobnicate"]).contains("--frobnicate"));
        assert!(error(&["a", "b", "c"]).contains("at most two revisions"));
        assert!(error(&["-C", "src", "-C", "tests"]).contains("at most one -C"));
        assert!(error(&["-C"]).contains("-C needs a path"));
        assert!(error(&["-C", ""]).contains("empty path"));
        assert!(error(&[""]).contains("empty revision"));
    }

    /// An argument difv cannot decode is an argument all the same — a
    /// revision git can refuse for itself, or a path — unless it starts with
    /// `-`, which makes it a flag difv does not know, never a revision git
    /// might obey as an option.
    #[test]
    #[cfg(unix)]
    fn an_undecodable_argument_is_an_argument() {
        use std::os::unix::ffi::OsStringExt;
        let flag = OsString::from_vec(b"--output=x\xff".to_vec());
        let Args::Error(refused) = parse_args([flag]) else {
            panic!("a flag difv cannot decode is still a flag");
        };
        assert!(refused.starts_with("unknown argument"), "{refused}");

        let bad = OsString::from_vec(vec![0xff, 0xfe, b'x']);
        assert_eq!(
            parse_args([bad.clone()]),
            Args::Open {
                revs: vec![bad.clone()],
                path: None,
            }
        );
        assert_eq!(
            parse_args([OsString::from("-C"), bad.clone()]),
            Args::Open {
                revs: Vec::new(),
                path: Some(bad),
            }
        );
    }

    /// A deletion that took the directory with it leaves neither the file nor
    /// its parent to start from, and that is exactly when the file is worth
    /// opening by name. The walk has to reach the repository above them.
    #[test]
    fn classification_walks_up_to_a_directory_that_exists() {
        let fixture = app::tests::Fixture::new("classify", "a\n", "b\n");
        let root = fixture.dir();
        let of = |path: &Path| classify(Some(OsString::from(path)));

        let none = classify(None);
        assert_eq!(none.start, Path::new("."));
        assert_eq!((none.typed, none.file), (None, None));

        let dir = of(root);
        assert_eq!(dir.start, root, "a directory is its own starting point");
        assert_eq!(dir.file, None);

        let file = of(&root.join("file.txt"));
        assert_eq!(file.start, root, "a file starts from its directory");
        assert_eq!(file.file.as_deref(), Some(root.join("file.txt").as_path()));

        let gone = of(&root.join("src/deep/auth.ts"));
        assert_eq!(gone.start, root, "past two directories that are not there");
        assert_eq!(
            gone.file.as_deref(),
            Some(root.join("src/deep/auth.ts").as_path())
        );
    }

    /// Whatever the walk had to settle for, the refusal names the path the
    /// user typed — and says which of the two mistakes it was.
    #[test]
    fn a_refusal_names_the_path_that_was_typed() {
        let outside = std::env::temp_dir().join(format!("difv-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let of = |path: &Path| classify(Some(OsString::from(path)));

        let missing = of(&outside.join("nope/deeper.txt"));
        let Err(message) = open_repo(&missing) else {
            panic!("a path outside a repository is refused");
        };
        assert!(message.starts_with("no such path: "), "{message}");
        assert!(message.ends_with("nope/deeper.txt"), "{message}");

        let there = of(&outside);
        let Err(message) = open_repo(&there) else {
            panic!("a directory outside a repository is refused");
        };
        assert_eq!(
            message,
            format!("not a git repository: {}", outside.display())
        );

        // No `-C` at all, run from outside a repository: there is nothing to
        // name. Built by hand rather than by changing the process's directory,
        // which every other test would share.
        let nothing = Target {
            typed: None,
            start: outside.clone(),
            file: None,
        };
        let Err(message) = open_repo(&nothing) else {
            panic!("no argument outside a repository is refused");
        };
        assert_eq!(message, "not a git repository", "nothing to name");
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// `difv <path>` is what difv took before `-C`, so the revision git
    /// refuses it as says where a path goes now.
    #[test]
    fn the_old_positional_path_is_pointed_at_dash_c() {
        let fixture = app::tests::Fixture::new("old-form", "a\n", "b\n");
        let repo = Repo::discover(fixture.dir()).unwrap();
        let file = OsString::from(fixture.dir().join("file.txt"));

        let Err(err) = repo.with_revs(vec![file.clone()]) else {
            panic!("a path is not a revision");
        };
        let pointed = bad_revision(&err, &[file]);
        assert!(pointed.contains("bad revision"), "{pointed}");
        assert!(pointed.ends_with(" — a path goes after -C"), "{pointed}");

        // An argument that is not on disk is only ever a bad revision.
        let repo = Repo::discover(fixture.dir()).unwrap();
        let Err(err) = repo.with_revs(vec![OsString::from("nope")]) else {
            panic!("a name that resolves to nothing is refused");
        };
        let plain = bad_revision(&err, &[OsString::from("nope")]);
        assert_eq!(plain, "bad revision 'nope'");
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
