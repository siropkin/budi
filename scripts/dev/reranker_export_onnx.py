#!/usr/bin/env python3
"""Export fine-tuned cross-encoder to ONNX int8 for budi's reranker.

Takes the PyTorch model from reranker_finetune.py, exports to ONNX,
quantizes to int8, and copies to budi's reranker cache.

Usage:
    source scripts/dev/reranker-venv/bin/activate
    python3 scripts/dev/reranker_export_onnx.py [--install]
"""

import argparse
import shutil
import platform
from pathlib import Path

import numpy as np
import onnx
import torch
from transformers import AutoModelForSequenceClassification, AutoTokenizer


RERANKER_CACHE = Path.home() / ".local" / "share" / "budi" / "reranker-cache"

if platform.machine() == "arm64":
    MODEL_FILENAME = "model_qint8_arm64.onnx"
else:
    MODEL_FILENAME = "model_quint8_avx2.onnx"


def export_to_onnx(model_dir: str, output_dir: str) -> Path:
    """Export PyTorch model to ONNX via torch.onnx.export."""
    output_path = Path(output_dir)
    output_path.mkdir(parents=True, exist_ok=True)
    onnx_path = output_path / "model.onnx"

    print(f"Loading fine-tuned model from {model_dir}...")
    model = AutoModelForSequenceClassification.from_pretrained(model_dir)
    tokenizer = AutoTokenizer.from_pretrained(model_dir)
    model.eval()

    # Create dummy inputs
    dummy = tokenizer(
        "What is fibonacci?",
        "fn fibonacci(n: u32) -> u32 { n }",
        return_tensors="pt",
        max_length=512,
        truncation=True,
        padding="max_length",
    )

    print("Exporting to ONNX...")
    torch.onnx.export(
        model,
        (dummy["input_ids"], dummy["attention_mask"], dummy["token_type_ids"]),
        str(onnx_path),
        input_names=["input_ids", "attention_mask", "token_type_ids"],
        output_names=["logits"],
        dynamic_axes={
            "input_ids": {0: "batch_size", 1: "sequence"},
            "attention_mask": {0: "batch_size", 1: "sequence"},
            "token_type_ids": {0: "batch_size", 1: "sequence"},
            "logits": {0: "batch_size"},
        },
        opset_version=14,
        dynamo=False,
    )

    print(f"ONNX model: {onnx_path} ({onnx_path.stat().st_size / 1024 / 1024:.1f} MB)")
    return onnx_path


def quantize_int8(onnx_path: Path, output_path: Path) -> Path:
    """Quantize ONNX model to int8."""
    from onnxruntime.quantization import quantize_dynamic, QuantType

    print(f"Quantizing to int8: {output_path}")
    quantize_dynamic(
        model_input=str(onnx_path),
        model_output=str(output_path),
        weight_type=QuantType.QInt8,
    )
    print(
        f"Quantized model: {output_path} "
        f"({output_path.stat().st_size / 1024 / 1024:.1f} MB)"
    )
    return output_path


def validate_onnx(onnx_path: Path):
    """Basic ONNX model validation."""
    print(f"Validating {onnx_path}...")
    model = onnx.load(str(onnx_path))
    onnx.checker.check_model(model)

    inputs = {inp.name for inp in model.graph.input}
    outputs = {out.name for out in model.graph.output}
    print(f"  Inputs: {inputs}")
    print(f"  Outputs: {outputs}")

    expected_inputs = {"input_ids", "attention_mask", "token_type_ids"}
    if not expected_inputs.issubset(inputs):
        print(f"  WARNING: Missing expected inputs: {expected_inputs - inputs}")
    else:
        print("  Input names OK")

    print("  ONNX validation passed")


def test_inference(onnx_path: Path, model_dir: str):
    """Quick inference test with the quantized model."""
    import onnxruntime as ort

    print("\nTesting inference...")
    tokenizer = AutoTokenizer.from_pretrained(model_dir)
    session = ort.InferenceSession(str(onnx_path))

    pairs = [
        ("How is the fibonacci function implemented?",
         "fn fibonacci(n: u32) -> u32 { if n <= 1 { return n; } fibonacci(n-1) + fibonacci(n-2) }"),
        ("How is the fibonacci function implemented?",
         "The weather in Paris is mild."),
        ("Where is the Flask app factory defined?",
         "def create_app(test_config=None):\n    app = Flask(__name__)\n    app.config.from_mapping(SECRET_KEY='dev')"),
        ("Where is the Flask app factory defined?",
         "import pytest\nfrom flask import Flask\ndef test_config():\n    assert not create_app().testing"),
        ("What calls handle_exception in Flask?",
         "def wsgi_app(self, environ, start_response):\n    ctx = self.request_context(environ)\n    try:\n        response = self.full_dispatch_request()\n    except Exception as e:\n        response = self.handle_exception(e)"),
        ("What calls handle_exception in Flask?",
         "class Config:\n    DEBUG = False\n    TESTING = False\n    SECRET_KEY = None"),
    ]

    for query, passage in pairs:
        encoding = tokenizer(
            query, passage, return_tensors="np",
            truncation=True, max_length=512, padding="max_length",
        )
        outputs = session.run(
            None,
            {
                "input_ids": encoding["input_ids"].astype(np.int64),
                "attention_mask": encoding["attention_mask"].astype(np.int64),
                "token_type_ids": encoding["token_type_ids"].astype(np.int64),
            },
        )
        logit = outputs[0][0][0]
        score = 1.0 / (1.0 + np.exp(-logit))
        label = "RELEVANT" if score > 0.5 else "irrelevant"
        print(f"  [{label}] score={score:.4f} logit={logit:.4f} | Q: {query[:50]} | P: {passage[:50]}")

    print("Inference test completed")


def install_to_cache(quantized_path: Path, model_dir: str):
    """Copy quantized model to budi's reranker cache."""
    RERANKER_CACHE.mkdir(parents=True, exist_ok=True)

    dest = RERANKER_CACHE / MODEL_FILENAME
    backup = RERANKER_CACHE / f"{MODEL_FILENAME}.pretrained.bak"

    if dest.exists() and not backup.exists():
        print(f"Backing up original model to {backup}")
        shutil.copy2(dest, backup)

    print(f"Installing fine-tuned model: {dest}")
    shutil.copy2(quantized_path, dest)
    print(f"Installed ({dest.stat().st_size / 1024 / 1024:.1f} MB)")


def main():
    parser = argparse.ArgumentParser(description="Export reranker to ONNX")
    parser.add_argument(
        "--model-dir",
        default="scripts/dev/reranker-finetuned",
        help="Fine-tuned model directory",
    )
    parser.add_argument(
        "--output-dir",
        default="scripts/dev/reranker-onnx",
        help="ONNX export directory",
    )
    parser.add_argument(
        "--install",
        action="store_true",
        help="Install to budi's reranker cache after export",
    )
    args = parser.parse_args()

    onnx_path = export_to_onnx(args.model_dir, args.output_dir)

    quantized_path = Path(args.output_dir) / MODEL_FILENAME
    quantize_int8(onnx_path, quantized_path)

    validate_onnx(quantized_path)

    test_inference(quantized_path, args.model_dir)

    if args.install:
        install_to_cache(quantized_path, args.model_dir)
    else:
        print(f"\nTo install: python3 {__file__} --install")
        print(f"Or manually: cp {quantized_path} {RERANKER_CACHE / MODEL_FILENAME}")


if __name__ == "__main__":
    main()
