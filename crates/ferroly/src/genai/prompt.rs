//! Prompt templates and stores.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;

use ferroly::codec::{Encode, Value};

use ferroly::genai::template::{PartialSource, RenderOptions, Template};
use ferroly::genai::{GenAiError, Message, MessagePart, Role};

/// The largest template file [`InMemoryPromptStore::load_dir`] accepts.
const MAX_TEMPLATE_FILE_BYTES: u64 = 1024 * 1024;
/// How deep `load_dir` descends into sub-directories.
const MAX_DIR_DEPTH: usize = 8;
/// File extensions `load_dir` treats as templates.
const TEMPLATE_EXTENSIONS: &[&str] = &["prompt", "tmpl", "txt", "md"];

/// A named, reusable prompt template.
///
/// Rendering uses the in-house [`ferroly::genai::template`] engine
/// (`{{ name }}` substitution, `#if` / `#each` sections, partials). The Go
/// `text/template` `{{.name}}` form is also accepted (the leading dot is
/// tolerated).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptTemplate {
    /// A unique identifier used to look the template up in a store.
    pub id: String,
    /// A human-readable name.
    pub name: String,
    /// The template source string.
    pub template: String,
    /// The version of this template, if it is versioned (`"3"`, `"1.2.0"`, …).
    pub version: Option<String>,
}

impl PromptTemplate {
    /// Creates an unversioned template.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            template: template.into(),
            version: None,
        }
    }

    /// Sets the version.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Renders the template with the given variables, **leniently**: an
    /// undefined variable renders as the empty string. Kept for existing
    /// callers; prefer [`render_strict`](Self::render_strict).
    pub fn render<T: Encode>(&self, vars: &T) -> Result<String, GenAiError> {
        ferroly::genai::template::render(&self.template, vars)
    }

    /// Renders the template with the given variables, **strictly**: an
    /// undefined variable is an error.
    pub fn render_strict<T: Encode>(&self, vars: &T) -> Result<String, GenAiError> {
        self.compile()?.render(vars)
    }

    /// Compiles the template source, reporting syntax errors.
    pub fn compile(&self) -> Result<Template, GenAiError> {
        Template::parse(&self.template)
            .map_err(|e| GenAiError::Template(format!("prompt '{}': {e}", self.id)))
    }
}

/// A lookup surface for stored prompt templates.
pub trait PromptStore: Send + Sync {
    /// Returns the latest version of a template by id, if present.
    fn get(&self, id: &str) -> Option<PromptTemplate>;

    /// Returns one specific version of a template. The default only knows the
    /// latest version; stores that keep several override it.
    fn get_version(&self, id: &str, version: &str) -> Option<PromptTemplate> {
        self.get(id)
            .filter(|t| t.version.as_deref() == Some(version))
    }
}

/// Resolves `{{> id }}` partials from a prompt store.
struct StorePartials<'a>(&'a dyn PromptStore);

impl PartialSource for StorePartials<'_> {
    fn partial(&self, name: &str) -> Option<String> {
        self.0.get(name).map(|t| t.template)
    }
}

/// Renders a stored template strictly, with the store's other templates
/// available as partials.
fn render_from_store(
    store: &dyn PromptStore,
    template: &PromptTemplate,
    vars: &Value,
) -> Result<String, GenAiError> {
    let partials = StorePartials(store);
    template
        .compile()?
        .render_with(vars, &RenderOptions::strict().partials(&partials))
        .map_err(|e| match e {
            GenAiError::Template(msg) => {
                GenAiError::Template(format!("prompt '{}': {msg}", template.id))
            }
            other => other,
        })
}

/// Orders version strings: dot-separated parts compare numerically when both
/// are numbers (`"1.10" > "1.9"`), otherwise as text.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// An in-memory [`PromptStore`] that keeps every version of every template.
///
/// [`get`](PromptStore::get) returns the latest version: the highest version
/// string (numeric dot-separated parts compare as numbers), with an
/// unversioned template ranking below any versioned one.
///
/// ```
/// use ferroly::genai::{InMemoryPromptStore, PromptStore, PromptTemplate};
///
/// let mut store = InMemoryPromptStore::new();
/// store.add(PromptTemplate::new("greet", "Greeting", "Hi {{ name }}").with_version("1"));
/// store.add(PromptTemplate::new("greet", "Greeting", "Hello {{ name }}!").with_version("2"));
///
/// assert_eq!(store.get("greet").unwrap().version.as_deref(), Some("2"));
/// assert_eq!(store.get_version("greet", "1").unwrap().template, "Hi {{ name }}");
/// ```
#[derive(Debug, Clone, Default)]
pub struct InMemoryPromptStore {
    /// Per id, the versions sorted oldest → latest.
    templates: BTreeMap<String, Vec<PromptTemplate>>,
}

