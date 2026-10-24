use crate::{
    Bounds, DevicePixels, Font, FontFeatures, FontId, FontMetrics, FontRun, FontStyle, FontWeight,
    GlyphId, LineLayout, Pixels, PlatformTextSystem, Point, RenderGlyphParams, SUBPIXEL_VARIANTS_X,
    SUBPIXEL_VARIANTS_Y, ShapedGlyph, ShapedRun, SharedString, Size, point, size,
};
use anyhow::{Context as _, Ok, Result};
use collections::HashMap;
use cosmic_text::{
    Attrs, AttrsList, CacheKey, Family, Font as CosmicTextFont, FontFeatures as CosmicFontFeatures,
    FontSystem, ShapeBuffer, ShapeLine, SwashCache,
};

use itertools::Itertools;
use parking_lot::RwLock;
use pathfinder_geometry::{
    rect::{RectF, RectI},
    vector::{Vector2F, Vector2I},
};
use smallvec::SmallVec;
use std::{borrow::Cow, sync::Arc};

pub(crate) struct CosmicTextSystem(RwLock<CosmicTextSystemState>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: SharedString,
    features: FontFeatures,
}

impl FontKey {
    fn new(family: SharedString, features: FontFeatures) -> Self {
        Self { family, features }
    }
}

struct CosmicTextSystemState {
    swash_cache: SwashCache,
    font_system: FontSystem,
    scratch: ShapeBuffer,
    /// Contains all already loaded fonts, including all faces. Indexed by `FontId`.
    loaded_fonts: Vec<LoadedFont>,
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    /// Cassetta patch: the `wght` instances of variable faces already registered, keyed by the
    /// default-instance `FontId` and the weight.
    variable_instances: HashMap<(FontId, u16), FontId>,
    /// Cassetta patch: rasterizes glyphs of variable instances (cosmic-text's `SwashCache`
    /// keys on the face alone and cannot carry variation coordinates).
    scale_context: swash::scale::ScaleContext,
}

struct LoadedFont {
    font: Arc<CosmicTextFont>,
    features: CosmicFontFeatures,
    is_known_emoji_font: bool,
    /// Cassetta patch: normalized variation coordinates of this instance; empty for the face's
    /// default instance.
    coords: Vec<swash::NormalizedCoord>,
}

impl CosmicTextSystem {
    pub(crate) fn new() -> Self {
        // todo(linux) make font loading non-blocking
        let font_system = new_font_system();

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            swash_cache: SwashCache::new(),
            scratch: ShapeBuffer::default(),
            loaded_fonts: Vec::new(),
            font_ids_by_family_cache: HashMap::default(),
            variable_instances: HashMap::default(),
            scale_context: swash::scale::ScaleContext::new(),
        }))
    }
}

/// Cassetta patch: `FontSystem::new()` with [`LinuxFallback`] in place of cosmic-text's
/// `PlatformFallback`. The database, its default families and the locale are built exactly as
/// `FontSystem::new_with_fonts` builds them (cosmic-text 0.14.2 `font/system.rs`).
fn new_font_system() -> FontSystem {
    let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
    let mut db = cosmic_text::fontdb::Database::new();
    db.load_system_fonts();
    db.set_monospace_family("Noto Sans Mono");
    db.set_sans_serif_family("Open Sans");
    db.set_serif_family("DejaVu Serif");
    let fallback = LinuxFallback::new(&db, &locale);
    FontSystem::new_with_locale_and_db_and_fallback(locale, db, fallback)
}

