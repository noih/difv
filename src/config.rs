use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
        }
    }
}

/// How one indent level is written. Detected per file; the config supplies the
/// fallback when a file has nothing to detect from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Indent {
    pub width: usize,
    pub tabs: bool,
}

impl Indent {
    pub fn unit(self) -> String {
        if self.tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.width)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub detect_indent: bool,
    pub indent_width: usize,
    pub use_tabs: bool,
    pub detect_line_ending: bool,
    pub line_ending: String,
    /// Diff rows one wheel notch moves.
    pub scroll_lines: usize,
    /// Carry the pane widths over to the next run.
    pub remember_layout: bool,
    /// How long after a keystroke scroll events are ignored, so a trackpad's
    /// momentum cannot drag the view off the line being edited. 0 turns it off,
    /// which is the right setting once the system's own inertia is off.
    pub momentum_delay_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            detect_indent: true,
            indent_width: 2,
            use_tabs: false,
            detect_line_ending: true,
            line_ending: "lf".to_string(),
            scroll_lines: 3,
            remember_layout: true,
            momentum_delay_ms: 300,
        }
    }
}

impl Config {
    /// Read the config file, always returning a usable value. Problems are
    /// reported rather than silently corrected: a setting the user believes is
    /// applied and is not is the worst failure a config file has.
    pub fn load() -> (Self, Vec<String>) {
        let Some(path) = config_path() else {
            return (Self::default(), Vec::new());
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (Self::default(), Vec::new());
        };
        Self::parse(&text, &path.display().to_string())
    }

    fn parse(text: &str, origin: &str) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(err) => {
                warnings.push(format!("{origin}: {err}"));
                return (Self::default(), warnings);
            }
        };
        // The settings panel is the list of keys difv has; a key missing from it
        // is a key the panel cannot show, so the two cannot drift apart.
        for key in table.keys() {
            if !crate::settings::ALL.iter().any(|s| s.label() == key) {
                warnings.push(format!("{origin}: unrecognised key `{key}`"));
            }
        }
        let mut config: Self = match table.try_into() {
            Ok(config) => config,
            Err(err) => {
                warnings.push(format!("{origin}: {err}"));
                return (Self::default(), warnings);
            }
        };

        let defaults = Self::default();
        if config.indent_width == 0 {
            warnings.push(format!(
                "{origin}: indent_width must be at least 1, using {}",
                defaults.indent_width
            ));
            config.indent_width = defaults.indent_width;
        }
        if config.scroll_lines == 0 {
            warnings.push(format!(
                "{origin}: scroll_lines must be at least 1, using {}",
                defaults.scroll_lines
            ));
            config.scroll_lines = defaults.scroll_lines;
        }
        if !matches!(config.line_ending.as_str(), "lf" | "crlf") {
            warnings.push(format!(
                "{origin}: line_ending must be \"lf\" or \"crlf\", using \"{}\"",
                defaults.line_ending
            ));
            config.line_ending = defaults.line_ending;
        }
        (config, warnings)
    }

    fn fallback_indent(&self) -> Indent {
        Indent {
            width: self.indent_width,
            tabs: self.use_tabs,
        }
    }

    fn fallback_eol(&self) -> Eol {
        if self.line_ending == "crlf" {
            Eol::Crlf
        } else {
            Eol::Lf
        }
    }

    pub fn indent_for(&self, lines: &[String]) -> Indent {
        if !self.detect_indent {
            return self.fallback_indent();
        }
        detect_indent(lines).unwrap_or_else(|| self.fallback_indent())
    }

    pub fn eol_for(&self, detected: Option<Eol>) -> Eol {
        match (self.detect_line_ending, detected) {
            (true, Some(eol)) => eol,
            _ => self.fallback_eol(),
        }
    }
}

/// Where difv keeps `config.toml` and `layout.toml`.
///
/// `XDG_CONFIG_HOME` is not difv's variable — it is the freedesktop base
/// directory spec, one setting that moves every tool's config at once, which is
/// why difv reads it rather than inventing a name. `DIFV_HOME` is the private
/// override for moving this tool alone, which is what a variable named after
/// difv can honestly promise.
fn config_dir() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(dir) = var("DIFV_HOME") {
        return Some(PathBuf::from(dir));
    }
    let base = match var("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(var("HOME")?).join(".config"),
    };
    Some(base.join("difv"))
}

pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

/// Written by difv rather than by the user, so it lives beside the config file
/// instead of inside it — rewriting a hand-edited file would lose its comments.
fn layout_path() -> Option<PathBuf> {
    Some(config_dir()?.join("layout.toml"))
}

/// The pane widths from the last run, if they were saved and still make sense.
/// Anything else means the default layout, which is always usable.
/// What a divider drag leaves behind: the three column widths, and the two
/// heights the left column is split into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub weights: [u16; 3],
    pub split: [u16; 2],
}

pub fn load_layout() -> Option<Layout> {
    parse_layout(&std::fs::read_to_string(layout_path()?).ok()?)
}

