//! The declaration macros behind `coolabah-protocol`'s event vocabulary.
//!
//! A proc macro rather than `macro_rules!` for three things `macro_rules!`
//! cannot do at all: expand to a single enum variant (so the `Kind` list no
//! longer has to be built by a tt-muncher with an accumulator), expand to a
//! whole match arm (so the five two-arm helper macros that spliced a pattern
//! and its expression separately are gone), and upper-case an identifier (so
//! the `const fn` that did it byte-wise is gone too).
//!
//! Nothing here is meant to be used directly: `coolabah-protocol` re-exports
//! both macros, and the code they generate names that crate.

mod changes;
mod env_fields;
mod events;
mod spelling;

use proc_macro::TokenStream;
use syn::parse_macro_input;

/// Declares the built-in events.
///
/// `Variant = "name" => Payload { field: Type }` gives a payload struct, an
/// `Event::Variant(Payload)`, and a `Kind::Variant` named `"name"`.
/// `Variant = "name" @scoped => ...` gives a `Kind::Variant(Item)` instead —
/// for an event that happens to one item rather than to the bar.
///
/// A doc comment on an event lands on both variants it becomes.
#[proc_macro]
pub fn events(input: TokenStream) -> TokenStream {
    let events = parse_macro_input!(input as events::Events);
    events::expand(&events).into()
}

/// Derives [the script environment] for a struct or an enum.
///
/// Every named field becomes a variable under its own name in upper case,
/// worked out here rather than at run time. `#[env(flatten)]` on a field
/// projects that field's own fields up alongside it, so a nested type reaches
/// a shell flat. On an enum the projection is the union of every variant's
/// fields, empty for the ones this variant does not carry, and
/// `#[env(name = value)]` on a variant fills one in that it implies rather
/// than stores.
///
/// [the script environment]: ../coolabah_protocol/event/trait.EnvFields.html
#[proc_macro_derive(EnvFields, attributes(env))]
pub fn derive_env_fields(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    env_fields::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives a concrete type's patch, and what applying it changes.
///
/// `#[changes(name = FooPatch, derive(...), attr(...))]` on the struct names
/// and shapes the generated patch; `#[changes(as = T)]` on a field gives the
/// patch a type of its own for it, which is how `Boolish` becomes
/// `BoolChange` and a nested struct becomes its own patch; `#[changes(attr(...))]`
/// on a field carries an attribute across.
#[proc_macro_derive(Changes, attributes(changes))]
pub fn derive_changes(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    changes::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives every impl that spells a type, from one table of spellings.
///
/// `#[spell("left", "l")]` on a variant declares them, canonical first: the
/// first is what [`Display`](std::fmt::Display) writes and every one is what
/// [`FromStr`](std::str::FromStr) accepts. `#[spell("popup.{}")]` does the
/// same for a variant whose spelling is computed from its payload.
///
/// `#[spelling(error = InvalidAlign)]` asks for the `FromStr` and says what a
/// bad spelling is; `#[spelling(serde)]` asks for a `Serialize`/`Deserialize`
/// pair that writes and reads the same words, and `serde(bool)` for one that
/// also takes the native boolean a Lua config writes. With neither, a type
/// only prints itself.
#[proc_macro_derive(Spelling, attributes(spelling, spell))]
pub fn derive_spelling(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    spelling::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