/// Cassetta patch: the font fallback lists, with cosmic-text's last-resort stage turned off.
///
/// cosmic-text's `FontFallbackIter` tries, for a cluster the requested font cannot draw, the
/// script's named families, then the common families, then **every face in the database**
/// (except `forbidden_fallback` families), loading each one (`make_shared_face_data` maps the
/// file) to shape the run with it. A cluster no single face can draw (a Latin base carrying a
/// Telugu combining mark, say) never stops that walk: one title mapped all 649 font files of a
/// desktop with Noto CJK installed (+102 MiB resident, a 52 ms shape). The walk cannot be cut
/// short from gpui (the iterator lives inside `ShapeLine::new`), so this `Fallback`:
///
/// - names every family in the database as forbidden, so the last stage loads nothing (faces
///   added later through `add_fonts` are not on the list and stay reachable);
/// - keeps cosmic-text's script lists, and for a script it has no list for names the installed
///   `Noto Sans <Script>` / `Noto Serif <Script>` family, resolved once from the database's
///   family names (no file is opened);
/// - extends the common list with the families the last stage used to find for common
///   characters: `Noto Sans Symbols 2` (cosmic-text spells it `Symbols2`, which matches
///   nothing), `Noto Sans Math`, `Noto Music`, and the locale's Noto Sans CJK family (fullwidth
///   forms and CJK punctuation in an otherwise Latin string).
struct LinuxFallback {
    common: Vec<&'static str>,
    forbidden: &'static [&'static str],
    by_script: HashMap<unicode_script::Script, Vec<&'static str>>,
}

impl LinuxFallback {
    fn new(db: &cosmic_text::fontdb::Database, locale: &str) -> Self {
        use cosmic_text::{Fallback, PlatformFallback};
        use unicode_script::Script;
        // Each face's first family name, the families with the most faces first (the last
        // stage checks a face against this list in order, so most faces match early).
        let mut counts: HashMap<&str, usize> = HashMap::default();
        for face in db.faces() {
            if let Some((name, _)) = face.families.first() {
                *counts.entry(name.as_str()).or_default() += 1;
            }
        }
        let mut names = counts.into_iter().collect::<Vec<_>>();
        names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
        let forbidden: &'static [&'static str] = Box::leak(
            names
                .iter()
                .map(|(name, _)| leak(name))
                .collect::<Vec<_>>()
                .into(),
        );

        let mut by_script: HashMap<Script, Vec<&'static str>> = HashMap::default();
        for &family in forbidden {
            for prefix in ["Noto Sans ", "Noto Serif "] {
                let Some(rest) = family.strip_prefix(prefix) else {
                    continue;
                };
                if let Some(script) = noto_script(rest) {
                    let list = by_script.entry(script).or_default();
                    // Sans before Serif whatever the order found.
                    if prefix == "Noto Sans " {
                        list.insert(0, family);
                    } else {
                        list.push(family);
                    }
                }
            }
        }
        by_script.retain(|script, _| PlatformFallback.script_fallback(*script, locale).is_empty());

        let han = PlatformFallback.script_fallback(Script::Han, locale);
        let mut common = PlatformFallback.common_fallback().to_vec();
        common.extend(["Noto Sans Symbols 2", "Noto Sans Math", "Noto Music"]);
        common.extend_from_slice(han);
        Self {
            common,
            forbidden,
            by_script,
        }
    }
}

/// Cassetta patch: the script a Noto family is named for, from the name after "Noto Sans " /
/// "Noto Serif ": the Unicode full name with spaces (`Tai Le`), run together (`PhagsPa`,
/// `NKo`), or one of Noto's four abbreviations. Weights and variants (`Arabic UI`, `Thai
/// Looped`, `Telugu Black`) match nothing, so each script gets its plain family.
fn noto_script(name: &str) -> Option<unicode_script::Script> {
    use unicode_script::Script;
    let full = match name {
        "EgyptHiero" => "Egyptian_Hieroglyphs",
        "ImpAramaic" => "Imperial_Aramaic",
        "InsPahlavi" => "Inscriptional_Pahlavi",
        "PsaPahlavi" => "Psalter_Pahlavi",
        _ => "",
    };
    if !full.is_empty() {
        return Script::from_full_name(full);
    }
    let underscored = name.replace(' ', "_");
    if let Some(script) = Script::from_full_name(&underscored) {
        return Some(script);
    }
    // `PhagsPa` → `Phags_Pa`.
    let mut split = String::with_capacity(name.len() + 2);
    let mut previous_lower = false;
    for c in name.chars() {
        if c.is_uppercase() && previous_lower {
            split.push('_');
        }
        previous_lower = c.is_lowercase();
        split.push(c);
    }
    if let Some(script) = Script::from_full_name(&split) {
        return Some(script);
    }
    // `NKo` → `Nko`.
    let mut chars = name.chars();
    let first = chars.next()?;
    Script::from_full_name(&format!("{first}{}", chars.as_str().to_lowercase()))
}

