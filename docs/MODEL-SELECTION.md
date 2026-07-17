# Selección del modelo de IA

## Decisión

Se integró Model2Vec POTION a través de `model2vec-rs` 0.2.1. Es un modelo de embeddings estáticos, adecuado para clasificar nombres y descriptores cortos de procesos con una huella mucho menor que un LLM o un encoder Transformer completo.

## Candidatos revisados

| Candidato | Huella aproximada | Evaluación para SysOpt |
|---|---:|---|
| SmolLM2-135M cuantizado | ~92-105 MB | Es generativo, consume más memoria y no está entrenado para telemetría; complejidad innecesaria. |
| all-MiniLM-L6-v2 ONNX | ~80-90 MB | Buenos embeddings, pero requiere runtime ONNX y una huella mayor. |
| POTION base 2M | ~16 MB de repositorio; ~7.6 MB de pesos | Recomendado para equipos con poca RAM. |
| POTION base 4M | ~31 MB de repositorio; ~15 MB de pesos | Equilibrio predeterminado para equipos medios. |
| POTION base 8M | ~61 MB de repositorio; ~30 MB de pesos | Más vocabulario para equipos con 12 GiB o más. |

## Razones técnicas

- inferencia nativa en Rust;
- no necesita Python, GPU ni servidor local;
- descarga desde Hugging Face y caché persistente;
- salida determinista y fácil de auditar;
- se limita a sugerir categorías, sin acceso directo al enforcement;
- fallback completo a reglas y aprendizaje online.

## Fuentes verificadas el 14 de julio de 2026

- https://huggingface.co/minishlab/potion-base-2M
- https://huggingface.co/minishlab/potion-base-4M
- https://huggingface.co/minishlab/potion-base-8M
- https://github.com/MinishLab/model2vec-rs
- https://huggingface.co/HuggingFaceTB/SmolLM2-135M-Instruct-GGUF
- https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2
