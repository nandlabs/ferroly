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
}

/// One command of the tree, flattened: a shell-safe identifier (the command
/// path joined with `__`), the command, and the flags it accepts (its own plus
/// the inherited global ones).
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
    }
}

fn collect<'a>(cmd: &'a Command, prefix: &str, inherited: &[&'a Arg], out: &mut Vec<Node<'a>>) {
    let id = if prefix.is_empty() {
        ident(&cmd.name)
    } else {
        format!("{prefix}__{}", ident(&cmd.name))
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

/// Reduces a name to characters valid in a shell function name.
fn ident(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
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
            let target = format!("{}__{}", node.id, ident(&sub.name));
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

/// Escapes a word for use inside a double-quoted bash string.
fn bash_dq(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if matches!(c, '"' | '$' | '`' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escapes a word for use as a bash `case` pattern.
fn bash_pattern(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | ':')) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn bash(root: &Command, nodes: &[Node<'_>]) -> String {
    let func = format!("_{}_completions", ident(&root.name));
    let mut out = String::new();
    out.push_str(&format!("# bash completion for {}\n", root.name));
    out.push_str(&format!("{func}() {{\n"));
    out.push_str("    local cur prev cmd i\n");
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

    out.push_str("    case \"${cmd}\" in\n");
    for node in nodes {
        out.push_str(&format!("        {})\n", node.id));

        // Completing the value of an option: its choices, or nothing (which
        // lets the shell fall back to file names).
        let valued: Vec<&Arg> = node
            .flags
            .iter()
            .copied()
            .filter(|a| a.takes_value())
            .collect();
        if !valued.is_empty() {
            out.push_str("            case \"${prev}\" in\n");
            for a in valued {
                let pattern: Vec<String> = spellings(a).iter().map(|s| bash_pattern(s)).collect();
                if pattern.is_empty() {
                    continue;
                }
                out.push_str(&format!("                {})\n", pattern.join("|")));
                if !a.choices.is_empty() {
                    let words: Vec<String> = a.choices.iter().map(|c| bash_dq(c)).collect();
                    out.push_str(&format!(
                        "                    COMPREPLY=($(compgen -W \"{}\" -- \"${{cur}}\"))\n",
                        words.join(" ")
                    ));
                }
                out.push_str("                    return 0\n");
                out.push_str("                    ;;\n");
            }
            out.push_str("            esac\n");
        }

        let mut words: Vec<String> = node.flags.iter().flat_map(|a| spellings(a)).collect();
        words.extend(builtins(node).into_iter().map(|(w, _)| w));
        for sub in &node.cmd.subcommands {
            words.push(sub.name.clone());
        }
        let words: Vec<String> = words.iter().map(|w| bash_dq(w)).collect();
        out.push_str(&format!(
            "            COMPREPLY=($(compgen -W \"{}\" -- \"${{cur}}\"))\n",
            words.join(" ")
        ));
        out.push_str("            ;;\n");
    }
    out.push_str("    esac\n");
    out.push_str("    return 0\n");
    out.push_str("}\n");
    out.push_str(&format!(
        "complete -F {func} -o bashdefault -o default {}\n",
        bash_pattern(&root.name)
    ));
    out
}

// ---- zsh -------------------------------------------------------------------

/// Quotes a string as one single-quoted zsh word.
fn zsh_sq(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A `_describe` item: `name:description`, with colons in the name escaped.
fn zsh_item(name: &str, about: &str) -> String {
    let name = name.replace('\\', "\\\\").replace(':', "\\:");
    let about = about.lines().next().unwrap_or_default();
    if about.is_empty() {
        zsh_sq(&name)
    } else {
        zsh_sq(&format!("{name}:{about}"))
    }
}

/// Escapes a word for use as a zsh `case` pattern.
fn zsh_pattern(word: &str) -> String {
    bash_pattern(word)
}

fn zsh(root: &Command, nodes: &[Node<'_>]) -> String {
    let func = format!("_{}", ident(&root.name));
    let mut out = String::new();
    out.push_str(&format!("#compdef {}\n\n", root.name));
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
            zsh_sq(&node.cmd.name)
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
