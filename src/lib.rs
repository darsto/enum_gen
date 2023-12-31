/* SPDX-License-Identifier: MIT
 * Copyright(c) 2023 Darek Stojaczyk
 */

#![doc = include_str!("../README.md")]

extern crate proc_macro;
extern crate syn;
#[macro_use]
extern crate quote;

use lazy_static::lazy_static;
use proc_macro2::{Group, Ident, Literal, Span, TokenStream, TokenTree};
use quote::{ToTokens, TokenStreamExt};
use std::{collections::HashMap, str::FromStr, sync::Mutex};
use syn::{parse_quote, Attribute, Field, Meta, Variant};

#[allow(clippy::from_str_radix_10)]
fn parse_int(str: &str) -> Result<usize, std::num::ParseIntError> {
    if let Some(str) = str.strip_prefix("0x") {
        usize::from_str_radix(str, 16)
    } else {
        usize::from_str_radix(str, 10)
    }
}

// State shared between #[enum_gen] and #[enum_gen_match] calls
struct GlobalState {
    enums: HashMap<String, EnumRef>,
    pending_match_fns: HashMap<String, Vec<EnumMatchFn>>,
}

impl GlobalState {
    pub fn new() -> Self {
        GlobalState {
            enums: HashMap::new(),
            pending_match_fns: HashMap::new(),
        }
    }
}

lazy_static! {
    static ref CACHE: Mutex<GlobalState> = Mutex::new(GlobalState::new());
}

/// Saved data about the generated (final) enum
#[derive(Debug, Clone)]
struct EnumRef {
    name: String,
    variants: Vec<EnumVariantRef>,
}

/// Enum variant in the generated (final) enum
#[derive(Debug, Clone)]
struct EnumVariantRef {
    id: EnumVariantId,
    name: String,
}

/// Enum variant extracted from the original enum.
struct EnumVariant {
    id: EnumVariantId,
    name: Ident,
    fields: Vec<Field>,
}

/// ToTokens into the final (generated) enum.
impl ToTokens for EnumVariant {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let name = &self.name;
        tokens.extend(quote! {
            #name (#name)
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum EnumVariantId {
    /// Regular match case
    Val(usize),
    /// Default match case
    Default,
}

impl ToTokens for EnumVariantId {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match self {
            EnumVariantId::Val(id) => tokens.append(Literal::usize_unsuffixed(*id)),
            EnumVariantId::Default => tokens.append(Ident::new("_", Span::call_site())),
        }
    }
}

#[derive(Clone, Copy)]
enum EnumMatchType {
    /// Match by ID
    Id,
    /// Match by &self
    Variant,
}

impl ToTokens for EnumMatchType {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match &self {
            EnumMatchType::Id => {
                tokens.append(Ident::new("id", Span::call_site()));
            }
            EnumMatchType::Variant => {
                tokens.append(Ident::new("self", Span::call_site()));
            }
        };
    }
}

struct EnumVariantMatch<'a> {
    match_by: EnumMatchType,
    enum_name: &'a Ident,
    variant: &'a EnumVariantRef,
    case: &'a TokenStream,
}

impl<'a> ToTokens for EnumVariantMatch<'a> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let enum_name = self.enum_name;
        let name = Ident::new(&self.variant.name, Span::call_site());
        let id = &self.variant.id;
        let case = &self.case;

        tokens.extend({
            match self.match_by {
                EnumMatchType::Id => quote! {
                    #id => {
                        use #name as EnumStructType;
                        use #enum_name::#name as EnumVariantType;
                        #case
                    },
                },
                EnumMatchType::Variant => quote! {
                    #enum_name::#name(inner) => {
                        use #name as EnumStructType;
                        use #enum_name::#name as EnumVariantType;
                        #case
                    },
                },
            }
        });
    }
}

struct EnumVariantMatcher<'a> {
    match_by: EnumMatchType,
    enum_name: &'a Ident,
    variants: &'a Vec<EnumVariantRef>,
    case: TokenStream,
}

