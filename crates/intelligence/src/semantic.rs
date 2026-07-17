use anyhow::{Context, Result};
use fs2::FileExt;
use hf_hub::{api::sync::ApiBuilder, Cache, Repo, RepoType};
use model2vec_rs::model::StaticModel;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use telemetry::ProcessSample;

const GIB: u64 = 1024 * 1024 * 1024;
const REQUIRED_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];
const MAX_SAFETENSORS_HEADER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOKENIZER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_MODEL_BYTES: u64 = 512 * 1024 * 1024;

// Revisiones inmutables verificadas para impedir que una modificación posterior de `main`
// cambie silenciosamente el modelo instalado. Los modelos remotos personalizados deben
// declarar una revisión SHA-1 completa mediante `repositorio@commit`.
const POTION_2M_REVISION: &str = "389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da";
const POTION_4M_REVISION: &str = "9b3cff412d30be9ae8603fe10224c224f3401869";
const POTION_8M_REVISION: &str = "ad78550e2b7d7f7a39d2abbc81b079aea6c20287";

const PROTOTYPES: &[(&str, &str)] = &[
    (
        "ide",
        "integrated development environment source code editor programming developer workspace",
    ),
    (
        "language_server",
        "programming language server code intelligence compiler analysis autocomplete diagnostics",
    ),
    (
        "build_tool",
        "software build tool compiler linker package manager dependency compilation",
    ),
    (
        "mobile_tooling",
        "android mobile application development emulator gradle studio device bridge",
    ),
    (
        "container_vm",
        "container virtual machine docker kubernetes virtualization development environment",
    ),
    (
        "game",
        "video game gaming application game engine launcher interactive entertainment",
    ),
    (
        "creative",
        "creative design photo image video audio editing animation three dimensional graphics",
    ),
    (
        "media_encoder",
        "video audio media encoding transcoding rendering export compression",
    ),
    (
        "streaming",
        "live streaming broadcasting screen capture recording audio mixer creator software",
    ),
    (
        "browser",
        "web browser internet application website tabs browsing",
    ),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedModelSpec {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticHint {
    pub category: String,
    pub similarity: f32,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticModelInfo {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub cache_dir: PathBuf,
    pub embedding_dimensions: usize,
    pub prototype_count: usize,
}

#[derive(Debug, Clone)]
struct PrototypeEmbedding {
    category: &'static str,
    vector: Vec<f32>,
}

pub struct SemanticProcessClassifier {
    model_id: String,
    revision: Option<String>,
    model: StaticModel,
    prototypes: Vec<PrototypeEmbedding>,
    cache: HashMap<String, Option<SemanticHint>>,
    cache_order: VecDeque<String>,
    max_cache_entries: usize,
    min_similarity: f32,
    healthy: bool,
}

impl SemanticProcessClassifier {
    /// Carga únicamente desde un directorio local o desde la caché. Nunca accede a la red.
    pub fn load_cached(
        configured_model_id: &str,
        cache_dir: &Path,
        total_memory_bytes: u64,
        min_similarity: f32,
        max_cache_entries: usize,
    ) -> Result<Self> {
        let spec = resolve_model_spec(configured_model_id, total_memory_bytes);
        let local_model_dir = locate_local_or_cached_model(&spec, cache_dir)?;
        Self::from_local_dir(spec, &local_model_dir, min_similarity, max_cache_entries)
    }

    fn from_local_dir(
        spec: ResolvedModelSpec,
        local_model_dir: &Path,
        min_similarity: f32,
        max_cache_entries: usize,
    ) -> Result<Self> {
        validate_model_directory(local_model_dir)?;
        let local_model_path = local_model_dir.to_string_lossy();
        let model = guarded_model_call("carga del modelo", || {
            StaticModel::from_pretrained(local_model_path.as_ref(), None, None, None)
        })?
        .with_context(|| {
            format!(
                "no se pudo cargar el modelo {} desde {}",
                spec.model_id,
                local_model_dir.display()
            )
        })?;
        let texts = PROTOTYPES
            .iter()
            .map(|(_, text)| (*text).to_owned())
            .collect::<Vec<_>>();
        let vectors = guarded_model_call("tokenización de prototipos", || {
            model.encode_with_args(&texts, Some(64), 64)
        })?;
        anyhow::ensure!(
            vectors.len() == PROTOTYPES.len(),
            "el modelo semántico devolvió una cantidad inesperada de embeddings"
        );
        anyhow::ensure!(
            vectors.iter().all(|vector| {
                !vector.is_empty() && vector.iter().all(|value| value.is_finite())
            }),
            "el modelo semántico devolvió embeddings inválidos"
        );
        let prototypes = PROTOTYPES
            .iter()
            .zip(vectors)
            .map(|((category, _), vector)| PrototypeEmbedding { category, vector })
            .collect::<Vec<_>>();

        Ok(Self {
            model_id: spec.model_id,
            revision: spec.revision,
            model,
            prototypes,
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            max_cache_entries,
            min_similarity,
            healthy: true,
        })
    }

    pub fn info(&self, cache_dir: &Path) -> SemanticModelInfo {
        SemanticModelInfo {
            model_id: self.model_id.clone(),
            revision: self.revision.clone(),
            cache_dir: cache_dir.to_path_buf(),
            embedding_dimensions: self
                .prototypes
                .first()
                .map_or(0, |prototype| prototype.vector.len()),
            prototype_count: self.prototypes.len(),
        }
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    pub fn classify_process(&mut self, process: &ProcessSample) -> Option<SemanticHint> {
        let descriptor = process_descriptor(process);
        if descriptor.is_empty() {
            return None;
        }
        if let Some(cached) = self.cache.get(&descriptor) {
            return cached.clone();
        }

        if !self.healthy {
            return None;
        }
        let model = &self.model;
        let vector = match guarded_model_call("tokenización de proceso", || {
            model.encode_single(&descriptor)
        }) {
            Ok(vector) => vector,
            Err(_) => {
                // model2vec-rs 0.2.1 usa `expect` en su ruta de tokenización.
                // Una entrada o tokenizer defectuoso no debe derribar el servicio.
                self.healthy = false;
                return None;
            }
        };
        let result = self.classify_vector(&vector);
        self.remember(descriptor, result.clone());
        result
    }

    fn classify_vector(&self, vector: &[f32]) -> Option<SemanticHint> {
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let mut ranked = self
            .prototypes
            .iter()
            .map(|prototype| {
                (
                    prototype.category,
                    cosine_similarity(vector, &prototype.vector),
                )
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let (category, similarity) = *ranked.first()?;
        if similarity < self.min_similarity {
            return None;
        }
        let second = ranked.get(1).map_or(-1.0, |value| value.1);
        let margin = (similarity - second).max(0.0);
        let similarity_confidence = ((similarity - self.min_similarity)
            / (1.0 - self.min_similarity).max(0.01))
        .clamp(0.0, 1.0);
        let margin_confidence = (margin / 0.22).clamp(0.0, 1.0);
        let confidence = (similarity_confidence * 0.68 + margin_confidence * 0.32).clamp(0.0, 1.0);
        Some(SemanticHint {
            category: category.to_owned(),
            similarity,
            confidence,
        })
    }

    fn remember(&mut self, key: String, value: Option<SemanticHint>) {
        if let Some(existing) = self.cache.get_mut(&key) {
            *existing = value;
            return;
        }
        self.cache_order.push_back(key.clone());
        self.cache.insert(key, value);
        while self.cache.len() > self.max_cache_entries {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.cache.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

/// Descarga de forma bloqueante, valida los artefactos y ejecuta una inferencia de prueba.
/// Se usa por el comando de diagnóstico explícito y por el trabajador en segundo
/// plano del servicio; los instaladores nunca esperan esta operación.
pub fn prefetch_model(
    configured_model_id: &str,
    cache_dir: &Path,
    total_memory_bytes: u64,
    min_similarity: f32,
    max_cache_entries: usize,
) -> Result<SemanticModelInfo> {
    let spec = resolve_model_spec(configured_model_id, total_memory_bytes);
    let local_model_dir = prepare_local_model(&spec, cache_dir)?;
    let classifier = SemanticProcessClassifier::from_local_dir(
        spec,
        &local_model_dir,
        min_similarity,
        max_cache_entries,
    )?;
    let probe = guarded_model_call("inferencia de validación", || {
        classifier.model.encode_single("visual studio code")
    })?;
    anyhow::ensure!(
        !probe.is_empty() && probe.iter().all(|value| value.is_finite()),
        "el modelo se descargó pero no superó la prueba de inferencia"
    );
    Ok(classifier.info(cache_dir))
}

pub fn resolve_model_id(configured: &str, total_memory_bytes: u64) -> String {
    resolve_model_spec(configured, total_memory_bytes).model_id
}

pub fn resolve_model_spec(configured: &str, total_memory_bytes: u64) -> ResolvedModelSpec {
    let configured = configured.trim();
    let selected = if !configured.eq_ignore_ascii_case("auto") && !configured.is_empty() {
        configured.to_owned()
    } else if total_memory_bytes < 6 * GIB {
        "minishlab/potion-base-2M".into()
    } else if total_memory_bytes < 12 * GIB {
        "minishlab/potion-base-4M".into()
    } else {
        "minishlab/potion-base-8M".into()
    };

    let expanded = expand_home_path(&selected);
    let local = expanded.is_dir() || looks_like_local_path(&selected);
    if local {
        return ResolvedModelSpec {
            model_id: expanded.to_string_lossy().into_owned(),
            revision: None,
            local: true,
        };
    }

    let (model_id, explicit_revision) = split_remote_revision(&selected);
    let revision = explicit_revision.or_else(|| match model_id.as_str() {
        "minishlab/potion-base-2M" => Some(POTION_2M_REVISION.to_owned()),
        "minishlab/potion-base-4M" => Some(POTION_4M_REVISION.to_owned()),
        "minishlab/potion-base-8M" => Some(POTION_8M_REVISION.to_owned()),
        _ => None,
    });
    ResolvedModelSpec {
        model_id,
        revision,
        local: false,
    }
}

fn repo_for_spec(spec: &ResolvedModelSpec) -> Result<Repo> {
    let revision = spec.revision.clone().context(
        "los modelos remotos personalizados deben fijar un commit inmutable: usa owner/modelo@<sha-de-40-caracteres>",
    )?;
    anyhow::ensure!(
        is_full_commit_sha(&revision),
        "la revisión remota debe ser un SHA hexadecimal completo de 40 caracteres"
    );
    Ok(Repo::with_revision(
        spec.model_id.clone(),
        RepoType::Model,
        revision,
    ))
}

fn locate_local_or_cached_model(spec: &ResolvedModelSpec, cache_dir: &Path) -> Result<PathBuf> {
    let candidate = Path::new(&spec.model_id);
    if candidate.is_dir() {
        validate_model_directory(candidate)?;
        return Ok(candidate.to_path_buf());
    }
    if spec.local {
        anyhow::bail!(
            "el directorio de modelo local no existe o no es válido: {}",
            candidate.display()
        );
    }

    let cache = Cache::new(cache_dir.to_path_buf());
    let cached_repo = cache.repo(repo_for_spec(spec)?);
    let paths = REQUIRED_FILES
        .iter()
        .map(|filename| {
            cached_repo.get(filename).with_context(|| {
                format!(
                    "el modelo {} aún no está completo en la caché local",
                    spec.model_id
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let model_dir = coherent_model_dir(&paths)?;
    validate_snapshot_revision(spec, &model_dir)?;
    Ok(model_dir)
}

fn prepare_local_model(spec: &ResolvedModelSpec, cache_dir: &Path) -> Result<PathBuf> {
    if spec.local {
        return locate_local_or_cached_model(spec, cache_dir);
    }

    anyhow::ensure!(
        !cache_dir.as_os_str().is_empty(),
        "directorio de modelos vacío"
    );
    ensure_model_cache_dir(cache_dir)?;

    // Un bloqueo global evita que dos instancias mezclen una descarga parcial de los
    // tres artefactos. hf-hub conserva además sus bloqueos atómicos por archivo.
    let lock_path = cache_dir.join(".sysopt-model-download.lock");
    let lock_file = open_download_lock(&lock_path)?;
    acquire_download_lock(&lock_file, Duration::from_secs(120))?;

    let result = (|| {
        if let Ok(path) = locate_local_or_cached_model(spec, cache_dir) {
            return Ok(path);
        }

        let api = ApiBuilder::from_cache(Cache::new(cache_dir.to_path_buf()))
            .with_progress(false)
            .with_retries(3)
            .build()
            .context("no se pudo inicializar el cliente de Hugging Face")?;
        let repo = api.repo(repo_for_spec(spec)?);
        let paths = REQUIRED_FILES
            .iter()
            .map(|filename| {
                repo.get(filename).with_context(|| {
                    format!(
                        "no se pudo descargar {filename} del modelo {}@{}",
                        spec.model_id,
                        spec.revision.as_deref().unwrap_or("main")
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let model_dir = coherent_model_dir(&paths)?;
        validate_snapshot_revision(spec, &model_dir)?;
        validate_model_directory(&model_dir)?;
        Ok(model_dir)
    })();

    let unlock_result = FileExt::unlock(&lock_file);
    if let Err(error) = unlock_result {
        return Err(error).context("no se pudo liberar el bloqueo de descarga");
    }
    result
}

fn validate_snapshot_revision(spec: &ResolvedModelSpec, model_dir: &Path) -> Result<()> {
    if spec.local {
        return Ok(());
    }
    let expected = spec
        .revision
        .as_deref()
        .context("el modelo remoto no tiene una revisión inmutable")?;
    let actual = model_dir
        .file_name()
        .and_then(|value| value.to_str())
        .context("la instantánea de Hugging Face no tiene un identificador válido")?;
    anyhow::ensure!(
        actual.eq_ignore_ascii_case(expected),
        "la instantánea local no coincide con la revisión solicitada: esperado {expected}, recibido {actual}"
    );
    Ok(())
}

fn ensure_model_cache_dir(cache_dir: &Path) -> Result<()> {
    let existed = match fs::symlink_metadata(cache_dir) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_dir(),
                "la caché de modelos no es un directorio regular seguro: {}",
                cache_dir.display()
            );
            true
        }
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", cache_dir.display()));
        }
    };
    if !existed {
        fs::create_dir_all(cache_dir)
            .with_context(|| format!("no se pudo crear {}", cache_dir.display()))?;
        let metadata = fs::symlink_metadata(cache_dir)
            .with_context(|| format!("no se pudo verificar {}", cache_dir.display()))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_dir(),
            "la caché de modelos creada no es segura: {}",
            cache_dir.display()
        );
        #[cfg(unix)]
        fs::set_permissions(cache_dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_download_lock(path: &Path) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "el bloqueo de descarga no es un archivo regular: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", path.display()));
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .with_context(|| format!("no se pudo abrir {}", path.display()))
}

fn acquire_download_lock(file: &File, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    anyhow::bail!(
                        "otra instancia mantiene bloqueada la descarga del modelo durante más de {} segundos",
                        timeout.as_secs()
                    );
                }
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error).context("no se pudo bloquear la descarga del modelo"),
        }
    }
}

fn guarded_model_call<T>(operation: &str, call: impl FnOnce() -> T) -> Result<T> {
    catch_unwind(AssertUnwindSafe(call)).map_err(|payload| {
        let detail = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("panic sin mensaje");
        anyhow::anyhow!("{operation} abortó de forma controlada: {detail}")
    })
}

fn split_remote_revision(value: &str) -> (String, Option<String>) {
    match value.rsplit_once('@') {
        Some((model_id, revision)) if !model_id.is_empty() && is_full_commit_sha(revision) => {
            (model_id.to_owned(), Some(revision.to_ascii_lowercase()))
        }
        _ => (value.to_owned(), None),
    }
}

fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn expand_home_path(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    let suffix = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"));
    match (suffix, home_dir()) {
        (Some(suffix), Some(home)) => home.join(suffix),
        _ => PathBuf::from(value),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn coherent_model_dir(paths: &[PathBuf]) -> Result<PathBuf> {
    let model_dir = paths
        .first()
        .and_then(|path| path.parent())
        .context("Hugging Face devolvió una ruta de modelo inválida")?
        .to_path_buf();
    anyhow::ensure!(
        paths
            .iter()
            .all(|path| path.parent() == Some(model_dir.as_path())),
        "los archivos del modelo quedaron en instantáneas incompatibles"
    );
    validate_model_directory(&model_dir)?;
    Ok(model_dir)
}

fn validate_model_directory(model_dir: &Path) -> Result<()> {
    for filename in REQUIRED_FILES {
        let path = model_dir.join(filename);
        anyhow::ensure!(
            path.is_file(),
            "falta el archivo obligatorio del modelo: {}",
            path.display()
        );
        let length = path
            .metadata()
            .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?
            .len();
        anyhow::ensure!(
            length > 0,
            "el archivo del modelo está vacío: {}",
            path.display()
        );
        let maximum = match filename {
            "config.json" => MAX_CONFIG_BYTES,
            "tokenizer.json" => MAX_TOKENIZER_BYTES,
            "model.safetensors" => MAX_MODEL_BYTES,
            _ => 0,
        };
        anyhow::ensure!(
            length <= maximum,
            "el archivo del modelo supera el límite seguro de tamaño: {}",
            path.display()
        );
    }

    for filename in ["config.json", "tokenizer.json"] {
        let path = model_dir.join(filename);
        let file =
            File::open(&path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
        let value: Value = serde_json::from_reader(file)
            .with_context(|| format!("JSON inválido en {}", path.display()))?;
        anyhow::ensure!(
            value.is_object(),
            "el archivo {} no contiene un objeto JSON",
            path.display()
        );
    }

    validate_safetensors(&model_dir.join("model.safetensors"))
}

fn validate_safetensors(path: &Path) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?
        .len();
    anyhow::ensure!(
        file_len >= 10,
        "archivo safetensors truncado: {}",
        path.display()
    );

    let mut length_bytes = [0_u8; 8];
    file.read_exact(&mut length_bytes)
        .with_context(|| format!("cabecera safetensors incompleta: {}", path.display()))?;
    let header_len = u64::from_le_bytes(length_bytes);
    anyhow::ensure!(
        (2..=MAX_SAFETENSORS_HEADER_BYTES).contains(&header_len)
            && header_len <= file_len.saturating_sub(8),
        "longitud de cabecera safetensors inválida en {}",
        path.display()
    );
    let header_size =
        usize::try_from(header_len).context("cabecera safetensors demasiado grande")?;
    let mut header = vec![0_u8; header_size];
    file.read_exact(&mut header)
        .with_context(|| format!("cabecera safetensors truncada: {}", path.display()))?;
    let value: Value = serde_json::from_slice(&header)
        .with_context(|| format!("cabecera safetensors inválida: {}", path.display()))?;
    anyhow::ensure!(
        value.is_object(),
        "la cabecera safetensors no es un objeto JSON: {}",
        path.display()
    );
    Ok(())
}

fn looks_like_local_path(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute()
        || value == "."
        || value == ".."
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with(".\\")
        || value.starts_with("..\\")
        || value.starts_with('~')
        || value.contains('\\')
}

fn process_descriptor(process: &ProcessSample) -> String {
    let mut parts = Vec::new();
    push_identifier(&mut parts, &process.name);
    if let Some(path) = &process.executable {
        if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
            push_identifier(&mut parts, name);
        }
        if let Some(parent) = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
        {
            push_identifier(&mut parts, parent);
        }
    }
    if let Some(path) = &process.cwd {
        for component in path.components().rev().take(2) {
            if let Some(value) = component.as_os_str().to_str() {
                push_identifier(&mut parts, value);
            }
        }
    }
    parts.sort();
    parts.dedup();
    parts.join(" ")
}

fn push_identifier(parts: &mut Vec<String>, value: &str) {
    let normalized = humanize_identifier(value);
    if !normalized.is_empty() && !parts.contains(&normalized) {
        parts.push(normalized);
    }
}

fn humanize_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 8);
    let mut previous_was_lower = false;
    for character in value.chars() {
        if character.is_ascii_uppercase() && previous_was_lower {
            output.push(' ');
        }
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            previous_was_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            if !output.ends_with(' ') {
                output.push(' ');
            }
            previous_was_lower = false;
        }
    }
    output
        .split_whitespace()
        .filter(|token| !matches!(*token, "exe" | "bin" | "app" | "x64" | "x86"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return -1.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (&left, &right) in left.iter().zip(right.iter()) {
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    let denominator = left_norm.sqrt() * right_norm.sqrt();
    if denominator <= f32::EPSILON {
        -1.0
    } else {
        (dot / denominator).clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecciona_modelo_y_revision_por_memoria() {
        let small = resolve_model_spec("auto", 4 * GIB);
        assert_eq!(small.model_id, "minishlab/potion-base-2M");
        assert_eq!(small.revision.as_deref(), Some(POTION_2M_REVISION));
        assert_eq!(
            resolve_model_id("auto", 8 * GIB),
            "minishlab/potion-base-4M"
        );
        assert_eq!(
            resolve_model_id("auto", 16 * GIB),
            "minishlab/potion-base-8M"
        );
        let custom = resolve_model_spec(
            "owner/custom@0123456789abcdef0123456789abcdef01234567",
            4 * GIB,
        );
        assert_eq!(custom.model_id, "owner/custom");
        assert_eq!(
            custom.revision.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        let mutable_custom = resolve_model_spec("owner/custom", 4 * GIB);
        assert!(mutable_custom.revision.is_none());
        assert!(repo_for_spec(&mutable_custom).is_err());
    }

    #[test]
    fn distingue_repositorio_hf_de_ruta_local() {
        assert!(!looks_like_local_path("minishlab/potion-base-2M"));
        assert!(looks_like_local_path("./models/potion"));
        assert!(looks_like_local_path("..\\models\\potion"));
    }

    #[test]
    fn normaliza_nombres_de_proceso() {
        assert_eq!(
            humanize_identifier("VisualStudioCode.exe"),
            "visual studio code"
        );
        assert_eq!(
            humanize_identifier("android-studio64.exe"),
            "android studio64"
        );
    }

    #[test]
    fn rechaza_safetensors_truncado() {
        let base = std::env::temp_dir().join(format!(
            "sysopt-semantic-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("model.safetensors");
        fs::write(&path, [0_u8; 9]).unwrap();
        assert!(validate_safetensors(&path).is_err());
        let _ = fs::remove_dir_all(base);
    }
}
