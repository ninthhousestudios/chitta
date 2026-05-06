"""BGE-M3 dense+sparse embedder via ONNX Runtime (CPU).

Mirrors the chitta-rs Rust embedder: same model, same sparse extraction
logic (threshold + max-weight-per-token-id), same 1024-dim dense output.
"""

from pathlib import Path

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

EMBEDDING_DIM = 1024
MAX_TOKENS = 8192
DEFAULT_MODEL_DIR = Path.home() / ".chitta" / "models" / "bge-m3-onnx"
DEFAULT_SPARSE_THRESHOLD = 0.01


class Embedder:
    def __init__(
        self,
        model_dir: Path = DEFAULT_MODEL_DIR,
        sparse_threshold: float = DEFAULT_SPARSE_THRESHOLD,
    ):
        model_path = model_dir / "bge_m3_model.onnx"
        tokenizer_path = model_dir / "tokenizer.json"

        self.tokenizer = Tokenizer.from_file(str(tokenizer_path))
        self.tokenizer.no_truncation()

        self.session = ort.InferenceSession(
            str(model_path),
            providers=["CPUExecutionProvider"],
        )
        self.sparse_threshold = sparse_threshold

        output_names = {o.name for o in self.session.get_outputs()}
        self._has_sparse = "sparse_weights" in output_names

        # Detect CLS/SEP token IDs from the tokenizer
        probe = self.tokenizer.encode("", add_special_tokens=True)
        self.cls_id = probe.ids[0]
        self.sep_id = probe.ids[-1]

    def tokenize_raw(self, text: str) -> list[int]:
        """Tokenize without special tokens — for chunking."""
        enc = self.tokenizer.encode(text, add_special_tokens=False)
        return list(enc.ids)

    def embed(self, text: str) -> tuple[list[float], dict[int, float]]:
        """Embed text (adds special tokens automatically)."""
        enc = self.tokenizer.encode(text, add_special_tokens=True)
        token_ids = list(enc.ids)
        attention_mask = list(enc.attention_mask)
        return self._run_inference(token_ids, attention_mask)

    def embed_chunk(
        self, content_ids: list[int]
    ) -> tuple[list[float], dict[int, float]]:
        """Embed pre-chunked content token IDs (wraps with CLS/SEP)."""
        token_ids = [self.cls_id] + content_ids + [self.sep_id]
        attention_mask = [1] * len(token_ids)
        return self._run_inference(token_ids, attention_mask)

    def decode_chunk(self, content_ids: list[int]) -> str:
        """Decode content token IDs back to text."""
        return self.tokenizer.decode(content_ids, skip_special_tokens=False)

    def _run_inference(
        self, token_ids: list[int], attention_mask: list[int]
    ) -> tuple[list[float], dict[int, float]]:
        if len(token_ids) > MAX_TOKENS:
            raise ValueError(
                f"{len(token_ids)} tokens exceeds {MAX_TOKENS}"
            )

        input_ids = np.array([token_ids], dtype=np.int64)
        attn_mask = np.array([attention_mask], dtype=np.int64)

        outputs = self.session.run(
            None,
            {"input_ids": input_ids, "attention_mask": attn_mask},
        )

        output_names = [o.name for o in self.session.get_outputs()]
        results = dict(zip(output_names, outputs))

        dense_key = "dense_embeddings" if "dense_embeddings" in results else "sentence_embedding"
        dense = results[dense_key].flatten().tolist()
        assert len(dense) == EMBEDDING_DIM, (
            f"expected {EMBEDDING_DIM}-dim, got {len(dense)}"
        )

        sparse: dict[int, float] = {}
        if self._has_sparse and "sparse_weights" in results:
            weights = results["sparse_weights"].flatten()
            if len(weights) == len(token_ids):
                for pos, tid in enumerate(token_ids):
                    w = float(weights[pos])
                    if w >= self.sparse_threshold:
                        if tid not in sparse or w > sparse[tid]:
                            sparse[tid] = w

        return dense, sparse
