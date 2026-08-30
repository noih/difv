# difv

Review and tweak your Git changes side by side, in the terminal.

Three panes: your changed files, `HEAD`, and the working tree. The working-tree
pane is an editor — fix the small things while you review, save, move on. Its
right edge is a ruler of the whole file, so every change is on screen even when
the lines are not. Built to be used while the files are still moving under it:
another editor, a formatter, a rebase.

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

Or, with a Rust toolchain (1.88+):

```bash
cargo install --git https://github.com/noih/difv
```

Apple Silicon macOS and Linux. `git` has to be on your `PATH`.

## Use

Run `difv` inside a Git repository and it shows what `git diff HEAD` would, with
an editor on the right. The arguments are `git diff`'s too, so there is nothing
new to learn:

```bash
difv                 # HEAD against the working tree
difv main            # a revision against the working tree
difv 3d8cab3^!       # what one commit changed
difv main..feature   # one revision against another
```

Anything `git` accepts as a revision works, because `git` is what resolves it.
A revision on the right is read-only; the working tree is not.

`difv -C <path>` points it at a repository you are not standing in, the way
`git -C` does. A directory only says which repository — the list is the whole
repository's changes either way — and a file says which repository *and* which
file to open on.

- Click, scroll, or arrow around; drag a border to resize; drag to select and
  `Ctrl+C` to copy from either side; right-click a file to copy its path. The
  right pane is editable: focus it and type. `Ctrl+S` saves, `Ctrl+Z` undoes,
  `Esc` gets you back to the file list.
- `?` shows every key. `,` opens the settings, which are also plain TOML in
  `~/.config/difv/config.toml` with every option explained.

## License

MIT
