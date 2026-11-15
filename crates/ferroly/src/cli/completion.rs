//! Shell completion script generation for a [`Command`] tree.

use super::{Arg, Command};

/// A shell [`Command::completions`] can generate a script for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Shell {
    /// GNU Bash (`complete -F`).
    Bash,
    /// Zsh (`compdef`).
    Zsh,
    /// fish (`complete -c`, with a helper function tracking the subcommand).
    Fish,
    /// PowerShell 5.1 and 7+ (`Register-ArgumentCompleter -Native`).
    PowerShell,
}

/// One command of the tree, flattened: a shell-safe identifier (the command
/// path, each name spelled by [`node_ident`], joined with `__`), the command,
/// and the flags it accepts (its own plus the inherited global ones).
struct Node<'a> {
    id: String,
    cmd: &'a Command,
    flags: Vec<&'a Arg>,
}

pub(super) fn generate(root: &Command, shell: Shell) -> String {
    let mut nodes = Vec::new();
    collect(root, "", &[], &mut nodes);
    match shell {
        Shell::Bash => bash(root, &nodes),
        Shell::Zsh => zsh(root, &nodes),
        Shell::Fish => fish(root, &nodes),
        Shell::PowerShell => powershell(root, &nodes),
    }
}

fn collect<'a>(cmd: &'a Command, prefix: &str, inherited: &[&'a Arg], out: &mut Vec<Node<'a>>) {
    let id = if prefix.is_empty() {
        node_ident(&cmd.name)
    } else {
        child_id(prefix, &cmd.name)
    };
    let flags = cmd.effective(inherited);
    let own = cmd.flags.len();
    let globals: Vec<&Arg> = flags
        .iter()
        .enumerate()
        .filter(|(idx, a)| *idx >= own || a.global)
        .map(|(_, a)| *a)
        .collect();
    out.push(Node {
        id: id.clone(),
        cmd,
        flags,
    });
    for sub in &cmd.subcommands {
        collect(sub, &id, &globals, out);
    }
}

