//! `#[derive(Encode)]` expansion.
//!
//! Two bodies are generated per type: `encode` (builds a `Value`) and
//! `encode_to` (streams to an `Encoder`). Both are produced from the same
//! [`FieldSet`] so they always agree on keys, order and representation.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{DeriveInput, Ident};

use crate::model::{Body, Container, Derive, Field, FieldSet, Repr, Style, Variant};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let c = Container::parse(&input)?;
    let name = &c.ident;
    let generics = c.generics_for(Derive::Encode);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let mut helper = TokenStream::new();
    let (encode, encode_to) = match &c.body {
        Body::Struct(fs) => {
            let bind = if fs.active().next().is_some() {
                let pattern = fs.pattern(&quote! { Self });
                quote! { let #pattern = self; }
            } else {
                TokenStream::new()
            };
            let value = value_content(fs, None);
            let stream = stream_content(fs, None);
            (quote! { #bind #value }, quote! { #bind #stream })
        }
        Body::Enum(variants, _) if variants.is_empty() => {
            (quote! { match *self {} }, quote! { match *self {} })
        }
        Body::Enum(variants, repr) => expand_enum(&c, variants, repr, &mut helper),
    };

    Ok(quote! {
        const _: () = {
            #helper

            #[automatically_derived]
            impl #impl_generics ::ferroly::codec::Encode for #name #ty_generics #where_clause {
                fn encode(&self) -> ::ferroly::codec::Value {
                    #encode
                }
                fn encode_to<__E: ::ferroly::codec::Encoder>(&self, __e: &mut __E) {
                    #encode_to
                }
            }
        };
    })
}

fn expand_enum(
    c: &Container,
    variants: &[Variant],
    repr: &Repr,
    helper: &mut TokenStream,
) -> (TokenStream, TokenStream) {
    let name = &c.ident;
    let mut value_arms = Vec::new();
    let mut stream_arms = Vec::new();
    // Externally tagged tuple/struct variants whose content is streamed through
    // the `__FerrolyContent` wrapper (an `Encode` value for `map_entry`).
    let mut wrapped: Vec<&Variant> = Vec::new();

    for v in variants {
        let vid = &v.ident;
        let key = &v.key;
        let path = quote! { Self::#vid };
        let pattern = v.fields.pattern(&path);
        match repr {
            Repr::Untagged => {
                let value = value_content(&v.fields, None);
                let stream = stream_content(&v.fields, None);
                value_arms.push(quote! { #pattern => #value, });
                stream_arms.push(quote! { #pattern => { #stream } });
            }
            Repr::Internal(tag) => {
                let value = value_content(&v.fields, Some((tag, key)));
                let stream = stream_content(&v.fields, Some((tag, key)));
                value_arms.push(quote! { #pattern => #value, });
                stream_arms.push(quote! { #pattern => { #stream } });
            }
            Repr::External if v.fields.style == Style::Unit => {
                value_arms.push(quote! {
                    #pattern => ::ferroly::codec::Value::Str(::std::string::String::from(#key)),
                });
                stream_arms.push(quote! {
                    #pattern => { ::ferroly::codec::Encoder::encode_str(__e, #key); }
                });
            }
            Repr::External => {
                let value = value_content(&v.fields, None);
                value_arms.push(quote! {
                    #pattern => ::ferroly::codec::Value::Object(::std::vec![
                        (::std::string::String::from(#key), #value)
                    ]),
                });
                let (stream_pattern, content) = match v.fields.newtype() {
                    Some(only) => {
                        let bind = &only.bind;
                        (pattern, quote! { #bind })
                    }
                    None => {
                        wrapped.push(v);
                        (
                            open_pattern(&v.fields, &path),
                            quote! { &__FerrolyContent(self) },
                        )
                    }
                };
                stream_arms.push(quote! {
                    #stream_pattern => {
                        ::ferroly::codec::Encoder::begin_map(__e, 1usize);
                        ::ferroly::codec::Encoder::map_entry(__e, #key, #content);
                        ::ferroly::codec::Encoder::end_map(__e);
                    }
                });
            }
        }
    }

    if !wrapped.is_empty() {
        let value_arms = wrapped.iter().map(|v| {
            let vid = &v.ident;
            let pattern = v.fields.pattern(&quote! { #name::#vid });
            let value = value_content(&v.fields, None);
            quote! { #pattern => #value, }
        });
        let stream_arms = wrapped.iter().map(|v| {
            let vid = &v.ident;
            let pattern = v.fields.pattern(&quote! { #name::#vid });
            let stream = stream_content(&v.fields, None);
            quote! { #pattern => { #stream } }
        });

        // `struct __FerrolyContent<'__ferroly, ..type's own generics..>(&Type<..>)`
        let mut decl_generics = c.generics.clone();
        decl_generics
            .params
            .insert(0, syn::parse_quote! { '__ferroly });
        let decl_where = &c.generics.where_clause;
        let mut impl_generics = c.generics_for(Derive::Encode);
        impl_generics
            .params
            .insert(0, syn::parse_quote! { '__ferroly });
        let (_, ty_generics, _) = c.generics.split_for_impl();
        let (w_impl, w_ty, w_where) = impl_generics.split_for_impl();

        *helper = quote! {
            struct __FerrolyContent #decl_generics (&'__ferroly #name #ty_generics) #decl_where;

            #[automatically_derived]
            impl #w_impl ::ferroly::codec::Encode for __FerrolyContent #w_ty #w_where {
                #[allow(unreachable_patterns)]
                fn encode(&self) -> ::ferroly::codec::Value {
                    match self.0 {
                        #(#value_arms)*
                        _ => ::ferroly::codec::Value::Null,
                    }
                }
                #[allow(unreachable_patterns)]
                fn encode_to<__E: ::ferroly::codec::Encoder>(&self, __e: &mut __E) {
                    match self.0 {
                        #(#stream_arms)*
                        _ => ::ferroly::codec::Encoder::encode_null(__e),
                    }
                }
            }
        };
    }

    (
        quote! { match self { #(#value_arms)* } },
        quote! { match self { #(#stream_arms)* } },
    )
}

/// A pattern matching `path` without binding any field.
fn open_pattern(fs: &FieldSet, path: &TokenStream) -> TokenStream {
    match fs.style {
        Style::Unit => quote! { #path },
        Style::Tuple => quote! { #path(..) },
        Style::Named => quote! { #path { .. } },
    }
}

/// An expression building the `Value` for a set of fields already bound to
/// their `bind` locals. `tag` (`(tag key, variant name)`) forces an object and
/// writes the tag entry first.
fn value_content(fs: &FieldSet, tag: Option<(&str, &str)>) -> TokenStream {
    if tag.is_none() {
        match fs.style {
            Style::Unit => return quote! { ::ferroly::codec::Value::Null },
            Style::Tuple => {
                if let Some(only) = fs.newtype() {
                    let bind = &only.bind;
                    return quote! { ::ferroly::codec::Encode::encode(#bind) };
                }
                let binds = fs.active().map(|f| &f.bind);
                return quote! {
                    ::ferroly::codec::Value::Array(::std::vec![
                        #(::ferroly::codec::Encode::encode(#binds)),*
                    ])
                };
            }
            Style::Named => {}
        }
    }

    let mut pushes = Vec::new();
    if let Some((tag, variant)) = tag {
        pushes.push(quote! {
            __fields.push((
                ::std::string::String::from(#tag),
                ::ferroly::codec::Value::Str(::std::string::String::from(#variant)),
            ));
        });
    }
    for f in fs.active() {
        let (bind, key) = (&f.bind, &f.key);
        pushes.push(if f.flatten {
            let flat = flat_ident(f);
            quote! { ::core::iter::Extend::extend(&mut __fields, #flat); }
        } else if f.skip_none {
            quote! {
                if let ::core::option::Option::Some(__v) = #bind {
                    __fields.push((
                        ::std::string::String::from(#key),
                        ::ferroly::codec::Encode::encode(__v),
                    ));
                }
            }
        } else {
            quote! {
                __fields.push((
                    ::std::string::String::from(#key),
                    ::ferroly::codec::Encode::encode(#bind),
                ));
            }
        });
    }
    if pushes.is_empty() {
        return quote! { ::ferroly::codec::Value::Object(::std::vec::Vec::new()) };
    }
    let prelude = flatten_prelude(fs, tag);
    quote! {{
        #prelude
        let mut __fields: ::std::vec::Vec<(::std::string::String, ::ferroly::codec::Value)> =
            ::std::vec::Vec::new();
        #(#pushes)*
        ::ferroly::codec::Value::Object(__fields)
    }}
}

/// The local holding a `flatten` field's entries (see [`flatten_prelude`]).
fn flat_ident(f: &Field) -> Ident {
    format_ident!("{}_flat", f.bind)
}

/// For each `flatten` field, `let __fN_flat: Vec<(String, Value)> = ...;` — the
/// entries it contributes to the enclosing object. Both encode paths start
/// with this, so they merge identically:
///
/// - an object contributes its entries, `Null` (an absent `Option`) nothing,
///   and any other shape — which cannot be merged — one entry under the
///   field's own key, rather than being dropped;
/// - an entry is left out when its key is already taken by one of the struct's
///   own fields (or the enum tag) or by an earlier `flatten` field, so no key
///   is ever written twice.
fn flatten_prelude(fs: &FieldSet, tag: Option<(&str, &str)>) -> TokenStream {
    let own_keys: Vec<&str> = tag
        .map(|(tag, _)| tag)
        .into_iter()
        .chain(fs.active().filter(|f| !f.flatten).map(|f| f.key.as_str()))
        .collect();
    let mut earlier: Vec<Ident> = Vec::new();
    let mut out = TokenStream::new();
    for f in fs.active().filter(|f| f.flatten) {
        let (bind, key, flat) = (&f.bind, &f.key, flat_ident(f));
        out.extend(quote! {
            let #flat: ::std::vec::Vec<(::std::string::String, ::ferroly::codec::Value)> =
                match ::ferroly::codec::Encode::encode(#bind) {
                    ::ferroly::codec::Value::Object(__o) => __o,
                    ::ferroly::codec::Value::Null => ::std::vec::Vec::new(),
                    __other => ::std::vec![(::std::string::String::from(#key), __other)],
                };
        });
        if !own_keys.is_empty() || !earlier.is_empty() {
            let own = if own_keys.is_empty() {
                quote! { false }
            } else {
                quote! { ::core::matches!(__entry.0.as_str(), #(#own_keys)|*) }
            };
            let mutable = if earlier.is_empty() {
                TokenStream::new()
            } else {
                quote! { mut }
            };
            out.extend(quote! {
                let #flat = {
                    let mut __kept: ::std::vec::Vec<(
                        ::std::string::String,
                        ::ferroly::codec::Value,
                    )> = ::std::vec::Vec::new();
                    for __entry in #flat {
                        let #mutable __taken: bool = #own;
                        #(
                            for __prev in &#earlier {
                                if __prev.0 == __entry.0 {
                                    __taken = true;
                                }
                            }
                        )*
                        if !__taken {
                            __kept.push(__entry);
                        }
                    }
                    __kept
                };
            });
        }
        earlier.push(flat);
    }
    out
}

/// Statements streaming a set of bound fields to the encoder `__e`. Mirrors
/// [`value_content`] entry for entry.
fn stream_content(fs: &FieldSet, tag: Option<(&str, &str)>) -> TokenStream {
    if tag.is_none() {
        match fs.style {
            Style::Unit => return quote! { ::ferroly::codec::Encoder::encode_null(__e); },
            Style::Tuple => {
                if let Some(only) = fs.newtype() {
                    let bind = &only.bind;
                    return quote! { ::ferroly::codec::Encode::encode_to(#bind, __e); };
                }
                let binds: Vec<_> = fs.active().map(|f| &f.bind).collect();
                let len = binds.len();
                return quote! {
                    ::ferroly::codec::Encoder::begin_seq(__e, #len);
                    #(::ferroly::codec::Encoder::seq_entry(__e, #binds);)*
                    ::ferroly::codec::Encoder::end_seq(__e);
                };
            }
            Style::Named => {}
        }
    }

    // `begin_map` is told the exact entry count: the fixed entries plus whatever
    // the `skip_none` and `flatten` fields contribute at run time.
    let mut fixed = 0usize;
    let prelude = flatten_prelude(fs, tag);
    let mut dynamic = Vec::new();
    let mut entries = Vec::new();
    if let Some((tag, variant)) = tag {
        fixed += 1;
        entries.push(quote! { ::ferroly::codec::Encoder::map_entry(__e, #tag, #variant); });
    }
    for f in fs.active() {
        let (bind, key) = (&f.bind, &f.key);
        if f.flatten {
            let flat = flat_ident(f);
            dynamic.push(quote! { __len += #flat.len(); });
            entries.push(quote! {
                for (__k, __v) in &#flat {
                    ::ferroly::codec::Encoder::map_entry(__e, __k, __v);
                }
            });
        } else if f.skip_none {
            dynamic.push(quote! {
                if ::core::option::Option::is_some(#bind) {
                    __len += 1usize;
                }
            });
            entries.push(quote! {
                if let ::core::option::Option::Some(__v) = #bind {
                    ::ferroly::codec::Encoder::map_entry(__e, #key, __v);
                }
            });
        } else {
            fixed += 1;
            entries.push(quote! { ::ferroly::codec::Encoder::map_entry(__e, #key, #bind); });
        }
    }
    let len = if dynamic.is_empty() {
        quote! { #fixed }
    } else {
        quote! {{
            let mut __len: usize = #fixed;
            #(#dynamic)*
            __len
        }}
    };
    quote! {
        #prelude
        ::ferroly::codec::Encoder::begin_map(__e, #len);
        #(#entries)*
        ::ferroly::codec::Encoder::end_map(__e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn generated(input: DeriveInput) -> String {
        expand(input).unwrap().to_string()
    }

    fn message(input: DeriveInput) -> String {
        match expand(input) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn raw_identifier_fields_use_the_plain_key() {
        let out = generated(parse_quote! { struct S { r#type: u8 } });
        assert!(out.contains("\"type\""), "{out}");
        assert!(!out.contains("\"r#type\""), "{out}");
    }

    #[test]
    fn generics_are_carried_onto_the_impl() {
        let out = generated(parse_quote! {
            struct Page<'a, T: Clone, const N: usize> where T: 'a { items: &'a [T; N] }
        });
        assert!(
            out.contains(
                "impl < 'a , T : Clone , const N : usize > :: ferroly :: codec :: Encode \
                 for Page < 'a , T , N > where T : 'a , T : :: ferroly :: codec :: Encode"
            ),
            "{out}"
        );
    }

    #[test]
    fn content_wrapper_only_for_externally_tagged_compound_variants() {
        let with = generated(parse_quote! { enum E { A, B(u8), C { x: u8 } } });
        assert!(with.contains("__FerrolyContent"));
        let without = generated(parse_quote! { enum E { A, B(u8) } });
        assert!(!without.contains("__FerrolyContent"));
        let untagged = generated(parse_quote! { #[ferroly(untagged)] enum E { C { x: u8 } } });
        assert!(!untagged.contains("__FerrolyContent"));
    }

    #[test]
    fn skipped_fields_are_never_emitted() {
        let out = generated(parse_quote! {
            struct S { kept: u8, #[ferroly(skip)] dropped: u8 }
        });
        assert!(
            out.contains("\"kept\"") && !out.contains("\"dropped\""),
            "{out}"
        );
    }

    #[test]
    fn flattened_entries_are_filtered_against_keys_already_taken() {
        let out = generated(parse_quote! {
            #[ferroly(rename_all = "camelCase")]
            struct S {
                own_key: u8,
                #[ferroly(flatten)]
                first: A,
                #[ferroly(skip)]
                hidden: u8,
                #[ferroly(flatten)]
                second: B,
            }
        });
        // The struct's own (renamed) key shadows flattened entries; a skipped
        // field owns no key.
        assert!(
            out.contains("matches ! (__entry . 0 . as_str () , \"ownKey\")"),
            "{out}"
        );
        // The second flattened field also yields to the first.
        assert!(out.contains("for __prev in & __f1_flat"), "{out}");
        assert!(!out.contains("for __prev in & __f3_flat"), "{out}");

        // With nothing to collide with, no filtering is generated at all.
        let out = generated(parse_quote! { struct S { #[ferroly(flatten)] only: A } });
        assert!(
            out.contains("__f0_flat") && !out.contains("__taken"),
            "{out}"
        );
    }

    #[test]
    fn rejected_shapes_report_spanned_errors() {
        assert!(message(parse_quote! { union U { a: u8, b: u16 } }).contains("unions"));
        assert!(
            message(parse_quote! { #[ferroly(nope)] struct S { a: u8 } })
                .contains("unknown ferroly container attribute")
        );
        assert!(
            message(parse_quote! { struct S { #[ferroly(nope)] a: u8 } })
                .contains("unknown ferroly field attribute")
        );
        assert!(message(parse_quote! { enum E { #[ferroly(nope)] A } })
            .contains("unknown ferroly variant attribute"));
        assert!(
            message(parse_quote! { struct S { a: u8, #[ferroly(rename = "a")] b: u8 } })
                .contains("duplicate field key \"a\"")
        );
    }
}
