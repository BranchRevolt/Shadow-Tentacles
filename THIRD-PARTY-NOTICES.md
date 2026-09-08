# Third-party notices

Shadow Tentacles is distributed under the GNU General Public License v3.0 or
later (see `LICENSE`). It builds on the components below; each keeps its own
license, and every one of them is compatible with the GPL.

## Compiled into the binary

| Component | License | What it does |
| --- | --- | --- |
| [espeak-ng](https://github.com/espeak-ng/espeak-ng) | **GPL-3.0-or-later** | text to phonemes, for reading aloud |
| [llama.cpp / ggml](https://github.com/ggml-org/llama.cpp) | MIT | runs the language model, via `llama-cpp-2` |
| [Piper](https://github.com/rhasspy/piper) | MIT | speech synthesis, via `piper-rs` |
| [ONNX Runtime](https://github.com/microsoft/onnxruntime) | MIT | runs the Piper voice models, via `ort` |
| [Tauri](https://tauri.app) | MIT / Apache-2.0 | the desktop window |
| [Symphonia](https://github.com/pdeljanov/Symphonia) and other Servo crates | MPL-2.0 | audio decoding, CSS parsing |
| 600+ Rust crates | MIT / Apache-2.0 / BSD / ISC / Zlib | see `Cargo.lock` |

espeak-ng is the reason this program is GPL rather than permissive: it is
GPL-3.0-or-later and is linked into the binary.

MPL-2.0 is per-file copyleft. Its §3.3 allows distribution inside a larger work
under the GPL, provided those files are not modified, and they are used here
exactly as published.

## Downloaded when the user asks for them

These are **not** part of the program and are not redistributed with it. The
application fetches them on request, and each carries its own license:

| Component | License |
| --- | --- |
| Qwen3 and other GGUF models from the catalog | per model, see the model card |
| [Piper voices](https://huggingface.co/rhasspy/piper-voices) | MIT |
| [RUAccent](https://github.com/Den4ikAI/ruaccent) stress dictionaries | MIT |

## A vendored crate

`vendor/piper-rs` is a copy of [piper-rs](https://github.com/thewh1teagle/piper-rs)
0.2.0 with one line changed: it pinned `ort` to `2.0.0-rc.12`, whose prebuilt ONNX
Runtime requires glibc 2.38 and ships no binary for macOS on Intel. The copy pins
`2.0.0-rc.10` instead, which has neither limitation and the same API. Nothing else
is modified, and the change is marked in `vendor/piper-rs/Cargo.toml`.

The crate declares `license = "MIT"` in its manifest. Its repository carries no
license file, so there is no copyright line to reproduce here.

## Source offer

The GPL requires that the corresponding source of every GPL component in a
binary release be available. The espeak-ng source used in a build is the copy
vendored inside the `espeak-rs-sys` crate at the exact version named in
`Cargo.lock`; that crate, and therefore that source, is published on crates.io.
The phoneme data carried inside the binary is generated from that same source
during the build.