/// Reduces a name to characters valid in a shell function name. Lossy, so it
/// only names the one function a script defines and never tells commands
/// apart (that is [`node_ident`]).
fn ident(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Spells a command name as a shell identifier unique to it: ASCII letters
/// and digits as they are, every other byte as `_xx` (lowercase hex), so
/// `sub-cmd` and `sub_cmd` stay apart. The result never contains `__` (the
/// `_` of an escape is always followed by a hex digit) and never ends with
/// `_`, so joining levels with `__` cannot make two paths collide either.
fn node_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

/// The id of the subcommand `name` of the node `parent`.
fn child_id(parent: &str, name: &str) -> String {
    format!("{parent}__{}", node_ident(name))
}

/// Characters a script never carries raw: control characters (a newline, or
/// for PowerShell a carriage return, ends a comment or a statement) and the
/// Unicode line and paragraph separators.
fn is_unsafe(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// Text for a `#` comment: unsafe characters written as Rust escapes (`\n`,
/// `\u{1b}`), so the comment stays on its one line.
fn comment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_unsafe(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// An unsafe character as a bash/zsh ANSI-C quoted word: `$'\xHH'` with one
/// escape per UTF-8 byte (bash 3.2+ and zsh).
fn ansi_c(c: char) -> String {
    let mut out = String::from("$'");
    let mut buf = [0u8; 4];
    for b in c.encode_utf8(&mut buf).bytes() {
        out.push_str(&format!("\\x{b:02x}"));
    }
    out.push('\'');
    out
}

/// Every spelling of a flag: `--long`, aliases, `-s`, short aliases.
fn spellings(a: &Arg) -> Vec<String> {
    a.long
        .iter()
        .chain(a.aliases.iter())
        .map(|l| format!("--{l}"))
        .chain(
            a.short
                .iter()
                .chain(a.short_aliases.iter())
                .map(|c| format!("-{c}")),
        )
        .collect()
}

/// The built-in flags a command answers to, minus any the program claimed.
fn builtins(node: &Node<'_>) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let mut add = |short: char, long: &str, about: &'static str| {
        if !node.flags.iter().any(|a| a.matches_long(long)) {
            out.push((format!("--{long}"), about));
        }
        if !node.flags.iter().any(|a| a.matches_short(short)) {
            out.push((format!("-{short}"), about));
        }
    };
    add('h', "help", "Print help");
    if node.cmd.version.is_some() {
        add('V', "version", "Print version");
    }
    out
}

/// The transitions `parent,word) cmd=child ;;` shared by both shells.
fn transitions(nodes: &[Node<'_>], quote: fn(&str) -> String) -> String {
    let mut out = String::new();
    for node in nodes {
        for sub in &node.cmd.subcommands {
            let target = child_id(&node.id, &sub.name);
            for word in std::iter::once(&sub.name).chain(sub.aliases.iter()) {
                out.push_str(&format!(
                    "            {},{}) cmd=\"{target}\" ;;\n",
                    node.id,
                    quote(word)
                ));
            }
        }
    }
    out
}

// ---- bash ------------------------------------------------------------------

/// Quotes a string as one bash/zsh word: single-quoted runs, with unsafe
/// characters spliced in as [`ansi_c`] words.
fn sh_sq(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    let mut open = false;
    for c in text.chars() {
        if is_unsafe(c) {
            if open {
                out.push('\'');
                open = false;
            }
            out.push_str(&ansi_c(c));
            continue;
        }
        if !open {
            out.push('\'');
            open = true;
        }
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push_str("''");
    } else if open {
        out.push('\'');
    }
    out
}

/// Escapes a word for use as a bash `case` pattern (or a plain bash word).
fn bash_pattern(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if is_unsafe(c) {
            // A backslash before a newline would be a line continuation.
            out.push_str(&ansi_c(c));
            continue;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | ':')) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `opts=(…)`: a bash array of candidate words, each quoted by [`sh_sq`].
fn bash_opts<S: AsRef<str>>(words: &[S]) -> String {
    let quoted: Vec<String> = words.iter().map(|w| sh_sq(w.as_ref())).collect();
    format!("opts=({})", quoted.join(" "))
}

fn bash(root: &Command, nodes: &[Node<'_>]) -> String {
    let func = format!("_{}_completions", ident(&root.name));
    let mut out = String::new();
    out.push_str(&format!("# bash completion for {}\n", comment(&root.name)));
    out.push_str(&format!("{func}() {{\n"));
    out.push_str("    local cur prev cmd i w\n");
    out.push_str("    local -a opts\n");
    out.push_str("    COMPREPLY=()\n");
    out.push_str("    cur=\"${COMP_WORDS[COMP_CWORD]}\"\n");
    out.push_str("    prev=\"${COMP_WORDS[COMP_CWORD-1]}\"\n");
    out.push_str(&format!("    cmd=\"{}\"\n", nodes[0].id));

    let moves = transitions(nodes, bash_pattern);
    if !moves.is_empty() {
        out.push_str("    for ((i = 1; i < COMP_CWORD; i++)); do\n");
        out.push_str("        case \"${cmd},${COMP_WORDS[i]}\" in\n");
        out.push_str(&moves);
        out.push_str("        esac\n");
        out.push_str("    done\n");
    }

    // Each branch fills `opts`, and a loop keeps the words starting with the
    // current one. `compgen -W` is not used: it runs its word list through
    // expansion, so a `$(…)` choice would execute and a `*` would glob.
    out.push_str("    case \"${cmd}\" in\n");
    for node in nodes {
        out.push_str(&format!("        {})\n", node.id));

        let mut words: Vec<String> = node.flags.iter().flat_map(|a| spellings(a)).collect();
        words.extend(builtins(node).into_iter().map(|(w, _)| w));
        for sub in &node.cmd.subcommands {
            words.push(sub.name.clone());
        }

        // Completing the value of an option: its choices, or nothing (which
        // lets the shell fall back to file names).
        let valued: Vec<&Arg> = node
            .flags
            .iter()
            .copied()
            .filter(|a| a.takes_value())
            .collect();
        if valued.is_empty() {
            out.push_str(&format!("            {}\n", bash_opts(&words)));
        } else {
            out.push_str("            case \"${prev}\" in\n");
            for a in valued {
                let pattern: Vec<String> = spellings(a).iter().map(|s| bash_pattern(s)).collect();
                if pattern.is_empty() {
                    continue;
                }
                out.push_str(&format!("                {})\n", pattern.join("|")));
                if a.choices.is_empty() {
                    out.push_str("                    return 0\n");
                } else {
                    out.push_str(&format!("                    {}\n", bash_opts(&a.choices)));
                }
                out.push_str("                    ;;\n");
            }
            out.push_str("                *)\n");
            out.push_str(&format!("                    {}\n", bash_opts(&words)));
            out.push_str("                    ;;\n");
            out.push_str("            esac\n");
        }
        out.push_str("            ;;\n");
    }
    out.push_str("    esac\n");
    out.push_str("    for w in \"${opts[@]}\"; do\n");
    out.push_str("        if [[ \"${w}\" == \"${cur}\"* ]]; then\n");
    out.push_str("            COMPREPLY+=(\"${w}\")\n");
    out.push_str("        fi\n");
    out.push_str("    done\n");
    out.push_str("    return 0\n");
    out.push_str("}\n");
    out.push_str(&format!(
        "complete -F {func} -o bashdefault -o default {}\n",
        bash_pattern(&root.name)
    ));
    out
}

// ---- zsh -------------------------------------------------------------------

/// A `_describe` item: `name:description`, with colons in the name escaped.
fn zsh_item(name: &str, about: &str) -> String {
    let name = name.replace('\\', "\\\\").replace(':', "\\:");
    let about = about.lines().next().unwrap_or_default();
    if about.is_empty() {
        sh_sq(&name)
    } else {
        sh_sq(&format!("{name}:{about}"))
    }
}

/// Escapes a word for use as a zsh `case` pattern.
fn zsh_pattern(word: &str) -> String {
    bash_pattern(word)
}

fn zsh(root: &Command, nodes: &[Node<'_>]) -> String {
    let func = format!("_{}", ident(&root.name));
    let mut out = String::new();
    out.push_str(&format!("#compdef {}\n\n", comment(&root.name)));
    out.push_str(&format!("{func}() {{\n"));
    out.push_str(&format!("    local cmd=\"{}\" i\n", nodes[0].id));
    out.push_str("    local -a opts\n");

    let moves = transitions(nodes, zsh_pattern);
    if !moves.is_empty() {
        out.push_str("    for ((i = 2; i < CURRENT; i++)); do\n");
        out.push_str("        case \"${cmd},${words[i]}\" in\n");
        out.push_str(&moves);
        out.push_str("        esac\n");
        out.push_str("    done\n");
    }

    out.push_str("    case \"${cmd}\" in\n");
    for node in nodes {
        out.push_str(&format!("        {})\n", node.id));

        let valued: Vec<&Arg> = node
            .flags
            .iter()
            .copied()
            .filter(|a| a.takes_value())
            .collect();
        if !valued.is_empty() {
            out.push_str("            case \"${words[CURRENT-1]}\" in\n");
            for a in valued {
                let pattern: Vec<String> = spellings(a).iter().map(|s| zsh_pattern(s)).collect();
                if pattern.is_empty() {
                    continue;
                }
                out.push_str(&format!("                {})\n", pattern.join("|")));
                if a.choices.is_empty() {
                    out.push_str("                    _files\n");
                } else {
                    let items: Vec<String> = a.choices.iter().map(|c| zsh_item(c, "")).collect();
                    out.push_str(&format!("                    opts=({})\n", items.join(" ")));
                    out.push_str("                    _describe -t values 'value' opts\n");
                }
                out.push_str("                    return\n");
                out.push_str("                    ;;\n");
            }
            out.push_str("            esac\n");
        }

        out.push_str("            opts=(\n");
        for a in &node.flags {
            let about = a.help.as_deref().unwrap_or_default();
            for s in spellings(a) {
                out.push_str(&format!("                {}\n", zsh_item(&s, about)));
            }
        }
        for (word, about) in builtins(node) {
            out.push_str(&format!("                {}\n", zsh_item(&word, about)));
        }
        for sub in &node.cmd.subcommands {
            let about = sub.about.as_deref().unwrap_or_default();
            out.push_str(&format!("                {}\n", zsh_item(&sub.name, about)));
        }
        out.push_str("            )\n");
        out.push_str(&format!(
            "            _describe -t arguments {} opts\n",
            sh_sq(&node.cmd.name)
        ));
        if !node.cmd.positionals.is_empty() {
            out.push_str("            _files\n");
        }
        out.push_str("            ;;\n");
    }
    out.push_str("    esac\n");
    out.push_str("}\n\n");
    out.push_str(&format!(
        "if [ \"${{funcstack[1]}}\" = \"{func}\" ]; then\n    {func} \"$@\"\nelse\n    compdef {func} {}\nfi\n",
        zsh_pattern(&root.name)
    ));
    out
}

// ---- fish ------------------------------------------------------------------

/// Quotes a string as one fish word: single-quoted, with unsafe characters
/// spliced in unquoted as `\uXXXX` escapes (fish joins the adjacent pieces
/// into one word).
fn fish_sq(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if is_unsafe(c) {
            out.push_str(&format!("'\\u{:04x}'", u32::from(c)));
            continue;
        }
        if matches!(c, '\\' | '\'') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// The first line of a help text, or nothing.
fn first_line(text: Option<&str>) -> &str {
    text.and_then(|t| t.lines().next()).unwrap_or_default()
}

fn fish(root: &Command, nodes: &[Node<'_>]) -> String {
    let prog = fish_sq(&root.name);
    let func = format!("__{}_complete_cmd", ident(&root.name));
    let mut out = String::new();
    out.push_str(&format!("# fish completion for {}\n", comment(&root.name)));
    // The command path so far: walk the words before the cursor through the
    // subcommand transitions, the way the bash script does.
    out.push_str(&format!("function {func}\n"));
    out.push_str("    set -l words (commandline -opc)\n");
    out.push_str("    set -e words[1]\n");
    out.push_str(&format!("    set -l cmd {}\n", nodes[0].id));
    out.push_str("    for w in $words\n");
    let mut first = true;
    for node in nodes {
        for sub in &node.cmd.subcommands {
            let target = child_id(&node.id, &sub.name);
            for word in std::iter::once(&sub.name).chain(sub.aliases.iter()) {
                let keyword = if first { "if" } else { "else if" };
                first = false;
                out.push_str(&format!(
                    "        {keyword} test \"$cmd,$w\" = {}\n            set cmd {target}\n",
                    fish_sq(&format!("{},{}", node.id, word))
                ));
            }
        }
    }
    if !first {
        out.push_str("        end\n");
    }
    out.push_str("    end\n");
    out.push_str("    echo $cmd\n");
    out.push_str("end\n\n");

    for node in nodes {
        let cond = format!("-n 'test ({func}) = {}'", node.id);
        let line = |rest: &str| format!("complete -c {prog} {cond} {rest}\n");
        // Without positionals there is nothing file-like to complete.
        if node.cmd.positionals.is_empty() {
            out.push_str(&line("-f"));
        }
        for a in &node.flags {
            let mut rest = String::new();
            for l in a.long.iter().chain(a.aliases.iter()) {
                rest.push_str(&format!("-l {} ", fish_sq(l)));
            }
            for c in a.short.iter().chain(a.short_aliases.iter()) {
                rest.push_str(&format!("-s {} ", fish_sq(&c.to_string())));
            }
            if rest.is_empty() {
                continue;
            }
            if a.takes_value() {
                if a.choices.is_empty() {
                    rest.push_str("-r -F ");
                } else {
                    let words: Vec<String> = a.choices.iter().map(|c| fish_sq(c)).collect();
                    rest.push_str(&format!("-x -a {} ", fish_sq(&words.join(" "))));
                }
            }
            let about = first_line(a.help.as_deref());
            if !about.is_empty() {
                rest.push_str(&format!("-d {}", fish_sq(about)));
            }
            out.push_str(&line(rest.trim_end()));
        }
        for (word, about) in builtins(node) {
            let spelling = match word.strip_prefix("--") {
                Some(long) => format!("-l {}", fish_sq(long)),
                None => format!("-s {}", fish_sq(word.trim_start_matches('-'))),
            };
            out.push_str(&line(&format!("{spelling} -d {}", fish_sq(about))));
        }
        for sub in &node.cmd.subcommands {
            let mut rest = format!("-f -a {}", fish_sq(&fish_sq(&sub.name)));
            let about = first_line(sub.about.as_deref());
            if !about.is_empty() {
                rest.push_str(&format!(" -d {}", fish_sq(about)));
            }
            out.push_str(&line(&rest));
        }
    }
    out
}

// ---- PowerShell ------------------------------------------------------------

/// Quotes a string as one PowerShell string literal: single-quoted, with the
/// typographic single quotes (which PowerShell also takes for quote
/// characters) doubled too. Text with unsafe characters becomes a
/// double-quoted string instead ([`ps_dq`]).
fn ps_sq(text: &str) -> String {
    if text.chars().any(is_unsafe) {
        return ps_dq(text);
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// A double-quoted PowerShell string: the characters special in an
/// expandable string (`` ` ``, `$` and the double-quote characters, typographic
/// ones included) escaped with a backtick, and unsafe characters written as
/// `$([char]0xHHHH)` (PowerShell 5.1 has no `` `u{…} `` escape).
fn ps_dq(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if is_unsafe(c) {
            out.push_str(&format!("$([char]0x{:04x})", u32::from(c)));
            continue;
        }
        if matches!(c, '`' | '$' | '"' | '\u{201C}' | '\u{201D}' | '\u{201E}') {
            out.push('`');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn powershell(root: &Command, nodes: &[Node<'_>]) -> String {
    const RESULT: &str = "[System.Management.Automation.CompletionResult]::new";
    const ORDINAL: &str = "[System.StringComparison]::Ordinal";
    let mut out = String::new();
    out.push_str(&format!(
        "# PowerShell completion for {}\n",
        comment(&root.name)
    ));
    out.push_str(&format!(
        "Register-ArgumentCompleter -Native -CommandName {}, {} -ScriptBlock {{\n",
        ps_sq(&root.name),
        ps_sq(&format!("{}.exe", root.name))
    ));
    out.push_str("    param($wordToComplete, $commandAst, $cursorPosition)\n");
    out.push_str(&format!("    $cmd = {}\n", ps_sq(&nodes[0].id)));
    out.push_str("    $prev = ''\n");
    // Walk the words before the one being completed.
    out.push_str(
        "    foreach ($element in @($commandAst.CommandElements | Select-Object -Skip 1)) {\n",
    );
    out.push_str("        if ($element.Extent.EndOffset -ge $cursorPosition) { break }\n");
    out.push_str(
        "        if ($element -is [System.Management.Automation.Language.StringConstantExpressionAst]) {\n",
    );
    out.push_str("            $word = $element.Value\n");
    out.push_str("        } else {\n");
    out.push_str("            $word = $element.Extent.Text\n");
    out.push_str("        }\n");
    let mut moves = String::new();
    for node in nodes {
        for sub in &node.cmd.subcommands {
            let target = child_id(&node.id, &sub.name);
            for word in std::iter::once(&sub.name).chain(sub.aliases.iter()) {
                moves.push_str(&format!(
                    "            {} {{ $cmd = {} }}\n",
                    ps_sq(&format!("{},{}", node.id, word)),
                    ps_sq(&target)
                ));
            }
        }
    }
    if !moves.is_empty() {
        out.push_str("        switch -Exact -CaseSensitive (\"$cmd,$word\") {\n");
        out.push_str(&moves);
        out.push_str("        }\n");
    }
    out.push_str("        $prev = $word\n");
    out.push_str("    }\n");
    out.push_str("    $values = $null\n");
    out.push_str("    $words = @()\n");
    out.push_str("    switch -Exact -CaseSensitive ($cmd) {\n");
    for node in nodes {
        out.push_str(&format!("        {} {{\n", ps_sq(&node.id)));
        for a in node.flags.iter().filter(|a| a.takes_value()) {
            let spelled: Vec<String> = spellings(a).iter().map(|s| ps_sq(s)).collect();
            if spelled.is_empty() {
                continue;
            }
            // An option's value: its choices, or nothing, so PowerShell falls
            // back to path completion.
            let then = if a.choices.is_empty() {
                "return".to_string()
            } else {
                let words: Vec<String> = a.choices.iter().map(|c| ps_sq(c)).collect();
                format!("$values = @({}); break", words.join(", "))
            };
            out.push_str(&format!(
                "            if (@({}) -ccontains $prev) {{ {then} }}\n",
                spelled.join(", ")
            ));
        }
        let mut item = |text: &str, kind: &str, about: &str| {
            let about = if about.is_empty() { text } else { about };
            out.push_str(&format!(
                "            $words += ,@({}, '{kind}', {})\n",
                ps_sq(text),
                ps_sq(about)
            ));
        };
        for a in &node.flags {
            let about = first_line(a.help.as_deref());
            for s in spellings(a) {
                item(&s, "ParameterName", about);
            }
        }
        for (word, about) in builtins(node) {
            item(&word, "ParameterName", about);
        }
        for sub in &node.cmd.subcommands {
            item(
                &sub.name,
                "ParameterValue",
                first_line(sub.about.as_deref()),
            );
        }
        out.push_str("        }\n");
    }
    out.push_str("    }\n");
    out.push_str("    if ($null -ne $values) {\n");
    out.push_str("        foreach ($value in $values) {\n");
    out.push_str(&format!(
        "            if ($value.StartsWith($wordToComplete, {ORDINAL})) {{\n"
    ));
    out.push_str(&format!(
        "                {RESULT}($value, $value, 'ParameterValue', $value)\n"
    ));
    out.push_str("            }\n");
    out.push_str("        }\n");
    out.push_str("        return\n");
    out.push_str("    }\n");
    out.push_str("    foreach ($w in $words) {\n");
    out.push_str(&format!(
        "        if ($w[0].StartsWith($wordToComplete, {ORDINAL})) {{\n"
    ));
    out.push_str(&format!(
        "            {RESULT}($w[0], $w[0], $w[1], $w[2])\n"
    ));
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_and_powershell_quoting_escapes_quotes() {
        assert_eq!(fish_sq(r"it's a \ test"), r"'it\'s a \\ test'");
        assert_eq!(ps_sq("it's"), "'it''s'");
        assert_eq!(ps_sq("it\u{2019}s"), "'it\u{2019}\u{2019}s'");
    }

    #[test]
    fn unsafe_characters_are_escaped_in_every_quoting() {
        assert_eq!(sh_sq("it's"), r"'it'\''s'");
        assert_eq!(sh_sq(""), "''");
        assert_eq!(sh_sq("a\nb"), r"'a'$'\x0a''b'");
        assert_eq!(sh_sq("\u{85}"), r"$'\xc2\x85'");
        assert_eq!(bash_pattern("a\nb c"), r"a$'\x0a'b\ c");
        assert_eq!(fish_sq("a\nb"), r"'a'\u000a'b'");
        assert_eq!(ps_sq("a\r$b`\""), "\"a$([char]0x000d)`$b```\"\"");
        assert_eq!(comment("a\n\u{1b}b"), r"a\n\u{1b}b");
    }

    /// The tree used by the injection tests: control characters in the
    /// program name, a subcommand, a flag, a choice and help texts.
    fn hostile() -> Command {
        Command::new("app\ntouch /tmp/pwned #\r\u{1b}[31m\u{2028}x")
            .version("1")
            .flag(
                Arg::new("f")
                    .long("fo\no")
                    .help("line\rnext")
                    .choices(["a\nb", "c"]),
            )
            .subcommand(Command::new("s\nx").about("run\u{0}it"))
    }

    // CORE-16: no generated script carries a raw control character, so no
    // name can end a comment (or a quoted word) early.
    #[test]
    fn control_characters_in_names_cannot_inject() {
        let scripts = [
            (Shell::Bash, &["#", "_app_", " ", "}", "complete "][..]),
            (
                Shell::Zsh,
                &["#compdef ", "_app_", " ", "}", "if ", "else", "fi"][..],
            ),
            (
                Shell::Fish,
                &["#", "function ", " ", "end", "complete "][..],
            ),
            (
                Shell::PowerShell,
                &["#", "Register-ArgumentCompleter ", " ", "}"][..],
            ),
        ];
        for (shell, starts) in scripts {
            let script = hostile().completions(shell);
            let bad: Vec<char> = script
                .chars()
                .filter(|&c| c != '\n' && is_unsafe(c))
                .collect();
            assert!(bad.is_empty(), "{shell:?}: raw {bad:?} in\n{script}");
            for line in script.lines().filter(|l| !l.is_empty()) {
                assert!(
                    starts.iter().any(|s| line.starts_with(s)),
                    "{shell:?}: stray line {line:?} in\n{script}"
                );
            }
            assert!(
                script.contains(r"app\ntouch /tmp/pwned #\r\u{1b}[31m\u{2028}x"),
                "{shell:?}: header not escaped in\n{script}"
            );
        }
    }

    // CORE-16 (bash): candidate words are never expanded, so a choice spelled
    // like a command substitution or a glob completes as itself.
    #[cfg(unix)]
    #[test]
    fn bash_candidates_are_not_expanded() {
        let cmd = Command::new("app").flag(Arg::new("format").long("format").choices([
            "$(echo INJECTED >&2)",
            "*",
            "a b",
            "json",
            "it's",
        ]));
        let run = |words: &str, cword: usize| {
            let driver = format!(
                "{}\nCOMP_WORDS=({words}); COMP_CWORD={cword}\n_app_completions\nprintf '<%s>\\n' \"${{COMPREPLY[@]}}\"\n",
                cmd.completions(Shell::Bash)
            );
            std::process::Command::new("bash")
                .arg("--norc")
                .arg("-c")
                .arg(driver)
                .output()
                .ok()
        };
        let Some(out) = run("app --format ''", 2) else {
            return; // no bash here
        };
        assert_eq!(String::from_utf8_lossy(&out.stderr), "");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "<$(echo INJECTED >&2)>\n<*>\n<a b>\n<json>\n<it's>\n"
        );
        let out = run("app --format j", 2).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "<json>\n");
        let out = run("app --f", 1).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "<--format>\n");
    }

    /// The part of `script` that completes the node `id`.
    fn block(shell: Shell, script: &str, id: &str) -> String {
        match shell {
            Shell::Bash | Shell::Zsh => {
                let start = format!("\n        {id})\n");
                let rest = &script[script.find(&start).expect(id) + start.len()..];
                rest[..rest.find("\n            ;;\n").unwrap()].to_string()
            }
            Shell::Fish => {
                let cond = format!("= {id}' ");
                script
                    .lines()
                    .filter(|l| l.starts_with("complete ") && l.contains(&cond))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Shell::PowerShell => {
                let start = format!("\n        '{id}' {{\n");
                let rest = &script[script.find(&start).expect(id) + start.len()..];
                rest[..rest.find("\n        }\n").unwrap()].to_string()
            }
        }
    }

    // CORE-17: commands whose names differ only in punctuation, at one level
    // or across levels, get distinct ids and keep their own flags.
    #[test]
    fn similar_names_get_distinct_ids() {
        let leaf = |name: &str, flag: &str| {
            Command::new(name.to_string()).flag(Arg::new(flag).long(flag.to_string()))
        };
        let root = Command::new("app")
            .subcommand(leaf("sub-cmd", "dash-only"))
            .subcommand(leaf("sub_cmd", "underscore-only"))
            .subcommand(leaf("a.b", "dot-only"))
            .subcommand(Command::new("a").subcommand(leaf("b", "nested-only")))
            .subcommand(leaf("a__b", "flat-only"));
        let mut nodes = Vec::new();
        collect(&root, "", &[], &mut nodes);
        let mut ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), nodes.len(), "{ids:?}");

        let own = [
            ("sub-cmd", "dash-only"),
            ("sub_cmd", "underscore-only"),
            ("a.b", "dot-only"),
            ("b", "nested-only"),
            ("a__b", "flat-only"),
        ];
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let script = root.completions(shell);
            for (name, flag) in own {
                let node = nodes.iter().find(|n| n.cmd.name == name).unwrap();
                let text = block(shell, &script, &node.id);
                for (_, other) in own {
                    assert_eq!(
                        text.contains(&format!("{other}'")),
                        other == flag,
                        "{shell:?} {name} / {other}:\n{text}"
                    );
                }
            }
        }
    }
}
