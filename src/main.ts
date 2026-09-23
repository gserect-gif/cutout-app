import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";

// ---------- Types mirrored from the Rust backend ----------

interface LoadedImageInfo {
  path: string;
  width: number;
  height: number;
}

interface RemoveBackgroundResult {
  resultPath: string;
  width: number;
  height: number;
  elapsedMs: number;
}

// ---------- DOM references ----------

const dropzone = document.getElementById("dropzone") as HTMLElement;
const fileInput = document.getElementById("fileInput") as HTMLInputElement;
const browseBtn = document.getElementById("browseBtn") as HTMLButtonElement;

const workspace = document.getElementById("workspace") as HTMLElement;
const fileNameEl = document.getElementById("fileName") as HTMLElement;
const fileDimsEl = document.getElementById("fileDims") as HTMLElement;
const newImageBtn = document.getElementById("newImageBtn") as HTMLButtonElement;
const removeBgBtn = document.getElementById("removeBgBtn") as HTMLButtonElement;

const originalImg = document.getElementById("originalImg") as HTMLImageElement;
const resultImg = document.getElementById("resultImg") as HTMLImageElement;
const resultPlaceholder = document.getElementById("resultPlaceholder") as HTMLElement;
const processingOverlay = document.getElementById("processingOverlay") as HTMLElement;
const processingLabel = document.getElementById("processingLabel") as HTMLElement;

const statusText = document.getElementById("statusText") as HTMLElement;
const exportBtn = document.getElementById("exportBtn") as HTMLButtonElement;
const toast = document.getElementById("toast") as HTMLElement;

// ---------- State ----------

let currentImagePath: string | null = null;
let currentImageWidth = 0;
let currentImageHeight = 0;
let currentResultPath: string | null = null;
let isProcessing = false;

const SUPPORTED_EXTENSIONS = ["png", "jpg", "jpeg", "webp"];

// ---------- Helpers ----------

function showToast(message: string, isError = false) {
  toast.textContent = message;
  toast.classList.remove("hidden", "error");
  if (isError) toast.classList.add("error");
  window.clearTimeout((showToast as any)._t);
  (showToast as any)._t = window.setTimeout(() => {
    toast.classList.add("hidden");
  }, 3200);
}

function setStatus(message: string, kind: "" | "error" | "success" = "") {
  statusText.textContent = message;
  statusText.classList.remove("error", "success");
  if (kind) statusText.classList.add(kind);
}

function extOf(path: string): string {
  const dot = path.lastIndexOf(".");
  return dot === -1 ? "" : path.slice(dot + 1).toLowerCase();
}

function resetWorkspace() {
  currentImagePath = null;
  currentImageWidth = 0;
  currentImageHeight = 0;
  currentResultPath = null;
  isProcessing = false;

  originalImg.src = "";
  resultImg.src = "";
  resultImg.classList.add("hidden");
  resultPlaceholder.classList.remove("hidden");
  processingOverlay.classList.add("hidden");

  exportBtn.disabled = true;
  removeBgBtn.disabled = false;
  removeBgBtn.textContent = "Remove Background";
  setStatus("");

  workspace.classList.add("hidden");
  dropzone.classList.remove("hidden");
}

async function loadImageFromPath(path: string) {
  const ext = extOf(path);
  if (!SUPPORTED_EXTENSIONS.includes(ext)) {
    showToast(`Unsupported file type: .${ext || "?"}. Use PNG, JPG, or WebP.`, true);
    return;
  }

  setStatus("Loading image…");
  try {
    const info = await invoke<LoadedImageInfo>("load_image", { path });

    currentImagePath = info.path;
    currentImageWidth = info.width;
    currentImageHeight = info.height;

    // Clear the previous result before loading the new preview. Now that
    // both original and result images are loaded via the asset protocol
    // (a direct file reference) rather than as base64 data URLs, there's no
    // large in-memory string to explicitly release here — just resetting
    // which file the result <img> points at.
    currentResultPath = null;
    resultImg.src = "";

    // Load the original image directly from disk via Tauri's asset
    // protocol, instead of round-tripping it through Rust as a base64
    // string. The browser/WebView streams the file itself.
    originalImg.src = convertFileSrc(info.path);
    resultImg.classList.add("hidden");
    resultPlaceholder.classList.remove("hidden");

    const shortName = info.path.split(/[\\/]/).pop() ?? info.path;
    fileNameEl.textContent = shortName;
    fileDimsEl.textContent = `${info.width} × ${info.height}px`;

    exportBtn.disabled = true;
    removeBgBtn.disabled = false;
    setStatus("Ready.");

    dropzone.classList.add("hidden");
    workspace.classList.remove("hidden");
  } catch (err) {
    console.error("load_image failed:", err);
    showToast(readableError(err), true);
    setStatus("Failed to load image.", "error");
  }
}

function readableError(err: unknown): string {
  if (typeof err === "string") return err;
  if (err && typeof err === "object" && "message" in err) return String((err as any).message);
  return "Something went wrong. Check the logs for details.";
}

// ---------- Drag & drop (browser-level, for visual feedback) ----------

let dragCounter = 0;