impl InMemoryPromptStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a template. A template with the same id **and** version replaces
    /// the stored one; a new version is kept next to the others.
    pub fn add(&mut self, template: PromptTemplate) -> &mut Self {
        let versions = self.templates.entry(template.id.clone()).or_default();
        versions.retain(|t| t.version != template.version);
        versions.push(template);
        versions.sort_by(|a, b| match (&a.version, &b.version) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => compare_versions(x, y),
        });
        self
    }

    /// The ids of all stored templates, sorted.
    pub fn ids(&self) -> Vec<String> {
        self.templates.keys().cloned().collect()
    }

    /// The versions stored for `id`, oldest first (an unversioned template is
    /// not listed).
    pub fn versions(&self, id: &str) -> Vec<String> {
        self.templates
            .get(id)
            .map(|v| v.iter().filter_map(|t| t.version.clone()).collect())
            .unwrap_or_default()
    }

    /// How many template ids are stored.
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Renders the latest version of `id` **strictly**, with every other
    /// template of the store available as a `{{> id }}` partial.
    pub fn render<T: Encode>(&self, id: &str, vars: &T) -> Result<String, GenAiError> {
        let template = self
            .get(id)
            .ok_or_else(|| GenAiError::TemplateNotFound(id.to_string()))?;
        render_from_store(self, &template, &vars.encode())
    }

    /// Like [`render`](Self::render) for one specific version.
    pub fn render_version<T: Encode>(
        &self,
        id: &str,
        version: &str,
        vars: &T,
    ) -> Result<String, GenAiError> {
        let template = self
            .get_version(id, version)
            .ok_or_else(|| GenAiError::TemplateNotFound(format!("{id}@{version}")))?;
        render_from_store(self, &template, &vars.encode())
    }

    /// Loads every template file under `dir` into a new store.
    ///
    /// A file `support/reply@2.prompt` becomes the template with id
    /// `support/reply` and version `2`; without `@version` it is unversioned.
    /// Files with the extensions `.prompt`, `.tmpl`, `.txt` and `.md` are
    /// read; everything else, hidden files and symbolic links are skipped.
    /// Sub-directories (up to 8 levels) contribute to the id with `/`.
    ///
    /// Every file is compiled while loading, so a syntax error surfaces here
    /// (with the file's path) and not at first use. A file larger than 1 MiB
    /// or not valid UTF-8 is an error.
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Self, GenAiError> {
        let mut store = InMemoryPromptStore::new();
        store.add_dir(dir)?;
        Ok(store)
    }

    /// Loads the template files under `dir` into this store (see
    /// [`load_dir`](Self::load_dir)). Returns how many templates were added.
    pub fn add_dir(&mut self, dir: impl AsRef<Path>) -> Result<usize, GenAiError> {
        let mut added = 0;
        self.walk_dir(dir.as_ref(), "", 0, &mut added)?;
        Ok(added)
    }

    fn walk_dir(
        &mut self,
        dir: &Path,
        prefix: &str,
        depth: usize,
        added: &mut usize,
    ) -> Result<(), GenAiError> {
        let io = |path: &Path, e: std::io::Error| {
            GenAiError::Template(format!("{}: {e}", path.display()))
        };
        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
            .map_err(|e| io(dir, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| io(dir, e))?;
        // Directory order is platform-dependent; sort for a stable result.
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue; // not addressable as a template id
            };
            if file_name.starts_with('.') {
                continue;
            }
            // `file_type` does not follow symbolic links: they are skipped, so
            // a link cannot pull in files from outside the directory.
            let file_type = entry.file_type().map_err(|e| io(&path, e))?;
            if file_type.is_dir() {
                if depth + 1 < MAX_DIR_DEPTH {
                    let nested = format!("{prefix}{file_name}/");
                    self.walk_dir(&path, &nested, depth + 1, added)?;
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some((stem, extension)) = file_name.rsplit_once('.') else {
                continue;
            };
            if !TEMPLATE_EXTENSIONS.contains(&extension) || stem.is_empty() {
                continue;
            }
            let size = entry.metadata().map_err(|e| io(&path, e))?.len();
            if size > MAX_TEMPLATE_FILE_BYTES {
                return Err(GenAiError::Template(format!(
                    "{}: template file is larger than {MAX_TEMPLATE_FILE_BYTES} bytes",
                    path.display()
                )));
            }
            let source = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            let (name, version) = match stem.split_once('@') {
                Some((name, version)) if !name.is_empty() && !version.is_empty() => {
                    (name, Some(version))
                }
                _ => (stem, None),
            };
            let id = format!("{prefix}{name}");
            let mut template = PromptTemplate::new(id.clone(), id, source);
            template.version = version.map(str::to_string);
            Template::parse(&template.template)
                .map_err(|e| GenAiError::Template(format!("{}: {e}", path.display())))?;
            self.add(template);
            *added += 1;
        }
        Ok(())
    }
}

impl PromptStore for InMemoryPromptStore {
    fn get(&self, id: &str) -> Option<PromptTemplate> {
        self.templates.get(id).and_then(|v| v.last()).cloned()
    }