impl<'a> ToTokens for EnumVariantMatcher<'a> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let mut default_variants = self
            .variants
            .iter()
            .filter(|v| matches!(v.id, EnumVariantId::Default));

        // Print some pretty messages for otherwise hard-to-debug problems
        let default_variant = default_variants.next().expect(
            "Default variant must be defined. E.g:\n\
                    \t#[attr(ID = _)]\n\
                    Unknown",
        );
        if default_variants.next().is_some() {
            panic!("Only one variant with default ID (_) can be defined.");
        }

        for variant in self.variants {
            if let EnumVariantId::Default = variant.id {
                continue;
            }

            let m = EnumVariantMatch {
                match_by: self.match_by,
                enum_name: self.enum_name,
                variant,
                case: &self.case,
            };
            m.to_tokens(tokens);
        }

        let m = EnumVariantMatch {
            match_by: self.match_by,
            enum_name: self.enum_name,
            variant: default_variant,
            case: &self.case,
        };
        m.to_tokens(tokens);
    }
}

impl TryFrom<Variant> for EnumVariant {
    type Error = ();

    fn try_from(variant: Variant) -> Result<Self, Self::Error> {
        let name = variant.ident.clone();
        let mut attrs = variant.attrs;
        let fields = variant.fields.into_iter().collect();

        // Parse variant's attributes
        let internal_attrs_idx = attrs
            .iter()
            .position(|a| match &a.meta {
                Meta::List(list) => {
                    if let Some(ident) = list.path.get_ident() {
                        *ident == "attr"
                    } else {
                        false
                    }
                }
                _ => false,
            })
            .expect("Each enum variant needs to be have an attr attribute. #[attr(ID = 0x42)]");
        let internal_attrs = attrs.remove(internal_attrs_idx);
        let Meta::List(internal_attrs) = internal_attrs.meta else {
            panic!("`attr` attribute needs to describe a list. E.g: #[attr(ID = 0x42)]");
        };

        let mut tokens_iter = internal_attrs.tokens.into_iter();
        let mut id: Option<EnumVariantId> = None;

        loop {
            let Some(token) = tokens_iter.next() else {
                break;
            };

            let TokenTree::Ident(ident) = token else {
                continue;
            };

            match ident.to_string().as_str() {
                "ID" => {
                    expect_punct_token(tokens_iter.next());
                    let value = tokens_iter
                        .next()
                        .expect("Unknown attr syntax. Expected `#[attr(ID = 0x42)]`");

                    id = Some(match &value {
                        TokenTree::Ident(ident) => {
                            if *ident == "_" {
                                EnumVariantId::Default
                            } else {
                                let str = value.to_string();
                                EnumVariantId::Val(
                                    parse_int(&str)
                                        .expect("Invalid ID attribute. Expected a number"),
                                )
                            }
                        }
                        _ => {
                            let str = value.to_string();
                            EnumVariantId::Val(
                                parse_int(&str).expect("Invalid ID attribute. Expected a number"),
                            )
                        }
                    });
                }
                name => {
                    panic!("Unknown attribute `{name}`")
                }
            }
        }

        if attrs.len() > 1 {
            panic!("Currently additional variant attributes are not supported");
        }

        let id = id.expect("Missing ID identifier.Each enum variant needs to be assigned an ID. #[attr(ID = 0x42)]");
        Ok(EnumVariant { id, name, fields })
    }
}

/// Argument to #[enum_gen(...)] macro that will be passed 1:1
/// to generated structs. Can be derive(Debug) or just e.g. no_mangle,
/// so the group is optional.
struct EnumAttribute {
    ident: Ident,
    group: Option<Group>,
}

fn expect_punct_token(token: Option<TokenTree>) {
    match token {
        Some(TokenTree::Punct(punct)) => {
            if punct.as_char() != '=' {
                panic!("Unknown parse_fn syntax. Expected `parse_fn = my_fn`");
            }
        }
        _ => panic!("parse_fn param should be followed by `= my_fn`. E.g. `parse_fn = my_fn`"),
    }
}

/// All arguments passed to #[enum_gen(...)] macro
struct EnumGenArgs {
    struct_attrs: Vec<EnumAttribute>,
}

/// Organize enum_gen macro arguments into a struct. Note that only a small
/// part of arguments are getting parsed, the rest is technically invalid syntax
/// until it's wrapped in #[] and used to decorate a struct.
/// For that reason, we don't try to parse it yet.
impl TryFrom<TokenStream> for EnumGenArgs {
    type Error = ();

