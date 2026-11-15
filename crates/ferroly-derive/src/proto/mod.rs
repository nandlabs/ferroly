//! `#[derive(Message)]`, `#[derive(Oneof)]` and `#[derive(Enumeration)]` for
//! `ferroly::proto`.
//!
//! Every field expands to one call per direction into
//! `::ferroly::proto::__private`, parameterised by a kind marker (the protobuf
//! scalar type) and chosen by the field's shape (implicit presence, `Option`,
//! `Vec`, map, oneof). All wire logic lives in that runtime module; this crate
//! only validates attributes and picks the calls.

use std::collections::{HashMap, HashSet};

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, GenericArgument, Ident, PathArguments, Type};

mod attrs;

use attrs::{enum_variant_attrs, field_attrs, no_container_attrs, FieldAttrs, Kind};

fn private() -> TokenStream {
    quote!(::ferroly::proto::__private)
}

fn marker(kind: Kind) -> TokenStream {
    let p = private();
    let m = Ident::new(kind.marker(), Span::call_site());
    quote!(#p::#m)
}

// ---- type inspection -------------------------------------------------------

/// The last path segment of `ty` and its generic type arguments.
fn last_segment(ty: &Type) -> Option<(String, Vec<&Type>)> {
    let Type::Path(tp) = ty else { return None };
    if tp.qself.is_some() {
        return None;
    }
    let seg = tp.path.segments.last()?;
    let args = match &seg.arguments {
        PathArguments::AngleBracketed(a) => a
            .args
            .iter()
            .filter_map(|g| match g {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((seg.ident.to_string(), args))
}

/// `Some(T)` when `ty` is spelled `Wrapper<T>`.
fn unwrap_one<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    match last_segment(ty) {
        Some((name, args)) if name == wrapper && args.len() == 1 => Some(args[0]),
        _ => None,
    }
}

fn is_bytes(ty: &Type) -> bool {
    unwrap_one(ty, "Vec").is_some_and(
        |inner| matches!(last_segment(inner), Some((n, a)) if n == "u8" && a.is_empty()),
    )
}

/// The kind a Rust type is written as when no attribute says otherwise.
fn infer_kind(ty: &Type) -> syn::Result<Kind> {
    if is_bytes(ty) {
        return Ok(Kind::Bytes);
    }
    if let Type::Reference(_) = ty {
        return Err(syn::Error::new(
            ty.span(),
            "protobuf fields must be owned; use `String` or `Vec<u8>` instead of a reference",
        ));
    }
    let Some((name, args)) = last_segment(ty) else {
        return Ok(Kind::Msg);
    };
    Ok(match (name.as_str(), args.len()) {
        ("i32", 0) => Kind::Int32,
        ("i64", 0) => Kind::Int64,
        ("u32", 0) => Kind::UInt32,
        ("u64", 0) => Kind::UInt64,
        ("f32", 0) => Kind::Float,
        ("f64", 0) => Kind::Double,
        ("bool", 0) => Kind::Bool,
        ("String", 0) => Kind::Str,
        ("EnumValue", 1) => Kind::Enum,
        ("i8" | "i16" | "u8" | "u16" | "isize" | "usize" | "i128" | "u128" | "char", 0) => {
            return Err(syn::Error::new(
                ty.span(),
                format!(
                    "protobuf has no `{name}` type; use i32, i64, u32 or u64 (with a type \
                     attribute such as `sint32` or `fixed64` to pick the encoding)"
                ),
            ))
        }
        _ => Kind::Msg,
    })
}

fn element_kind(attrs: &FieldAttrs, ty: &Type) -> syn::Result<Kind> {
    match attrs.kind {
        Some((kind, _)) => Ok(kind),
        None => infer_kind(ty),
    }
}

fn is_map_type(ty: &Type) -> bool {
    matches!(last_segment(ty), Some((n, a)) if (n == "HashMap" || n == "BTreeMap") && a.len() >= 2)
}

// ---- #[derive(Message)] ----------------------------------------------------

enum Shape {
    /// Implicit presence: written unless default.
    Implicit(Kind),
    /// proto2 `required`: always written; decoding fails when absent.
    Required(Kind),
    /// `Option<T>`: explicit presence.
    Optional(Kind),
    /// `Vec<T>`.
    Repeated(Kind, bool),
    /// `HashMap<K, V>` / `BTreeMap<K, V>`.
    Map(Kind, Kind),
    /// `Option<O>` where `O: Oneof`.
    Oneof(Box<Type>),
    Unknown,
    Skip,
}

struct MessageField {
    member: syn::Member,
    name: String,
    tag: u32,
    shape: Shape,
}

pub fn expand_message(input: DeriveInput) -> syn::Result<TokenStream> {
    no_container_attrs(&input.attrs, "Message")?;
    reject_generics(&input, "Message")?;
    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => &n.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    &input.ident,
                    "#[derive(Message)] needs a struct with named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "#[derive(Message)] supports only structs; use #[derive(Oneof)] for a oneof \
                 enum and #[derive(Enumeration)] for a protobuf enum",
            ))
        }
    };

    let mut model = Vec::new();
    let mut seen_tags: HashMap<u32, String> = HashMap::new();
    let mut unknown_seen: Option<String> = None;
    for field in fields {
        let ident = field.ident.clone().expect("named field");
        let name = ident.unraw().to_string();
        let a = field_attrs(&field.attrs)?;
        let shape = field_shape(&a, &field.ty, field.span())?;
        let tag = a.tag.map_or(0, |(t, _)| t);
        if let Shape::Unknown = shape {
            if let Some(prev) = &unknown_seen {
                return Err(syn::Error::new(
                    a.unknown.unwrap_or_else(Span::call_site),
                    format!("only one field may be #[proto(unknown)]; `{prev}` already is"),
                ));
            }
            unknown_seen = Some(name.clone());
        }
        if let Some((t, span)) = a.tag {
            if let Some(prev) = seen_tags.insert(t, name.clone()) {
                return Err(syn::Error::new(
                    span,
                    format!("field number {t} is already used by `{prev}`"),
                ));
            }
        }
        model.push(MessageField {
            member: syn::Member::Named(ident),
            name,
            tag,
            shape,
        });
    }

    let ident = &input.ident;
    let p = private();
    let mut encode = Vec::new();
    let mut len = Vec::new();
    let mut len_cached = Vec::new();
    let mut arms = Vec::new();
    let mut oneof_merges = Vec::new();
    let mut required_locals = Vec::new();
    let mut required_bits = Vec::new();
    let mut required_names = Vec::new();
    let mut oneof_types: Vec<&Type> = Vec::new();
    let mut unknown_member = None;
    // What `__forget_required` walks: values this message owns that can hold
    // required-field records (singular sub-messages and oneofs; repeated and
    // map elements are checked before they are stored).
    let mut forgets = Vec::new();

    for (i, f) in model.iter().enumerate() {
        let m = &f.member;
        let tag = f.tag;
        match &f.shape {
            Shape::Implicit(kind) => {
                let k = marker(*kind);
                encode.push(quote! { #p::encode_implicit::<#k, _>(#tag, &self.#m, __buf, __c); });
                len.push(quote! { #p::len_implicit::<#k, _>(#tag, &self.#m) });
                len_cached.push(quote! { #p::len_implicit_cached::<#k, _>(#tag, &self.#m, __c) });
                arms.push(quote! { #tag => #p::merge_value::<#k, _>(__wt, &mut self.#m, __r)?, });
                if *kind == Kind::Msg {
                    forgets.push(quote! { #p::forget_message(&self.#m, __r); });
                }
            }
            Shape::Required(kind) => {
                if *kind == Kind::Msg {
                    forgets.push(quote! { #p::forget_message(&self.#m, __r); });
                }
                let k = marker(*kind);
                let seen = format_ident!("__seen_{}", i);
                let field_name = &f.name;
                encode.push(quote! { #p::encode_always::<#k, _>(#tag, &self.#m, __buf, __c); });
                len.push(quote! { #p::len_always::<#k, _>(#tag, &self.#m) });
                len_cached.push(quote! { #p::len_always_cached::<#k, _>(#tag, &self.#m, __c) });
                arms.push(quote! {
                    #tag => {
                        let __used = #p::merge_value::<#k, _>(__wt, &mut self.#m, __r)?;
                        #seen |= __used;
                        __used
                    }
                });
                if required_bits.len() == 64 {
                    return Err(syn::Error::new(
                        m.span(),
                        "a message can have at most 64 `required` fields",
                    ));
                }
                let bit = 1u64 << required_bits.len();
                required_locals.push(quote! { let mut #seen = false; });
                required_bits.push(quote! { if #seen { 0 } else { #bit } });
                required_names.push(field_name.clone());
            }
            Shape::Optional(kind) => {
                if *kind == Kind::Msg {
                    forgets.push(quote! { #p::forget_optional_message(&self.#m, __r); });
                }
                let k = marker(*kind);
                encode.push(quote! { #p::encode_optional::<#k, _>(#tag, &self.#m, __buf, __c); });
                len.push(quote! { #p::len_optional::<#k, _>(#tag, &self.#m) });
                len_cached.push(quote! { #p::len_optional_cached::<#k, _>(#tag, &self.#m, __c) });
                arms.push(
                    quote! { #tag => #p::merge_optional::<#k, _>(__wt, &mut self.#m, __r)?, },
                );
            }
            Shape::Repeated(k, packed) => {
                let k = marker(*k);
                encode.push(
                    quote! { #p::encode_repeated::<#k, _>(#tag, &self.#m, #packed, __buf, __c); },
                );
                len.push(quote! { #p::len_repeated::<#k, _>(#tag, &self.#m, #packed) });
                len_cached.push(
                    quote! { #p::len_repeated_cached::<#k, _>(#tag, &self.#m, #packed, __c) },
                );
                arms.push(
                    quote! { #tag => #p::merge_repeated::<#k, _>(__wt, &mut self.#m, __r)?, },
                );
            }
            Shape::Map(kk, vk) => {
                let kk = marker(*kk);
                let vk = marker(*vk);
                encode.push(quote! { #p::encode_map::<#kk, #vk, _>(#tag, &self.#m, __buf, __c); });
                len.push(quote! { #p::len_map::<#kk, #vk, _>(#tag, &self.#m) });
                len_cached.push(quote! { #p::len_map_cached::<#kk, #vk, _>(#tag, &self.#m, __c) });
                arms.push(
                    quote! { #tag => #p::merge_map::<#kk, #vk, _>(__wt, &mut self.#m, __r)?, },
                );
            }
            Shape::Oneof(ty) => {
                encode.push(quote! { #p::encode_oneof(&self.#m, __buf, __c); });
                len.push(quote! { #p::len_oneof(&self.#m) });
                len_cached.push(quote! { #p::len_oneof_cached(&self.#m, __c) });
                oneof_merges.push(quote! { || #p::merge_oneof(&mut self.#m, __tag, __wt, __r)? });
                forgets.push(quote! { #p::forget_oneof(&self.#m, __r); });
                oneof_types.push(ty.as_ref());
            }
            Shape::Unknown => unknown_member = Some(m.clone()),
            Shape::Skip => {}
        }
    }

    let (unknown_encode, unknown_len, unknown_merge) = match &unknown_member {
        // Fully qualified, so a trait in scope with a method of the same name
        // cannot take the call.
        Some(m) => (
            quote! { #p::UnknownFields::encode_raw(&self.#m, __buf); },
            quote! { #p::UnknownFields::encoded_len(&self.#m) },
            quote! { #p::UnknownFields::merge_field(&mut self.#m, __tag, __wt, __r)?; },
        ),
        None => (
            quote! {},
            quote! { 0 },
            quote! { #p::Reader::skip_field(__r, __tag, __wt)?; },
        ),
    };

    // A oneof's field numbers must not collide with the message's own, nor
    // with another oneof's; the derive cannot see them, so a const check does.
    let mut tag_list: Vec<u32> = model.iter().filter(|f| f.tag != 0).map(|f| f.tag).collect();
    tag_list.sort_unstable();
    let mut checks = Vec::new();
    for (i, ty) in oneof_types.iter().enumerate() {
        let msg = format!(
            "a oneof in `{ident}` reuses a field number of the message or of another oneof"
        );
        checks.push(quote! {
            const _: () = ::core::assert!(
                #p::tags_disjoint(&[#(#tag_list),*], <#ty as #p::Oneof>::TAGS),
                #msg
            );
        });
        for other in &oneof_types[i + 1..] {
            checks.push(quote! {
                const _: () = ::core::assert!(
                    #p::tags_disjoint(<#ty as #p::Oneof>::TAGS, <#other as #p::Oneof>::TAGS),
                    #msg
                );
            });
        }
    }

    let arms_or_unused = if arms.is_empty() && oneof_merges.is_empty() {
        quote! { let __used = false; }
    } else {
        quote! {
            let __used = match __tag {
                #(#arms)*
                _ => false #(#oneof_merges)*,
            };
        }
    };

    // Missing required fields go to the reader, which checks them once every
    // occurrence of this message has been merged.
    let required_note = if required_bits.is_empty() {
        quote! {}
    } else {
        let type_name = ident.to_string();
        quote! {
            #p::Reader::note_required(
                __r,
                (self as *const Self).cast::<()>(),
                ::core::any::type_name::<Self>(),
                #type_name,
                0 #(| #required_bits)*,
                &[#(#required_names),*],
            )?;
        }
    };
    if !required_bits.is_empty() {
        forgets.insert(
            0,
            quote! {
                #p::Reader::forget_required(
                    __r,
                    (self as *const Self).cast::<()>(),
                    ::core::any::type_name::<Self>(),
                );
            },
        );
    }
    let forget_fn = if forgets.is_empty() {
        quote! {}
    } else {
        quote! {
            fn __forget_required(&self, __r: &mut #p::Reader<'_>) {
                #(#forgets)*
            }
        }
    };

    Ok(quote! {
        #(#checks)*

        impl #p::Message for #ident {
            fn encode_raw(&self, __buf: &mut ::std::vec::Vec<u8>) {
                let mut __c = #p::SizeCache::new();
                <Self as #p::Message>::__encoded_len_cached(self, &mut __c);
                <Self as #p::Message>::__encode_cached(self, __buf, &mut __c);
            }

            fn encoded_len(&self) -> usize {
                0 #(+ #len)* + #unknown_len
            }

            #[allow(unused_variables)]
            fn __encoded_len_cached(&self, __c: &mut #p::SizeCache) -> usize {
                0 #(+ #len_cached)* + #unknown_len
            }

            #[allow(unused_variables)]
            fn __encode_cached(&self, __buf: &mut ::std::vec::Vec<u8>, __c: &mut #p::SizeCache) {
                #(#encode)*
                #unknown_encode
            }

            #[allow(unused_variables)]
            fn merge_from(
                &mut self,
                __r: &mut #p::Reader<'_>,
            ) -> ::core::result::Result<(), #p::ProtoError> {
                #(#required_locals)*
                while let ::core::option::Option::Some((__tag, __wt)) = #p::Reader::read_key(__r)? {
                    #arms_or_unused
                    if !__used {
                        #unknown_merge
                    }
                }
                #required_note
                ::core::result::Result::Ok(())
            }

            #forget_fn
        }
    })
}

fn field_shape(a: &FieldAttrs, ty: &Type, span: Span) -> syn::Result<Shape> {
    if let Some(s) = a.unknown {
        if a.tag.is_some() || a.kind.is_some() || a.extra_except("unknown").is_some() {
            return Err(syn::Error::new(
                s,
                "#[proto(unknown)] takes no other proto attribute",
            ));
        }
        return Ok(Shape::Unknown);
    }
    if let Some(s) = a.skip {
        if a.tag.is_some() || a.kind.is_some() || a.extra_except("skip").is_some() {
            return Err(syn::Error::new(
                s,
                "#[proto(skip)] takes no other proto attribute",
            ));
        }
        return Ok(Shape::Skip);
    }
    if let Some(s) = a.oneof {
        if let Some((_, span)) = a.tag {
            return Err(syn::Error::new(
                span,
                "a #[proto(oneof)] field takes no tag; the tags are on the oneof's variants",
            ));
        }
        if a.kind.is_some() || a.extra_except("oneof").is_some() {
            return Err(syn::Error::new(
                s,
                "#[proto(oneof)] takes no other proto attribute",
            ));
        }
        return match unwrap_one(ty, "Option") {
            Some(inner) => Ok(Shape::Oneof(Box::new(inner.clone()))),
            None => Err(syn::Error::new(
                ty.span(),
                "a #[proto(oneof)] field must be `Option<TheOneofEnum>`",
            )),
        };
    }
    if a.tag.is_none() {
        return Err(syn::Error::new(
            span,
            "missing #[proto(tag = N)]; mark fields that are not encoded with #[proto(skip)]",
        ));
    }
    let packed = a.packed;
    let reject_packed = |what: &str| -> syn::Result<()> {
        match packed {
            Some((_, s)) => Err(syn::Error::new(
                s,
                format!("`packed` applies only to repeated fields, not {what}"),
            )),
            None => Ok(()),
        }
    };

    if a.map.is_some() || is_map_type(ty) {
        let Some((_, args)) = last_segment(ty).filter(|_| is_map_type(ty)) else {
            return Err(syn::Error::new(
                ty.span(),
                "a map field must be a `HashMap<K, V>` or `BTreeMap<K, V>`",
            ));
        };
        reject_packed("maps")?;
        if let Some((_, s)) = a.kind {
            return Err(syn::Error::new(
                s,
                "a map field gives its types as `map(key = \"...\", value = \"...\")`",
            ));
        }
        if let Some(s) = a.required {
            return Err(syn::Error::new(s, "a map field cannot be `required`"));
        }
        let (key, value) = match a.map {
            Some((k, v, _)) => (k, v),
            None => (None, None),
        };
        let key = match key {
            Some(k) => k,
            None => infer_kind(args[0])?,
        };
        let value = match value {
            Some(v) => v,
            None => infer_kind(args[1])?,
        };
        if !key.map_key() {
            return Err(syn::Error::new(
                ty.span(),
                "a map key must be an integer, bool or string type",
            ));
        }
        return Ok(Shape::Map(key, value));
    }

    if let Some(inner) = unwrap_one(ty, "Option") {
        reject_packed("optional fields")?;
        if let Some(s) = a.required {
            return Err(syn::Error::new(
                s,
                "a `required` field is not an `Option`; `Option` already means optional",
            ));
        }
        return Ok(Shape::Optional(element_kind(a, inner)?));
    }

    if let Some(inner) = unwrap_one(ty, "Vec")
        .filter(|_| !is_bytes(ty) || a.kind.is_some_and(|(k, _)| k != Kind::Bytes))
    {
        if let Some(s) = a.required {
            return Err(syn::Error::new(s, "a repeated field cannot be `required`"));
        }
        let kind = element_kind(a, inner)?;
        let packed = match packed {
            Some((true, s)) if !kind.packable() => {
                return Err(syn::Error::new(
                    s,
                    "only repeated scalar numeric, bool and enum fields can be packed",
                ))
            }
            Some((p, _)) => p,
            None => kind.packable(),
        };
        return Ok(Shape::Repeated(kind, packed));
    }

    reject_packed("singular fields")?;
    let kind = element_kind(a, ty)?;
    Ok(if a.required.is_some() {
        Shape::Required(kind)
    } else {
        Shape::Implicit(kind)
    })
}

fn reject_generics(input: &DeriveInput, derive: &str) -> syn::Result<()> {
    if input.generics.params.is_empty() && input.generics.where_clause.is_none() {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(
            &input.generics,
            format!("#[derive({derive})] does not support generic parameters"),
        ))
    }
}

// ---- #[derive(Oneof)] ------------------------------------------------------

pub fn expand_oneof(input: DeriveInput) -> syn::Result<TokenStream> {
    no_container_attrs(&input.attrs, "Oneof")?;
    reject_generics(&input, "Oneof")?;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(Oneof)] needs an enum with one newtype variant per field",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "a oneof needs at least one variant",
        ));
    }
    let ident = &input.ident;
    let p = private();
    let mut tags = Vec::new();
    let mut seen: HashMap<u32, String> = HashMap::new();
    let mut encode = Vec::new();
    let mut len = Vec::new();
    let mut len_cached = Vec::new();
    let mut arms = Vec::new();
    let mut forgets = Vec::new();
    for v in &data.variants {
        let vname = &v.ident;
        let ty = match &v.fields {
            Fields::Unnamed(u) if u.unnamed.len() == 1 => &u.unnamed[0].ty,
            _ => return Err(syn::Error::new_spanned(
                v,
                "a oneof variant must be a newtype carrying the field value, like `Name(String)`",
            )),
        };
        if !v
            .fields
            .iter()
            .next()
            .is_some_and(|f| f.attrs.iter().all(|a| !a.path().is_ident("proto")))
        {
            return Err(syn::Error::new_spanned(
                v,
                "put #[proto(...)] on the variant, not on its field",
            ));
        }
        let a = field_attrs(&v.attrs)?;
        if let Some((span, key)) = a.extra_except("") {
            return Err(syn::Error::new(
                span,
                format!(
                    "`{key}` is not allowed on a oneof variant; only `tag` and a protobuf type are"
                ),
            ));
        }
        let Some((tag, tag_span)) = a.tag else {
            return Err(syn::Error::new_spanned(
                v,
                "missing #[proto(tag = N)] on the oneof variant",
            ));
        };
        if let Some(prev) = seen.insert(tag, vname.to_string()) {
            return Err(syn::Error::new(
                tag_span,
                format!("field number {tag} is already used by `{prev}`"),
            ));
        }
        if unwrap_one(ty, "Option").is_some()
            || (unwrap_one(ty, "Vec").is_some() && !is_bytes(ty))
            || is_map_type(ty)
        {
            return Err(syn::Error::new(
                ty.span(),
                "a oneof variant holds a single value (no `Option`, `Vec` or map)",
            ));
        }
        let element = element_kind(&a, ty)?;
        if element == Kind::Msg {
            forgets.push(quote! { #ident::#vname(__v) => #p::forget_message(__v, __r), });
        }
        let kind = marker(element);
        tags.push(tag);
        encode.push(
            quote! { #ident::#vname(__v) => #p::encode_always::<#kind, _>(#tag, __v, __buf, __c), },
        );
        len.push(quote! { #ident::#vname(__v) => #p::len_always::<#kind, _>(#tag, __v), });
        len_cached.push(
            quote! { #ident::#vname(__v) => #p::len_always_cached::<#kind, _>(#tag, __v, __c), },
        );
        // A new variant is created in place, so each later occurrence of it
        // merges into the same value (where its required fields are tracked).
        arms.push(quote! {
            #tag => {
                if __wt != #p::wire_type_of::<#kind, #ty>() {
                    return ::core::result::Result::Ok(false);
                }
                if !::core::matches!(__slot, ::core::option::Option::Some(#ident::#vname(_))) {
                    #p::oneof_replace::<Self, #kind, #ty>(__slot, __r)?;
                    *__slot = ::core::option::Option::Some(
                        #ident::#vname(::core::default::Default::default()),
                    );
                }
                match __slot {
                    ::core::option::Option::Some(#ident::#vname(__v)) => {
                        #p::merge_value::<#kind, _>(__wt, __v, __r)
                    }
                    _ => ::core::result::Result::Ok(false),
                }
            }
        });
    }
    // The variant a new one replaces is dropped: forget what it owns.
    let forget_fn = if forgets.is_empty() {
        quote! {}
    } else {
        let rest = if forgets.len() < tags.len() {
            quote! { _ => {} }
        } else {
            quote! {}
        };
        quote! {
            fn __forget_required(&self, __r: &mut #p::Reader<'_>) {
                match self {
                    #(#forgets)*
                    #rest
                }
            }
        }
    };
    Ok(quote! {
        impl #p::Oneof for #ident {
            const TAGS: &'static [u32] = &[#(#tags),*];

            fn encode_raw(&self, __buf: &mut ::std::vec::Vec<u8>) {
                let mut __c = #p::SizeCache::new();
                <Self as #p::Oneof>::__encoded_len_cached(self, &mut __c);
                <Self as #p::Oneof>::__encode_cached(self, __buf, &mut __c);
            }

            fn encoded_len(&self) -> usize {
                match self { #(#len)* }
            }

            fn __encoded_len_cached(&self, __c: &mut #p::SizeCache) -> usize {
                match self { #(#len_cached)* }
            }

            fn __encode_cached(&self, __buf: &mut ::std::vec::Vec<u8>, __c: &mut #p::SizeCache) {
                match self { #(#encode)* }
            }

            fn merge_field(
                __slot: &mut ::core::option::Option<Self>,
                __tag: u32,
                __wt: #p::WireType,
                __r: &mut #p::Reader<'_>,
            ) -> ::core::result::Result<bool, #p::ProtoError> {
                match __tag {
                    #(#arms)*
                    _ => ::core::result::Result::Ok(false),
                }
            }

            #forget_fn
        }
    })
}

// ---- #[derive(Enumeration)] ------------------------------------------------

pub fn expand_enumeration(input: DeriveInput) -> syn::Result<TokenStream> {
    no_container_attrs(&input.attrs, "Enumeration")?;
    reject_generics(&input, "Enumeration")?;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(Enumeration)] needs a fieldless enum",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "a protobuf enum needs at least one value",
        ));
    }
    let ident = &input.ident;
    let mut names = HashSet::new();
    let mut from_i32 = Vec::new();
    let mut to_name = Vec::new();
    let mut from_name = Vec::new();
    let mut fits = Vec::new();
    for v in &data.variants {
        if !matches!(v.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                v,
                "a protobuf enum value carries no data; use #[derive(Oneof)] for alternatives with values",
            ));
        }
        let a = enum_variant_attrs(&v.attrs)?;
        let vname = &v.ident;
        let name = a
            .name
            .as_ref()
            .map_or_else(|| vname.unraw().to_string(), syn::LitStr::value);
        if !names.insert(name.clone()) {
            let span = a
                .name
                .as_ref()
                .map_or_else(|| vname.span(), syn::LitStr::span);
            return Err(syn::Error::new(
                span,
                format!("duplicate enum value name {name:?}"),
            ));
        }
        let msg = format!(
            "the value of `{ident}::{}` does not fit in i32, the type of protobuf enum numbers",
            vname.unraw()
        );
        fits.push(quote! {
            const _: () = ::core::assert!(
                ::ferroly::proto::__private::fits_i32(#ident::#vname as i128),
                #msg
            );
        });
        from_i32.push(quote! {
            if __value == #ident::#vname as i32 {
                return ::core::option::Option::Some(#ident::#vname);
            }
        });
        to_name.push(quote! { #ident::#vname => #name, });
        from_name.push(quote! { #name => ::core::option::Option::Some(#ident::#vname), });
    }
    Ok(quote! {
        #(#fits)*

        impl ::ferroly::proto::Enumeration for #ident {
            fn to_i32(self) -> i32 {
                self as i32
            }

            fn from_i32(__value: i32) -> ::core::option::Option<Self> {
                #(#from_i32)*
                ::core::option::Option::None
            }

            fn proto_name(self) -> &'static str {
                match self { #(#to_name)* }
            }

            fn from_proto_name(__name: &str) -> ::core::option::Option<Self> {
                match __name {
                    #(#from_name)*
                    _ => ::core::option::Option::None,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn generated(r: syn::Result<TokenStream>) -> String {
        r.unwrap().to_string()
    }

    fn message(r: syn::Result<TokenStream>) -> String {
        match r {
            Ok(t) => panic!("expected an error, got {t}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn shapes_pick_the_right_helpers() {
        let out = generated(expand_message(parse_quote! {
            struct S {
                #[proto(tag = 1)] a: i32,
                #[proto(tag = 2, sint64)] b: i64,
                #[proto(tag = 3)] c: Option<String>,
                #[proto(tag = 4)] d: Vec<u32>,
                #[proto(tag = 5, packed = false)] e: Vec<i32>,
                #[proto(tag = 6)] f: Vec<String>,
                #[proto(tag = 7)] g: Vec<u8>,
                #[proto(tag = 8, map(key = "string", value = "sint32"))] h: BTreeMap<String, i32>,
                #[proto(tag = 9)] i: Option<Box<S>>,
                #[proto(tag = 10)] j: EnumValue<E>,
                #[proto(tag = 11, required)] k: u64,
                #[proto(oneof)] l: Option<O>,
                #[proto(unknown)] u: UnknownFields,
                #[proto(skip)] cache: u8,
            }
        }));
        let has = |s: &str| assert!(out.contains(s), "missing {s:?} in {out}");
        has("encode_implicit :: < :: ferroly :: proto :: __private :: Int32 , _ > (1u32");
        has("encode_implicit :: < :: ferroly :: proto :: __private :: SInt64 , _ > (2u32");
        has("encode_optional :: < :: ferroly :: proto :: __private :: Str , _ > (3u32");
        has("encode_repeated :: < :: ferroly :: proto :: __private :: UInt32 , _ > (4u32 , & self . d , true");
        has("encode_repeated :: < :: ferroly :: proto :: __private :: Int32 , _ > (5u32 , & self . e , false");
        has("encode_repeated :: < :: ferroly :: proto :: __private :: Str , _ > (6u32 , & self . f , false");
        has("encode_implicit :: < :: ferroly :: proto :: __private :: Bytes , _ > (7u32");
        has("encode_map :: < :: ferroly :: proto :: __private :: Str , :: ferroly :: proto :: __private :: SInt32 , _ > (8u32");
        has("encode_optional :: < :: ferroly :: proto :: __private :: Msg , _ > (9u32");
        has("encode_implicit :: < :: ferroly :: proto :: __private :: Enum , _ > (10u32");
        has("encode_always :: < :: ferroly :: proto :: __private :: UInt64 , _ > (11u32");
        has("\"S\" , 0 | if __seen_10 { 0 } else { 1u64 } , & [\"k\"] ,");
        has("merge_oneof (& mut self . l");
        has("UnknownFields :: merge_field (& mut self . u");
        has("note_required");
        has("< O as :: ferroly :: proto :: __private :: Oneof > :: TAGS");
        assert!(!out.contains("self . cache"), "{out}");
    }

    #[test]
    fn without_unknown_fields_they_are_skipped() {
        let out = generated(expand_message(
            parse_quote! { struct S { #[proto(tag = 1)] a: i32 } },
        ));
        assert!(out.contains("Reader :: skip_field (__r"), "{out}");
        assert!(!out.contains("note_required"), "{out}");
        let empty = generated(expand_message(parse_quote! { struct S {} }));
        assert!(empty.contains("let __used = false"), "{empty}");
    }

    #[test]
    fn message_errors() {
        let dup = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1)] a: i32, #[proto(tag = 1)] b: i32 }
        }));
        assert_eq!(dup, "field number 1 is already used by `a`");
        let missing = message(expand_message(parse_quote! { struct S { a: i32 } }));
        assert!(
            missing.starts_with("missing #[proto(tag = N)]"),
            "{missing}"
        );
        let small = message(expand_message(
            parse_quote! { struct S { #[proto(tag = 1)] a: u8 } },
        ));
        assert!(small.starts_with("protobuf has no `u8` type"), "{small}");
        let packed_str = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1, packed)] a: Vec<String> }
        }));
        assert_eq!(
            packed_str,
            "only repeated scalar numeric, bool and enum fields can be packed"
        );
        let packed_one = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1, packed)] a: i32 }
        }));
        assert_eq!(
            packed_one,
            "`packed` applies only to repeated fields, not singular fields"
        );
        let oneof = message(expand_message(
            parse_quote! { struct S { #[proto(oneof)] a: O } },
        ));
        assert_eq!(
            oneof,
            "a #[proto(oneof)] field must be `Option<TheOneofEnum>`"
        );
        let oneof_tag = message(expand_message(parse_quote! {
            struct S { #[proto(oneof, tag = 3)] a: Option<O> }
        }));
        assert!(
            oneof_tag.starts_with("a #[proto(oneof)] field takes no tag"),
            "{oneof_tag}"
        );
        let req = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1, required)] a: Option<i32> }
        }));
        assert!(
            req.starts_with("a `required` field is not an `Option`"),
            "{req}"
        );
        let two_unknown = message(expand_message(parse_quote! {
            struct S { #[proto(unknown)] a: UnknownFields, #[proto(unknown)] b: UnknownFields }
        }));
        assert_eq!(
            two_unknown,
            "only one field may be #[proto(unknown)]; `a` already is"
        );
        let unknown_tag = message(expand_message(parse_quote! {
            struct S { #[proto(unknown, tag = 2)] a: UnknownFields }
        }));
        assert_eq!(
            unknown_tag,
            "#[proto(unknown)] takes no other proto attribute"
        );
        let map = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1, map(key = "double", value = "int32"))] a: HashMap<f64, i32> }
        }));
        assert_eq!(map, "a map key must be an integer, bool or string type");
        let not_map = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1, map)] a: Vec<i32> }
        }));
        assert_eq!(
            not_map,
            "a map field must be a `HashMap<K, V>` or `BTreeMap<K, V>`"
        );
        let tuple = message(expand_message(parse_quote! { struct S(i32); }));
        assert_eq!(tuple, "#[derive(Message)] needs a struct with named fields");
        let generic = message(expand_message(
            parse_quote! { struct S<T> { #[proto(tag = 1)] a: T } },
        ));
        assert_eq!(
            generic,
            "#[derive(Message)] does not support generic parameters"
        );
        let en = message(expand_message(parse_quote! { enum E { A } }));
        assert!(
            en.starts_with("#[derive(Message)] supports only structs"),
            "{en}"
        );
        let reference = message(expand_message(parse_quote! {
            struct S { #[proto(tag = 1)] a: &'static str }
        }));
        assert!(
            reference.starts_with("protobuf fields must be owned"),
            "{reference}"
        );
    }

    #[test]
    fn oneof_expansion_and_errors() {
        let out = generated(expand_oneof(parse_quote! {
            enum O { #[proto(tag = 4)] Name(String), #[proto(tag = 9, sint32)] Id(i32) }
        }));
        assert!(
            out.contains("const TAGS : & 'static [u32] = & [4u32 , 9u32]"),
            "{out}"
        );
        assert!(
            out.contains(
                "encode_always :: < :: ferroly :: proto :: __private :: SInt32 , _ > (9u32"
            ),
            "{out}"
        );

        let dup = message(expand_oneof(parse_quote! {
            enum O { #[proto(tag = 4)] A(String), #[proto(tag = 4)] B(i32) }
        }));
        assert_eq!(dup, "field number 4 is already used by `A`");
        let unit = message(expand_oneof(
            parse_quote! { enum O { #[proto(tag = 1)] A } },
        ));
        assert!(
            unit.starts_with("a oneof variant must be a newtype"),
            "{unit}"
        );
        let no_tag = message(expand_oneof(parse_quote! { enum O { A(i32) } }));
        assert_eq!(no_tag, "missing #[proto(tag = N)] on the oneof variant");
        let packed = message(expand_oneof(
            parse_quote! { enum O { #[proto(tag = 1, packed)] A(i32) } },
        ));
        assert!(
            packed.starts_with("`packed` is not allowed on a oneof variant"),
            "{packed}"
        );
        let repeated = message(expand_oneof(
            parse_quote! { enum O { #[proto(tag = 1)] A(Vec<i32>) } },
        ));
        assert!(
            repeated.starts_with("a oneof variant holds a single value"),
            "{repeated}"
        );
        let reserved = message(expand_oneof(
            parse_quote! { enum O { #[proto(tag = 19000)] A(i32) } },
        ));
        assert!(reserved.contains("reserved"), "{reserved}");
        let empty = message(expand_oneof(parse_quote! { enum O {} }));
        assert_eq!(empty, "a oneof needs at least one variant");
        let s = message(expand_oneof(parse_quote! { struct O; }));
        assert!(s.starts_with("#[derive(Oneof)] needs an enum"), "{s}");
    }

    #[test]
    fn enumeration_expansion_and_errors() {
        let out = generated(expand_enumeration(parse_quote! {
            enum Color { #[proto(name = "COLOR_RED")] Red = 0, Green = 1 }
        }));
        assert!(out.contains("Color :: Red => \"COLOR_RED\""), "{out}");
        assert!(
            out.contains("\"Green\" => :: core :: option :: Option :: Some (Color :: Green)"),
            "{out}"
        );
        assert!(out.contains("self as i32"), "{out}");
        assert!(out.contains("fits_i32 (Color :: Red as i128)"), "{out}");

        let data = message(expand_enumeration(parse_quote! { enum E { A(i32) } }));
        assert!(
            data.starts_with("a protobuf enum value carries no data"),
            "{data}"
        );
        let dup = message(expand_enumeration(parse_quote! {
            enum E { #[proto(name = "X")] A, #[proto(name = "X")] B }
        }));
        assert_eq!(dup, "duplicate enum value name \"X\"");
        let empty = message(expand_enumeration(parse_quote! { enum E {} }));
        assert_eq!(empty, "a protobuf enum needs at least one value");
        let container = message(expand_enumeration(
            parse_quote! { #[proto(open)] enum E { A } },
        ));
        assert_eq!(
            container,
            "#[derive(Enumeration)] takes no #[proto(...)] attribute on the type"
        );
    }
}
