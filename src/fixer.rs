use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use pyrefly_python::module_name::ModuleName;

use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::imports::ImportOccurrence;
use crate::output::LifeGuardAnalysis;
use crate::source_map::Sources;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TargetVersion {
    Py314,
    Py315,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub offset: u32,
    pub text: String,
}

fn defines_lazy_modules(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim_start();
        line.strip_prefix("__lazy_modules__").is_some_and(|rest| {
            let rest = rest.trim_start();
            rest.starts_with('=') || rest.starts_with(':')
        })
    })
}

fn edits_for_occurrences(
    occurrences: &[&ImportOccurrence],
    target_version: TargetVersion,
    content: Option<&str>,
) -> Vec<FileEdit> {
    match target_version {
        TargetVersion::Py315 => {
            // Multiple names in one statement have the same statement offset. A
            // keyword belongs to the statement, not each imported name.
            let mut offsets: Vec<u32> = occurrences
                .iter()
                .map(|occ| u32::from(occ.offset))
                .collect();
            offsets.sort_unstable();
            offsets.dedup();
            offsets
                .into_iter()
                .map(|offset| FileEdit {
                    offset,
                    text: "lazy ".to_owned(),
                })
                .collect()
        }
        TargetVersion::Py314 => {
            // Rewriting an existing declaration is deliberately out of scope:
            // do not risk changing a hand-authored collection.
            if content.is_some_and(defines_lazy_modules) {
                return Vec::new();
            }

            let mut targets: Vec<&str> =
                occurrences.iter().map(|occ| occ.target.as_str()).collect();
            targets.sort_unstable();
            targets.dedup();

            let declaration = match targets.as_slice() {
                [] => return Vec::new(),
                [target] => format!("__lazy_modules__ = (\"{target}\",)\n"),
                _ => format!(
                    "__lazy_modules__ = ({})\n",
                    targets
                        .iter()
                        .map(|target| format!("\"{target}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let offset = occurrences
                .iter()
                .map(|occ| u32::from(occ.offset))
                .min()
                .expect("non-empty occurrences have an offset");
            vec![FileEdit {
                offset,
                text: declaration,
            }]
        }
    }
}

pub fn generate_and_apply_fixes(
    analysis: &LifeGuardAnalysis,
    occurrences_map: &AHashMap<ModuleName, Vec<ImportOccurrence>>,
    sources: &Sources,
    target_version: TargetVersion,
    dry_run: bool,
) -> Result<()> {
    let mut edits_by_file: AHashMap<PathBuf, Vec<FileEdit>> = AHashMap::default();

    for (module_name, occurrences) in occurrences_map {
        // 1. Skip if the file being edited is in `load_imports_eagerly`.
        // The mapping from `module_name` to file is defined by `sources.get_source_path(module_name)` below.
        if analysis.output.load_imports_eagerly.contains(module_name) {
            continue;
        }

        // We only edit files that actually map to a local source file.
        let path = match sources.get_source_path(module_name) {
            Some(p) => p,
            None => continue, // Cannot edit builtins or missing files.
        };

        // 2. Filter eligible occurrences. A `lazy` keyword applies to the
        // entire statement, so skip a statement if any of its imported names
        // is ineligible or it is already lazy.
        let blocked_offsets: AHashSet<u32> = if target_version == TargetVersion::Py315 {
            occurrences
                .iter()
                .filter(|occ| {
                    occ.is_lazy || !analysis.output.lazy_eligible.contains_key(&occ.target)
                })
                .map(|occ| u32::from(occ.offset))
                .collect()
        } else {
            AHashSet::default()
        };
        let mut eligible_occurrences = Vec::new();
        for occ in occurrences {
            if !(target_version == TargetVersion::Py315
                && (occ.is_lazy || blocked_offsets.contains(&u32::from(occ.offset))))
                && analysis.output.lazy_eligible.contains_key(&occ.target)
            {
                eligible_occurrences.push(occ);
            }
        }

        if eligible_occurrences.is_empty() {
            continue;
        }

        let content = match target_version {
            TargetVersion::Py314 => match fs::read_to_string(&path) {
                Ok(content) => Some(content),
                Err(_) => continue,
            },
            TargetVersion::Py315 => None,
        };
        let edits =
            edits_for_occurrences(&eligible_occurrences, target_version, content.as_deref());
        if !edits.is_empty() {
            edits_by_file.entry(path).or_default().extend(edits);
        }
    }

    if edits_by_file.is_empty() {
        println!("No fixes to apply.");
        return Ok(());
    }

    let mut success_count = 0;
    let mut fail_count = 0;

    for (path, mut edits) in edits_by_file {
        // Sort back-to-front so that applying an edit doesn't invalidate subsequent offsets.
        edits.sort_by(|a, b| b.offset.cmp(&a.offset));

        if dry_run {
            println!("Would fix {} ({} edits)", path.display(), edits.len());
            for edit in &edits {
                println!("  @ offset {}: {:?}", edit.offset, edit.text);
            }
            success_count += 1;
            continue;
        }

        match apply_edits_to_file(&path, &edits) {
            Ok(_) => {
                success_count += 1;
            }
            Err(e) => {
                eprintln!("Failed to apply fixes to {}: {}", path.display(), e);
                fail_count += 1;
            }
        }
    }

    if dry_run {
        println!(
            "Dry run complete. {} files would be modified.",
            success_count
        );
    } else {
        println!(
            "Fixes applied to {} files. ({} failed)",
            success_count, fail_count
        );
    }

    Ok(())
}

fn apply_edits_to_file(path: &PathBuf, edits: &[FileEdit]) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let mut modified = content.clone();

    for edit in edits {
        let offset = edit.offset as usize;
        // Verify offset is within bounds
        if offset <= modified.len() {
            // Find the character boundary if offset is not aligned (rare, but possible with non-ASCII).
            // Ruff's TextSize is in bytes, so offset should exactly map to a byte index.
            if modified.is_char_boundary(offset) {
                modified.insert_str(offset, &edit.text);
            } else {
                anyhow::bail!("Offset {} is not on a char boundary", offset);
            }
        } else {
            anyhow::bail!("Offset {} is out of bounds", offset);
        }
    }

    // Write to a temporary file first, then atomically replace.
    let parent = path.parent().unwrap_or(std::path::Path::new(""));
    let temp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));

    fs::write(&temp_path, modified)?;
    fs::rename(&temp_path, path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use pyrefly_python::module_name::ModuleName;
    use ruff_text_size::TextSize;

    use super::*;

    #[test]
    fn py315_inserts_one_keyword_per_statement_at_byte_offsets() {
        let occurrences = [
            ImportOccurrence {
                target: ModuleName::from_str("alpha"),
                offset: TextSize::from(7),
                is_import_from: false,
                is_lazy: false,
            },
            ImportOccurrence {
                target: ModuleName::from_str("beta"),
                offset: TextSize::from(7),
                is_import_from: false,
                is_lazy: false,
            },
            ImportOccurrence {
                target: ModuleName::from_str("gamma"),
                offset: TextSize::from(25),
                is_import_from: true,
                is_lazy: false,
            },
        ];
        let refs = occurrences.iter().collect::<Vec<_>>();

        assert_eq!(
            edits_for_occurrences(&refs, TargetVersion::Py315, None),
            vec![
                FileEdit {
                    offset: 7,
                    text: "lazy ".to_owned()
                },
                FileEdit {
                    offset: 25,
                    text: "lazy ".to_owned()
                },
            ]
        );
    }

    #[test]
    fn py314_creates_a_sorted_tuple_and_skips_existing_declaration() {
        let occurrences = [
            ImportOccurrence {
                target: ModuleName::from_str("zebra"),
                offset: TextSize::from(20),
                is_import_from: false,
                is_lazy: false,
            },
            ImportOccurrence {
                target: ModuleName::from_str("apple"),
                offset: TextSize::from(10),
                is_import_from: true,
                is_lazy: false,
            },
        ];
        let refs = occurrences.iter().collect::<Vec<_>>();

        assert_eq!(
            edits_for_occurrences(&refs, TargetVersion::Py314, Some("import apple\n")),
            vec![FileEdit {
                offset: 10,
                text: "__lazy_modules__ = (\"apple\", \"zebra\")\n".to_owned(),
            }]
        );
        assert!(
            edits_for_occurrences(
                &refs,
                TargetVersion::Py314,
                Some("__lazy_modules__: tuple[str, ...] = ()\n"),
            )
            .is_empty()
        );
    }
}