    fn get_version(&self, id: &str, version: &str) -> Option<PromptTemplate> {
        self.templates
            .get(id)?
            .iter()
            .find(|t| t.version.as_deref() == Some(version))
            .cloned()
    }
}

impl PartialSource for InMemoryPromptStore {
    fn partial(&self, name: &str) -> Option<String> {
        self.get(name).map(|t| t.template)
    }
}

impl Message {
    /// Renders a stored template into a message, **leniently** (an undefined
    /// variable renders as the empty string). Kept for existing callers;
    /// prefer [`from_prompt`](Message::from_prompt).
    pub fn from_prompt_id<T: Encode>(
        role: Role,
        store: &dyn PromptStore,
        template_id: &str,
        vars: &T,
    ) -> Result<Message, GenAiError> {
        let template = store
            .get(template_id)
            .ok_or_else(|| GenAiError::TemplateNotFound(template_id.to_string()))?;
        let text = template.render(vars)?;
        Ok(Message {
            role,
            id: Some(template_id.to_string()),
            parts: vec![MessagePart::Text(text)],
        })
    }

    /// Renders the latest version of a stored template into a message,
    /// **strictly**: an undefined variable is an error. The store's other
    /// templates are available as `{{> id }}` partials.
    pub fn from_prompt<T: Encode>(
        role: Role,
        store: &dyn PromptStore,
        template_id: &str,
        vars: &T,
    ) -> Result<Message, GenAiError> {
        let template = store
            .get(template_id)
            .ok_or_else(|| GenAiError::TemplateNotFound(template_id.to_string()))?;
        let text = render_from_store(store, &template, &vars.encode())?;
        Ok(Message {
            role,
            id: Some(template_id.to_string()),
            parts: vec![MessagePart::Text(text)],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(ferroly::codec::Encode)]
    struct Vars {
        name: String,
        topic: String,
    }

    fn vars(name: &str) -> Vars {
        Vars {
            name: name.into(),
            topic: "Rust".into(),
        }
    }

    #[test]
    fn renders_template() {
        let t = PromptTemplate::new("greet", "Greeting", "Hello {{ name }}, about {{ topic }}?");
        assert_eq!(t.render(&vars("Ada")).unwrap(), "Hello Ada, about Rust?");
        assert_eq!(
            t.render_strict(&vars("Ada")).unwrap(),
            "Hello Ada, about Rust?"
        );
    }

    #[test]
    fn strict_render_rejects_missing_variables() {
        let t = PromptTemplate::new("g", "G", "Hello {{ nmae }}");
        assert_eq!(t.render(&vars("Ada")).unwrap(), "Hello ");
        let e = t.render_strict(&vars("Ada")).unwrap_err().to_string();
        assert!(e.contains("undefined variable 'nmae'"), "{e}");
    }

    #[test]
    fn message_from_prompt_id() {
        let mut store = InMemoryPromptStore::new();
        store.add(PromptTemplate::new("g", "Greeting", "Hi {{ name }}"));
        let msg = Message::from_prompt_id(Role::User, &store, "g", &vars("Bob")).unwrap();
        assert_eq!(msg.text_content(), "Hi Bob");
        let msg = Message::from_prompt(Role::User, &store, "g", &vars("Bob")).unwrap();
        assert_eq!(msg.text_content(), "Hi Bob");
    }

    #[test]
    fn missing_template_errors() {
        let store = InMemoryPromptStore::new();
        let err = Message::from_prompt_id(Role::User, &store, "nope", &()).unwrap_err();
        assert!(matches!(err, GenAiError::TemplateNotFound(_)));
    }

    #[test]
    fn versions_are_ordered_and_addressable() {
        let mut store = InMemoryPromptStore::new();
        store
            .add(PromptTemplate::new("p", "P", "v10").with_version("1.10"))
            .add(PromptTemplate::new("p", "P", "v9").with_version("1.9"))
            .add(PromptTemplate::new("p", "P", "plain"));
        assert_eq!(store.versions("p"), vec!["1.9", "1.10"]);
        assert_eq!(store.get("p").unwrap().template, "v10");
        assert_eq!(store.get_version("p", "1.9").unwrap().template, "v9");
        assert!(store.get_version("p", "2").is_none());
        // Re-adding a version replaces it.
        store.add(PromptTemplate::new("p", "P", "v10b").with_version("1.10"));
        assert_eq!(store.get("p").unwrap().template, "v10b");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_renders_with_partials() {
        let mut store = InMemoryPromptStore::new();
        store.add(PromptTemplate::new("sig", "Signature", "-- {{ name }}"));
        store.add(PromptTemplate::new(
            "mail",
            "Mail",
            "About {{ topic }}\n{{> sig }}",
        ));
        assert_eq!(
            store.render("mail", &vars("Ada")).unwrap(),
            "About Rust\n-- Ada"
        );
        assert!(matches!(
            store.render("nope", &vars("Ada")),
            Err(GenAiError::TemplateNotFound(_))
        ));
    }
}
