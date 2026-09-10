//! `#[derive(Changes)]` — a concrete type's patch, and what applying it
//! actually changes.
//!
//! Generated from the concrete type rather than written beside it, so a new
//! property is one line in one place and the two cannot drift. What
//! `struct-patch` could not express is the three things this is for: a patch
//! field whose type is not the target's (`Boolish` becomes `BoolChange`,
//! because a config writes `drawing=toggle` and only the daemon knows what it
//! currently is), a nested patch that recurses rather than being compared
//! whole, and a nested patch that is *flattened* — one the wire and the
//! command line spell as if its properties were the outer patch's own.
//!
//! The reading half is written here rather than derived, for one reason
//! measured the hard way: `#[serde(flatten)]` buffers every unclaimed key
//! into `serde`'s own value tree before any field sees it, and a buffered
//! `f64` field handed the string `4` out of argv fails. The generated
//! [`Patch::absorb`] hands the live `MapAccess` to whichever field knows what
//! the value is for, so nothing is ever buffered — and, as a consequence,
//! unknown-key reporting belongs to whoever is being read rather than to a
//! catch-all field that the first flattened member would swallow.

use proc_macro2::TokenStream;
use quote::{ToTokens as _, format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, Ident, Lit, LitStr, Meta, Path,
    Result, Token, Type, parenthesized,
};

/// One `#[changes(...)]` argument, on the struct or on a field.
enum Arg {
    /// `name = RunPatch`: what to call the generated struct.
    Name(Ident),
    /// `what = "an item"`: how a config would name this table in a sentence,
    /// for the message a stray key gets.
    What(LitStr),
    /// `gap(slider = "nothing draws a slider yet")`: a real `SketchyBar`
    /// property this patch has no field for, and why.
    Gap(LitStr, LitStr),
    /// `derive(...)`: handed to the patch struct's own derive.
    Derive(TokenStream),
    /// `attr(...)`: an attribute to put on the patch struct, or on the patch
    /// field — `serde(alias = "string")` and the like. Passed through
    /// verbatim; any `serde(alias = ...)`/`serde(rename = ...)` inside is
    /// additionally read as a key spelling, since the reading half is
    /// generated here rather than by `serde`'s derive.
    Attr(TokenStream),
    /// `as = BoolChange`: the patch's type for this field, where it differs
    /// from the target's.
    As(Type),
    /// `flatten`: this field's own patch is spelled inline — its properties
    /// are keys of the outer patch, not of a table under this name.
    Flatten,
    /// `required`: [`construct`](crate) cannot invent this one. A patch that
    /// does not name it cannot build the target at all, which is the
    /// difference between `--add` and `--set`.
    Required,
    /// `read_with = path`: a function that reads this field's value, for the
    /// one property whose input shape is not its own type's.
    ReadWith(Path),
    /// `scalar = text`: the field a bare scalar fills, for a patch a config
    /// may write as one — `label = "12:00"` for `label = { text = "12:00" }`.
    Scalar(Ident),
    /// `skip`: state a config cannot set, so the patch has no field for it —
    /// a count the daemon works out, a name that identifies rather than
    /// configures. [`construct`](crate) takes it as an argument instead, so
    /// it still cannot be forgotten.
    Skip,
}

