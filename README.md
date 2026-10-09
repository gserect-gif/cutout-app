# Cutout

Remove image backgrounds on your own computer. No account, no upload, no
subscription, no resolution limit. Everything runs offline.

## Features

- Drag and drop, browse, or paste an image (PNG, JPG, WebP, BMP, TIFF, GIF)
- Batch mode: pick several images or a whole folder (subfolders included) and
  export everything at once
- Four AI models, from fast to best quality (see below)
- Output keeps the original resolution. Export as PNG, JPG or WebP, with a
  transparent background, any solid color from a color wheel, or a picture of
  your choice behind the cutout
- Edge slider to make the cutout's edge sharper or softer
- Remembers its window size and position
- Zoom and pan the preview, Dark / Light / System theme
- Cancel any run at any time (Esc works too)

## Install (Windows)

1. Download `Cutout_x.y.z_x64-setup.exe` from the Releases page.
2. Run it.

**Windows may show a blue "Windows protected your PC" (SmartScreen) warning.**
This appears for any new app that isn't code-signed with a paid certificate,
and Cutout isn't signed yet. It doesn't mean anything is wrong with the file.
To continue, click **More info**, then **Run anyway**. You only need to do
this once.

## System requirements

- Windows 10 or 11, 64-bit
- A processor with **AVX2** support. This covers most PCs made since about
  2013-2015. Older CPUs (for example AMD A-series "Kaveri" chips and Intel
  Sandy/Ivy Bridge) can't run the AI engine, and Cutout will tell you so when
  it starts.
- 8 GB of RAM or more recommended. The Best models briefly use several GB
  while processing an image.
- No GPU needed. Cutout runs on the CPU.

## Models

| Name | Speed | Notes |
| --- | --- | --- |
| Fast | quickest | Good for simple subjects and quick checks |
| Balanced | fast | Good everyday default |
| Best | slower | Fine detail such as hair |
| Best+ | slowest | Sharpest results, best for final exports |

Licenses and credits for each model are in `THIRD_PARTY_NOTICES.md`.

## Privacy

Cutout never connects to the internet to process images. Your pictures stay
on your computer. (The Windows installer may download Microsoft's WebView2
component if your PC doesn't already have it.)

## Troubleshooting

Cutout writes a small log to `%TEMP%\cutout-perf.log` each time it starts.
If something goes wrong, attach that file when you report the problem.

## Build from source

You need Node.js (LTS), Rust, and on Windows the Microsoft C++ Build Tools
("Desktop development with C++"). WebView2 is preinstalled on Windows 11.

1. The AI models are not stored in git. Put these four files in `models/`:

   | File | Download |
   | --- | --- |
   | `model.onnx` | https://huggingface.co/onnx-community/BiRefNet_lite-ONNX/resolve/main/onnx/model.onnx |
   | `model_fp16.onnx` | https://huggingface.co/onnx-community/BiRefNet-ONNX/resolve/main/onnx/model_fp16.onnx |
   | `isnet-general-use.onnx` | https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx |
   | `u2netp.onnx` | https://github.com/danielgatis/rembg/releases/download/v0.0.0/u2netp.onnx |

2. Install dependencies: `npm install`
3. Run in development: `npm run tauri dev` (the first build takes a while)
4. Build the installer: `npm run tauri build`

On Windows the installer is written to
`src-tauri/target/release/bundle/nsis/`.

## Install (macOS, Apple Silicon)

Download the `.dmg`, open it and drag Cutout to Applications. Intel Macs are
not supported, because the AI engine has no Intel-Mac version.

Cutout isn't notarized by Apple, so macOS blocks the first launch. To open
it anyway: try to open Cutout once, then go to **System Settings > Privacy &
Security**, scroll down and click **Open Anyway**. If macOS says the app is
"damaged", run this once in Terminal and try again:

```bash
xattr -cr /Applications/Cutout.app
```

## Install (Linux, x86_64)

- **Debian / Ubuntu:** `sudo apt install ./Cutout_x.y.z_amd64.deb`
- **Any distro:** download the `.AppImage`, run `chmod +x Cutout_*.AppImage`,
  then double-click it or run it from a terminal.

Needs Ubuntu 24.04 or newer (or a distro with an equally new system
libraries) and a processor
with AVX2. On Linux, if the AVX2 check fails Cutout can only print a message
to the terminal, so launch it from a terminal if it won't start.

The macOS and Linux builds are new and have had less testing than Windows.

## Build the installers with GitHub Actions

`.github/workflows/build.yml` builds Windows, macOS and Linux installers on
GitHub's servers. Run it from the Actions tab (**Build installers > Run
workflow**) and download the installers from the run page, or push a tag such
as `v0.1.0` to get a draft Release with all three attached.

## License

Cutout is MIT licensed (see `LICENSE`). Bundled models and libraries have
their own licenses, listed in `THIRD_PARTY_NOTICES.md`.
