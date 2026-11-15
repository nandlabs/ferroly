# `ferroly::template` — a sandboxed Jinja2-subset template engine

**Feature:** `template` · **Module:** `ferroly::template` · **Pulls in:** `codec`

A defined subset of [Jinja2](https://jinja.palletsprojects.com/), the language
LLM chat templates are written in (and many configuration, report and email
templates). A model's chat format ships as a Jinja2 program: GGUF files keep it
in the `tokenizer.chat_template` metadata key and Hugging Face repos in
`tokenizer_config.json` → `chat_template`. A server that accepts OpenAI-style
`messages` runs that program to build the prompt the model was trained on.

Templates usually come from files the host does not control (a downloaded
model), so the engine is **sandboxed and bounded**: no I/O, typed errors with
source positions, limits on every resource, and no panic on any input.

This is a separate module from the Handlebars-style
[`genai::template`](genai.md#prompt-templates) prompt engine, which has a
different syntax (`{{#if}}`, `{{#each}}`, partials). Use this one to run
templates written for Jinja2.

## Quick start

```rust
use ferroly::codec::json;
use ferroly::template::{Options, Template, TemplateErrorKind};

// ChatML, as SmolLM2 ships it (abridged).
let source = "{% for message in messages %}{{ '<|im_start|>' + message['role'] + '\n' \
              + message['content'] + '<|im_end|>' + '\n' }}{% endfor %}\
              {% if add_generation_prompt %}{{ '<|im_start|>assistant\n' }}{% endif %}";

// Parse once: syntax errors and unknown filters, tests or functions fail here.
let template = Template::parse(source, &Options::default())?;

// Render many times, from many threads: `Template` is `Send + Sync`.
let context = json::from_str(
    r#"{"messages":[{"role":"user","content":"Hi"}],"add_generation_prompt":true}"#,
)?;
assert_eq!(
    template.render(&context)?,
    "<|im_start|>user\nHi<|im_end|>\n<|im_start|>assistant\n",
);

// A template can refuse its input:
let llama2_like = Template::parse(
    "{% if messages|length > 1 %}{{ raise_exception('one message only') }}{% endif %}ok",
    &Options::default(),
)?;
let err = llama2_like.render(&json::from_str(r#"{"messages":[{},{}]}"#)?).unwrap_err();
assert!(matches!(err.kind(), TemplateErrorKind::Raised(m) if m == "one message only"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

## API

| Item | Kind | Purpose |
|---|---|---|
| `Template::parse(source, &Options)` | fn | Parses and checks the whole template once. |
| `Template::render(&Value)` | method | Renders against a `codec::Value` map (or `Value::Null` for an empty context). The context is only read. |
| `Template::source()` | method | The source text. |
| `Options` | struct | `trim_blocks` (on), `lstrip_blocks` (on), `keep_trailing_newline` (off), `raise_exception` (on), `function(name, f)`, `limits(Limits)`. |
| `Limits` | struct | Public fields with `Default`: `max_source_bytes`, `max_depth`, `max_iterations`, `max_calls`, `max_output_bytes`, `max_alloc_bytes`, `max_steps`. |
| `TemplateError` | struct | `kind()`, `offset()` (byte offset), `line()`, `column()` (1-based; columns in characters), `raised_message()`. Converts into `ferroly::Error`. |
| `TemplateErrorKind` | enum | See [Errors](#errors). |

A `Template` is immutable and cheap to clone (an `Arc` inside); render it
concurrently from as many threads as you like.

### Host functions

A template sees nothing of the host unless the host registers a function.
Functions get the positional arguments as `codec::Value`s and return a value or
an error message:

```rust
use ferroly::codec::Value;
use ferroly::template::{Options, Template};

// Llama 3.1+ templates call strftime_now(format); the clock is the host's call.
let options = Options::default().function("strftime_now", |args: &[Value]| -> Result<Value, String> {
    let format = args.first().and_then(Value::as_str).ok_or("strftime_now needs a format")?;
    Ok(Value::from(format!("<today as {format}>")))
});
let t = Template::parse("Today: {{ strftime_now('%d %b %Y') }}", &options)?;
assert_eq!(t.render(&Value::Null)?, "Today: <today as %d %b %Y>");
# Ok::<(), ferroly::template::TemplateError>(())
```

Keyword arguments to a host function are a `Type` error. An `Err(message)`
becomes `TemplateErrorKind::Function { name, message }`. A host function
shadows a built-in global with the same name. `raise_exception(message)` is
built in and on by default, because chat templates in the wild assume it;
`Options::raise_exception(false)` removes it.

## The language

### Statements

| Statement | Notes |
|---|---|
| `{{ expr }}` | Prints the value as Python's `str()` would. **No autoescaping**: `"`, `'`, `<`, `>` and `&` pass through. Printed data is never parsed again, so a `{{` inside a value prints as is. |
| `{% if %}` `{% elif %}` `{% else %}` `{% endif %}` | Python truthiness: `false`, `0`, `""`, empty list or map, `none` and undefined are false. |
| `{% for x in seq %}` … `{% else %}` … `{% endfor %}` | Over a list, a map (its keys), or a string (its characters). `{% for k, v in m.items() %}` unpacks. `{% for x in xs if cond %}` filters (and `loop.*` counts the kept items). `{% else %}` runs when nothing was iterated. `{% break %}` and `{% continue %}` work. |
| `{% set x = expr %}`, `{% set a, b = 1, 2 %}`, `{% set ns.attr = expr %}` | Assignment. See [Scoping](#scoping). |
| `{% set x %}…{% endset %}` | Captures the block's output into `x`. |
| `{% macro m(a, b=1) %}…{% endmacro %}` | Called as `m(1)` or `m(1, b=2)`; returns its output as a string. Missing arguments are undefined; extra or unknown ones are a `Type` error. Recursion is allowed and bounded by `max_depth`. A macro defined inside a loop or another macro is a closure: it sees the variables visible where it is defined (see [Scoping](#scoping)). |
| `{# comment #}` | Removed. |
| `{% raw %}…{% endraw %}` | Literal text. |

`include`, `import`, `from`, `extends`, `block`, `call`, `filter`, `with`,
`autoescape`, `trans`, `do` and recursive loops are rejected by `parse` with
`Unsupported`. Any other unknown statement is a `Syntax` error.

### Whitespace

As Hugging Face `transformers` and llama.cpp configure Jinja2 for chat
templates:

- `trim_blocks` (on): the first newline after a `{% … %}` or `{# … #}` tag is
  dropped; one after `}}` is kept.
- `lstrip_blocks` (on): spaces and tabs before a block or comment tag that
  starts a line are removed.
- `{%-`, `{{-`, `{#-` remove all whitespace (newlines included) before the tag;
  `-%}`, `-}}`, `-#}` remove all whitespace after it. `{%+` and `+%}` turn
  `lstrip_blocks` / `trim_blocks` off for that tag.
- One newline at the very end of the template is dropped
  (`keep_trailing_newline(true)` keeps it).
- `\r\n` and `\r` in template text and string literals become `\n`, as in
  Jinja2, so a template checked out with Windows line endings renders the same.

### Expressions

Precedence, lowest to highest (Jinja2's): `x if c else y`, `or`, `and`, `not`,
comparisons (`== != < > <= >=`, `in`, `not in`, chainable: `1 < x < 5`), `+ -`,
`~`, `* / // %`, `**`, unary `-` / `+`, then filters (`|`) and tests (`is`),
then subscripts, attribute access and calls. A filter binds to the term before
it, tighter than `+`: `'x' + m['content'] | trim + 'y'` trims only the content.

| Kind | Supported |
|---|---|
| Literals | `'single'` and `"double"` strings (may contain newlines; escapes `\n \t \r \\ \' \" \xHH \uXXXX \UXXXXXXXX` and octal; adjacent strings concatenate), integers (`_`, `0x`, `0o`, `0b`), floats, `true` / `false` / `none` and `True` / `False` / `None`, lists `[…]`, maps `{'k': v}` (string keys), tuples `a, b` (evaluated as lists). |
| Arithmetic | `+` (numbers, strings, lists), `-`, `*` (also string or list repetition), `/` (true division), `//` and `%` (Python floor semantics), `**`, `~` (string concatenation of any values). Integer overflow and division by zero are `Type` errors. |
| Logic | `and` / `or` short-circuit and return an operand, as in Python; `not`. |
| Equality | Numbers compare by value (`1 == 1.0`, `true == 1`); values of different kinds are unequal, never an error (`'x' != false` is true). |
| Access | `m['key']`, `m.key` (dot access on maps), `xs[0]`, `xs[-1]`, `xs[1:]`, `xs[:-1]`, `s[::-1]`, `xs.0`. A missing key or index is undefined. |
| Calls | Macros, globals, host functions, methods; positional then keyword arguments. |

### Undefined values

An unset name, a missing key or an out-of-range index is **undefined**. It is
false in an `if`, prints as nothing, iterates as empty and has `length` 0; the
`default` filter replaces it. Subscripting it, reading an attribute of it,
calling it or using it in arithmetic or ordering is an `Undefined` error. Every
chat template's "no generation prompt" path relies on this: llama.cpp leaves
`add_generation_prompt` unset rather than false.

### Scoping

- An `if` opens no scope: a `set` inside it is visible after it.
- Each iteration of a `for` loop has its own scope. A `set` in the loop body
  (including inside an `if` in it) lasts for the rest of that iteration and
  is gone afterwards; it never changes a variable outside the loop.
- `namespace(a=0)` creates the one mutable object; `{% set ns.a = … %}` inside
  a loop changes it for good. Use it for state that must survive a loop.
- A macro sees its arguments, the template's top-level variables (as they
  are when it is called) and the context, not its caller's loop variables.
  A macro defined inside a loop body or another macro also sees the
  variables visible where it is defined, such as the loop variable and
  `loop`, with the values they had at the definition, and can call itself
  by name.
- The context is read-only. A top-level `{% set messages = messages[1:] %}`
  shadows the context variable for the rest of the template.

### Tests

`defined`, `undefined`, `none`, `string`, `number`, `integer`, `float`,
`boolean`, `mapping`, `sequence`, `iterable`, `callable`, `true`, `false`,
`lower`, `upper`, `odd`, `even`, `divisibleby`, `eq` / `equalto` / `==`,
`ne` / `!=`, `lt` / `lessthan` / `<`, `le` / `<=`, `gt` / `greaterthan` / `>`,
`ge` / `>=`, `in`, `sameas`, `escaped`. `is not` negates. A test that takes one
argument may omit the parentheses: `x is divisibleby 3`, `r is equalto 'user'`.

### Filters

| Filter | Notes |
|---|---|
| `length` / `count` | Items of a list or map, characters of a string; 0 for undefined. |
| `tojson(indent=None, ensure_ascii=False, separators=None, sort_keys=False)` | As Hugging Face's chat-template `tojson`: keys in input order (unless `sort_keys`), no HTML escaping, non-ASCII written as is, Python's `json.dumps` spacing. |
| `string`, `list`, `items`, `first`, `last`, `reverse`, `sort(reverse, case_sensitive, attribute)`, `unique`, `min`, `max`, `sum(attribute, start)`, `join(d, attribute)`, `dictsort` | Python / Jinja2 semantics. `sort` and `dictsort` are stable; numbers order exactly (integers against floats too) with NaN after every other number; keys of kinds that cannot be ordered (a number and a string, two maps that differ) are a `Type` error. |
| `default` / `d(value='', boolean=False)` | Replaces an undefined value (or any falsy one with `boolean=true`). |
| `select`, `reject`, `selectattr`, `rejectattr` | With a test name and its arguments, e.g. `selectattr('role', 'equalto', 'user')`; without one, by truthiness. |
| `map(attribute='x', default=…)`, `map('filter', …)` | Attribute paths may be dotted (`'function.name'`); with a `default`, any part of the path that is missing gives the default, as in Jinja2. Each level of `map('map', …)` counts as a level of nesting. |
| `trim(chars)`, `upper`, `lower`, `capitalize`, `title`, `replace(old, new, count)`, `indent(width=4, first=False, blank=False)`, `center`, `wordcount`, `escape` / `e`, `safe` | String filters; `escape` HTML-escapes explicitly (there is no autoescaping). |
| `int(default=0, base=10)`, `float(default=0.0)`, `abs`, `round(precision=0, method='common')` | Python conversions; `round` rounds half to even, as Python does. |
| `attr(name)` | An attribute (not a map item), as in Jinja2. |

### Methods and globals

- Strings: `strip`, `lstrip`, `rstrip`, `lower`, `upper`, `title`,
  `capitalize`, `startswith`, `endswith` (a string or a list of them),
  `split`, `rsplit`, `splitlines`, `replace`, `find`, `count`, `join`,
  `isalnum`, `isalpha`, `isdigit`, `isspace`, `islower`, `isupper`.
- Maps: `items`, `keys`, `values`, `get(key, default)`.
- Lists: `index`, `count`. Mutating methods (`append`, `pop`, …) are absent,
  as in Jinja2's immutable sandbox.
- Loop: `loop.index`, `index0`, `revindex`, `revindex0`, `first`, `last`,
  `length`, `previtem`, `nextitem`, `depth`, `depth0`, `cycle(…)`.
- Globals: `range(stop)` / `range(start, stop, step)`, `namespace(…)`,
  `dict(…)`, `raise_exception(message)`, and registered host functions.

## Errors

`parse` rejects everything it can: an unclosed tag, string or comment, an end
tag with no open block, an unknown filter, test, statement or function (a call
to a name that is not a global, a host function, or a macro or variable of the
template), unsupported statements, and nesting deeper than `max_depth`.

| `TemplateErrorKind` | When |
|---|---|
| `Syntax(msg)` | Malformed template text. |
| `UnknownFilter(name)`, `UnknownTest(name)`, `UnknownFunction(name)` | A name the engine does not know (also when a filter or test is named in a string literal, as in `select('odd')`). |
| `Unsupported(msg)` | `include`, `import`, `extends`, `call` blocks and the rest of what this subset leaves out. |
| `Type(msg)` | Wrong types for an operation (`'a' + [1]`), division by zero, bad filter arguments, a non-map context. |
| `Undefined(msg)` | An undefined value subscripted, called or used in arithmetic or ordering. |
| `Raised(msg)` | The template called `raise_exception(msg)`. |
| `Function { name, message }` | A host function returned an error. |
| `LimitExceeded(name)` | A [limit](#limits) was exceeded; `name` is the `Limits` field (`"max_steps"`, `"max_depth"`, …). |

Every error carries the byte offset plus line and column of the construct
that failed. `Display` reads like
`unknown filter 'bogus' (line 3, column 10)`.

## Limits

| Field | Default | Bounds |
|---|---|---|
| `max_source_bytes` | 256 KiB | The template source. Parsing holds the tokens and the syntax tree at once, which peaks at up to about 100 times the source (a 1 MiB source of tiny expressions needs about 95 MB), so the default stays well above real chat templates (under 20 KB) but far below what would make parsing costly. |
| `max_depth` | 64 | Nesting of blocks plus expression terms, checked at parse time, and macro calls at render time. A long run of one operator (`a + b + c + …`, `x ~ y ~ z`) is not nesting. |
| `max_iterations` | 1,000,000 | Loop iterations per render; filters that walk a list count its items. |
| `max_calls` | 1,000,000 | Filter, test, function, method and macro calls per render. |
| `max_output_bytes` | 16 MiB | The output (and any macro or `set` block's captured output). |
| `max_alloc_bytes` | 256 MiB | Total bytes of strings and lists the template builds during a render. Large values are refused before they are allocated (`'x' * 10**12`, `range(10**12)`), and printing or joining a list that repeats one large string is capped as it is written. |
| `max_steps` | 100,000,000 | Total work per render, and so its running time (spending it all takes a few seconds at most in a release build). One step per statement, expression evaluated, loop iteration and call, plus steps in proportion to what an operation walks: one per list item or map entry compared, copied, iterated or scanned, one per 64 bytes of text searched, compared, sliced, counted or converted. Real chat templates use a few thousand. |

Real chat templates need a depth under 10, fewer than a hundred iterations
and a few kilobytes of output. Values (from the context or built by the
template) nested more than 512 levels deep are refused with
`LimitExceeded("max_depth")`.

The counting limits bound how much a template does; `max_steps` bounds how
long it takes. Without it, a loop of a million iterations (within
`max_iterations`) over an operation that walks a large value, such as
`'x' in s` on a long string or `a == b` on two long lists, would run for
hours. Maps, scopes, namespaces and argument lists are hashed once they hold
more than a few names, so a template or context with many names stays
linear.

Stack: with the defaults, a render needs under 256 KiB of stack in a
release build. A debug build needs up to 1 MiB for templates nested to
`max_depth`, and up to about 2 MiB to print (`tojson`) a value nested close
to the 512-level limit. If you raise `max_depth`, parse and render on a
thread with a larger stack.

## Sandbox

A template sees only its context, the variables it sets and the registered
globals. There is no file, environment, clock, network or host-object access
unless the host registers a function for it. `namespace` objects, the one
mutable kind, are emptied at the end of every render, so a namespace that
refers to itself cannot outlive it. The module contains no `unsafe` code and
adds no dependencies.

## How it is tested

- **Golden chat templates** (`tests/template_chat.rs`). Nine chat templates
  render five conversations (one user message; system + user; multi-turn;
  special characters including quotes, a tab, non-ASCII, an emoji and literal
  `{{ not_a_var }}` / `{% if %}` text; two user messages in a row) with
  `add_generation_prompt` true and false. Mistral v0.3 and Qwen2.5 also render
  a tool-calling exchange. The output must match byte for byte, and a refusing
  template must raise the recorded message. Each template is parsed once, and
  every "no generation prompt" case gives the same bytes with the flag false
  and unset.
  - Copied verbatim from the model repos at pinned revisions: ChatML (SmolLM2),
    Zephyr (TinyLlama), Mistral v0.2 and v0.3, Qwen2.5 (all Apache-2.0) and
    Phi-3 (MIT).
  - The Llama-2, Llama-3 and Gemma-2 formats are written from scratch for the
    tests, because their published templates are under the model licences.
    They use the same Jinja2 constructs.
  - The expected output was produced by reference Jinja2 3.1.4, used only
    locally as an oracle, in the environment `transformers` uses.
- **Conformance** (`tests/template_conformance.rs`): about 200 small templates
  covering each construct, filter, test and method, with the output or error
  class Jinja2 3.1.4 gives for them.
- **The feature request's fixture set** (`external_fixture_manifest_matches`
  in `tests/template_chat.rs`). The committed goldens above come from
  Jinja2, and three of the seven requested templates are replaced by
  hand-written ones, because the upstream Llama-2, Llama-3 and Gemma-2
  templates cannot be committed. To check the real templates and the
  llama.cpp b9850 goldens, set `FERROLY_TEMPLATE_FIXTURES` to the directory
  holding the set's `manifest.json` and run
  `cargo test -p ferroly --features template,hash --test template_chat`; each
  template is parsed once, and every "no generation prompt" case is checked
  with the flag unset and false. Without the variable the test does nothing.
- **Errors and limits** (`tests/template_engine.rs`): parse errors and their
  positions, runtime error kinds, every limit (deep nesting, runaway
  recursion, nested loops over a large list, huge output, exponential string
  growth, self-referencing namespaces, deeply nested data, work in linear
  operations, loops left with `break`), options and host functions; and
  regression tests for inputs that once hung, panicked or overflowed the
  stack (`[] * 10**17`, sorting NaN, `i128`-sized slice steps and `range`
  bounds, `map('map', …)`, unpacking a huge string, macros and maps with
  100,000 names). Each runs on its own thread under a time limit, so a hang
  fails the test instead of stalling the suite.
- **No panic on any input.** A generative test over random token soup
  (including the largest integer literals, NaN and infinities, empty and huge
  repetition, mixed-kind sorts and nested `map`) and random contexts,
  mutation of the real templates, and replay of the fuzz seed corpus, each
  render timed. The `template_jinja` cargo-fuzz target (`fuzz/`) runs on a
  schedule, with overflow checks on and a render that takes seconds treated
  as a crash.

## Differences from Jinja2

- Tuples are lists: `(1, 2)`, `m.items()` and `dictsort` yield lists, which
  print as `[1, 2]` where Python prints `(1, 2)`. `range()` returns a list.
- A call to an unknown function, and an unknown filter or test, is a parse
  error rather than a render-time one.
- A test argument without parentheses is read only for tests that take one,
  so `x is defined if y else z` parses as expected.
- Map literals take string keys only.
- `sort` orders NaN after every other number (Python leaves an arbitrary
  order).
- A macro defined inside a loop or another macro captures the values its
  enclosing variables have at the definition; Jinja2's closures see later
  assignments too.
- The `\N{NAME}` string escape is refused with `Unsupported` (it would need
  the Unicode name table).
- `'a b'.split(' ', 2**64)` and similar out-of-range counts saturate where
  Python raises `OverflowError`.
- `tojson` is the Hugging Face variant (no HTML escaping, input key order), not
  Jinja2's HTML-safe one.
- `printf`-style `'%s' % x` formatting, `*args` / `**kwargs`, `loop.changed`,
  macro `varargs` / `kwargs` / `caller` and the left-out statements are not
  supported.

## See also

- [`codec`](codec.md): the `Value` model the context is written in.
- [`genai`](genai.md#prompt-templates): the Handlebars-style prompt templates.
