#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "torch>=2.0",
#     "transformers>=4.40",
#     "onnx>=1.15",
#     "onnxscript>=0.1",
#     "safetensors>=0.4",
#     "huggingface-hub>=0.20",
# ]
# ///
"""Download full BAAI/bge-m3 and export ONNX with dense + sparse outputs.

Steps:
  1. Downloads full BAAI/bge-m3 model (PyTorch + sparse_linear.pt)
     into ~/.chitta/models/bge-m3-full/ for archival.
  2. Exports custom ONNX with two outputs:
       - dense_embeddings  (1 x 1024)
       - sparse_weights    (1 x seq_len)
  3. Copies tokenizer.json alongside the ONNX model.
  4. Backs up old model and installs new one at ~/.chitta/models/bge-m3-onnx/.
"""

import argparse
import os
import shutil
import sys
from pathlib import Path

import torch
import torch.nn as nn
from huggingface_hub import snapshot_download
from transformers import AutoModel, AutoTokenizer


REPO_ID = "BAAI/bge-m3"
FULL_MODEL_DIR = Path.home() / ".chitta" / "models" / "bge-m3-full"
ONNX_INSTALL_DIR = Path.home() / ".chitta" / "models" / "bge-m3-onnx"
ONNX_FILENAME = "bge_m3_model.onnx"


class BGEM3DenseSparse(nn.Module):
    """Wrapper that produces dense_embeddings and sparse_weights from BGE-M3."""

    def __init__(self, encoder: nn.Module, sparse_linear: nn.Linear):
        super().__init__()
        self.encoder = encoder
        self.sparse_linear = sparse_linear

    def forward(
        self, input_ids: torch.Tensor, attention_mask: torch.Tensor
    ) -> tuple[torch.Tensor, torch.Tensor]:
        outputs = self.encoder(input_ids=input_ids, attention_mask=attention_mask)
        last_hidden = outputs.last_hidden_state  # (B, seq, hidden)

        # Dense: CLS pooling + L2 normalize
        cls_emb = last_hidden[:, 0]  # (B, hidden)
        dense = nn.functional.normalize(cls_emb, p=2, dim=-1)

        # Sparse: linear projection -> relu, masked by attention
        sparse_raw = self.sparse_linear(last_hidden).squeeze(-1)  # (B, seq)
        sparse = torch.relu(sparse_raw) * attention_mask.float()

        return dense, sparse


def download_full_model(dest: Path) -> Path:
    print(f"Downloading {REPO_ID} to {dest} ...")
    path = snapshot_download(
        REPO_ID,
        local_dir=str(dest),
        local_dir_use_symlinks=False,
    )
    print(f"Downloaded to {path}")
    return Path(path)


def load_model(model_dir: Path) -> BGEM3DenseSparse:
    print(f"Loading encoder from {model_dir} ...")
    encoder = AutoModel.from_pretrained(str(model_dir), trust_remote_code=False)
    encoder.eval()

    sparse_path = model_dir / "sparse_linear.pt"
    if not sparse_path.exists():
        print(f"ERROR: {sparse_path} not found. The model may be incomplete.")
        sys.exit(1)

    print(f"Loading sparse_linear from {sparse_path} ...")
    sparse_state = torch.load(sparse_path, map_location="cpu", weights_only=True)
    hidden_dim = encoder.config.hidden_size
    sparse_linear = nn.Linear(hidden_dim, 1)
    sparse_linear.load_state_dict(sparse_state)
    sparse_linear.eval()

    return BGEM3DenseSparse(encoder, sparse_linear)


def export_onnx(model: BGEM3DenseSparse, output_path: Path) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)

    dummy_ids = torch.ones(1, 16, dtype=torch.long)
    dummy_mask = torch.ones(1, 16, dtype=torch.long)

    print(f"Exporting ONNX to {output_path} ...")
    torch.onnx.export(
        model,
        (dummy_ids, dummy_mask),
        str(output_path),
        input_names=["input_ids", "attention_mask"],
        output_names=["dense_embeddings", "sparse_weights"],
        dynamic_axes={
            "input_ids": {0: "batch", 1: "seq"},
            "attention_mask": {0: "batch", 1: "seq"},
            "dense_embeddings": {0: "batch"},
            "sparse_weights": {0: "batch", 1: "seq"},
        },
        opset_version=17,
        do_constant_folding=True,
    )

    import onnx

    onnx_model = onnx.load(str(output_path), load_external_data=False)
    output_names = [o.name for o in onnx_model.graph.output]
    print(f"ONNX outputs: {output_names}")

    total = output_path.stat().st_size
    for suffix in [".data", "_data"]:
        data_path = Path(str(output_path) + suffix)
        if data_path.exists():
            total += data_path.stat().st_size
    print(f"ONNX size: {total / 1e9:.2f} GB")


def install_model(onnx_path: Path, tokenizer_json: Path, install_dir: Path) -> None:
    backup_dir = install_dir.parent / "bge-m3-onnx-backup"
    if install_dir.exists():
        print(f"Backing up existing model to {backup_dir} ...")
        if backup_dir.exists():
            shutil.rmtree(backup_dir)
        shutil.copytree(install_dir, backup_dir)
        shutil.rmtree(install_dir)

    install_dir.mkdir(parents=True, exist_ok=True)
    target_onnx = install_dir / ONNX_FILENAME
    target_tok = install_dir / "tokenizer.json"

    print(f"Installing to {install_dir} ...")
    shutil.copy2(onnx_path, target_onnx)
    for suffix in [".data", "_data"]:
        data_src = Path(str(onnx_path) + suffix)
        if data_src.exists():
            target_data = install_dir / (ONNX_FILENAME + suffix)
            shutil.copy2(data_src, target_data)
    shutil.copy2(tokenizer_json, target_tok)

    print("Installed files:")
    for f in sorted(install_dir.iterdir()):
        print(f"  {f.name}  ({f.stat().st_size / 1e6:.1f} MB)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--model-dir",
        type=Path,
        default=FULL_MODEL_DIR,
        help=f"Where to download/find the full model (default: {FULL_MODEL_DIR})",
    )
    parser.add_argument(
        "--install-dir",
        type=Path,
        default=ONNX_INSTALL_DIR,
        help=f"Where to install the ONNX model (default: {ONNX_INSTALL_DIR})",
    )
    parser.add_argument(
        "--skip-download",
        action="store_true",
        help="Skip download if model already exists",
    )
    parser.add_argument(
        "--no-install",
        action="store_true",
        help="Export only, don't install to install-dir",
    )
    args = parser.parse_args()

    model_dir = args.model_dir
    if not args.skip_download or not (model_dir / "sparse_linear.pt").exists():
        download_full_model(model_dir)
    else:
        print(f"Using existing model at {model_dir}")

    model = load_model(model_dir)

    export_dir = model_dir / "onnx-dense-sparse"
    export_dir.mkdir(parents=True, exist_ok=True)
    onnx_path = export_dir / ONNX_FILENAME
    export_onnx(model, onnx_path)

    tokenizer_json = model_dir / "tokenizer.json"
    if not tokenizer_json.exists():
        print("WARNING: tokenizer.json not found in model dir, checking HF cache...")
        sys.exit(1)

    if not args.no_install:
        install_model(onnx_path, tokenizer_json, args.install_dir)

    print("\nDone. Restart chitta to pick up the new model.")


if __name__ == "__main__":
    main()
