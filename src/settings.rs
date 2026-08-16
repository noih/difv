use crate::config::Config;

/// One editable line of the settings panel. Adding a config key means adding it
/// here too, which is what keeps the panel and the file in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    DetectIndent,
    IndentWidth,
    UseTabs,
    DetectLineEnding,
    LineEnding,
    ScrollLines,
    RememberLayout,
    MomentumDelayMs,
}

pub const ALL: [Setting; 8] = [
    Setting::DetectIndent,
    Setting::IndentWidth,
    Setting::UseTabs,
    Setting::DetectLineEnding,
    Setting::LineEnding,
    Setting::ScrollLines,
    Setting::RememberLayout,
    Setting::MomentumDelayMs,
];

/// Wide enough for a comfortable step, narrow enough to stay readable.
const MAX_NUMBER: usize = 16;

/// A momentum guard is a coarse thing: 50ms is the smallest step worth having,
/// and a second is longer than any trackpad's tail.
const MS_STEP: isize = 50;
const MAX_MS: isize = 1000;

impl Setting {
    /// What the panel calls this setting. The `config.toml` key is in `help`
    /// instead: a panel of key names reads like a struct definition, but the key
    /// still has to be findable by anyone editing the file by hand.
    pub fn name(self) -> &'static str {
        match self {
            Setting::DetectIndent => "Detect indentation",
            Setting::IndentWidth => "Indent width",
            Setting::UseTabs => "Indent with tabs",
            Setting::DetectLineEnding => "Detect line endings",
            Setting::LineEnding => "Line endings",
            Setting::ScrollLines => "Wheel scroll step",
            Setting::RememberLayout => "Remember pane widths",
            Setting::MomentumDelayMs => "Ignore scrolling after a keystroke",
        }
    }

    /// The `config.toml` key.
    pub fn label(self) -> &'static str {
        match self {
            Setting::DetectIndent => "detect_indent",
            Setting::IndentWidth => "indent_width",
            Setting::UseTabs => "use_tabs",
            Setting::DetectLineEnding => "detect_line_ending",
            Setting::LineEnding => "line_ending",
            Setting::ScrollLines => "scroll_lines",
            Setting::RememberLayout => "remember_layout",
            Setting::MomentumDelayMs => "momentum_delay_ms",
        }
    }

    /// One line on what the setting does, shown for the selected row. Says what
    /// changes, not what the value is — the value is already on screen.
    pub fn help(self) -> &'static str {
        match self {
            Setting::DetectIndent => {
                "Read each file's own indentation. Off uses the width below everywhere."
            }
            Setting::IndentWidth => "Spaces per indent for files with none to detect. 1-16.",
            Setting::UseTabs => "Indent with a tab for files with none to detect.",
            Setting::DetectLineEnding => {
                "Keep each file's own line endings. Off writes the choice below."
            }
            Setting::LineEnding => "Line ending for files with none to detect.",
            Setting::ScrollLines => "Diff rows one wheel notch moves. 1-16.",
            Setting::RememberLayout => "Carry pane widths over to the next run, in layout.toml.",
            Setting::MomentumDelayMs => {
                "A trackpad keeps scrolling after your fingers leave it. Lowest is off."
            }
        }
    }

    /// The value as the panel shows it. `true`/`false` and a bare millisecond
    /// count are how the file stores it, not how it reads.
    pub fn value(self, config: &Config) -> String {
        let flag = |on: bool| if on { "on" } else { "off" }.to_string();
        match self {
            Setting::DetectIndent => flag(config.detect_indent),
            Setting::IndentWidth => plural(config.indent_width, "space"),
            Setting::UseTabs => flag(config.use_tabs),
            Setting::DetectLineEnding => flag(config.detect_line_ending),
            Setting::LineEnding => config.line_ending.to_uppercase(),
            Setting::ScrollLines => plural(config.scroll_lines, "row"),
            Setting::RememberLayout => flag(config.remember_layout),
            Setting::MomentumDelayMs => match config.momentum_delay_ms {
                0 => "off".to_string(),
                ms => format!("{ms} ms"),
            },
        }
    }

    /// A step of the value, in the direction the user pressed. Two-valued
    /// settings flip either way: there is nowhere else for them to go.
    pub fn adjust(self, config: &mut Config, step: isize) {
        match self {
            Setting::DetectIndent => config.detect_indent = !config.detect_indent,
            Setting::UseTabs => config.use_tabs = !config.use_tabs,
            Setting::DetectLineEnding => config.detect_line_ending = !config.detect_line_ending,
            Setting::RememberLayout => config.remember_layout = !config.remember_layout,
            Setting::LineEnding => {
                config.line_ending = if config.line_ending == "lf" {
                    "crlf"
                } else {
                    "lf"
                }
                .into();
            }
            Setting::IndentWidth => config.indent_width = step_number(config.indent_width, step),
            Setting::ScrollLines => config.scroll_lines = step_number(config.scroll_lines, step),
            // Milliseconds, so it steps in a unit anyone would actually pick.
            Setting::MomentumDelayMs => {
                let next = config.momentum_delay_ms as isize + step * MS_STEP;
                config.momentum_delay_ms = next.clamp(0, MAX_MS) as u64;
            }
        }
    }
}

fn plural(n: usize, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

fn step_number(value: usize, step: isize) -> usize {
    (value as isize + step).clamp(1, MAX_NUMBER as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_step_within_bounds_and_flags_flip() {
        let mut config = Config::default();
        Setting::ScrollLines.adjust(&mut config, -1);
        Setting::ScrollLines.adjust(&mut config, -1);
        Setting::ScrollLines.adjust(&mut config, -1);
        // Never past a value the config would reject on the way back in.
        assert_eq!(config.scroll_lines, 1);

        for _ in 0..40 {
            Setting::IndentWidth.adjust(&mut config, 1);
        }
        assert_eq!(config.indent_width, MAX_NUMBER);

        Setting::UseTabs.adjust(&mut config, -1);
        assert!(config.use_tabs);
        Setting::LineEnding.adjust(&mut config, 1);
        assert_eq!(config.line_ending, "crlf");
        Setting::LineEnding.adjust(&mut config, 1);
        assert_eq!(config.line_ending, "lf");
    }
}
