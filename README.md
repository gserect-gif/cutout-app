# Cutout — Local Background Remover

A desktop app that removes image backgrounds entirely on your own computer.
No account, no upload, no subscription, no resolution limit.

---

## 1. One-time setup (Windows 11)

You need two things installed: **Node.js** and **Rust**. If you already have
them, skip to step 2.

1. Install Node.js LTS: https://nodejs.org (download the "LTS" installer, run
   it, accept the defaults).
2. Install Rust: https://rustup.rs — download `rustup-init.exe`, run it,
   choose the default install option (`1`) when prompted.
3. Install the Tauri Windows prerequisites:
   - **Microsoft C++ Build Tools** — https://visualstudio.microsoft.com/visual-cpp-build-tools/
     Run the installer, check **"Desktop development with C++"**, install.
   - **WebView2** — already preinstalled on virtually all Windows 11 machines.
     If `npm run tauri dev` complains about it later, get it from
     https://developer.microsoft.com/microsoft-edge/webview2/
4. Close and reopen your terminal (PowerShell or Windows Terminal) so the new
   `PATH` entries take effect. Verify:

   ```powershell
   node --version
   rustc --version
   cargo --version
   ```

   All three should print a version number. If any says "not recognized",
   the corresponding install didn't complete — reopen the terminal or
   re-run that installer.

## 2. Get the AI model

The app uses **BiRefNet_lite**, exported to ONNX, MIT-licensed. It is not
checked into this project (224 MB) — download it once:

1. Go to: https://huggingface.co/onnx-community/BiRefNet_lite-ONNX/resolve/main/onnx/model.onnx
2. Save the file as `models/model.onnx` in this project folder (i.e.
   alongside `src-tauri/`, `src/`, `package.json` — NOT inside `src-tauri`).

Folder should look like:

```text
bgremover/
├── models/
│   └── model.onnx      ← the file you just downloaded
├── src/
├── src-tauri/
├── package.json
└── ...
```

## 3. Install dependencies

Open a terminal in this project folder and run:

```powershell
npm install
```

This downloads the frontend dependencies. The Rust dependencies (including
`ort` for ONNX Runtime) are fetched automatically the first time you run or
build the app — that first run will take a few minutes while Cargo compiles
everything.

## 4. Run it (development mode)

```powershell
npm run tauri dev
```

The first run compiles the whole Rust backend from scratch, so it can take
**5–15 minutes** depending on your machine — this is normal and only happens
once (or after dependency changes). Subsequent runs start in seconds.

A window should open with the app. Drop an image in, click **Remove
Background**, and you should see a transparent-checkerboard result appear.

**If something errors here, copy the full terminal output and send it back —
that's the fastest way to get it fixed.**

## 5. Build the installable app

Once `npm run tauri dev` works correctly:

```powershell
npm run tauri build
```

This produces the production build. On Windows you'll get both:

- An `.msi` installer
- An `.exe` (NSIS-based) installer

Look for them under:

```text
src-tauri/target/release/bundle/msi/Cutout_0.1.0_x64_en-US.msi
src-tauri/target/release/bundle/nsis/Cutout_0.1.0_x64-setup.exe
```

Run either installer, then launch **Cutout** from the Start menu like any
other app.

## Building for macOS or Linux later

The same source works unmodified — Tauri cross-packages to whatever OS you
run `npm run tauri build` on:

- **macOS**: run the same setup (Xcode Command Line Tools instead of MSVC
  Build Tools) on a Mac, `npm install`, `npm run tauri build` → produces a
  `.app` and `.dmg`.
- **Linux**: install the Tauri Linux prerequisites (webkit2gtk, etc. — see
  https://v2.tauri.app/start/prerequisites/#linux), then the same commands →
  produces an `.AppImage` and `.deb`.

You cannot cross-compile a Windows `.exe` from macOS/Linux or vice versa in
one step — you build once per OS, on that OS (or its CI runner).

## Model & license

- **Model:** BiRefNet_lite (ONNX export), by the BiRefNet authors
  (ZhengPeng7 et al.), ONNX conversion by the `onnx-community` on Hugging
  Face.
- **License:** MIT — free to use, modify, and redistribute, including
  bundling inside this app.
- **Source:** https://huggingface.co/onnx-community/BiRefNet_lite-ONNX

## What V1 does and doesn't do

Does: import PNG/JPG/WebP via drag-drop or browse, run local AI background
removal, preserve full original resolution, export a transparent PNG,
CPU-only (no GPU required).

Doesn't (by design, for V1): batch processing, multiple models, background
replacement, edge refinement controls, cloud anything.
