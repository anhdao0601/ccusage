use crate::{
    cli::SharedArgs, collect_files_with_extension, parse_tz, read_files_parallel, LoadedEntry,
    PricingMap, Result,
};

use super::{parser, paths};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Amp, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let mut entries = Vec::new();
    let tz = parse_tz(shared.timezone.as_deref());
    for path in paths::paths()? {
        let threads_dir = path.join("threads");
        let mut files = Vec::new();
        collect_files_with_extension(&threads_dir, "json", &mut files);
        let loaded = read_files_parallel(&files, shared.single_thread, |file| {
            parser::read_thread_file(file, tz.as_ref(), shared.mode, Some(pricing))
        });
        for file_entries in loaded {
            entries.extend(file_entries?);
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}