    fn try_from(tokens: TokenStream) -> Result<Self, Self::Error> {
        let mut tokens_iter = tokens.into_iter();
        let mut attrs: Vec<EnumAttribute> = Vec::new();

        loop {
            // The macro argument can be derive(Debug) - with brackets,
            // or without them - e.g. no_mangle
            let Some(ident) = tokens_iter.next() else {
                break;
            };
            let TokenTree::Ident(ident) = ident else {
                panic!(
                    "Malformed #[enum_gen(...)] syntax. Expected Ident-s. Example: \n\
                        \t#[enum_gen(derive(Debug, Default), repr(C, packed))]"
                );
            };

            let group = match tokens_iter.next() {
                Some(TokenTree::Group(group)) => {
                    let group = group.clone();
                    // skip the following comma (or nothing)
                    tokens_iter.next();
                    Some(group)
                }
                _ => {
                    // we consumed a comma (or nothing)
                    None
                }
            };

            attrs.push(EnumAttribute { ident, group });
        }

        Ok(EnumGenArgs {
            struct_attrs: attrs,
        })
    }
}

/// Procedural macro to generate structures from enum variants. The variants can
/// be assigned a numerical ID, which can be automatically matched by functions
/// attributed with #[`enum_gen_match_id`].
///
/// The enum variants must have either named data (struct like) or no data at all.
///
/// # Examples
///
/// ```rust
/// use enum_gen::*;
///
/// #[enum_gen(derive(Debug, Default), repr(C, packed))]
/// pub enum Payload {
///     #[attr(ID = 0x2b)]
///     Hello { a: u8, b: u64, c: u64, d: u8 },
///     #[attr(ID = 0x42)]
///     Goodbye { a: u8, e: u8 },
///     #[attr(ID = _)]
///     Invalid,
/// }
/// ```
///
/// The `#[attr(ID = ...)]` is a mandatory attribute for every variant. The IDs must
/// be unique, and there must be exactly one `#[attr(ID = _)]` variant which corresponds
/// to the "default" case.
///
/// This will generate the following code:
/// ```rust
/// pub enum Payload {
///     Hello(Hello),
///     Goodbye(Goodbye),
///     Invalid(Invalid),
/// }
/// #[derive(Debug, Default)]
/// #[repr(C, packed)]
///     pub struct Hello {
///     pub a: u8,
///     pub b: u64,
///     pub c: u64,
///     pub d: u8,
/// }
/// impl Hello {
///     pub const ID: usize = 43usize;
/// }
/// #[derive(Debug, Default)]
/// #[repr(C, packed)]
///     pub struct Goodbye {
///     pub a: u8,
///     pub e: u8,
/// }
/// impl Goodbye {
///     pub const ID: usize = 66usize;
/// }
/// #[derive(Debug, Default)]
/// #[repr(C, packed)]
/// pub struct Invalid {}
/// ```
///
/// The IDs aren't particularly useful on their own, but can be grealy leveraged
/// with another #[enum_gen_match_id] proc macro.  See its documentation for details.
#[proc_macro_attribute]
pub fn enum_gen(
    attr: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let attr: TokenStream = attr.into();
    let args: EnumGenArgs = attr.try_into().unwrap();

    let ast = syn::parse_macro_input!(input as syn::DeriveInput);
    let enum_vis = ast.vis;
    let enum_attrs = ast.attrs;
    let enum_ident = ast.ident;

    // Extract the enum variants
    let variants: Vec<syn::Variant> = match ast.data {
        syn::Data::Enum(data_enum) => data_enum.variants.into_iter().collect(),
        _ => panic!("#[derive(ZerocopyEnum)] expects enum"),
    };

    // Organize info about variants
    let variants = variants
        .into_iter()
        .map(|mut variant| {
            // set visibility to each field
            for f in &mut variant.fields {
                f.vis = enum_vis.clone();
            }
            EnumVariant::try_from(variant)
        })
        .collect::<Result<Vec<EnumVariant>, _>>()
        .unwrap();

    // Re-create the original enum, now referencing soon-to-be-created structs
    let mut ret_stream = quote! {
        #(#enum_attrs)*
        #enum_vis enum #enum_ident {
            #(#variants),*
        }
    };

    // Generate struct attributes (this is the first time their syntax is checked)
    let attributes: Vec<Attribute> = args
        .struct_attrs
        .into_iter()
        .map(|t| {
            let ident = t.ident;
            match t.group {
                None => parse_quote!(
                    #[#ident]
                ),
                Some(group) => parse_quote!(
                    #[#ident #group]
                ),
            }
        })
        .collect();

    // For each EnumVariant generate a struct and its impl
    for v in &variants {
        let EnumVariant { id, name, fields } = &v;

        ret_stream.extend(quote! {
            #(#attributes)*
            #enum_vis struct #name {
                #(#fields,)*
            }
        });

        if let EnumVariantId::Val(id) = id {
            ret_stream.extend(quote! {
                impl #name {
                    pub const ID: usize = #id;
                }
            });
        }
    }

    // Lastly, save a global ref to this enum
    if let Ok(mut cache) = CACHE.lock() {
        let prev_val = cache.enums.insert(
            enum_ident.to_string(),
            EnumRef {
                name: enum_ident.to_string(),
                variants: variants
                    .iter()
                    .map(|v| EnumVariantRef {
                        id: v.id,
                        name: v.name.to_string(),
                    })
                    .collect(),
            },
        );

        if prev_val.is_some() {
            // TODO Lift this limitation after Span::source_file() is implemented
            // https://github.com/rust-lang/rust/issues/54725
            // We would put source file into the hashmap id, although ideally we would
            // like caller's module instead.
            drop(cache);
            panic!("Enum name conflict! Consider using a different unique name, then create an alias to desired name");
        } else if let Some(pending_match_fns) =
            cache.pending_match_fns.remove(&enum_ident.to_string())
        {
            let enumref = cache.enums.get(&enum_ident.to_string()).unwrap();

            for pending in pending_match_fns {
                enum_gen_match_with_enum(enumref, &pending);
            }
        }
    } else {
        panic!("Internal chache is corrupted. Fix other problems and restart the compilation")
    }

    ret_stream.into()
}

