//! Helper-attribute parsing for the derives.

use syn::{Attribute, LitStr};

/// Container-level `#[ferroly(...)]` options.
#[derive(Default)]
pub struct ContainerAttrs {
    pub rename_all: Option<String>,
}

pub fn container_attrs(attrs: &[Attribute]) -> syn::Result<ContainerAttrs> {
    let mut c = ContainerAttrs::default();
    for a in attrs {
        if a.path().is_ident("ferroly") {
            // Propagate parse errors (e.g. `rename_all = 123`) and reject unknown
            // keys (e.g. a typo) instead of silently ignoring them.
            a.parse_nested_meta(|m| {
                if m.path.is_ident("rename_all") {
                    let s: LitStr = m.value()?.parse()?;
                    c.rename_all = Some(s.value());
                    Ok(())
                } else {
                    Err(m.error("unknown ferroly container attribute"))
                }
            })?;
        }
    }
    Ok(c)
}

/// Field/variant-level `#[ferroly(...)]` options.
#[derive(Default)]
pub struct MemberAttrs {
    pub rename: Option<String>,
    pub skip_none: bool,
}

pub fn member_attrs(attrs: &[Attribute]) -> syn::Result<MemberAttrs> {
    let mut m = MemberAttrs::default();
    for a in attrs {
        if a.path().is_ident("ferroly") {
            a.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    let s: LitStr = meta.value()?.parse()?;
                    m.rename = Some(s.value());
                    Ok(())
                } else if meta.path.is_ident("skip_none") {
                    m.skip_none = true;
                    Ok(())
                } else {
                    Err(meta.error("unknown ferroly field/variant attribute"))
                }
            })?;
        }
    }
    Ok(m)
}

/// The display strategy declared by `#[error(...)]` on an error variant.
pub enum ErrorDisplay {
    /// `#[error(transparent)]` — delegate `Display`/`source` to the sole field.
    Transparent,
    /// `#[error("...")]` — a format string.
    Format(LitStr),
}

/// Parses the `#[error(...)]` attribute on an error variant.
pub fn error_display(attrs: &[Attribute]) -> Option<ErrorDisplay> {
    for a in attrs {
        if a.path().is_ident("error") {
            if let Ok(id) = a.parse_args::<syn::Ident>() {
                if id == "transparent" {
                    return Some(ErrorDisplay::Transparent);
                }
            }
            if let Ok(lit) = a.parse_args::<LitStr>() {
                return Some(ErrorDisplay::Format(lit));
            }
        }
    }
    None
}

/// Whether a field carries `#[from]`.
pub fn has_from(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("from"))
}

/// Whether a field carries `#[source]` (or `#[from]`, which implies a source).
pub fn is_source(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().is_ident("source") || a.path().is_ident("from"))
}
