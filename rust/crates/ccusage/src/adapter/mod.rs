use std::{
    path::{Path, PathBuf},
    thread,
};

pub(crate) mod all;
pub(crate) mod amp;
pub(crate) mod claude;
pub(crate) mod codebuff;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod droid;
pub(crate) mod gemini;
pub(crate) mod goose;
pub(crate) mod hermes;
pub(crate) mod kilo;
pub(crate) mod kimi;
pub(crate) mod ncode;
pub(crate) mod openclaw;
pub(crate) mod opencode;
pub(crate) mod pi;
pub(crate) mod qwen;
pub(crate) mod zcode;

pub(crate) fn read_files_parallel<T, F>(files: &[PathBuf], single_thread: bool, read: F) -> Vec<T>
where
    T: Send,
    F: Fn(&Path) -> T + Sync,
{
    let worker_count = if single_thread {
        1
    } else {
        thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(files.len())
    };
    if worker_count <= 1 {
        return files.iter().map(|file| read(file.as_path())).collect();
    }

    let chunks = crate::chunk_file_indexes_by_size(files, worker_count);
    let read = &read;
    thread::scope(|scope| {
        let handles = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .into_iter()
                        .map(|index| (index, read(files[index].as_path())))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(files.len());
        results.resize_with(files.len(), || None);
        for (index, value) in handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("file read worker panicked"))
        {
            results[index] = Some(value);
        }
        results
            .into_iter()
            .map(|value| value.expect("file read worker returned every file"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::read_files_parallel;
    use ccusage_test_support::Fixture;

    #[test]
    fn parallel_reads_preserve_input_order() {
        let fixture = Fixture::new();
        let files = (0..64)
            .map(|index| {
                fixture.write_file(
                    format!("file-{index:03}.txt"),
                    format!("{index}:{}", "x".repeat((index % 11) * 32 + 1)),
                )
            })
            .collect::<Vec<_>>();
        let read = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .unwrap()
                .split(':')
                .next()
                .unwrap()
                .to_string()
        };

        assert_eq!(
            read_files_parallel(&files, false, read),
            read_files_parallel(&files, true, read)
        );
    }
}
