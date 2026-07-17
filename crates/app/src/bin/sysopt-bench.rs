use anyhow::{Context, Result};
use policy::{Classifier, PolicyEngine, ResourcePolicyConfig, SignatureDb, SystemMode};
use serde::Serialize;
use smart_cache::{SmartCache, SmartCacheConfig};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use telemetry::{PowerSnapshot, PressureSnapshot, ProcessSample, SystemSnapshot};

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    schema_version: u32,
    generated_at_unix_secs: u64,
    platform: String,
    classification: Vec<ClassificationResult>,
    smart_cache: Vec<CacheResult>,
}

#[derive(Debug, Serialize)]
struct ClassificationResult {
    process_count: usize,
    iterations: usize,
    total_microseconds: u128,
    average_microseconds: f64,
    p95_microseconds: u128,
    actions_last_iteration: usize,
}

#[derive(Debug, Serialize)]
struct CacheResult {
    dataset: String,
    files: usize,
    bytes: u64,
    elapsed_milliseconds: u128,
    planned_files: usize,
    planned_bytes: u64,
}

fn main() -> Result<()> {
    let mut output = PathBuf::from("sysopt-benchmark.json");
    let mut dataset = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = PathBuf::from(args.next().context("--output requiere ruta")?),
            "--dataset" => {
                dataset = Some(PathBuf::from(
                    args.next().context("--dataset requiere ruta")?,
                ))
            }
            "-h" | "--help" => {
                println!("sysopt-bench [--output archivo.json] [--dataset directorio]");
                return Ok(());
            }
            other => anyhow::bail!("argumento desconocido: {other}"),
        }
    }

    let signatures = SignatureDb::try_embedded_default()?;
    let classifier = Classifier::rules();
    let mut classification = Vec::new();
    for count in [100usize, 500, 2_000] {
        classification.push(benchmark_classification(count, &classifier, &signatures)?);
    }

    let mut cleanup = None;
    let dataset = match dataset {
        Some(path) => path,
        None => {
            let path = std::env::temp_dir().join(format!(
                "sysopt-bench-{}-{}",
                std::process::id(),
                unix_now_secs()
            ));
            create_dataset(&path, 512, 64 * 1024)?;
            cleanup = Some(path.clone());
            path
        }
    };
    let smart_cache = vec![benchmark_cache(&dataset)?];
    let result = BenchmarkResult {
        schema_version: 1,
        generated_at_unix_secs: unix_now_secs(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        classification,
        smart_cache,
    };
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, serde_json::to_vec_pretty(&result)?)?;
    println!("benchmark guardado en {}", output.display());
    if let Some(path) = cleanup {
        let _ = fs::remove_dir_all(path);
    }
    Ok(())
}

fn benchmark_classification(
    process_count: usize,
    classifier: &Classifier,
    signatures: &SignatureDb,
) -> Result<ClassificationResult> {
    let snapshot = synthetic_snapshot(process_count);
    let iterations = if process_count <= 100 {
        300
    } else if process_count <= 500 {
        150
    } else {
        60
    };
    let mut samples = Vec::with_capacity(iterations);
    let mut actions_last_iteration = 0usize;
    let started = Instant::now();
    for _ in 0..iterations {
        let iteration = Instant::now();
        let prediction = classifier.predict(&snapshot, signatures)?;
        let actions = PolicyEngine::decide_with_config(
            prediction.mode,
            &snapshot,
            signatures,
            &ResourcePolicyConfig::default(),
        );
        actions_last_iteration = actions.len();
        samples.push(iteration.elapsed().as_micros());
    }
    samples.sort_unstable();
    let total = started.elapsed().as_micros();
    let p95_index = ((samples.len() - 1) as f64 * 0.95).round() as usize;
    Ok(ClassificationResult {
        process_count,
        iterations,
        total_microseconds: total,
        average_microseconds: total as f64 / iterations as f64,
        p95_microseconds: samples[p95_index.min(samples.len() - 1)],
        actions_last_iteration,
    })
}

fn benchmark_cache(root: &Path) -> Result<CacheResult> {
    let (files, bytes) = dataset_stats(root)?;
    let state_path = std::env::temp_dir().join(format!(
        "sysopt-bench-cache-state-{}-{}.json",
        std::process::id(),
        unix_now_secs()
    ));
    let mut cache = SmartCache::new(SmartCacheConfig {
        roots: vec![root.to_path_buf()],
        state_path: Some(state_path.clone()),
        persist_state: false,
        max_scan_entries: files.saturating_add(32).max(128),
        ..SmartCacheConfig::default()
    })?;
    let started = Instant::now();
    let report = cache.warm_paths_now(&[root.to_path_buf()], false)?;
    let _ = fs::remove_file(state_path);
    Ok(CacheResult {
        dataset: root.display().to_string(),
        files,
        bytes,
        elapsed_milliseconds: started.elapsed().as_millis(),
        planned_files: report.files_warmed,
        planned_bytes: report.bytes_warmed,
    })
}

fn synthetic_snapshot(process_count: usize) -> SystemSnapshot {
    let names = [
        "code", "rustc", "chrome", "discord", "docker", "steam", "obs", "updater", "indexer",
        "blender", "node", "java",
    ];
    let processes = (0..process_count)
        .map(|index| ProcessSample {
            pid: 10_000u32.saturating_add(index as u32),
            start_time: 1_700_000_000u64.saturating_add(index as u64),
            name: names[index % names.len()].to_owned(),
            cpu_percent: ((index * 17) % 100) as f32,
            memory_bytes: ((index % 32 + 1) as u64) * 16 * 1024 * 1024,
            read_bytes: ((index * 4096) % 1_000_000) as u64,
            written_bytes: ((index * 2048) % 500_000) as u64,
            cwd: Some(PathBuf::from(format!("/tmp/project-{}", index % 16))),
            executable: None,
        })
        .collect::<Vec<_>>();
    SystemSnapshot {
        global_cpu_percent: 72.0,
        used_memory_bytes: 8 * 1024 * 1024 * 1024,
        available_memory_bytes: 8 * 1024 * 1024 * 1024,
        total_memory_bytes: 16 * 1024 * 1024 * 1024,
        process_count,
        top_processes: processes.clone(),
        io_processes: processes.into_iter().take(8).collect(),
        pressure: PressureSnapshot::default(),
        power: PowerSnapshot::default(),
    }
}

fn create_dataset(root: &Path, files: usize, bytes_per_file: usize) -> Result<()> {
    fs::create_dir_all(root)?;
    let data = vec![0x5Au8; bytes_per_file];
    for index in 0..files {
        let directory = root.join(format!("group-{}", index % 16));
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(format!("file-{index}.bin")), &data)?;
    }
    Ok(())
}

fn dataset_stats(root: &Path) -> Result<(usize, u64)> {
    const MAX_DATASET_ENTRIES: usize = 1_000_000;
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("no se pudo inspeccionar {}", root.display()))?;
    anyhow::ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "el dataset debe ser un directorio regular, no un enlace: {}",
        root.display()
    );

    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut visited = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path)
            .with_context(|| format!("no se pudo recorrer {}", path.display()))?
        {
            let entry = entry?;
            visited = visited.saturating_add(1);
            anyhow::ensure!(
                visited <= MAX_DATASET_ENTRIES,
                "dataset excede el límite de {MAX_DATASET_ENTRIES} entradas"
            );
            let file_type = entry.file_type()?;
            // No seguir symlinks/junctions: un benchmark no debe escapar del
            // dataset ni quedar atrapado en ciclos de directorios.
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let metadata = entry.metadata()?;
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok((files, bytes))
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
