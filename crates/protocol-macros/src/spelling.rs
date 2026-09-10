//! `#[derive(Spelling)]` — one spelling table per type, driving every impl
//! that spells it.
//!
//! The same words used to be written up to four times per type: once in
//! `Display`, once in `FromStr`, and once each side of a hand-written serde
//! pair. Nothing made the four agree, and twice this session they did not —
//! a `Position` whose `Deserialize` read strings while its `Serialize` wrote
//! a variant index, so every `--add` failed in silence. Declared once here, a
//! mismatch is not a bug to test for; it cannot be spelled.
//!
//! `#[spell("left", "l")]` on a variant gives its spellings, canonical
//! first: the first is what `Display` writes, and every one is what
//! `FromStr` accepts. `#[spell("popup.{}")]` is the same for a variant whose
//! spelling is computed from its payload — the `{}` is the payload, so the
//! prefix and suffix are declared once and drive both halves too.
//!
//! Two rules worth stating, because both are load-bearing:
//!
//! * The literal table is matched against the *folded* input — whatever
//!   `#[spelling(fold = ...)]` normalises away — while a payload variant and
//!   any escape hatch see the original, so `popup.Clock` keeps the host's
//!   own case.
//! * Payload variants are tried in a deliberate order: the literal table,
//!   then payload variants with an affix to recognise them by, then the bare
//!   `{}` ones, which match anything and so must go last -- and last of all
//!   a `rest` variant, which claims whatever is still unspoken for. That is
//!   what lets `PressTarget` read `3` as a menu index and everything else as
//!   the name of a mirrored item.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Attribute, Data, DeriveInput, Error, Fields, Ident, LitStr, Path, Result, Token, Type};

/// One `#[spelling(...)]` argument, on the type.
enum Arg {
    /// `error = InvalidAlign`: [`std::str::FromStr::Err`]. Its presence is
    /// what asks for a `FromStr` at all — a type that only ever prints
    /// itself says nothing here.
    Error(Path),
    /// `unknown = InvalidQuery::Unknown`: what to build from the offending
    /// text when nothing matched. Defaults to the error type itself, which
    /// every one of these that is a newtype already is.
    Unknown(Path),
    /// `fold = none | lower | dashes`: how input is normalised before the
    /// literal table sees it.
    Fold(Fold),
    /// `rest = path`: `fn(&str) -> Result<Self, Err>`, tried when nothing
    /// else claimed the text. Terminal, so no "unknown" arm is generated at
    /// all — which is what lets [`crate`]'s `BoolChange` hand `on`/`!off`
    /// straight to `Boolish` rather than restating its eight spellings.
    Rest(Path),
    /// `serde`: a `Serialize`/`Deserialize` pair that writes and reads the
    /// same spellings. `serde(bool)` additionally takes a native boolean,
    /// for the values a Lua config writes as one.
    Serde { any_bool: bool },
    /// `expecting = "..."`: what the `serde(bool)` visitor says it wanted.
    Expecting(LitStr),
}

/// What the input is put through before the literal table is consulted.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Fold {
    /// Matched exactly. `Query` needs this: `BAR` is an item name, not the
    /// bar.
    #[default]
    None,
    Lower,
    /// Lower case, and `_` read as `-`, so `center_left` and `center-left`
    /// are one spelling.
    Dashes,
}

