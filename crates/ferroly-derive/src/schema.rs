//! `#[derive(JsonSchema)]` expansion.
//!
//! Builds the JSON-Schema document for a type from the same [`Container`]
//! model the `Encode` / `Decode` derives use, so keys, renames, optional
//! fields and the enum representation always match the wire form. The
//! generated code only calls `::ferroly::codec::schema::__private`, which
//! assembles the schema objects.

use proc_macro2::TokenStream;
use quote::quote;
use syn::DeriveInput;

use crate::model::{Body, Container, Derive, Field, FieldSet, Repr, Style, Variant};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let c = Container::parse(&input)?;
    let name = &c.ident;
    let generics = c.generics_for(Derive::Schema);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let deny = c.deny_unknown_fields;

    let body = match &c.body {
        Body::Struct(fs) => content(fs, None, deny),
        Body::Enum(variants, repr) => enumeration(variants, repr, deny),
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::ferroly::codec::schema::JsonSchema for #name #ty_generics #where_clause {
            fn json_schema() -> ::ferroly::codec::Value {
                // A recursive type gets `{}` where it nests itself, rather
                // than recursing without bound.
                ::ferroly::codec::schema::__private::recursive::<Self>(|| #body)
            }
        }
    })
}

/// The schema of one field's type.
fn type_schema(f: &Field) -> TokenStream {
    let ty = &f.ty;
    quote! { <#ty as ::ferroly::codec::schema::JsonSchema>::json_schema() }
}

/// The schema of a set of fields, as `Encode` writes them. `tag` (`(tag key,
/// variant name)`) forces an object whose tag member is that variant name.
fn content(fs: &FieldSet, tag: Option<(&str, &str)>, deny: bool) -> TokenStream {
    let private = quote! { ::ferroly::codec::schema::__private };
    if tag.is_none() {
        match fs.style {
            Style::Unit => return quote! { #private::null() },
            Style::Tuple => {
                if let Some(only) = fs.newtype() {
                    return type_schema(only);
                }
                let items = fs.active().map(type_schema);
                return quote! { #private::tuple(::std::vec![#(#items),*]) };
            }
            Style::Named => {}
        }
    }
    let mut steps = Vec::new();
    if let Some((tag, variant)) = tag {
        steps.push(quote! { #private::tag(&mut __props, &mut __required, #tag, #variant); });
    }
    for f in fs.active() {
        let schema = type_schema(f);
        if f.flatten {
            let optional = f.option;
            steps.push(quote! {
                #private::flatten(&mut __props, &mut __required, #schema, #optional);
            });
        } else {
            let key = &f.key;
            let required = !(f.option || f.default.is_some() || f.skip_none);
            steps.push(quote! {
                #private::field(&mut __props, &mut __required, #key, #schema, #required);
            });
        }
    }
    quote! {{
        let mut __props = ::std::vec::Vec::new();
        let mut __required = ::std::vec::Vec::new();
        #(#steps)*
        #private::object(__props, __required, #deny)
    }}
}

fn enumeration(variants: &[Variant], repr: &Repr, deny: bool) -> TokenStream {
    let private = quote! { ::ferroly::codec::schema::__private };
    match repr {
        Repr::External => {
            let mut units = Vec::new();
            let mut alternatives = Vec::new();
            for v in variants {
                for name in v.names() {
                    if v.fields.style == Style::Unit {
                        units.push(name.clone());
                    } else {
                        let inner = content(&v.fields, None, deny);
                        alternatives.push(quote! { #private::wrap(#name, #inner) });
                    }
                }
            }
            quote! {
                #private::one_of(&[#(#units),*], ::std::vec![#(#alternatives),*])
            }
        }
        Repr::Internal(tag) => {
            let mut names = Vec::new();
            let mut alternatives = Vec::new();
            for v in variants {
                for name in v.names() {
                    names.push(name.clone());
                    alternatives.push(content(&v.fields, Some((tag, name)), deny));
                }
            }
            quote! {
                #private::internal(#tag, &[#(#names),*], ::std::vec![#(#alternatives),*])
            }
        }
        Repr::Untagged => {
            let alternatives = variants.iter().map(|v| content(&v.fields, None, deny));
            quote! { #private::one_of(&[], ::std::vec![#(#alternatives),*]) }
        }
    }
}