/// Parsed #[enum_gen_match[_id](...)]. In case the enum definition is not available,
/// and the impl needs to be stored in the global state.
struct EnumMatchFn {
    match_by: EnumMatchType,
    fn_str: String,
}

fn enum_gen_match_with_enum(
    enumref: &EnumRef,
    enum_match_fn: &EnumMatchFn,
) -> proc_macro2::TokenStream {
    let enum_name = Ident::new(&enumref.name, Span::call_site());
    let mut tokens: Vec<TokenTree> = proc_macro2::TokenStream::from_str(&enum_match_fn.fn_str)
        .unwrap()
        .into_iter()
        .collect();

    // We're expecting a function, so last Group should be the function body
    let body = tokens
        .pop()
        .and_then(|t| {
            if let TokenTree::Group(g) = t {
                Some(g.stream())
            } else {
                None
            }
        })
        .expect("#[enum_gen_match[_id](...)] has to be used on function definition");

    let variant_matcher = EnumVariantMatcher {
        match_by: enum_match_fn.match_by,
        enum_name: &enum_name,
        variants: &enumref.variants,
        case: body,
    };

    let match_by = &variant_matcher.match_by;
    quote!(
        #(#tokens)* {
            match #match_by {
                #variant_matcher
            }
        }
    )
}

fn process_match_fn(enum_name: String, enum_match_fn: EnumMatchFn) -> proc_macro::TokenStream {
    if enum_name.is_empty() {
        panic!("Argument is missing. Expected `#[enum_gen_match(MyEnumName)]`");
    }

    let mut cache = CACHE.lock().unwrap();
    if let Some(enumref) = cache.enums.get(&enum_name) {
        enum_gen_match_with_enum(enumref, &enum_match_fn).into()
    } else {
        // We may be called before #[enum_gen], so handle it by storing
        // this (stringified) function into cache. Unfortunately we don't
        // know if the enum exists at all. If it doesn't, this function
        // won't be ever instantiated, and won't generate any warning.
        let pending_vec = cache
            .pending_match_fns
            .entry(enum_name)
            .or_insert(Vec::new());
        pending_vec.push(enum_match_fn);
        proc_macro::TokenStream::new()
    }
}

