# Third-party notices

## Runtime dependencies

### model2vec-rs 0.2.1

- Project: MinishLab/model2vec-rs
- Purpose: native Rust inference.
- License: MIT.
- Source: https://github.com/MinishLab/model2vec-rs

### hf-hub 0.4.3

- Project: huggingface/hf-hub
- Purpose: retrieval and cache layout for Hugging Face model artifacts.
- License: Apache-2.0.
- Source: https://github.com/huggingface/hf-hub

### POTION Model2Vec models

- Models: `minishlab/potion-base-2M`, `potion-base-4M`, `potion-base-8M`.
- Purpose: static text embeddings for process-category suggestions.
- License declared by the repositories: MIT.
- Source: https://huggingface.co/minishlab

Models are not bundled. The SysOpt service downloads the selected model on first use, validates it and stores it in the user cache. Installers do not wait for this operation.

## CI-only tools

### cargo-audit 0.22.2

- Project: RustSec/rustsec
- Purpose: audit the locked dependency graph against the RustSec advisory database.
- License: Apache-2.0 OR MIT.
- Source: https://github.com/rustsec/rustsec