impl cosmic_text::Fallback for LinuxFallback {
    fn common_fallback(&self) -> &[&'static str] {
        &self.common
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        self.forbidden
    }

    fn script_fallback(&self, script: unicode_script::Script, locale: &str) -> &[&'static str] {
        let named = cosmic_text::PlatformFallback.script_fallback(script, locale);
        if !named.is_empty() {
            return named;
        }
        self.by_script.get(&script).map_or(&[], Vec::as_slice)
    }
}

impl Default for CosmicTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for CosmicTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut result = self
            .0
            .read()
            .font_system
            .db()
            .faces()
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect_vec();
        result.sort();
        result.dedup();
        result
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        // todo(linux): Do we need to use CosmicText's Font APIs? Can we consolidate this to use font_kit?
        let mut state = self.0.write();
        let key = FontKey::new(font.family.clone(), font.features.clone());
        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&key) {
            font_ids.as_slice()
        } else {
            let font_ids = state.load_family(&font.family, &font.features)?;
            state.font_ids_by_family_cache.insert(key.clone(), font_ids);
            state.font_ids_by_family_cache[&key].as_ref()
        };

        // todo(linux) ideally we would make fontdb's `find_best_match` pub instead of using font-kit here
        let candidate_properties = candidates
            .iter()
            .map(|font_id| {
                let database_id = state.loaded_font(*font_id).font.id();
                let face_info = state.font_system.db().face(database_id).expect("");
                face_info_into_properties(face_info)
            })
            .collect::<SmallVec<[_; 4]>>();

        let ix =
            font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
                .context("requested font family contains no font matching the other parameters")?;

        let font_id = candidates[ix];
        Ok(state.variable_instance(font_id, font.weight))
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self
            .0
            .read()
            .loaded_font(font_id)
            .font
            .as_swash()
            .metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent, // todo(linux) confirm this is correct
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            // todo(linux): Compute this correctly
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.0.read();
        let loaded_font = lock.loaded_font(font_id);
        let glyph_metrics = loaded_font.font.as_swash().glyph_metrics(&loaded_font.coords);
        let glyph_id = glyph_id.0 as u16;
        // todo(linux): Compute this correctly
        // see https://github.com/servo/font-kit/blob/master/src/loaders/freetype.rs#L614-L620
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }
}

