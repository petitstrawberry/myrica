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
/// Myrica registers supported font files installed below the conventional font roots.
/// `MYRICA_FONT_PATH` may add more roots using the platform path separator.
#[cfg(target_os = "scarlet")]
pub(super) fn load_font_context() -> FontContext {
    use std::path::{Path, PathBuf};

    use parley::fontique::{Blob, GenericFamily};

    let mut font_roots: Vec<PathBuf> = std::env::var_os("MYRICA_FONT_PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    font_roots.push(PathBuf::from("/share/fonts"));
    font_roots.retain(|path| path.is_dir());
    font_roots.sort_unstable();
    font_roots.dedup();

    let mut context = FontContext::new();
    // Fontique's path loader uses memmap2, whose non-Unix stub always returns
    // `Unsupported`. Keep the font data in memory until Scarlet has file-backed
    // mappings or opts into a compatible memmap2 backend.
    let mut loaded_files = 0usize;
    let mut loaded_faces = 0usize;
    for root in &font_roots {
        load_fonts_from_root(&mut context, root, &mut loaded_files, &mut loaded_faces);
    }

    eprintln!("[myrica:fonts] loaded {loaded_faces} face(s) from {loaded_files} file(s)");

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

    fn load_fonts_from_root(
        context: &mut FontContext,
        root: &Path,
        loaded_files: &mut usize,
        loaded_faces: &mut usize,
    ) {
        const MAX_DEPTH: usize = 16;

        let mut directories = vec![(root.to_path_buf(), 0usize)];
        while let Some((directory, depth)) = directories.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                // Native getdents implementations can expose these entries.
                // Recursing into them revisits the font tree (and its parents).
                if entry.file_name() == "." || entry.file_name() == ".." {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    if depth < MAX_DEPTH {
                        directories.push((path, depth + 1));
                    }
                    continue;
                }
                if !is_supported_font(&path) {
                    continue;
                }

                let data = match std::fs::read(&path) {
                    Ok(data) => data,
                    Err(error) => {
                        eprintln!("[myrica:fonts] could not read {}: {error}", path.display());
                        continue;
                    }
                };
                let registered = context.collection.register_fonts(Blob::from(data), None);
                *loaded_files += 1;
                *loaded_faces += registered
                    .iter()
                    .map(|(_, fonts)| fonts.len())
                    .sum::<usize>();
            }
        }
    }

    fn is_supported_font(path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "ttf" | "otf" | "ttc" | "otc"
                )
            })
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