impl Parse for Arg {
    fn parse(input: ParseStream) -> Result<Self> {
        if input.peek(Token![as]) {
            input.parse::<Token![as]>()?;
            input.parse::<Token![=]>()?;
            return Ok(Self::As(input.parse()?));
        }
        let key: Ident = input.parse()?;
        match key.to_string().as_str() {
            "skip" => Ok(Self::Skip),
            "flatten" => Ok(Self::Flatten),
            "required" => Ok(Self::Required),
            "name" => {
                input.parse::<Token![=]>()?;
                Ok(Self::Name(input.parse()?))
            }
            "what" => {
                input.parse::<Token![=]>()?;
                Ok(Self::What(input.parse()?))
            }
            "read_with" => {
                input.parse::<Token![=]>()?;
                Ok(Self::ReadWith(input.parse()?))
            }
            "scalar" => {
                input.parse::<Token![=]>()?;
                Ok(Self::Scalar(input.parse()?))
            }
            "gap" => {
                let inner;
                parenthesized!(inner in input);
                let key: LitStr = inner.parse()?;
                inner.parse::<Token![=]>()?;
                Ok(Self::Gap(key, inner.parse()?))
            }
            "derive" | "attr" => {
                let inner;
                parenthesized!(inner in input);
                let tokens = inner.parse()?;
                Ok(if key == "derive" {
                    Self::Derive(tokens)
                } else {
                    Self::Attr(tokens)
                })
            }
            _ => Err(Error::new_spanned(
                key,
                "expected `name`, `what`, `gap`, `derive`, `attr`, `scalar`, `skip`, `flatten`, \
                 `required`, `read_with` or `as`",
            )),
        }
    }
}

fn args(attrs: &[Attribute]) -> Result<Vec<Arg>> {
    let mut args = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("changes") {
            args.extend(attr.parse_args_with(Punctuated::<Arg, Token![,]>::parse_terminated)?);
        }
    }
    Ok(args)
}

/// Everything that is not ours, kept as it was — doc comments above all, so
/// the patch reads like the thing it patches.
fn passthrough(attrs: &[Attribute]) -> Vec<&Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .collect()
}

/// Every `#[serde(...)]` written on the declaration, carried to the patch
/// verbatim.
///
/// No whitelist and no opinion: whatever a declaration says about how a
/// property is spelled, the patch of it says too. The derive adds attributes
/// of its own — `skip_serializing_if` on every option, the `colour` alias —
/// and stands down where the author already wrote one, since an explicit
/// attribute is a deliberate act and the derive's is only a default.
fn serde_attrs(attrs: &[Attribute]) -> Vec<TokenStream> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("serde"))
        .map(|attr| attr.meta.to_token_stream())
        .collect()
}

/// Whether the author already spelled `name` inside one of these
/// `serde(...)` attributes, so the derive should not add its own.
fn already_says(attrs: &[TokenStream], name: &str) -> bool {
    attrs.iter().any(|tokens| {
        let Ok(Meta::List(list)) = syn::parse2::<Meta>(tokens.clone()) else {
            return false;
        };
        if !list.path.is_ident("serde") {
            return false;
        }
        list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|inner| inner.iter().any(|entry| entry.path().is_ident(name)))
    })
}