pub fn save_layout(layout: Layout) -> anyhow::Result<()> {
    let Some(path) = layout_path() else {
        return Ok(());
    };
    write(&path, &render_layout(layout))
}

/// Write the whole file. difv owns every key it knows about, so anything the
/// user wrote around them — comments, ordering, blank lines — does not survive a
/// save from inside the app.
pub fn save(path: &std::path::Path, config: &Config) -> anyhow::Result<()> {
    write(path, &render(config))
}

/// Both files are difv's own, and the settings panel rewrites the config on
/// every keypress, so they go through the same atomic write the user's source
/// files get: an interrupted save must never leave a truncated config.
fn write(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::git::write_atomic(path, text.as_bytes())
}

fn render(config: &Config) -> String {
    let Config {
        detect_indent,
        indent_width,
        use_tabs,
        detect_line_ending,
        line_ending,
        scroll_lines,
        remember_layout,
        momentum_delay_ms,
    } = config;
    format!(
        "# Written by difv. Comments and ordering are not preserved when difv\n\
         # saves this file from its settings panel.\n\
         detect_indent = {detect_indent}\n\
         indent_width = {indent_width}\n\
         use_tabs = {use_tabs}\n\
         detect_line_ending = {detect_line_ending}\n\
         line_ending = \"{line_ending}\"\n\
         scroll_lines = {scroll_lines}\n\
         remember_layout = {remember_layout}\n\
         momentum_delay_ms = {momentum_delay_ms}\n"
    )
}

fn parse_layout(text: &str) -> Option<Layout> {
    let table: toml::Table = text.parse().ok()?;
    let numbers = |key: &str| -> Option<Vec<u16>> {
        Some(
            table
                .get(key)?
                .as_array()?
                .iter()
                .filter_map(|value| u16::try_from(value.as_integer()?).ok())
                .filter(|width| *width > 0)
                .collect(),
        )
    };
    Some(Layout {
        weights: <[u16; 3]>::try_from(numbers("weights")?.as_slice()).ok()?,
        // A file written before the Commits pane existed has widths and no
        // split; its widths are still what the user arranged.
        split: numbers("split")
            .and_then(|split| <[u16; 2]>::try_from(split.as_slice()).ok())
            .unwrap_or(DEFAULT_SPLIT),
    })
}

/// The Changes pane and the Commits pane share the column evenly until a drag
/// says otherwise.
pub const DEFAULT_SPLIT: [u16; 2] = [1, 1];

fn render_layout(layout: Layout) -> String {
    let [files, old, new] = layout.weights;
    let [changes, commits] = layout.split;
    format!(
        "# Written by difv. Set `remember_layout = false` in config.toml to stop.\n\
         weights = [{files}, {old}, {new}]\n\
         split = [{changes}, {commits}]\n"
    )
}

/// The dominant line ending in the raw file, or `None` when it has none.
pub fn detect_eol(text: &str) -> Option<Eol> {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    match (crlf, lf) {
        (0, 0) => None,
        (crlf, lf) if crlf > lf => Some(Eol::Crlf),
        _ => Some(Eol::Lf),
    }
}