impl CosmicTextSystemState {
    fn loaded_font(&self, font_id: FontId) -> &LoadedFont {
        &self.loaded_fonts[font_id.0]
    }

    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let db = self.font_system.db_mut();
        for bytes in fonts {
            match bytes {
                Cow::Borrowed(embedded_font) => {
                    db.load_font_data(embedded_font.to_vec());
                }
                Cow::Owned(bytes) => {
                    db.load_font_data(bytes);
                }
            }
        }
        Ok(())
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
    ) -> Result<SmallVec<[FontId; 4]>> {
        // TODO: Determine the proper system UI font.
        let name = crate::text_system::font_name_with_fallbacks(name, "IBM Plex Sans");

        let families = self
            .font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| *name == family.0))
            .map(|face| (face.id, face.post_script_name.clone()))
            .collect::<SmallVec<[_; 4]>>();

        let mut loaded_font_ids = SmallVec::new();
        for (font_id, postscript_name) in families {
            let font = self
                .font_system
                .get_font(font_id)
                .context("Could not load font")?;

            // HACK: To let the storybook run and render Windows caption icons. We should actually do better font fallback.
            let allowed_bad_font_names = [
                "SegoeFluentIcons", // NOTE: Segoe fluent icons postscript name is inconsistent
                "Segoe Fluent Icons",
            ];

            if font.as_swash().charmap().map('m') == 0
                && !allowed_bad_font_names.contains(&postscript_name.as_str())
            {
                self.font_system.db_mut().remove_face(font.id());
                continue;
            };

            let font_id = FontId(self.loaded_fonts.len());
            loaded_font_ids.push(font_id);
            self.loaded_fonts.push(LoadedFont {
                font,
                features: features.try_into()?,
                is_known_emoji_font: check_is_known_emoji_font(&postscript_name),
                coords: Vec::new(),
            });
        }

        Ok(loaded_font_ids)
    }

    /// Cassetta patch: when `font_id` is the default instance of a variable face with a `wght`
    /// axis and `weight` is more than 50 away from the axis default, returns a `FontId` for that
    /// weight's instance (registered once, sharing the face). fontdb lists only a variable
    /// font's default instance, so without this every weight of such a family draws at its
    /// default.
    fn variable_instance(&mut self, font_id: FontId, weight: FontWeight) -> FontId {
        let loaded_font = self.loaded_font(font_id);
        if !loaded_font.coords.is_empty() {
            return font_id;
        }
        let face = loaded_font.font.as_swash();
        let Some(axis) = face
            .variations()
            .find_by_tag(swash::tag_from_bytes(b"wght"))
        else {
            return font_id;
        };
        let weight = weight.0.clamp(axis.min_value(), axis.max_value());
        if (weight - axis.default_value()).abs() <= 50.0 {
            return font_id;
        }
        let key = (font_id, weight.round() as u16);
        if let Some(instance) = self.variable_instances.get(&key) {
            return *instance;
        }
        let instance = LoadedFont {
            font: loaded_font.font.clone(),
            features: loaded_font.features.clone(),
            is_known_emoji_font: loaded_font.is_known_emoji_font,
            coords: face
                .variations()
                .normalized_coords([("wght", weight)])
                .collect(),
        };
        let instance_id = FontId(self.loaded_fonts.len());
        self.loaded_fonts.push(instance);
        self.variable_instances.insert(key, instance_id);
        instance_id
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let loaded_font = self.loaded_font(font_id);
        let glyph_metrics = loaded_font.font.as_swash().glyph_metrics(&loaded_font.coords);
        Ok(Size {
            width: glyph_metrics.advance_width(glyph_id.0 as u16),
            height: glyph_metrics.advance_height(glyph_id.0 as u16),
        })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.loaded_font(font_id).font.as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.loaded_fonts[params.font_id.0].font;
        let subpixel_shift = point(
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
            params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
        );
        let cache_key = CacheKey::new(
            font.id(),
            params.glyph_id.0 as u16,
            (params.font_size * params.scale_factor).into(),
            (subpixel_shift.x, subpixel_shift.y.trunc()),
            cosmic_text::CacheKeyFlags::empty(),
        )
        .0;
        let image = if self.loaded_fonts[params.font_id.0].coords.is_empty() {
            self.swash_cache
                .get_image(&mut self.font_system, cache_key)
                .clone()
        } else {
            self.render_variable_glyph(params.font_id, cache_key)
        }
        .with_context(|| format!("no image for {params:?}"))?;
        Ok(Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        })
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        } else {
            let bitmap_size = glyph_bounds.size;
            let font = &self.loaded_fonts[params.font_id.0].font;
            let subpixel_shift = point(
                params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
                params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
            );
            let cache_key = CacheKey::new(
                font.id(),
                params.glyph_id.0 as u16,
                (params.font_size * params.scale_factor).into(),
                (subpixel_shift.x, subpixel_shift.y.trunc()),
                cosmic_text::CacheKeyFlags::empty(),
            )
            .0;
            let mut image = if self.loaded_fonts[params.font_id.0].coords.is_empty() {
                self.swash_cache
                    .get_image(&mut self.font_system, cache_key)
                    .clone()
            } else {
                self.render_variable_glyph(params.font_id, cache_key)
            }
            .with_context(|| format!("no image for {params:?}"))?;

            if params.is_emoji {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }

            Ok((bitmap_size, image.data))
        }
    }

    /// Cassetta patch: renders a glyph of a variable instance the way cosmic-text's
    /// `swash_image` renders the default instance (same sources, format, hinting, offset), with
    /// the instance's variation coordinates applied.
    fn render_variable_glyph(
        &mut self,
        font_id: FontId,
        cache_key: CacheKey,
    ) -> Option<swash::scale::image::Image> {
        use swash::scale::{Render, Source, StrikeWith};
        let loaded_font = &self.loaded_fonts[font_id.0];
        let mut scaler = self
            .scale_context
            .builder(loaded_font.font.as_swash())
            .size(f32::from_bits(cache_key.font_size_bits))
            .hint(true)
            .normalized_coords(&loaded_font.coords)
            .build();
        Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .format(swash::zeno::Format::Alpha)
        .offset(swash::zeno::Vector::new(
            cache_key.x_bin.as_float(),
            cache_key.y_bin.as_float(),
        ))
        .render(&mut scaler, cache_key.glyph_id)
    }

    /// This is used when cosmic_text has chosen a fallback font instead of using the requested
    /// font, typically to handle some unicode characters. When this happens, `loaded_fonts` may not
    /// yet have an entry for this fallback font, and so one is added.
    ///
    /// Note that callers shouldn't use this `FontId` somewhere that will retrieve the corresponding
    /// `LoadedFont.features`, as it will have an arbitrarily chosen or empty value. The only
    /// current use of this field is for the *input* of `layout_line`, and so it's fine to use
    /// `font_id_for_cosmic_id` when computing the *output* of `layout_line`.
    fn font_id_for_cosmic_id(&mut self, id: cosmic_text::fontdb::ID) -> FontId {
        if let Some(ix) = self
            .loaded_fonts
            .iter()
            .position(|loaded_font| loaded_font.font.id() == id)
        {
            FontId(ix)
        } else {
            let font = self.font_system.get_font(id).unwrap();
            let face = self.font_system.db().face(id).unwrap();

            let font_id = FontId(self.loaded_fonts.len());
            self.loaded_fonts.push(LoadedFont {
                font,
                features: CosmicFontFeatures::new(),
                is_known_emoji_font: check_is_known_emoji_font(&face.post_script_name),
                coords: Vec::new(),
            });

            font_id
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let mut attrs_list = AttrsList::new(&Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            let loaded_font = self.loaded_font(run.font_id);
            let font = self.font_system.db().face(loaded_font.font.id()).unwrap();

            attrs_list.add_span(
                offs..(offs + run.len),
                &Attrs::new()
                    .metadata(run.font_id.0)
                    .family(Family::Name(&font.families.first().unwrap().0))
                    .stretch(font.stretch)
                    .style(font.style)
                    .weight(font.weight)
                    .font_features(loaded_font.features.clone()),
            );
            offs += run.len;
        }

        let line = ShapeLine::new(
            &mut self.font_system,
            text,
            &attrs_list,
            cosmic_text::Shaping::Advanced,
            4,
        );
        let mut layout_lines = Vec::with_capacity(1);
        line.layout_to_buffer(
            &mut self.scratch,
            font_size.0,
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            None,
            &mut layout_lines,
            None,
        );
        let layout = layout_lines.first().unwrap();

        let mut runs: Vec<ShapedRun> = Vec::new();
        // Cassetta patch: cosmic-text shapes a variable instance with the default instance's
        // advances; `x_shift` carries the HVAR difference along the line.
        let mut x_shift = 0.0;
        for glyph in &layout.glyphs {
            let mut font_id = FontId(glyph.metadata);
            let mut loaded_font = self.loaded_font(font_id);
            if loaded_font.font.id() != glyph.font_id {
                font_id = self.font_id_for_cosmic_id(glyph.font_id);
                loaded_font = self.loaded_font(font_id);
            }
            let is_emoji = loaded_font.is_known_emoji_font;
            let x = glyph.x + x_shift;
            if !loaded_font.coords.is_empty() {
                let face = loaded_font.font.as_swash();
                let scale = glyph.font_size / face.metrics(&[]).units_per_em as f32;
                let instance = face.glyph_metrics(&loaded_font.coords);
                let default = face.glyph_metrics(&[]);
                x_shift += (instance.advance_width(glyph.glyph_id)
                    - default.advance_width(glyph.glyph_id))
                    * scale;
            }

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && is_emoji {
                continue;
            }

            let shaped_glyph = ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji,
            };

            if let Some(last_run) = runs
                .last_mut()
                .filter(|last_run| last_run.font_id == font_id)
            {
                last_run.glyphs.push(shaped_glyph);
            } else {
                runs.push(ShapedRun {
                    font_id,
                    glyphs: vec![shaped_glyph],
                });
            }
        }

        LineLayout {
            font_size,
            width: (layout.w + x_shift).into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }
}