dropzone.addEventListener("dragenter", (e) => {
  e.preventDefault();
  dragCounter++;
  dropzone.classList.add("drag-over");
});

dropzone.addEventListener("dragover", (e) => {
  e.preventDefault();
});

dropzone.addEventListener("dragleave", (e) => {
  e.preventDefault();
  dragCounter = Math.max(0, dragCounter - 1);
  if (dragCounter === 0) dropzone.classList.remove("drag-over");
});

dropzone.addEventListener("drop", (e) => {
  e.preventDefault();
  dragCounter = 0;
  dropzone.classList.remove("drag-over");
  // Actual file path resolution happens via Tauri's native drag-drop event
  // (registered below), since browser File objects don't expose real FS paths
  // reliably across platforms in a webview context.
});

// Tauri v2 native file-drop event (gives real OS file paths)
import { getCurrentWebview } from "@tauri-apps/api/webview";

getCurrentWebview().onDragDropEvent((event) => {
  if (event.payload.type === "over") {
    dropzone.classList.add("drag-over");
  } else if (event.payload.type === "drop") {
    dropzone.classList.remove("drag-over");
    const paths = event.payload.paths;
    if (paths && paths.length > 0) {
      loadImageFromPath(paths[0]);
    }
  } else {
    dropzone.classList.remove("drag-over");
  }
});

// ---------- Browse button ----------

browseBtn.addEventListener("click", async () => {
  try {
    const selected = await open({
      multiple: false,
      filters: [{ name: "Images", extensions: SUPPORTED_EXTENSIONS }],
    });
    if (typeof selected === "string") {
      await loadImageFromPath(selected);
    }
  } catch (err) {
    console.error("open dialog failed:", err);
    showToast(readableError(err), true);
  }
});

// Fallback hidden <input type=file> is kept for environments where the dialog
// plugin isn't available; browseBtn primarily drives the dialog plugin above.
fileInput.addEventListener("change", () => {
  // Not used directly since we rely on the native dialog plugin for real
  // filesystem paths; left as a safety no-op.
});

// ---------- New image ----------

newImageBtn.addEventListener("click", () => {
  resetWorkspace();
});

// ---------- Remove background ----------

removeBgBtn.addEventListener("click", async () => {
  if (!currentImagePath || isProcessing) return;

  isProcessing = true;
  removeBgBtn.disabled = true;
  exportBtn.disabled = true;
  resultPlaceholder.classList.add("hidden");
  resultImg.classList.add("hidden");
  processingOverlay.classList.remove("hidden");
  processingLabel.textContent = "Removing background…";
  setStatus("Processing…");

  const start = performance.now();
  try {
    const result = await invoke<RemoveBackgroundResult>("remove_background", {
      path: currentImagePath,
    });

    currentResultPath = result.resultPath;
    // Cache-bust: if this session reprocesses the same source image twice,
    // the result filename could coincidentally repeat in edge cases: append
    // a query string so the WebView doesn't serve a stale cached copy.
    resultImg.src = `${convertFileSrc(result.resultPath)}?t=${Date.now()}`;
    resultImg.classList.remove("hidden");
    resultPlaceholder.classList.add("hidden");

    exportBtn.disabled = false;
    setStatus(`Done in ${(result.elapsedMs / 1000).toFixed(1)}s · ${result.width} × ${result.height}px`, "success");
  } catch (err) {
    console.error("remove_background failed:", err);
    showToast(readableError(err), true);
    setStatus("Background removal failed.", "error");
    resultPlaceholder.classList.remove("hidden");
  } finally {
    isProcessing = false;
    removeBgBtn.disabled = false;
    processingOverlay.classList.add("hidden");
    void start;
  }
});

// ---------- Export ----------

exportBtn.addEventListener("click", async () => {
  if (!currentResultPath || !currentImagePath || isProcessing) return;

  // Prevent "Remove Background" from running (and deleting the result file
  // this export is about to read) while the native save dialog is open.
  // The dialog is normally modal on Windows, making this unlikely in
  // practice, but guarding explicitly costs nothing and closes the gap.
  isProcessing = true;
  removeBgBtn.disabled = true;

  const baseName = currentImagePath.split(/[\\/]/).pop()?.replace(/\.[^.]+$/, "") ?? "cutout";

  try {
    const destination = await save({
      defaultPath: `${baseName}-cutout.png`,
      filters: [{ name: "PNG Image", extensions: ["png"] }],
    });
    if (!destination) return;

    setStatus("Exporting…");
    await invoke("export_png", {
      resultPath: currentResultPath,
      destination,
    });

    setStatus(`Saved to ${destination}`, "success");
    showToast("Exported successfully.");

    try {
      await revealItemInDir(destination);
    } catch (revealErr) {
      // Non-fatal: exporting succeeded even if we can't open the folder.
      console.warn("Could not reveal exported file:", revealErr);
    }
  } catch (err) {
    console.error("export_png failed:", err);
    showToast(readableError(err), true);
    setStatus("Export failed.", "error");
  } finally {
    isProcessing = false;
    removeBgBtn.disabled = false;
  }
});

// ---------- Init ----------

resetWorkspace();