/// Provide EnumStructType and EnumVariantType aliases to the function body,
/// which correspond to enum variant with provided `id`. The `id` is expected
/// to be one of the function parameters.
///
/// This works by replacing the function body with an `id` match expression,
/// where every match arm is filled with the original body, just preceeded with
/// different `use X as EnumStructType`. For this reason it's recommended to
/// keep the function body minimal, potentially separating the generic logic to
/// another helper function: `fn inner_logic_not_worth_duplicating<T: MyTrait>(v: &T)`.
///
/// # Examples
/// ```rust
/// use enum_gen::*;
///
/// #[enum_gen(derive(Debug, Default), repr(C, packed))]
/// pub enum Payload {
///     #[attr(ID = 0x2b)]
///     Hello { a: u8, b: u64, c: u64, d: u8 },
///     #[attr(ID = 0x42)]
///     Goodbye { a: u8, e: u8 },
///     #[attr(ID = _)]
///     Invalid,
/// }
///
/// #[enum_gen_match_id(Payload)]
/// pub fn default(id: usize) -> Payload {
///     EnumVariantType(EnumStructType::default())
/// }
/// ```
///
/// The `default` function expands to:
///
/// ```ignore
/// pub fn default(id: usize) -> Payload {
///     match id {
///         43 => {
///             use Hello as EnumStructType;
///             use Payload::Hello as EnumVariantType;
///             EnumVariantType(EnumStructType::default())
///         }
///         66 => {
///             use Goodbye as EnumStructType;
///             use Payload::Goodbye as EnumVariantType;
///             EnumVariantType(EnumStructType::default())
///         }
///         _ => {
///             use Invalid as EnumStructType;
///             use Payload::Invalid as EnumVariantType;
///             EnumVariantType(EnumStructType::default())
///         }
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn enum_gen_match_id(
    attr: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let attr: TokenStream = attr.into();
    let enum_name = attr.to_string();

    let enum_match_fn = EnumMatchFn {
        match_by: EnumMatchType::Id,
        fn_str: input.to_string(),
    };

    process_match_fn(enum_name, enum_match_fn)
}

/// Similar to #[`enum_gen_match_id`], but matches on `self` instead.
/// The inner structure of variant is available through `inner` variable.
/// This macro can be used on function with either `self`, `&self` or
/// `&mut self` parameter.
///
/// # Examples
/// ```rust
/// use enum_gen::*;
///
/// #[enum_gen(derive(Debug, Default), repr(C, packed))]
/// pub enum Payload {
///     #[attr(ID = 0x2b)]
///     Hello { a: u8, b: u64, c: u64, d: u8 },
///     #[attr(ID = 0x42)]
///     Goodbye { a: u8, e: u8 },
///     #[attr(ID = _)]
///     Invalid,
/// }
///
/// impl Payload {
///     #[enum_gen_match_self(Payload)]
///     pub fn size(&self) -> usize {
///         std::mem::size_of_val(inner)
///     }
/// }
/// ```
///
/// The `size` function expands to:
///
/// ```ignore
/// impl Payload {
///     pub fn size(&self) -> usize {
///         match &self {
///             Payload::Hello(inner) => {
///                 use Hello as EnumStructType;
///                 use Payload::Hello as EnumVariantType;
///                 std::mem::size_of_val(inner)
///             }
///             Payload::Goodbye(inner) => {
///                 use Goodbye as EnumStructType;
///                 use Payload::Goodbye as EnumVariantType;
///                 std::mem::size_of_val(inner)
///             }
///             Payload::Invalid(inner) => {
///                 use Invalid as EnumStructType;
///                 use Payload::Invalid as EnumVariantType;
///                 std::mem::size_of_val(inner)
///             }
///         }
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn enum_gen_match_self(
    attr: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let attr: TokenStream = attr.into();
    let enum_name = attr.to_string();

    let enum_match_fn = EnumMatchFn {
        match_by: EnumMatchType::Variant,
        fn_str: input.to_string(),
    };

    process_match_fn(enum_name, enum_match_fn)
}
