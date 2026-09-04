//! Platform font discovery for the Blitz backend.

use blitz_dom::FontContext;

/// Build a font context backed by the host platform's native font database.
#[cfg(not(target_os = "scarlet"))]
pub(super) fn load_font_context() -> FontContext {
    FontContext::new()
}

/// Build a font context from Scarlet's installed font directories.
///
/// Scarlet does not yet expose a system font-discovery service. Until it does,
/// Myrica registers every font installed below the conventional font roots.
/// `MYRICA_FONT_PATH` may add more roots using the platform path separator.
#[cfg(target_os = "scarlet")]
pub(super) fn load_font_context() -> FontContext {
    use std::path::PathBuf;

    use parley::fontique::GenericFamily;

    let mut font_roots: Vec<PathBuf> = std::env::var_os("MYRICA_FONT_PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    font_roots.extend([
        PathBuf::from("/fonts"),
        PathBuf::from("/system/share/fonts"),
    ]);
    font_roots.retain(|path| path.is_dir());
    font_roots.sort_unstable();
    font_roots.dedup();

    let mut context = FontContext::new();
    context.collection.load_fonts_from_paths(&font_roots);

    // Without a platform font database, Fontique cannot infer Scarlet's
    // generic CSS families. Preserve every installed family as a fallback so
    // that pages can still select a font containing the requested script.
    let family_names: Vec<String> = context
        .collection
        .family_names()
        .map(str::to_owned)
        .collect();
    let family_ids: Vec<_> = family_names
        .iter()
        .filter_map(|name| context.collection.family_id(name))
        .collect();
    for generic in [
        GenericFamily::SansSerif,
        GenericFamily::Serif,
        GenericFamily::Monospace,
        GenericFamily::SystemUi,
    ] {
        context
            .collection
            .append_generic_families(generic, family_ids.iter().copied());
    }

    context
}

#[cfg(test)]
mod tests {
    use super::load_font_context;

    #[test]
    #[cfg(not(target_os = "scarlet"))]
    fn host_discovers_at_least_one_font_family() {
        let mut context = load_font_context();
        assert!(context.collection.family_names().next().is_some());
    }
}
