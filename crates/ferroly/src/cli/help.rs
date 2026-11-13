//! Help-text rendering for [`Command`].

use super::{Arg, Command};

/// Total line width descriptions are wrapped to.
const WIDTH: usize = 80;
/// Widest left column that still shares a line with its description.
const MAX_LEFT: usize = 28;

impl Command {
    /// Renders the help text. `path` names the parent commands (outermost
    /// first) and `inherited` holds their global flags.
    pub(super) fn render_help(&self, path: &[&str], inherited: &[&Arg]) -> String {
        let mut out = String::new();
        let mut full: Vec<&str> = path.to_vec();
        full.push(self.name.as_str());
        let full = full.join(" ");

        match &self.version {
            Some(v) => out.push_str(&format!("{full} {v}\n")),
            None => out.push_str(&format!("{full}\n")),
        }
        if let Some(about) = &self.about {
            for line in wrap(about, WIDTH) {
                out.push_str(&line);
                out.push('\n');
            }
        }

        out.push_str("\nUsage: ");
        out.push_str(&full);
        out.push_str(" [OPTIONS]");
        for a in self.flags.iter().filter(|a| a.required) {
            out.push(' ');
            out.push_str(&a.spelled());
            if a.takes_value() {
                out.push_str(&format!(" <{}>", a.name.to_uppercase()));
            }
        }
        for p in &self.positionals {
            out.push(' ');
            out.push_str(&positional_label(p));
        }
        if !self.subcommands.is_empty() {
            out.push_str(if self.subcommand_required {
                " <COMMAND>"
            } else {
                " [COMMAND]"
            });
        }
        out.push('\n');

        if !self.positionals.is_empty() {
            let rows: Vec<(String, String)> = self
                .positionals
                .iter()
                .map(|p| (positional_label(p), describe(p)))
                .collect();
            section(&mut out, "Arguments", &rows);
        }

        let mut rows: Vec<(String, String)> = self
            .flags
            .iter()
            .map(|a| (flag_label(a), describe(a)))
            .collect();
        let globals: Vec<&Arg> = inherited
            .iter()
            .copied()
            .filter(|a| !self.defines(&a.name))
            .collect();
        let claimed_short = |c: char| {
            self.flags
                .iter()
                .chain(globals.iter().copied())
                .any(|a| a.matches_short(c))
        };
        let claimed_long = |l: &str| {
            self.flags
                .iter()
                .chain(globals.iter().copied())
                .any(|a| a.matches_long(l))
        };
        if let Some(label) = builtin_label('h', "help", &claimed_short, &claimed_long) {
            rows.push((label, "Print help".to_string()));
        }
        if self.version.is_some() {
            if let Some(label) = builtin_label('V', "version", &claimed_short, &claimed_long) {
                rows.push((label, "Print version".to_string()));
            }
        }
        section(&mut out, "Options", &rows);

        if !globals.is_empty() {
            let rows: Vec<(String, String)> = globals
                .iter()
                .map(|a| (flag_label(a), describe(a)))
                .collect();
            section(&mut out, "Global options", &rows);
        }

        if !self.subcommands.is_empty() {
            let rows: Vec<(String, String)> = self
                .subcommands
                .iter()
                .map(|s| {
                    let mut desc = s.about.clone().unwrap_or_default();
                    if !s.aliases.is_empty() {
                        annotate(&mut desc, &format!("[aliases: {}]", s.aliases.join(", ")));
                    }
                    (s.name.clone(), desc)
                })
                .collect();
            section(&mut out, "Commands", &rows);
        }

        out
    }
}

/// `-h, --help`, or whichever half the program has not claimed for itself.
fn builtin_label(
    short: char,
    long: &str,
    claimed_short: &dyn Fn(char) -> bool,
    claimed_long: &dyn Fn(&str) -> bool,
) -> Option<String> {
    match (claimed_short(short), claimed_long(long)) {
        (false, false) => Some(format!("-{short}, --{long}")),
        (true, false) => Some(format!("    --{long}")),
        (false, true) => Some(format!("-{short}")),
        (true, true) => None,
    }
}