/// Every spelling a key arrives under: the field's own name, whatever
/// `serde(alias = ...)`/`serde(rename = ...)` was passed through, and — for
/// any colour — the British one.
///
/// The colour rule is applied rather than declared for the reason every other
/// rule here is: `color`, `border_color` and `highlight_color` all want it,
/// and so does the next one somebody adds. A list of them would rot.
/// Deserialize-only, so `--query` keeps printing one spelling.
fn spellings(ident: &Ident, attrs: &[TokenStream]) -> Vec<String> {
    let mut names = vec![ident.to_string()];
    for tokens in attrs {
        let Ok(meta) = syn::parse2::<Meta>(tokens.clone()) else {
            continue;
        };
        let Meta::List(list) = &meta else { continue };
        if !list.path.is_ident("serde") {
            continue;
        }
        let Ok(inner) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        else {
            continue;
        };
        for entry in inner {
            let Meta::NameValue(pair) = entry else {
                continue;
            };
            if !(pair.path.is_ident("alias") || pair.path.is_ident("rename")) {
                continue;
            }
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(text),
                ..
            }) = pair.value
            {
                names.push(text.value());
            }
        }
    }
    for spelling in names.clone() {
        if spelling.contains("color") {
            names.push(spelling.replace("color", "colour"));
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// `click_script` as `ClickScript`, for naming a generated helper after the
/// field it serves.
fn camel(ident: &Ident) -> String {
    ident
        .to_string()
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// One field of the patch being generated, resolved from the declaration.
struct Field<'a> {
    ident: &'a Ident,
    /// The target's own type for this field.
    target_ty: &'a Type,
    /// The patch's type for it, which is the same unless `as` said otherwise.
    ty: Type,
    docs: Vec<&'a Attribute>,
    attrs: Vec<TokenStream>,
    /// Every key this field answers to.
    keys: Vec<String>,
    flatten: bool,
    required: bool,
    read_with: Option<Path>,
}

/// How the patch reads itself.
///
/// Always a `Visitor`, never `#[serde(flatten)]` or `#[serde(untagged)]`,
/// which say the same thing far more briefly and cannot be used here: both
/// buffer the input into `serde`'s own value tree before any field reads from
/// it, and every leaf then reads from that copy rather than from the format.
/// The CLI's argv deserializer is where that shows: `padding_left=5` arrives
/// as the string `5` and only becomes an `f64` because the *field's* type is
/// what asks. Buffer it first and the field is asking a string for a number.
///
/// `deserialize_any` rather than `deserialize_map` for a patch with a
/// [`Arg::Scalar`] field, so `label = "12:00"` and
/// `label = { string = "12:00" }` are one type's two spellings rather than
/// two front ends' guesses.
fn reader(patch: &Ident, scalar: Option<&Ident>) -> TokenStream {
    let Some(scalar) = scalar else {
        return quote! {
            impl<'de> ::serde::Deserialize<'de> for #patch {
                fn deserialize<D: ::serde::Deserializer<'de>>(
                    deserializer: D,
                ) -> ::std::result::Result<Self, D::Error> {
                    deserializer.deserialize_map(::coolabah_protocol::patch::Reader::<Self>::new())
                }
            }
        };
    };
    let expecting = format!("{patch} as a table of properties, or a bare string for `{scalar}`");
    let visitor = format_ident!("{patch}Visitor");
    quote! {
        impl<'de> ::serde::Deserialize<'de> for #patch {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                deserializer.deserialize_any(#visitor)
            }
        }

        #[doc = "The bare-string half of [`"]
        #[doc = stringify!(#patch)]
        #[doc = "`]'s two spellings; the table half is the shared reader."]
        struct #visitor;

        impl<'de> ::serde::de::Visitor<'de> for #visitor {
            type Value = #patch;

            fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(#expecting)
            }

            fn visit_str<E: ::serde::de::Error>(
                self,
                text: &str,
            ) -> ::std::result::Result<#patch, E> {
                ::std::result::Result::Ok(#patch {
                    #scalar: ::std::option::Option::Some(
                        ::std::borrow::ToOwned::to_owned(text),
                    ),
                    ..::std::default::Default::default()
                })
            }

            fn visit_map<A: ::serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> ::std::result::Result<#patch, A::Error> {
                ::serde::de::Visitor::visit_map(
                    ::coolabah_protocol::patch::Reader::<#patch>::new(),
                    map,
                )
            }
        }
    }
}