/// One pass over the file: tab-led against space-led lines by majority, and for
/// spaces the most common step between consecutive indented lines. Returns
/// `None` when there is nothing to go on.
//
// ponytail: majority + modal-delta heuristic, the same shape editors use. If it
// guesses wrong on real files, the next step is weighting by nesting depth
// rather than a parser.
fn detect_indent(lines: &[String]) -> Option<Indent> {
    let mut tabs = 0usize;
    let mut spaces = 0usize;
    let mut steps = [0usize; 9];
    let mut previous = 0usize;

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let width = line.len() - line.trim_start_matches(' ').len();
        if width == 0 {
            previous = 0;
            continue;
        }
        spaces += 1;
        if let Some(step) = width.checked_sub(previous).filter(|s| (1..=8).contains(s)) {
            steps[step] += 1;
        }
        previous = width;
    }

    if tabs == 0 && spaces == 0 {
        return None;
    }
    if tabs > spaces {
        return Some(Indent {
            width: 4,
            tabs: true,
        });
    }
    let (width, count) = steps
        .iter()
        .enumerate()
        .skip(1)
        .max_by_key(|(_, count)| **count)?;
    (*count > 0).then_some(Indent { width, tabs: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn missing_and_partial_files_still_produce_a_config() {
        let (config, warnings) = Config::parse("", "test");
        assert!(warnings.is_empty());
        assert_eq!(config.indent_width, 2);

        let (config, warnings) = Config::parse("indent_width = 8", "test");
        assert!(warnings.is_empty());
        assert_eq!(config.indent_width, 8);
        assert!(config.detect_indent);
    }

    #[test]
    fn malformed_file_falls_back_and_reports() {
        let (config, warnings) = Config::parse("indent_width = ", "test");
        assert_eq!(config.indent_width, 2);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn invalid_values_fall_back_and_report() {
        let (config, warnings) = Config::parse("indent_width = 0", "test");
        assert_eq!(config.indent_width, 2);
        assert_eq!(warnings.len(), 1);

        let (config, warnings) = Config::parse("line_ending = \"cr\"", "test");
        assert_eq!(config.line_ending, "lf");
        assert_eq!(warnings.len(), 1);

        let (config, warnings) = Config::parse("scroll_lines = 0", "test");
        assert_eq!(config.scroll_lines, 3);
        assert_eq!(warnings.len(), 1);

        let (config, warnings) = Config::parse("scroll_lines = 1", "test");
        assert_eq!(config.scroll_lines, 1);
        assert!(warnings.is_empty());
    }

    #[test]
    fn unknown_key_is_reported_but_not_fatal() {
        let (config, warnings) = Config::parse("wrap = true\nindent_width = 2", "test");
        assert_eq!(config.indent_width, 2);
        assert!(warnings[0].contains("wrap"));
    }

    #[test]
    fn a_written_config_reads_back_as_itself() {
        let config = Config {
            scroll_lines: 7,
            line_ending: "crlf".to_string(),
            use_tabs: true,
            remember_layout: false,
            ..Config::default()
        };

        let (round_tripped, warnings) = Config::parse(&render(&config), "test");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(round_tripped, config);
    }

    #[test]
    fn a_saved_layout_round_trips_and_junk_is_ignored() {
        let layout = Layout {
            weights: [30, 40, 50],
            split: [7, 5],
        };
        assert_eq!(parse_layout(&render_layout(layout)), Some(layout));

        // A file written before the Commits pane existed still says what the
        // user arranged about the columns.
        assert_eq!(
            parse_layout("weights = [30, 40, 50]"),
            Some(Layout {
                weights: [30, 40, 50],
                split: DEFAULT_SPLIT,
            })
        );
        // A split that cannot be trusted is the default, not a refusal: the
        // widths beside it are still good.
        assert_eq!(
            parse_layout("weights = [30, 40, 50]\nsplit = [0, 3]"),
            Some(Layout {
                weights: [30, 40, 50],
                split: DEFAULT_SPLIT,
            })
        );

        // A file that cannot be trusted means the default layout, never a pane
        // the user cannot see or a panic.
        assert_eq!(parse_layout(""), None);
        assert_eq!(parse_layout("weights = [1, 2]"), None);
        assert_eq!(parse_layout("weights = [1, 0, 2]"), None);
        assert_eq!(parse_layout("weights = [-1, 2, 3]"), None);
        assert_eq!(parse_layout("weights = \"wide\""), None);
        assert_eq!(parse_layout("not toml at all ["), None);
    }

    #[test]
    fn indent_is_detected_from_the_file() {
        let two = lines("fn a() {\n  let x = 1;\n    if x {\n      y();\n    }\n}");
        assert_eq!(
            detect_indent(&two),
            Some(Indent {
                width: 2,
                tabs: false
            })
        );

        let four = lines("fn a() {\n    let x = 1;\n        y();\n}");
        assert_eq!(
            detect_indent(&four),
            Some(Indent {
                width: 4,
                tabs: false
            })
        );
    }

    #[test]
    fn tab_indentation_wins_by_majority() {
        let mixed = lines("a\n\tb\n\tc\n\td\n  e");
        assert_eq!(detect_indent(&mixed).map(|i| i.tabs), Some(true));

        let mostly_spaces = lines("a\n  b\n    c\n  d\n\te");
        assert_eq!(detect_indent(&mostly_spaces).map(|i| i.tabs), Some(false));
    }

    #[test]
    fn a_file_without_indentation_detects_nothing() {
        assert_eq!(detect_indent(&lines("one\ntwo\nthree")), None);
        assert_eq!(detect_indent(&[]), None);
    }

    #[test]
    fn detection_can_be_turned_off() {
        let (config, _) = Config::parse("detect_indent = false\nindent_width = 8", "test");
        let two_space = lines("a\n  b");
        assert_eq!(config.indent_for(&two_space).width, 8);

        let (config, _) =
            Config::parse("detect_line_ending = false\nline_ending = \"crlf\"", "test");
        assert_eq!(config.eol_for(Some(Eol::Lf)), Eol::Crlf);
    }

    #[test]
    fn eol_is_detected_by_majority_and_falls_back() {
        assert_eq!(detect_eol("a\r\nb\r\n"), Some(Eol::Crlf));
        assert_eq!(detect_eol("a\nb\n"), Some(Eol::Lf));
        assert_eq!(detect_eol("a\r\nb\nc\n"), Some(Eol::Lf));
        assert_eq!(detect_eol("no newline"), None);

        let config = Config::default();
        assert_eq!(config.eol_for(None), Eol::Lf);
        assert_eq!(config.eol_for(Some(Eol::Crlf)), Eol::Crlf);
    }

    #[test]
    fn indent_unit_reflects_the_style() {
        assert_eq!(
            Indent {
                width: 3,
                tabs: false
            }
            .unit(),
            "   "
        );
        assert_eq!(
            Indent {
                width: 4,
                tabs: true
            }
            .unit(),
            "\t"
        );
    }
}