impl Parse for Arg {
    fn parse(input: ParseStream) -> Result<Self> {
        let key: Ident = input.parse()?;
        match key.to_string().as_str() {
            "error" => {
                input.parse::<Token![=]>()?;
                Ok(Self::Error(input.parse()?))
            }
            "unknown" => {
                input.parse::<Token![=]>()?;
                Ok(Self::Unknown(input.parse()?))
            }
            "rest" => {
                input.parse::<Token![=]>()?;
                Ok(Self::Rest(input.parse()?))
            }
            "expecting" => {
                input.parse::<Token![=]>()?;
                Ok(Self::Expecting(input.parse()?))
            }
            "fold" => {
                input.parse::<Token![=]>()?;
                let how: Ident = input.parse()?;
                match how.to_string().as_str() {
                    "none" => Ok(Self::Fold(Fold::None)),
                    "lower" => Ok(Self::Fold(Fold::Lower)),
                    "dashes" => Ok(Self::Fold(Fold::Dashes)),
                    _ => Err(Error::new_spanned(
                        how,
                        "expected `none`, `lower` or `dashes`",
                    )),
                }
            }
            "serde" => {
                let mut any_bool = false;
                if input.peek(syn::token::Paren) {
                    let inner;
                    syn::parenthesized!(inner in input);
                    let extra: Ident = inner.parse()?;
                    if extra != "bool" {
                        return Err(Error::new_spanned(extra, "expected `bool`"));
                    }
                    any_bool = true;
                }
                Ok(Self::Serde { any_bool })
            }
            _ => Err(Error::new_spanned(
                key,
                "expected `error`, `unknown`, `fold`, `rest`, `serde` or `expecting`",
            )),
        }
    }
}

/// One `#[spell(...)]` argument, on a variant.
enum VariantArg {
    Spelling(LitStr),
    /// `parse = path`: `fn(&str) -> Result<Option<Self>, Err>`, for the one
    /// shape a prefix and a suffix cannot express — `Query::Item`, which has
    /// to reject the queries `SketchyBar` has and coolabah does not before
    /// taking the word as an item name.
    Parse(Path),
    /// `rest = path`: `fn(&str) -> Result<Self, Err>`, and nothing after it.
    Rest(Path),
}

impl Parse for VariantArg {
    fn parse(input: ParseStream) -> Result<Self> {
        if input.peek(LitStr) {
            return Ok(Self::Spelling(input.parse()?));
        }
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        match key.to_string().as_str() {
            "parse" => Ok(Self::Parse(input.parse()?)),
            "rest" => Ok(Self::Rest(input.parse()?)),
            _ => Err(Error::new_spanned(
                key,
                "expected a spelling, `parse = ...` or `rest = ...`",
            )),
        }
    }
}

fn args<T: Parse>(attrs: &[Attribute], name: &str) -> Result<Vec<T>> {
    let mut parsed = Vec::new();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident(name)) {
        parsed.extend(attr.parse_args_with(Punctuated::<T, Token![,]>::parse_terminated)?);
    }
    Ok(parsed)
}

/// A variant's spelling: a word, or a word with the payload in it.
enum Spelling {
    /// Every literal this variant answers to, canonical first.
    Words(Vec<LitStr>),
    /// `popup.{}` — what goes before the payload and what goes after.
    // Boxed: a `syn::Type` dwarfs the word list beside it, and otherwise
    // every variant of every table would be sized for the largest one.
    Payload(Box<Payload>),
}

/// A spelling computed from what its variant carries.
struct Payload {
    prefix: String,
    suffix: String,
    ty: Type,
}

struct Variant<'a> {
    ident: &'a Ident,
    /// The pattern that names this variant's payload, if any: `(value)`,
    /// `{ .. }` or nothing.
    binding: TokenStream,
    spelling: Spelling,
    parse: Option<Path>,
    rest: Option<Path>,
}

/// Reads the `{}` out of a payload spelling.
fn split(literal: &LitStr, fields: &Fields) -> Result<Spelling> {
    let text = literal.value();
    let Some((prefix, suffix)) = text.split_once("{}") else {
        return Ok(Spelling::Words(vec![literal.clone()]));
    };
    if suffix.contains("{}") {
        return Err(Error::new_spanned(
            literal,
            "a spelling carries the payload once, not twice",
        ));
    }
    let Fields::Unnamed(unnamed) = fields else {
        return Err(Error::new_spanned(
            literal,
            "`{}` names the variant's one payload, and this variant has none",
        ));
    };
    let mut iter = unnamed.unnamed.iter();
    let (Some(field), None) = (iter.next(), iter.next()) else {
        return Err(Error::new_spanned(
            literal,
            "`{}` names the variant's one payload, and this variant has several",
        ));
    };
    Ok(Spelling::Payload(Box::new(Payload {
        prefix: prefix.to_owned(),
        suffix: suffix.to_owned(),
        ty: field.ty.clone(),
    })))
}

