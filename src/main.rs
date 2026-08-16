mod app;
mod clipboard;
mod config;
mod diff;
mod editor;
mod git;
mod settings;
mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyEventKind,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::style::Print;

use app::App;
use config::Config;

/// How long the user has to be idle before a coarse stretch under the viewport
/// is refined — after typing that outran the budget, or after scrolling into a
/// stretch that did. Only armed while there is one on screen.
const REFINE_PAUSE: Duration = Duration::from_millis(300);

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

/// difv takes no arguments — it always compares `HEAD` against the working
/// tree — but a command line tool that cannot say what version it is cannot be
/// packaged or reported against.
fn answer_flags() -> bool {
    let Some(flag) = std::env::args().nth(1) else {
        return false;
    };
    match flag.as_str() {
        "-V" | "--version" => println!("difv {}", env!("CARGO_PKG_VERSION")),
        "-h" | "--help" => println!(
            "difv {}\n{}\n\n\
             Usage: difv\n\n\
             Run it inside a Git repository. It takes no arguments: `HEAD` on the\n\
             left, the working tree on the right, and the right side is editable.\n\
             Press `?` inside for the keys, `,` for the settings.\n\n\
             Config: $XDG_CONFIG_HOME/difv/config.toml, or ~/.config/difv/config.toml\n\
             DIFV_HOME moves it: $DIFV_HOME/config.toml instead.",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_DESCRIPTION"),
        ),
        other => {
            eprintln!("difv: unknown argument `{other}` — try `difv --help`");
            std::process::exit(2);
        }
    }
    true
}

fn main() -> Result<()> {
    if answer_flags() {
        return Ok(());
    }
    // Both of these run before the alternate screen so their output lands on a
    // normal terminal rather than a torn-down TUI.
    let (config, warnings) = Config::load();
    for warning in &warnings {
        eprintln!("difv: {warning}");
    }
    let mut app = App::new(config)?;

    refuse_suspension();
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

/// `Ctrl+Z` is undo inside difv, and raw mode already stops the terminal driver
/// from turning it into a signal. Some terminals send `SIGTSTP` themselves
/// anyway, which stops difv before it can turn mouse reporting back off and
/// leaves the shell being written to on every mouse move. Since suspension is
/// not something difv offers, refusing the signal is what makes the key mean
/// one thing everywhere.
fn refuse_suspension() {
    // SAFETY: setting a disposition to SIG_IGN is async-signal-safe and touches
    // no state of ours.
    unsafe {
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
        // timeout is armed only when a rebuild ran out of time and a pause
        // would let it finish properly.
        let pending = app.wants_refine();
        if pending && !event::poll(REFINE_PAUSE)? {
            app.refine();
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
