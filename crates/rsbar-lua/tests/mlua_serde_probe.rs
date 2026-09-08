//! What `mlua`'s own serde bridge makes of the shapes a real config writes.
//!
//! This file is the standing answer to "does the Lua side need a
//! deserializer of its own?". It did once — 879 lines of one — and every
//! shape below is one of the reasons it existed, now answered by the *type*
//! being read instead. That is what makes them answers rather than
//! workarounds: the same coercion serves argv and the wire, because it is not
//! in either front end.
//!
//! If one of these ever fails, the fix is in `rsbar_protocol`, not here.

use mlua::{Lua, LuaSerdeExt};
use rsbar_protocol::{BoolChange, Color, ItemPatch};

fn mlua_reads(src: &str) -> Result<ItemPatch, String> {
    let lua = Lua::new();
    let value: mlua::Value = lua.load(src).eval().map_err(|e| e.to_string())?;
    lua.from_value::<ItemPatch>(value)
        .map_err(|e| e.to_string())
}

#[test]
fn a_flag_reads_as_a_lua_boolean_and_as_sketchybars_own_string() {
    // `Boolish`/`BoolChange` answer both `visit_bool` and `visit_str`, which
    // is what lets one type serve a config writing either.
    assert_eq!(
        mlua_reads(r"{ drawing = true }")
            .unwrap()
            .geometry
            .unwrap()
            .drawing,
        Some(BoolChange::True)
    );
    assert_eq!(
        mlua_reads(r#"{ drawing = "off" }"#)
            .unwrap()
            .geometry
            .unwrap()
            .drawing,
        Some(BoolChange::False)
    );
    assert_eq!(
        mlua_reads(r#"{ drawing = "toggle" }"#)
            .unwrap()
            .geometry
            .unwrap()
            .drawing,
        Some(BoolChange::Toggle)
    );
}

#[test]
fn a_colour_reads_as_the_bare_number_a_lua_config_writes() {
    // `colors.red` in a config is `0xffff0000`, a Lua *number*; the daemon
    // prints the same colour back as the string `0xffff0000`. `Color` asks
    // `deserialize_any` and takes either, so neither side has to know which
    // one the other used.
    let by_number = mlua_reads(r"{ background = { color = 0xffff0000 } }").unwrap();
    let by_string = mlua_reads(r#"{ background = { color = "0xffff0000" } }"#).unwrap();
    assert_eq!(
        by_number.geometry.unwrap().background.unwrap().color,
        Some(Color(0xffff_0000))
    );
    assert_eq!(
        by_string.geometry.unwrap().background.unwrap().color,
        Some(Color(0xffff_0000))
    );

    // And a number that is not a colour says so rather than truncating to
    // some shade nobody asked for.
    assert!(mlua_reads(r"{ background = { color = 1.5 } }").is_err());
}

#[test]
fn a_bare_string_is_the_text_of_the_half_it_was_written_for() {
    // `label = "12:00"` is how every config writes the common case, and
    // `RunPatch` takes it as well as the table it is sugar for.
    let text = |patch: ItemPatch| patch.label.unwrap().text;
    assert_eq!(
        text(mlua_reads(r#"{ label = "12:00" }"#).unwrap()),
        Some("12:00".into())
    );
    assert_eq!(
        text(mlua_reads(r#"{ label = { string = "12:00" } }"#).unwrap()),
        Some("12:00".into())
    );
}

#[test]
fn a_font_reads_as_a_table_of_parts_as_well_as_the_joined_spelling() {
    // A real config writes the table (four times in the user's own
    // `default.lua`); the daemon writes back the joined string.
    let flat = mlua_reads(r#"{ icon = { font = "Hack:Bold:14" } }"#).unwrap();
    let parts = mlua_reads(r"{ icon = { font = { family = 'Hack', style = 'Bold', size = 14 } } }")
        .unwrap();
    assert_eq!(flat.icon.unwrap().font, parts.icon.unwrap().font);
}

#[test]
fn a_key_no_patch_has_a_field_for_costs_that_key_and_nothing_else() {
    // The reason there is no hand-written deserializer here any more: one
    // stray property is named through `tracing` by the patch itself, and the
    // rest of the table still applies.
    let patch = mlua_reads(r"{ nonsense_key = 1, y_offset = 3 }").unwrap();
    assert_eq!(patch.geometry.unwrap().y_offset, Some(3.0));

    // `alias = { color = ... }` tints the mirrored icon in SketchyBar's own
    // helper. rsbar carries only the name, so the table is dropped rather
    // than refused -- the user's config does this.
    let patch = mlua_reads(r"{ alias = { color = 0xffff0000 } }").unwrap();
    assert_eq!(patch.alias, None);
}