fn variants(input: &DeriveInput) -> Result<Vec<Variant<'_>>> {
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(
            &input.ident,
            "a spelling table is a list of variants, so this derives on an enum",
        ));
    };
    data.variants
        .iter()
        .map(|variant| {
            let mut words = Vec::new();
            let mut parse = None;
            let mut rest = None;
            for arg in args::<VariantArg>(&variant.attrs, "spell")? {
                match arg {
                    VariantArg::Spelling(word) => words.push(word),
                    VariantArg::Parse(path) => parse = Some(path),
                    VariantArg::Rest(path) => rest = Some(path),
                }
            }
            let Some(first) = words.first() else {
                return Err(Error::new_spanned(
                    &variant.ident,
                    "every variant needs a spelling: `#[spell(\"left\", \"l\")]`",
                ));
            };
            let spelling = match split(first, &variant.fields)? {
                Spelling::Words(_) => Spelling::Words(words),
                payload @ Spelling::Payload(_) => {
                    if words.len() > 1 {
                        return Err(Error::new_spanned(
                            &words[1],
                            "a payload spelling is the only one its variant has",
                        ));
                    }
                    payload
                }
            };
            let binding = match &variant.fields {
                Fields::Unit => quote!(),
                Fields::Named(_) => quote!({ .. }),
                Fields::Unnamed(_) if matches!(spelling, Spelling::Payload(_)) => {
                    quote!((value))
                }
                Fields::Unnamed(_) => quote!((..)),
            };
            Ok(Variant {
                ident: &variant.ident,
                binding,
                spelling,
                parse,
                rest,
            })
        })
        .collect()
}