impl TryFrom<&FontFeatures> for CosmicFontFeatures {
    type Error = anyhow::Error;

    fn try_from(features: &FontFeatures) -> Result<Self> {
        let mut result = CosmicFontFeatures::new();
        for feature in features.0.iter() {
            let name_bytes: [u8; 4] = feature
                .0
                .as_bytes()
                .try_into()
                .context("Incorrect feature flag format")?;

            let tag = cosmic_text::FeatureTag::new(&name_bytes);

            result.set(tag, feature.1);
        }
        Ok(result)
    }
}

impl From<RectF> for Bounds<f32> {
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for cosmic_text::Weight {
    fn from(value: FontWeight) -> Self {
        cosmic_text::Weight(value.0 as u16)
    }
}

impl From<FontStyle> for cosmic_text::Style {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => cosmic_text::Style::Normal,
            FontStyle::Italic => cosmic_text::Style::Italic,
            FontStyle::Oblique => cosmic_text::Style::Oblique,
        }
    }
}

fn font_into_properties(font: &crate::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            crate::FontStyle::Normal => font_kit::properties::Style::Normal,
            crate::FontStyle::Italic => font_kit::properties::Style::Italic,
            crate::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: Default::default(),
    }
}

fn face_info_into_properties(
    face_info: &cosmic_text::fontdb::FaceInfo,
) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match face_info.style {
            cosmic_text::Style::Normal => font_kit::properties::Style::Normal,
            cosmic_text::Style::Italic => font_kit::properties::Style::Italic,
            cosmic_text::Style::Oblique => font_kit::properties::Style::Oblique,
        },
        // both libs use the same values for weight
        weight: font_kit::properties::Weight(face_info.weight.0.into()),
        stretch: match face_info.stretch {
            cosmic_text::Stretch::Condensed => font_kit::properties::Stretch::CONDENSED,
            cosmic_text::Stretch::Expanded => font_kit::properties::Stretch::EXPANDED,
            cosmic_text::Stretch::ExtraCondensed => font_kit::properties::Stretch::EXTRA_CONDENSED,
            cosmic_text::Stretch::ExtraExpanded => font_kit::properties::Stretch::EXTRA_EXPANDED,
            cosmic_text::Stretch::Normal => font_kit::properties::Stretch::NORMAL,
            cosmic_text::Stretch::SemiCondensed => font_kit::properties::Stretch::SEMI_CONDENSED,
            cosmic_text::Stretch::SemiExpanded => font_kit::properties::Stretch::SEMI_EXPANDED,
            cosmic_text::Stretch::UltraCondensed => font_kit::properties::Stretch::ULTRA_CONDENSED,
            cosmic_text::Stretch::UltraExpanded => font_kit::properties::Stretch::ULTRA_EXPANDED,
        },
    }
}

