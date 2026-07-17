use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryPatterns {
    patterns: Vec<String>,
}

/// Base de patrones "nombre de proceso -> categoría".
/// Se puede reemplazar por un TOML externo (--signatures ruta.toml)
/// para que cualquiera agregue soporte para un IDE, editor o herramienta
/// nueva sin tocar el código Rust.
#[derive(Debug, Clone)]
pub struct SignatureDb {
    categories: HashMap<String, Vec<String>>, // patrones ya en minúsculas
}

const DEFAULT_SIGNATURES: &str = include_str!("../signatures.default.toml");

impl SignatureDb {
    /// Base embebida en el binario. La variante `try_` permite propagar un
    /// error de empaquetado sin provocar un panic en producción.
    pub fn try_embedded_default() -> Result<Self> {
        Self::parse(DEFAULT_SIGNATURES).context("signatures.default.toml embebido es inválido")
    }

    /// Compatibilidad para consumidores que no pueden propagar errores. Ante
    /// un recurso embebido inválido degrada a una base vacía y segura.
    pub fn embedded_default() -> Self {
        Self::try_embedded_default().unwrap_or_else(|_| Self {
            categories: HashMap::new(),
        })
    }

    pub fn load_from_file(path: &str) -> Result<Self> {
        const MAX_SIGNATURES_BYTES: u64 = 4 * 1024 * 1024;
        let path_ref = Path::new(path);
        let metadata = fs::symlink_metadata(path_ref)
            .with_context(|| format!("no se pudo inspeccionar el archivo de firmas: {path}"))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "se rechazó un enlace simbólico como archivo de firmas: {path}"
        );
        anyhow::ensure!(
            metadata.is_file(),
            "el archivo de firmas no es un archivo regular: {path}"
        );
        anyhow::ensure!(
            metadata.len() <= MAX_SIGNATURES_BYTES,
            "el archivo de firmas supera el límite de {MAX_SIGNATURES_BYTES} bytes: {path}"
        );
        let mut file = open_signatures_read(path_ref)
            .with_context(|| format!("no se pudo abrir el archivo de firmas: {path}"))?;
        let opened = file
            .metadata()
            .with_context(|| format!("no se pudo verificar el archivo de firmas: {path}"))?;
        anyhow::ensure!(
            opened.is_file(),
            "el archivo de firmas dejó de ser regular: {path}"
        );
        anyhow::ensure!(
            opened.len() <= MAX_SIGNATURES_BYTES,
            "el archivo de firmas supera el límite de {MAX_SIGNATURES_BYTES} bytes: {path}"
        );
        let mut content = String::with_capacity(opened.len() as usize);
        file.read_to_string(&mut content)
            .with_context(|| format!("no se pudo leer el archivo de firmas: {path}"))?;
        anyhow::ensure!(
            content.len() as u64 <= MAX_SIGNATURES_BYTES,
            "el archivo de firmas cambió mientras se leía: {path}"
        );
        Self::parse(&content).with_context(|| format!("formato inválido en {path}"))
    }

    fn parse(content: &str) -> Result<Self> {
        let raw: HashMap<String, CategoryPatterns> = toml::from_str(content)?;
        anyhow::ensure!(
            !raw.is_empty(),
            "el archivo de firmas no contiene categorías"
        );
        let mut categories = HashMap::with_capacity(raw.len());
        for (category, values) in raw {
            let category = category.trim().to_lowercase();
            anyhow::ensure!(
                !category.is_empty(),
                "hay una categoría de firmas sin nombre"
            );
            let patterns: Vec<String> = values
                .patterns
                .into_iter()
                .map(|pattern| pattern.trim().to_lowercase())
                .filter(|pattern| !pattern.is_empty())
                .collect();
            anyhow::ensure!(
                !patterns.is_empty(),
                "la categoría {category} no contiene patrones válidos"
            );
            anyhow::ensure!(
                categories.insert(category.clone(), patterns).is_none(),
                "la categoría {category} está duplicada"
            );
        }
        Ok(Self { categories })
    }

    /// Todas las categorías cuyo patrón hace match (substring, sin distinguir
    /// mayúsculas) con el nombre de proceso dado. Un proceso puede caer en
    /// varias categorías a la vez (ej. un binario que sea IDE y LSP embebido).
    pub fn categorize(&self, process_name: &str) -> Vec<&str> {
        let name = process_name.to_lowercase();
        self.categories
            .iter()
            .filter(|(_, patterns)| patterns.iter().any(|p| name.contains(p.as_str())))
            .map(|(cat, _)| cat.as_str())
            .collect()
    }

    pub fn is_category(&self, process_name: &str, category: &str) -> bool {
        let name = process_name.to_lowercase();
        let category = category.trim().to_lowercase();
        self.categories.get(&category).is_some_and(|patterns| {
            patterns
                .iter()
                .any(|pattern| name.contains(pattern.as_str()))
        })
    }

    pub fn category_count(&self) -> usize {
        self.categories.len()
    }
}

#[cfg(unix)]
fn open_signatures_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_signatures_read(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_signatures_read(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmas_son_case_insensitive_y_rechazan_categorias_vacias() {
        let db = SignatureDb::parse("[ide]\npatterns = [\"Code\"]\n").unwrap();
        assert!(db.is_category("CODE.EXE", "ide"));
        assert!(SignatureDb::parse("[ide]\npatterns = [\"  \"]\n").is_err());
    }
}