/// Writes the payload between whatever surrounds it, without paying for a
/// format string where there is nothing around it to write.
fn wrap(prefix: &str, suffix: &str) -> TokenStream {
    let inner = quote!(::std::fmt::Display::fmt(value, f));
    if prefix.is_empty() && suffix.is_empty() {
        return inner;
    }
    let before = (!prefix.is_empty()).then(|| quote!(f.write_str(#prefix)?;));
    let after = (!suffix.is_empty()).then(|| quote!(f.write_str(#suffix)?;));
    quote! {{
        #before
        #inner?;
        #after
        ::std::result::Result::Ok(())
    }}
}

/// Everything the table says about itself, rather than about one variant.
#[derive(Default)]
struct Options {
    error: Option<Path>,
    unknown: Option<Path>,
    fold: Fold,
    rest: Option<Path>,
    serde: Option<bool>,
    expecting: Option<LitStr>,
}

impl Options {
    fn read(attrs: &[Attribute]) -> Result<Self> {
        let mut options = Self::default();
        for arg in args::<Arg>(attrs, "spelling")? {
            match arg {
                Arg::Error(path) => options.error = Some(path),
                Arg::Unknown(path) => options.unknown = Some(path),
                Arg::Fold(how) => options.fold = how,
                Arg::Rest(path) => options.rest = Some(path),
                Arg::Serde { any_bool } => options.serde = Some(any_bool),
                Arg::Expecting(text) => options.expecting = Some(text),
            }
        }
        Ok(options)
    }
}

/// The canonical spelling of each variant, which is the only one printed.
fn display(ty: &Ident, variants: &[Variant<'_>]) -> TokenStream {
    let arms = variants.iter().map(|variant| {
        let (name, binding) = (variant.ident, &variant.binding);
        let body = match &variant.spelling {
            Spelling::Words(words) => {
                let canonical = &words[0];
                quote!(f.write_str(#canonical))
            }
            Spelling::Payload(payload) => wrap(&payload.prefix, &payload.suffix),
        };
        quote!(Self::#name #binding => #body,)
    });
    quote! {
        #[automatically_derived]
        impl ::std::fmt::Display for #ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match self {
                    #(#arms)*
                }
            }
        }
    }
}

/// The literal table, asked first and asked of the folded text.
fn table(variants: &[Variant<'_>], fold: Fold) -> Result<Option<TokenStream>> {
    let mut asks = Vec::new();
    for variant in variants {
        let Spelling::Words(words) = &variant.spelling else {
            continue;
        };
        if !variant.binding.is_empty() {
            return Err(Error::new_spanned(
                variant.ident,
                "a variant spelled by a word alone carries nothing to read a payload from; \
                 give it a `{}` spelling, or drop `error = ...` and print it only",
            ));
        }
        let name = variant.ident;
        asks.push(quote! {
            if ::std::matches!(&*folded, #(#words)|*) {
                return ::std::result::Result::Ok(Self::#name);
            }
        });
    }
    if asks.is_empty() {
        return Ok(None);
    }
    let folding = match fold {
        Fold::None => quote!(::std::borrow::Cow::Borrowed(s)),
        Fold::Lower => quote!(::std::borrow::Cow::Owned(s.to_ascii_lowercase())),
        Fold::Dashes => quote!(::std::borrow::Cow::Owned(
            s.replace('_', "-").to_ascii_lowercase()
        )),
    };
    Ok(Some(quote! {
        let folded: ::std::borrow::Cow<'_, str> = #folding;
        #(#asks)*
    }))
}

/// One payload variant's attempt at the text, built inside out so the head of
/// the chain is a refutable pattern: an affix that is not there means "not
/// this variant", and so does a payload that will not read.
fn attempt(name: &Ident, payload: &Payload) -> TokenStream {
    let (prefix, suffix, ty) = (&payload.prefix, &payload.suffix, &payload.ty);
    let taken = quote!(return ::std::result::Result::Ok(Self::#name(value)););
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => quote! {
            if let ::std::result::Result::Ok(value) = s.parse::<#ty>() { #taken }
        },
        (false, true) => quote! {
            if let ::std::option::Option::Some(rest) = s.strip_prefix(#prefix)
                && let ::std::result::Result::Ok(value) = rest.parse::<#ty>()
            { #taken }
        },
        (true, false) => quote! {
            if let ::std::option::Option::Some(rest) = s.strip_suffix(#suffix)
                && let ::std::result::Result::Ok(value) = rest.parse::<#ty>()
            { #taken }
        },
        (false, false) => quote! {
            if let ::std::option::Option::Some(rest) = s.strip_prefix(#prefix)
                && let ::std::option::Option::Some(rest) = rest.strip_suffix(#suffix)
                && let ::std::result::Result::Ok(value) = rest.parse::<#ty>()
            { #taken }
        },
    }
}

/// Reading a spelling back: the table, then the payload variants in the one
/// order that can work, then whatever claims what is left.
fn from_str(ty: &Ident, variants: &[Variant<'_>], options: Options) -> Result<TokenStream> {
    let error = options.error.clone().expect("checked by the caller");
    let matching = table(variants, options.fold)?;
    // Affixed payload variants before bare ones: a bare `{}` matches
    // anything, so it can only ever be asked last.
    let mut affixed = Vec::new();
    let mut bare = Vec::new();
    let mut claimed = None;
    for variant in variants {
        let name = variant.ident;
        if let Some(path) = &variant.rest {
            if claimed.is_some() {
                return Err(Error::new_spanned(
                    name,
                    "only one variant can claim whatever is left",
                ));
            }
            claimed = Some(quote!(#path(s)));
        } else if let Some(path) = &variant.parse {
            bare.push(quote! {
                if let ::std::option::Option::Some(value) = #path(s)? {
                    return ::std::result::Result::Ok(value);
                }
            });
        } else if let Spelling::Payload(payload) = &variant.spelling {
            let reading = attempt(name, payload);
            if payload.prefix.is_empty() && payload.suffix.is_empty() {
                bare.push(reading);
            } else {
                affixed.push(reading);
            }
        }
    }
    let tail = claimed
        .or_else(|| options.rest.map(|path| quote!(#path(s))))
        .unwrap_or_else(|| {
            let unknown = options.unknown.unwrap_or_else(|| error.clone());
            quote!(::std::result::Result::Err(#unknown(::std::borrow::ToOwned::to_owned(s))))
        });
    Ok(quote! {
        #[automatically_derived]
        impl ::std::str::FromStr for #ty {
            type Err = #error;

            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                #matching
                #(#affixed)*
                #(#bare)*
                #tail
            }
        }
    })
}

/// The two halves of the wire form, both spelled by the impls above -- which
/// is the whole point: a `Serialize` writing one shape and a `Deserialize`
/// expecting another is the bug this derive exists to make unrepresentable.
fn serde_pair(ty: &Ident, any_bool: bool, expecting: Option<&LitStr>) -> TokenStream {
    let writer = quote! {
        /// Written as the spelling it prints, so what `--query` shows and what
        /// crosses the wire cannot come apart.
        #[automatically_derived]
        impl ::serde::Serialize for #ty {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
    };
    if !any_bool {
        return quote! {
            #writer

            /// Through [`FromStr`](std::str::FromStr) rather than `serde`'s own
            /// enum path, which would put a Rust variant name where a config's
            /// spelling belongs.
            #[automatically_derived]
            impl<'de> ::serde::Deserialize<'de> for #ty {
                fn deserialize<D: ::serde::Deserializer<'de>>(
                    deserializer: D,
                ) -> ::std::result::Result<Self, D::Error> {
                    let text = <::std::borrow::Cow<'de, str> as ::serde::Deserialize<'de>>
                        ::deserialize(deserializer)?;
                    ::std::str::FromStr::from_str(&text).map_err(::serde::de::Error::custom)
                }
            }
        };
    }
    let visitor = quote::format_ident!("{ty}Spelling");
    let wanted = expecting.map_or_else(|| format!("a {ty} spelling, or a boolean"), LitStr::value);
    let doc = format!(" How a [`{ty}`] reads itself: its own spelling, or a native boolean.");
    quote! {
        #writer

        #[doc = #doc]
        struct #visitor;

        impl ::serde::de::Visitor<'_> for #visitor {
            type Value = #ty;

            fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(#wanted)
            }

            fn visit_str<E: ::serde::de::Error>(
                self,
                text: &str,
            ) -> ::std::result::Result<#ty, E> {
                ::std::str::FromStr::from_str(text).map_err(::serde::de::Error::custom)
            }

            fn visit_bool<E: ::serde::de::Error>(
                self,
                value: bool,
            ) -> ::std::result::Result<#ty, E> {
                ::std::result::Result::Ok(::std::convert::From::from(value))
            }
        }

        /// Through `deserialize_any` rather than `deserialize_str`, because the
        /// two inputs disagree about what they hand over: the wire carries the
        /// string this type writes, while a Lua config writes a native `true`.
        /// The *value* says which, and both are taken.
        #[automatically_derived]
        impl<'de> ::serde::Deserialize<'de> for #ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                deserializer.deserialize_any(#visitor)
            }
        }
    }
}

/// # Errors
///
/// Returns a [`syn::Error`] for a table that cannot be read: a variant with no
/// spelling, a payload spelling on a variant with nothing to put in it, or a
/// `FromStr` asked for over a spelling that could not produce one.
pub fn expand(input: &DeriveInput) -> Result<TokenStream> {
    let ty = &input.ident;
    let options = Options::read(&input.attrs)?;
    let variants = variants(input)?;
    let printing = display(ty, &variants);
    if options.error.is_none() {
        if options.serde.is_some() {
            return Err(Error::new_spanned(
                ty,
                "reading a spelling back needs `error = ...` to say what a bad one is",
            ));
        }
        return Ok(printing);
    }
    let (serde, expecting) = (options.serde, options.expecting.clone());
    let reading = from_str(ty, &variants, options)?;
    let wire = serde.map(|any_bool| serde_pair(ty, any_bool, expecting.as_ref()));
    Ok(quote! { #printing #reading #wire })
}
