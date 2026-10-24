# Cassetta's patches to gpui 0.2.2

This repository is [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui)
0.2.2 exactly as published on crates.io (first commit, `Cargo.toml` as
published, registry bookkeeping files dropped), plus two patches for
[Cassetta](https://github.com/briantoll/cassetta), one commit each, which
pins it by rev:

```toml
[patch.crates-io]
gpui = { git = "https://github.com/briantoll/gpui", rev = "<sha>" }
```

gpui is Zed Industries' work under the Apache License 2.0 (`LICENSE-APACHE`);
the patches are offered under the same terms. The version stays `0.2.2`.

1. **Variable-font weights on Linux** (2026-10-13) — below.
2. **The font fallback's last stage off on Linux** (2026-10-24) — below.

# Patch 1: variable-font weights on Linux

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

# Patch 2: the font fallback's last stage off on Linux

## Why

cosmic-text 0.14.2's `FontFallbackIter` (`src/font/fallback/mod.rs`) looks for a
face that can draw a cluster the requested font cannot in four stages: the
requested family, the script's named families (`fallback/unix.rs`), the common
families, and last **every face in the database** not in `forbidden_fallback`,
each loaded through `FontSystem::get_font` → `fontdb::make_shared_face_data`,
which maps the file, and kept in the font cache for the life of the process. A
cluster that no single face can draw never stops that walk. On Brian's library
the one title `"Alfie Silas\u{c00}"` (a Telugu combining mark on a Latin `s`:
Noto Sans Telugu has no `s`, Adwaita Sans no Telugu) mapped **all 649 font files**
of an Omarchy desktop with Noto CJK installed, the 15 Noto CJK `.ttc`
collections among them: font-file RSS 1 → 104 MiB, peak RSS 484 MiB on a
whole-library scroll, a 52 ms shape. The walk lives inside `ShapeLine::new`, so
gpui cannot memoize or cut it short from outside; the one input it reads is the
`Fallback` the `FontSystem` was built with.

## The hunks (`src/platform/linux/text_system.rs`, plus `Cargo.toml`)

| Where | What |
|---|---|
| `Cargo.toml` | `sys-locale = "0.3"` and `unicode-script = "0.5"` (the versions cosmic-text 0.14.2 builds) as optional Linux/FreeBSD dependencies, enabled by `wayland` and `x11` beside `cosmic-text`. |
| `CosmicTextSystem::new()` | `new_font_system()` instead of `FontSystem::new()`. |
| `new_font_system()` (new) | `FontSystem::new_with_fonts` step for step (the locale through `sys_locale`, `load_system_fonts`, the three default families) with `LinuxFallback` in place of `PlatformFallback`. |
| `LinuxFallback` (new) | `forbidden_fallback`: every family name in the database (each face's first name, the families with most faces first), leaked once, so the last stage loads nothing; faces added later with `add_fonts` are not on it. `script_fallback`: cosmic-text's list, else the installed `Noto Sans <Script>` then `Noto Serif <Script>`, resolved once from the family names (no file opened). `common_fallback`: cosmic-text's list plus `Noto Sans Symbols 2` (cosmic-text's `Noto Sans Symbols2` matches no installed name), `Noto Sans Math`, `Noto Music` and the locale's Noto Sans CJK family (fullwidth forms and CJK punctuation in Latin text). |
| `noto_script()` (new) | The script a Noto family name stands for: `Tai Le` → `Tai_Le`, `PhagsPa` → `Phags_Pa`, `NKo` → `Nko`, Noto's four abbreviations (`EgyptHiero`, `ImpAramaic`, `InsPahlavi`, `PsaPahlavi`); weights and variants (`Telugu Black`, `Arabic UI`) match nothing. A unit test. |

## Measured (Cassetta's harness, the headless output, 2026-10-24)

| | stock fallback | patch 2 |
|---|---|---|
| Songs scroll of the 62,643-song library: font files mapped | 649 (15 `.ttc`) | 11 (1 `.ttc`) |
| … their resident size | 104 MiB | 3.6 MiB |
| … peak RSS | 484 MiB | 317 MiB |
| slowest single `ShapeLine::new` | 52 ms (`Alfie Silas\u{c00}`) | under 4 ms |
| lines left with a `.notdef` glyph | 1 (that title) | the same 1 |

A fixture of 15 titles mixing Han, Hiragana, Katakana, Hangul, Cyrillic, Arabic,
Hebrew, Thai, emoji, fullwidth forms, Symbols 2, Musical Symbols, Ol Chiki,
Cham, Balinese, Phags-pa and N'Ko renders every one of them, and the captures
match the stock build's except ♩ and ∮ (drawn from a named common family now,
not the first face the walk reached).

## What it gives up

A character that no named family covers, but that some other installed face
does (a Nerd Font's private-use icons, a script with neither a cosmic-text list
nor a `Noto Sans <Script>` family), draws as `.notdef` instead of being found
by the walk. On the desktop measured, no character in the library is such a
case.

# On the next gpui bump

Start a fresh branch `cassetta-<version>` from the verbatim published crate and
re-check each patch against upstream first (`gpui_wgpu/src/cosmic_text_system.rs`
in Zed main now holds the Linux text system), one commit per patch kept:

1. Variable weights: grep for `normalized_coords`, `variations` or `wght`. If
   upstream applies variable weights, drop the patch; else re-apply its hunks.
2. The fallback: check which cosmic-text the new gpui builds. If its
   `FontFallbackIter` no longer loads every face to test coverage (a
   codepoint-coverage check before `get_font`, or a bounded last stage), drop
   the patch; else re-apply it, re-reading cosmic-text's `new_with_fonts` so
   `new_font_system()` still builds the same database. Re-measure: the
   Songs scroll's mapped font files (`/proc/<pid>/maps`) on a desktop with
   Noto CJK installed.

If no patch is left, drop the fork.