fn positional_label(p: &Arg) -> String {
    let dots = if p.variadic { "..." } else { "" };
    if p.required {
        format!("<{}>{dots}", p.name)
    } else {
        format!("[{}]{dots}", p.name)
    }
}

fn flag_label(a: &Arg) -> String {
    let mut col = String::new();
    match a.short {
        Some(c) => col.push_str(&format!("-{c}, ")),
        None => col.push_str("    "),
    }
    match &a.long {
        Some(l) => col.push_str(&format!("--{l}")),
        None if a.short.is_some() => {
            // Drop the separator that expected a long form to follow.
            col.truncate(col.len() - 2);
        }
        None => col.push_str(&a.name),
    }
    if a.takes_value() {
        col.push_str(&format!(" <{}>", a.name.to_uppercase()));
    }
    if a.multiple || a.count {
        col.push_str("...");
    }
    col
}

fn annotate(desc: &mut String, note: &str) {
    if !desc.is_empty() {
        desc.push(' ');
    }
    desc.push_str(note);
}

/// The description column: help text plus default/env/choices/alias notes.
fn describe(a: &Arg) -> String {
    let mut desc = a.help.clone().unwrap_or_default();
    if let Some(d) = &a.default {
        annotate(&mut desc, &format!("[default: {d}]"));
    }
    if let Some(e) = &a.env {
        annotate(&mut desc, &format!("[env: {e}]"));
    }
    if !a.choices.is_empty() {
        annotate(
            &mut desc,
            &format!("[possible values: {}]", a.choices.join(", ")),
        );
    }
    let aliases: Vec<String> = a
        .aliases
        .iter()
        .map(|l| format!("--{l}"))
        .chain(a.short_aliases.iter().map(|c| format!("-{c}")))
        .collect();
    if !aliases.is_empty() {
        annotate(&mut desc, &format!("[aliases: {}]", aliases.join(", ")));
    }
    desc
}

/// Writes a titled two-column block, wrapping the right column.
fn section(out: &mut String, title: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    out.push_str(&format!("\n{title}:\n"));
    let widest = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .filter(|w| *w <= MAX_LEFT)
        .max()
        .unwrap_or(0);
    // Two spaces of indent, the label column, two spaces of gutter.
    let indent = 2 + widest + 2;
    let pad = " ".repeat(indent);
    for (label, desc) in rows {
        let lines = wrap(desc, WIDTH.saturating_sub(indent));
        let width = label.chars().count();
        if lines.is_empty() {
            out.push_str(&format!("  {label}\n"));
            continue;
        }
        let mut lines = lines.into_iter();
        if width <= widest {
            let first = lines.next().unwrap_or_default();
            out.push_str(&format!(
                "  {label}{}  {first}\n",
                " ".repeat(widest - width)
            ));
        } else {
            // The label is too wide for the column: description starts below.
            out.push_str(&format!("  {label}\n"));
        }
        for line in lines {
            out.push_str(&pad);
            out.push_str(&line);
            out.push('\n');
        }
    }
}

/// Greedy word wrap to `width` columns; a single over-long word keeps its own
/// line rather than being split. Explicit newlines are preserved.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        let mut len = 0;
        for word in para.split_whitespace() {
            let w = word.chars().count();
            if len > 0 && len + 1 + w > width {
                lines.push(std::mem::take(&mut line));
                len = 0;
            }
            if len > 0 {
                line.push(' ');
                len += 1;
            }
            line.push_str(word);
            len += w;
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_word_boundaries() {
        let lines = wrap("alpha beta gamma delta epsilon zeta eta theta", 22);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= 22));
        assert_eq!(
            lines.join(" "),
            "alpha beta gamma delta epsilon zeta eta theta"
        );
    }

    #[test]
    fn wrap_of_empty_text_is_empty() {
        assert!(wrap("", 40).is_empty());
    }
}
