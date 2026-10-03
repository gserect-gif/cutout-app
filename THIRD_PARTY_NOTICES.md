# Third-party notices

Cutout is released under the MIT License (see `LICENSE`). It bundles the AI
models and libraries below, each under its own license.

## AI models (bundled in the installer)

| Cutout name | File | Model | License | Source |
| --- | --- | --- | --- | --- |
| Fast | `u2netp.onnx` | U²-Net-P (Qin et al.) | Apache-2.0 | https://github.com/xuebinqin/U-2-Net |
| Balanced | `isnet-general-use.onnx` | IS-Net general use (Qin et al., ECCV 2022), ONNX export from rembg | Apache-2.0 | https://github.com/xuebinqin/DIS |
| Best | `model.onnx` | BiRefNet_lite (Zheng et al.), ONNX export by onnx-community | MIT | https://huggingface.co/onnx-community/BiRefNet_lite-ONNX |
| Best+ | `model_fp16.onnx` | BiRefNet (Zheng et al.), fp16 ONNX export by onnx-community | MIT | https://huggingface.co/onnx-community/BiRefNet-ONNX |

Notes:

- The weights are used unmodified. Cutout runs them locally and sends nothing
  anywhere.
- The Apache-2.0 license text is at https://www.apache.org/licenses/LICENSE-2.0
- The MIT license text for BiRefNet is at
  https://github.com/ZhengPeng7/BiRefNet/blob/main/LICENSE
- The IS-Net authors license their code under Apache-2.0 and publish the
  DIS5K training dataset under separate terms of use. Cutout does not
  redistribute that dataset.

## Libraries

| Library | License |
| --- | --- |
| Tauri | MIT or Apache-2.0 |
| ONNX Runtime (via the `ort` crate) | MIT |
| `image` crate | MIT or Apache-2.0 |

The full dependency list is in `package.json` and `src-tauri/Cargo.toml`.
