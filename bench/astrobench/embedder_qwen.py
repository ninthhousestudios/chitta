"""Qwen3-VL-Embedding-2B text embedder for astrobench comparison.

Loads the model via PyTorch (bf16 on CPU). Text-only — no sparse output.
Same interface as embedder.py so ingest/eval can swap transparently.
"""

import sys
from pathlib import Path

import torch
import torch.nn.functional as F

EMBEDDING_DIM = 2048
MAX_TOKENS = 8192
DEFAULT_MODEL_DIR = Path.home() / "soft" / "Qwen3-VL-Embedding" / "models" / "Qwen3-VL-Embedding-2B"

sys.path.insert(0, str(DEFAULT_MODEL_DIR.parent.parent / "src"))
from models.qwen3_vl_embedding import Qwen3VLForEmbedding

from transformers.models.qwen3_vl.processing_qwen3_vl import Qwen3VLProcessor


class Embedder:
    def __init__(
        self,
        model_dir: Path = DEFAULT_MODEL_DIR,
        sparse_threshold: float = 0.0,
    ):
        self.model = Qwen3VLForEmbedding.from_pretrained(
            str(model_dir),
            trust_remote_code=True,
            dtype=torch.bfloat16,
        )
        self.model.eval()

        self.processor = Qwen3VLProcessor.from_pretrained(
            str(model_dir), padding_side="right"
        )
        self.tokenizer = self.processor.tokenizer
        self._has_sparse = False

    def tokenize_raw(self, text: str) -> list[int]:
        enc = self.tokenizer.encode(text, add_special_tokens=False)
        return list(enc)

    def embed(self, text: str) -> tuple[list[float], dict[int, float]]:
        return self._embed_text(text)

    def embed_chunk(
        self, content_ids: list[int]
    ) -> tuple[list[float], dict[int, float]]:
        text = self.tokenizer.decode(content_ids, skip_special_tokens=False)
        return self._embed_text(text)

    def decode_chunk(self, content_ids: list[int]) -> str:
        return self.tokenizer.decode(content_ids, skip_special_tokens=False)

    @torch.no_grad()
    def _embed_text(self, text: str) -> tuple[list[float], dict[int, float]]:
        instruction = "Represent the user's input."
        messages = [
            {"role": "system", "content": [{"type": "text", "text": instruction}]},
            {"role": "user", "content": [{"type": "text", "text": text}]},
        ]
        formatted = self.processor.apply_chat_template(
            messages, add_generation_prompt=True, tokenize=False
        )
        inputs = self.processor(
            text=[formatted],
            truncation=True,
            max_length=MAX_TOKENS,
            padding=True,
            return_tensors="pt",
        )
        inputs = {k: v.to(self.model.device) for k, v in inputs.items()}

        outputs = self.model(**inputs)
        hidden = outputs.last_hidden_state
        attn = inputs["attention_mask"]

        # Last-token pooling (same as upstream)
        flipped = attn.flip(dims=[1])
        last_pos = flipped.argmax(dim=1)
        col = attn.shape[1] - last_pos - 1
        row = torch.arange(hidden.shape[0], device=hidden.device)
        pooled = hidden[row, col]

        dense = F.normalize(pooled, p=2, dim=-1)
        dense_list = dense.squeeze(0).float().tolist()
        assert len(dense_list) == EMBEDDING_DIM, (
            f"expected {EMBEDDING_DIM}-dim, got {len(dense_list)}"
        )

        return dense_list, {}