#[expect(clippy::too_many_lines, reason = "one generated item after another")]
pub(crate) fn expand(input: &DeriveInput) -> Result<TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(Error::new_spanned(
            &input.ident,
            "Changes is for a struct: a patch names fields",
        ));
    };
    let Fields::Named(declared) = &data.fields else {
        return Err(Error::new_spanned(
            &input.ident,
            "Changes needs named fields",
        ));
    };

    let mut name = None;
    let mut what = None;
    let mut scalar = None;
    let mut gaps = Vec::new();
    let mut derives = Vec::new();
    let mut attrs = serde_attrs(&input.attrs);
    for arg in args(&input.attrs)? {
        match arg {
            Arg::Name(ident) => name = Some(ident),
            Arg::What(text) => what = Some(text),
            Arg::Gap(key, note) => gaps.push(quote!((#key, #note))),
            Arg::Derive(tokens) => derives.push(tokens),
            Arg::Attr(tokens) => attrs.push(tokens),
            Arg::Scalar(ident) => scalar = Some(ident),
            other => {
                return Err(Error::new_spanned(
                    &input.ident,
                    match other {
                        Arg::As(_) => "`as` belongs on a field",
                        Arg::Flatten => "`flatten` belongs on a field",
                        Arg::Required => "`required` belongs on a field",
                        Arg::ReadWith(_) => "`read_with` belongs on a field",
                        _ => "`skip` belongs on a field",
                    },
                ));
            }
        }
    }
    let target = &input.ident;
    let patch = name.ok_or_else(|| {
        Error::new_spanned(target, "`#[changes(name = ...)]` names the patch struct")
    })?;
    let what = what.ok_or_else(|| {
        Error::new_spanned(
            target,
            "`#[changes(what = \"an item\")]` names this table as a config would",
        )
    })?;

    let mut fields = Vec::new();
    let mut skipped = Vec::new();
    for field in &declared.named {
        let ident = field.ident.as_ref().expect("a named field has a name");
        let mut resolved = Field {
            ident,
            target_ty: &field.ty,
            ty: field.ty.clone(),
            docs: passthrough(&field.attrs),
            attrs: serde_attrs(&field.attrs),
            keys: Vec::new(),
            flatten: false,
            required: false,
            read_with: None,
        };
        let mut drop_it = false;
        for arg in args(&field.attrs)? {
            match arg {
                Arg::As(substitute) => resolved.ty = substitute,
                Arg::Attr(tokens) => resolved.attrs.push(tokens),
                Arg::Flatten => resolved.flatten = true,
                Arg::Required => resolved.required = true,
                Arg::ReadWith(reader) => resolved.read_with = Some(reader),
                Arg::Skip => drop_it = true,
                _ => {
                    return Err(Error::new_spanned(
                        ident,
                        "only `as`, `attr`, `flatten`, `required`, `read_with` and `skip` belong \
                         on a field",
                    ));
                }
            }
        }
        if drop_it {
            skipped.push((ident, &field.ty, passthrough(&field.attrs)));
            continue;
        }
        if resolved.required && resolved.flatten {
            return Err(Error::new_spanned(
                ident,
                "a flattened field is required through its own fields, not as a whole",
            ));
        }
        resolved.keys = spellings(ident, &resolved.attrs);
        fields.push(resolved);
    }

    // `None` here means "the request did not mention this", and an absent key
    // reads back as exactly that -- so writing it is 200-odd bytes of field
    // names for properties nobody set, on every `--set`.
    let skip_none = quote!(#[serde(skip_serializing_if = "::std::option::Option::is_none")]);

    let mut declarations = Vec::new();
    let mut accessors = Vec::new();
    let mut merges = Vec::new();
    let mut record = Vec::new();
    let mut anything = Vec::new();
    let mut own_arms = Vec::new();
    let mut flat_arms = Vec::new();
    let mut own_keys = Vec::new();
    let mut nested_keys = Vec::new();
    let mut folds = Vec::new();
    let mut seeds = Vec::new();
    let mut missing = Vec::new();
    let mut built = Vec::new();
    for field in &fields {
        let Field {
            ident,
            target_ty,
            ty,
            docs,
            attrs,
            keys,
            flatten,
            required,
            read_with,
        } = field;
        let flatten_attr =
            (*flatten && !already_says(attrs, "flatten")).then(|| quote!(#[serde(flatten)]));
        let skip_attr =
            (!flatten && !already_says(attrs, "skip_serializing_if")).then(|| skip_none.clone());
        declarations.push(quote! {
            #(#docs)*
            #( #[#attrs] )*
            #flatten_attr
            #skip_attr
            pub #ident: ::std::option::Option<#ty>,
        });

        // `Option<M>` uniformly: `Some(())` for a leaf that moved, and the
        // nested record for a field that is itself patched.
        let moved_ty = quote! {
            <#ty as ::coolabah_protocol::Changes<#target_ty>>::Moved
        };
        record.push(quote! {
            #(#docs)*
            pub #ident: ::std::option::Option<#moved_ty>,
        });
        anything.push(quote!(self.#ident.is_some()));

        let changed = format_ident!("{ident}_changed");
        let apply = format_ident!("apply_{ident}");
        let asking =
            format!(" What `{ident}` becomes, or `None` when this patch does not move it.");
        let writing =
            format!(" Writes that change into `{ident}`, and says whether there was one.");
        // One shape for every field, whatever the patch spells it as: a leaf
        // borrows itself out of the patch, a `BoolChange` resolves against
        // what is there, and a nested patch walks its own fields the same
        // way. All three are `Changes::changed`.
        accessors.push(quote! {
            #[doc = #asking]
            #[must_use]
            pub fn #changed(
                &self,
                current: &#target_ty,
            ) -> ::std::option::Option<::std::borrow::Cow<'_, #target_ty>> {
                ::coolabah_protocol::Changes::changed(self.#ident.as_ref()?, current)
            }

            #[doc = #writing]
            pub fn #apply(&self, target: &mut #target_ty) -> bool {
                let ::std::option::Option::Some(value) = self.#changed(target) else {
                    return false;
                };
                *target = ::std::borrow::Cow::into_owned(value);
                true
            }
        });
        merges.push(quote! {
            moved.#ident = self
                .#ident
                .as_ref()
                .and_then(|change| ::coolabah_protocol::Changes::moved(change, &current.#ident))
                .map(|(value, detail)| {
                    next.#ident = ::std::borrow::Cow::into_owned(value);
                    detail
                });
        });
        folds.push(quote! {
            if let ::std::option::Option::Some(value) = other.#ident {
                self.#ident = ::std::option::Option::Some(
                    match self.#ident.take() {
                        ::std::option::Option::Some(mine) => {
                            use ::coolabah_protocol::patch::Fold as _;
                            mine.folded(value)
                        }
                        ::std::option::Option::None => value,
                    },
                );
            }
        });

        if *flatten {
            nested_keys.push(quote!(<#ty as ::coolabah_protocol::patch::Fields>::KEYS));
            // Tried on a copy, so a key none of them claims does not leave an
            // empty sub-patch behind that `Default` would then differ from.
            flat_arms.push(quote! {
                {
                    let mut child = self.#ident.take().unwrap_or_default();
                    let claimed = ::coolabah_protocol::patch::Patch::absorb(&mut child, key, map)?;
                    if claimed || child != ::std::default::Default::default() {
                        self.#ident = ::std::option::Option::Some(child);
                    }
                    if claimed {
                        return ::std::result::Result::Ok(true);
                    }
                }
            });
        } else {
            for key in keys {
                own_keys.push(quote!(#key));
            }
            // `match` rather than `if let`, because the two arms are the two
            // ways a field's value is read and reading them side by side is
            // the point; clippy's suggestion would bury the common case in an
            // `else`.
            #[allow(clippy::single_match_else)]
            let read = match read_with {
                Some(reader) => {
                    // A `Deserializer`-taking function cannot be held as a
                    // value -- it is generic over the format -- so it becomes
                    // a seed, which is the shape `MapAccess` takes.
                    let seed = format_ident!("{patch}{}Seed", camel(ident));
                    let seed_doc = format!(
                        " How [`{patch}::{ident}`] reads its value, which is not its own \
                         type's shape."
                    );
                    seeds.push(quote! {
                        #[doc = #seed_doc]
                        struct #seed;

                        impl<'de> ::serde::de::DeserializeSeed<'de> for #seed {
                            type Value = ::std::option::Option<#ty>;

                            fn deserialize<D: ::serde::Deserializer<'de>>(
                                self,
                                deserializer: D,
                            ) -> ::std::result::Result<Self::Value, D::Error> {
                                #reader(deserializer)
                            }
                        }
                    });
                    quote!(::serde::de::MapAccess::next_value_seed(map, #seed)?)
                }
                None => quote! {
                    ::std::option::Option::Some(
                        ::serde::de::MapAccess::next_value::<#ty>(map)?,
                    )
                },
            };
            // A key seen twice folds rather than replaces, which is what
            // makes `label=12:34 label.color=0xff00ff00` mean the table a
            // config would have written by hand.
            let patterns = keys.iter().map(|key| quote!(#key));
            own_arms.push(quote! {
                #(#patterns)|* => {
                    let incoming = #read;
                    if let ::std::option::Option::Some(value) = incoming {
                        self.#ident = ::std::option::Option::Some(
                            match self.#ident.take() {
                                ::std::option::Option::Some(mine) => {
                                    use ::coolabah_protocol::patch::Fold as _;
                                    mine.folded(value)
                                }
                                ::std::option::Option::None => value,
                            },
                        );
                    }
                }
            });
        }

        let key = ident.to_string();
        if *required {
            if quote!(#ty).to_string() != quote!(#target_ty).to_string() {
                return Err(Error::new_spanned(
                    ident,
                    "a required field is taken from the patch as it stands, so its patch type \
                     must be the target's own",
                ));
            }
            missing.push(quote! {
                if self.#ident.is_none() {
                    absent.push(#key);
                }
            });
            built.push(quote! {
                #ident: ::std::clone::Clone::clone(
                    self.#ident.as_ref().expect("`missing` reported none absent"),
                ),
            });
        } else if *flatten {
            missing.push(quote! {
                absent.extend(
                    self.#ident
                        .clone()
                        .unwrap_or_default()
                        .absent_fields(),
                );
            });
            built.push(quote! {
                #ident: self
                    .#ident
                    .clone()
                    .unwrap_or_default()
                    .construct()?,
            });
        } else {
            built.push(quote! {
                #ident: match self.#ident.as_ref() {
                    ::std::option::Option::Some(change) => ::coolabah_protocol::Changes::changes(
                        change,
                        &<#target_ty as ::std::default::Default>::default(),
                    )
                    .unwrap_or_default(),
                    ::std::option::Option::None => ::std::default::Default::default(),
                },
            });
        }
    }

    // Without their doc comments: a parameter cannot carry one.
    let skipped_args = skipped.iter().map(|(ident, ty, _)| quote!(#ident: #ty));
    let skipped_names = skipped.iter().map(|(ident, ..)| quote!(#ident,));

    let record_name = format_ident!("{target}Moved");
    let record_doc = format!(" Which of [`{target}`]'s fields a [`{patch}`] moved.");
    let reading = reader(&patch, scalar.as_ref());
    let docs = passthrough(&input.attrs);
    let construct_doc = format!(
        " A complete [`{target}`] from this patch, rather than a change to one.\n\
         \n\
         Construction, not application: `--add` has to end up with a whole\n\
         item, and a property with no meaningful default -- one marked\n\
         `#[changes(required)]` -- has to have been named. Every one that was\n\
         not is reported at once, so a config author fixes them together.\n\
         \n\
         An exhaustive struct literal, deliberately: a property added to\n\
         [`{target}`] fails to compile here rather than being quietly\n\
         defaulted, and a property this patch cannot carry is an argument\n\
         rather than something forgotten.\n\
         \n\
         # Errors\n\
         \n\
         Returns [`Missing`](coolabah_protocol::patch::Missing) naming every\n\
         required property this patch does not set."
    );
    Ok(quote! {
        #(#docs)*
        ///
        /// A partial update: `None` on a field means "leave it as it is",
        /// which is not the same as setting it to the default.
        // A patch exists to cross the wire, so it is serialisable by
        // construction rather than by every declaration remembering to ask.
        // Only the writing half is derived: see this module's own docs for
        // why the reading half cannot be.
        #[derive(::serde::Serialize)]
        #( #[derive(#derives)] )*
        #( #[#attrs] )*
        pub struct #patch {
            #(#declarations)*
        }

        #reading

        #(#seeds)*

        impl ::coolabah_protocol::patch::Fields for #patch {
            const WHAT: &'static str = #what;
            const KEYS: &'static [&'static str] = &[#(#own_keys),*];
            const NESTED: &'static [&'static [&'static str]] = &[#(#nested_keys),*];
            const GAPS: &'static [(&'static str, &'static str)] = &[#(#gaps),*];
        }

        impl<'de> ::coolabah_protocol::patch::Patch<'de> for #patch {
            fn absorb<A: ::serde::de::MapAccess<'de>>(
                &mut self,
                key: &str,
                map: &mut A,
            ) -> ::std::result::Result<bool, A::Error> {
                match key {
                    #(#own_arms)*
                    _ => {
                        #(#flat_arms)*
                        return ::std::result::Result::Ok(false);
                    }
                }
                ::std::result::Result::Ok(true)
            }

        }

        impl #patch {
            #(#accessors)*

            /// Two spellings of the same property as one: whatever the later
            /// names, it names, and a nested patch folds field by field
            /// rather than replacing what is under it whole.
            ///
            /// Inherent rather than an impl of
            /// [`Fold`](coolabah_protocol::patch::Fold), so it wins method
            /// resolution against that trait's blanket "the later value
            /// replaces the earlier" — which is the right answer for a leaf
            /// and the wrong one for a patch.
            #[must_use]
            pub fn folded(mut self, other: Self) -> Self {
                #(#folds)*
                self
            }

            /// Every required property this patch does not name.
            #[must_use]
            pub fn absent_fields(&self) -> ::std::vec::Vec<&'static str> {
                let mut absent = ::std::vec::Vec::new();
                #(#missing)*
                absent
            }

            /// The same, as the error a front end shows, or `None` when
            /// nothing is missing.
            #[must_use]
            pub fn missing(&self) -> ::std::option::Option<::coolabah_protocol::patch::Missing> {
                ::coolabah_protocol::patch::Missing::new(
                    <Self as ::coolabah_protocol::patch::Fields>::WHAT,
                    self.absent_fields(),
                )
            }

            #[doc = #construct_doc]
            pub fn construct(
                &self,
                #(#skipped_args),*
            ) -> ::std::result::Result<#target, ::coolabah_protocol::patch::Missing> {
                if let ::std::option::Option::Some(missing) = self.missing() {
                    return ::std::result::Result::Err(missing);
                }
                ::std::result::Result::Ok(#target {
                    #(#skipped_names)*
                    #(#built)*
                })
            }
        }

        #[doc = #record_doc]
        ///
        /// `Some` for a field that moved. A leaf carries nothing beyond that;
        /// a field that is itself patched carries its own record, so a caller
        /// can re-do only the work the change actually implies.
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct #record_name {
            #(#record)*
        }

        impl #record_name {
            /// Whether anything moved at all.
            #[must_use]
            pub fn any(&self) -> bool {
                #(#anything)||*
            }
        }

        impl ::coolabah_protocol::Changes<#target> for #patch {
            type Moved = #record_name;

            /// Owned by construction: a whole new value has to be built, and
            /// only the fields that moved are written into it.
            fn moved(
                &self,
                current: &#target,
            ) -> ::std::option::Option<(::std::borrow::Cow<'_, #target>, #record_name)> {
                let mut next = ::std::clone::Clone::clone(current);
                let mut moved = #record_name::default();
                #(#merges)*
                if moved.any() {
                    ::std::option::Option::Some((::std::borrow::Cow::Owned(next), moved))
                } else {
                    ::std::option::Option::None
                }
            }
        }
    })
}
