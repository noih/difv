# difv

Review and tweak your Git changes side by side, in the terminal.

Like the VS Code diff editor, cut down to the part you actually use before a
commit: see what changed, jump between files, fix the small things, move on.

```
┌ Changes (4) ────────┬ HEAD / Before ──────────┬ Working Tree / Current ─────┐
│ M src/auth.ts       │ 1 const timeout = 3000  │ 1 const timeout = 5000      │
│ D src/old.ts        │ 2 oldCall()             │ 2 newCall()                 │
│ M src/user.ts       │                         │ 3 anotherLine()             │
│ A src/utils.ts      │ 3 return user           │ 4 return user               │
└─────────────────────┴─────────────────────────┴─────────────────────────────┘
 [settings ,] [help ?]                                             1-4/4
```

## Install

```bash
brew install noih/tap/difv
```

Homebrew installs a prebuilt binary and needs no toolchain. With a Rust
toolchain you can build from the repository or a clone instead:

```bash
cargo install --git https://github.com/noih/difv
cargo install --path .
```

`cargo install` puts `difv` in `~/.cargo/bin` and wants Rust 1.88 or newer
(edition 2024 and let chains). Either way `git` has to be on your `PATH`: difv
shells out to it rather than linking a git library.

**Platforms:** Apple Silicon macOS, and Linux on x86_64 and arm64. Homebrew
refuses on an Intel Mac rather than serving a binary nobody asked for, and
`cargo install` still builds there. Windows is not supported: difv ignores
`SIGTSTP` so `Ctrl+Z` can be undo, and it looks for its config under
`$XDG_CONFIG_HOME` or `$HOME`, neither of which Windows has. Nothing else
stands in the way, so a port is mostly those two spots.

## Usage

```bash
difv
```

Run it inside a Git repository. It takes no arguments — `HEAD` on the left, the
working tree on the right, untracked files included — beyond `--version` and
`--help`.

The Current pane is editable: focus it and type. There is no mode to enter and
no mode to leave, which does mean `q` and `r` type a letter while you are in it.
`Esc` first.

| Key | Action |
| --- | --- |
| `↑` `↓` | Select file / scroll diff / move cursor |
| `Shift+Tab` | Next pane, from anywhere |
| `Tab` | One indent, in the Current pane |
| `Alt+↑` `Alt+↓` | Previous / next change |
| `←` `→` | Horizontal scroll / move cursor |
| `PageUp` `PageDown` `Home` `End` | Scroll / move cursor |
| `Ctrl+S` | Save the file |
| `Ctrl+Z` `Ctrl+Y` | Undo / redo |
| `Ctrl+C` `Ctrl+X` `Ctrl+V` | Copy / cut / paste (Current pane) |
| `Ctrl+B` | Hide / show the file list |
| `,` | Settings panel |
| `?` | Key list (`↑` `↓` scrolls it on a short terminal) |
| `r` | Reload changes |
| `Esc` | Back to file list |
| `q` | Quit |

The footer carries `[settings ,]` and `[help ?]` — click either, or press its
key. The right of the footer shows which rows are on screen. Either panel takes
the keyboard and the mouse while it is open, so nothing you do to it reaches the
diff underneath.

Mouse works too: click a file, click to place the cursor, drag to select. The
wheel scrolls whichever pane the pointer is over and focuses it, so you rarely
need `Shift+Tab` at all. A sideways two-finger swipe scrolls horizontally, in
terminals that report it (WezTerm, kitty, and Ghostty do; iTerm2 and macOS
Terminal.app do not). `Shift` or `Option` + wheel scrolls sideways too, and
`←` `→` always work. Drag the border between two panes to resize them; the
proportion survives a terminal resize.

Because difv captures the mouse, your terminal's own selection is off while it
runs — hold `Shift` (or `Option` in iTerm2) while dragging to get it back.
`Ctrl+C` copies through `pbcopy`, `wl-copy`, `xclip` or `xsel`, whichever is
installed, and also emits OSC 52, so the copy reaches your local clipboard even
over SSH or inside tmux.

difv expects the files to be moving under it — a formatter, a build, a rebase,
another editor. If one changes on disk, difv picks it up; if you have unsaved
edits in it, difv flags it instead and refuses to save over the other write.
`*` marks a file with unsaved edits, `!` one that was rewritten under them; the
Current pane's title carries the `*` too, for when the file list is hidden.

CJK and other wide characters count as the two columns they are drawn in, so the
cursor, the selection and the scroll bound all land where the text is.

## Configuration

Optional, at `$XDG_CONFIG_HOME/difv/config.toml` (or
`~/.config/difv/config.toml`). `XDG_CONFIG_HOME` belongs to the freedesktop base
directory spec rather than to difv — it moves every tool's config at once. Set
`DIFV_HOME` to move difv's alone; both `config.toml` and `layout.toml` follow
it.

Indentation and line endings are detected from each file; those settings are the
fallback for files that give nothing to detect from.

```toml
detect_indent = true
indent_width = 2          # 1-16
use_tabs = false
detect_line_ending = true
line_ending = "lf"        # "lf" or "crlf"
scroll_lines = 3          # diff rows per wheel notch, 1-16
remember_layout = true
momentum_delay_ms = 300   # ignore scrolling for this long after a keystroke, 0-1000
```

Set either `detect_*` to `false` to force the fixed value everywhere.

A trackpad keeps sending scroll events after your fingers leave it, and those
arriving just after a keystroke drag the view off the line you are editing.
`momentum_delay_ms` ignores scrolling for that long after each keystroke. Set it
to `0` if you have turned inertia off in System Settings → Accessibility →
Pointer Control → Trackpad Options, or if you prefer the wheel to always answer.

`,` opens the same settings inside difv: `↑` `↓` to pick one, `←` `→` to change
it, `Esc` to close. A change applies at once and is written to `config.toml`
straight away — there is no save step. That rewrite replaces the whole file, so
comments you added by hand do not survive a change made from the panel; edit the
file directly if you want to keep them.

With `remember_layout` on, pane widths you drag are written to `layout.toml`
beside the config when difv exits, and restored on the next run. Delete that
file to go back to the default split.

## Status

The viewer and the editor are in. Soft wrap, syntax highlighting, word-level
diff, and search come after.

## Cutting a release

`brew install` works off the binaries a tag builds, and the tap is a second,
tiny repository that points at them:

1. `git tag v1.0.0 && git push origin v1.0.0`. The release workflow builds
   Apple Silicon macOS and Linux on both architectures, attaches the tarballs
   and their checksums, and generates `difv.rb` with those checksums filled in.
2. Copy that `difv.rb` from the release into `noih/homebrew-tap` as
   `Formula/difv.rb`. The `homebrew-` prefix is what makes
   `brew install noih/tap/difv` find it, and one tap holds any later tools too.

Step 2 is one file per release; a token in this repository could push it, at the
cost of a secret that can write to another repository.
