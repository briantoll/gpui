# Cassetta's patch to gpui 0.2.2

This repository is [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui)
0.2.2 exactly as published on crates.io (first commit, `Cargo.toml` as
published, registry bookkeeping files dropped), plus one patch for
[Cassetta](https://github.com/briantoll/cassetta), which pins it by rev:

```toml
[patch.crates-io]
gpui = { git = "https://github.com/briantoll/gpui", rev = "<sha>" }
```

gpui is Zed Industries' work under the Apache License 2.0 (`LICENSE-APACHE`);
the patch is offered under the same terms. The version stays `0.2.2`.

## Why

On Linux, gpui 0.2.2 draws every weight of a **variable font** at the
font's default instance. Adwaita Sans (GNOME's and Omarchy's UI font) ships
as one variable file (`wght` 100–900, default 400), so `FontWeight::BOLD`
and `SEMIBOLD` drew regular. Three places lose the weight:

1. fontdb 0.16 (through cosmic-text 0.14.2) lists only a variable font's
   default instance, so `font_id()`'s best match for any weight is the
   regular face;
2. cosmic-text's `SwashCache` builds its swash scaler without variation
   coordinates (`cosmic-text-0.14.2/src/swash.rs:29-62`);
3. shaping and `glyph_metrics(&[])` use the default instance's advances.

Zed main (checked 2026-10-13, `crates/gpui_wgpu/src/cosmic_text_system.rs`)
has not fixed this either, so there was no upstream diff to port.

## The hunks (all in `src/platform/linux/text_system.rs`, plus `Cargo.toml`)

| Where | What |
|---|---|
| `Cargo.toml` | `swash = "0.2.10"` (the version cosmic-text 0.14.2 uses) as an optional Linux/FreeBSD dependency, enabled by the `wayland` and `x11` features beside `cosmic-text`. |
| `CosmicTextSystemState` | Two fields: `variable_instances: HashMap<(FontId, u16), FontId>` (each weight instance registered once) and `scale_context: swash::scale::ScaleContext` (renders instance glyphs). Initialised in `new()`. |
| `LoadedFont` | `coords: Vec<swash::NormalizedCoord>`, empty for a face's default instance; every existing constructor passes `Vec::new()`. |
| `font_id()` | After `find_best_match`, returns `state.variable_instance(font_id, font.weight)`. |
| `variable_instance()` (new) | If the matched face has a `wght` axis and the requested weight (clamped to the axis) is more than 50 from the axis default, registers a `LoadedFont` sharing the same `Arc<CosmicTextFont>` with `normalized_coords([("wght", weight)])` and returns its `FontId`. Otherwise returns the face unchanged, so static families (a real Bold file) and regular text take exactly the old path. `opsz` stays at its default. |
| `typographic_bounds()`, `advance()` | `glyph_metrics(&loaded_font.coords)` instead of `&[]`, so HVAR advances apply. |
| `raster_bounds()`, `rasterize_glyph()` | Build the same `CacheKey` as before; a default instance goes through `SwashCache` as before, an instance through `render_variable_glyph()`. |
| `render_variable_glyph()` (new) | cosmic-text's `swash_image` (same sources, `Format::Alpha`, `hint(true)`, sub-pixel offset from the key's bins) with `.normalized_coords(&coords)`. gpui's atlas caches per `RenderGlyphParams`, so each glyph renders once per size/variant. |
| `layout_line()` | cosmic-text shapes the instance with the default advances; for glyphs of an instance the loop adds `(advance(coords) − advance(default)) × size / upem` to a running `x_shift`, positions each glyph at `glyph.x + x_shift`, and widens the line by the total. Kerning from shaping is kept (only the HVAR delta is added). |

Not changed: `font_metrics()` (ascent/descent/line height stay the default
instance's, so line heights do not move with weight), fallback fonts chosen
by cosmic-text inside a run (they draw at their own default weight), macOS
and Windows text systems.

## On the next gpui bump

Re-check upstream first (`gpui_wgpu/src/cosmic_text_system.rs` in Zed main
now holds the Linux text system): grep for `normalized_coords`, `variations`
or `wght`. If upstream applies variable weights, drop this fork. If not,
re-apply the hunks above onto the new published source in a fresh branch
`cassetta-<version>`, starting again from the verbatim published crate.