fn check_is_known_emoji_font(postscript_name: &str) -> bool {
    // TODO: Include other common emoji fonts
    postscript_name == "NotoColorEmoji"
}

#[cfg(test)]
mod tests {
    use super::noto_script;
    use unicode_script::Script;

    /// Cassetta patch: the Noto family names map to their scripts; weights and variants do not.
    #[test]
    fn noto_family_names_map_to_scripts() {
        assert_eq!(noto_script("Telugu"), Some(Script::Telugu));
        assert_eq!(noto_script("Tai Le"), Some(Script::Tai_Le));
        assert_eq!(noto_script("Ol Chiki"), Some(Script::Ol_Chiki));
        assert_eq!(noto_script("PhagsPa"), Some(Script::Phags_Pa));
        assert_eq!(noto_script("NKo"), Some(Script::Nko));
        assert_eq!(noto_script("EgyptHiero"), Some(Script::Egyptian_Hieroglyphs));
        assert_eq!(noto_script("Telugu Black"), None);
        assert_eq!(noto_script("Arabic UI"), None);
        assert_eq!(noto_script("Thai Looped"), None);
        assert_eq!(noto_script("CJK SC"), None);
        assert_eq!(noto_script("Symbols 2"), None);
        assert_eq!(noto_script("Mono"), None);
        assert_eq!(noto_script(""), None);
    }
}
