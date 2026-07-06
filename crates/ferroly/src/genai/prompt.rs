//! Prompt templates and stores.

use std::collections::HashMap;

use ferroly::codec::Encode;

use ferroly::genai::{GenAiError, Message, MessagePart, Role};

/// A named, reusable prompt template.
///
/// Rendering uses the in-house [`ferroly::genai::template`] engine (`{{ name }}` /
/// `{{ a.b }}` substitution). The Go `text/template` `{{.name}}` form is
/// also accepted (the leading dot is tolerated).
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    /// A unique identifier used to look the template up in a store.
    pub id: String,
    /// A human-readable name.
    pub name: String,
    /// The template source string.
    pub template: String,
}

impl PromptTemplate {
    /// Creates a template.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            template: template.into(),
        }
    }

    /// Renders the template with the given variables.
    pub fn render<T: Encode>(&self, vars: &T) -> Result<String, GenAiError> {
        ferroly::genai::template::render(&self.template, vars)
    }
}

/// A lookup surface for stored prompt templates.
pub trait PromptStore: Send + Sync {
    /// Returns a template by id, if present.
    fn get(&self, id: &str) -> Option<PromptTemplate>;
}

/// An in-memory [`PromptStore`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryPromptStore {
    templates: HashMap<String, PromptTemplate>,
}

impl InMemoryPromptStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a template.
    pub fn add(&mut self, template: PromptTemplate) -> &mut Self {
        self.templates.insert(template.id.clone(), template);
        self
    }
}

impl PromptStore for InMemoryPromptStore {
    fn get(&self, id: &str) -> Option<PromptTemplate> {
        self.templates.get(id).cloned()
    }
}

impl Message {
    /// Renders a stored template into a message.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(ferroly::codec::Encode)]
    struct Vars {
        name: String,
        topic: String,
    }

    #[test]
    fn renders_template() {
        let t = PromptTemplate::new("greet", "Greeting", "Hello {{ name }}, about {{ topic }}?");
        let out = t
            .render(&Vars {
                name: "Ada".into(),
                topic: "Rust".into(),
            })
            .unwrap();
        assert_eq!(out, "Hello Ada, about Rust?");
    }

    #[test]
    fn message_from_prompt_id() {
        let mut store = InMemoryPromptStore::new();
        store.add(PromptTemplate::new("g", "Greeting", "Hi {{ name }}"));
        let msg = Message::from_prompt_id(
            Role::User,
            &store,
            "g",
            &Vars {
                name: "Bob".into(),
                topic: "x".into(),
            },
        )
        .unwrap();
        assert_eq!(msg.text_content(), "Hi Bob");
    }

    #[test]
    fn missing_template_errors() {
        let store = InMemoryPromptStore::new();
        let err = Message::from_prompt_id(Role::User, &store, "nope", &()).unwrap_err();
        assert!(matches!(err, GenAiError::TemplateNotFound(_)));
    }
}
