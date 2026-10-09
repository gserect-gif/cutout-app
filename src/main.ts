import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWebview } from "@tauri-apps/api/webview";

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

interface ExportInfo {
  originalSizeBytes: number | null;
  exportedSizeBytes: number;
}

interface ModelAvailability {
  key: string;
  available: boolean;
  sizeMb: number | null;
  description: string;
}

type BatchItemResult =
  | {
      status: "success";
      sourcePath: string;
      resultPath: string;
      width: number;
      height: number;
      elapsedMs: number;
    }
  | {
      status: "failed";
      sourcePath: string;
      error: string;
    };

// ---------- DOM references ----------

const dropzone = document.getElementById("dropzone") as HTMLElement;
const fileInput = document.getElementById("fileInput") as HTMLInputElement;
const browseBtn = document.getElementById("browseBtn") as HTMLButtonElement;
const browseMultipleBtn = document.getElementById("browseMultipleBtn") as HTMLButtonElement;
const browseFolderBtn = document.getElementById("browseFolderBtn") as HTMLButtonElement;

const workspace = document.getElementById("workspace") as HTMLElement;
const fileNameEl = document.getElementById("fileName") as HTMLElement;
const fileDimsEl = document.getElementById("fileDims") as HTMLElement;
const imageBackBtn = document.getElementById("imageBackBtn") as HTMLButtonElement;
const removeBgBtn = document.getElementById("removeBgBtn") as HTMLButtonElement;
const cancelBtn = document.getElementById("cancelBtn") as HTMLButtonElement;

const originalImg = document.getElementById("originalImg") as HTMLImageElement;
const resultImg = document.getElementById("resultImg") as HTMLImageElement;
const resultPlaceholder = document.getElementById("resultPlaceholder") as HTMLElement;
const processingOverlay = document.getElementById("processingOverlay") as HTMLElement;
const processingLabel = document.getElementById("processingLabel") as HTMLElement;

const statusText = document.getElementById("statusText") as HTMLElement;
const exportBtn = document.getElementById("exportBtn") as HTMLButtonElement;
const toast = document.getElementById("toast") as HTMLElement;

const batchWorkspace = document.getElementById("batchWorkspace") as HTMLElement;
const batchSummary = document.getElementById("batchSummary") as HTMLElement;
const batchList = document.getElementById("batchList") as HTMLElement;
const batchBackBtn = document.getElementById("batchBackBtn") as HTMLButtonElement;
const batchStartBtn = document.getElementById("batchStartBtn") as HTMLButtonElement;
const batchCancelBtn = document.getElementById("batchCancelBtn") as HTMLButtonElement;
const batchStatusText = document.getElementById("batchStatusText") as HTMLElement;
const batchExportBtn = document.getElementById("batchExportBtn") as HTMLButtonElement;

const settingsBtn = document.getElementById("settingsBtn") as HTMLButtonElement;
const settingsOverlay = document.getElementById("settingsOverlay") as HTMLElement;
const settingsCloseBtn = document.getElementById("settingsCloseBtn") as HTMLButtonElement;
const themeToggle = document.getElementById("themeToggle") as HTMLElement;

const modelSelect = document.getElementById("modelSelect") as HTMLSelectElement;
const batchModelSelect = document.getElementById("batchModelSelect") as HTMLSelectElement;

const originalFrame = document.getElementById("originalFrame") as HTMLElement;
const resultFrame = document.getElementById("resultFrame") as HTMLElement;
const zoomInBtn = document.getElementById("zoomInBtn") as HTMLButtonElement;
const zoomOutBtn = document.getElementById("zoomOutBtn") as HTMLButtonElement;
const zoomResetBtn = document.getElementById("zoomResetBtn") as HTMLButtonElement;

// ---------- State ----------

let currentImagePath: string | null = null;
let currentImageWidth = 0;
let currentImageHeight = 0;
let currentResultPath: string | null = null;
let isProcessing = false;

// Remembers the folder the user last exported to, within this session, so
// repeated exports default to the same place instead of always opening
// wherever the OS's save dialog last happened to be.
let lastExportDir: string | null = null;

interface BatchItem {
  sourcePath: string;
  fileName: string;
  status: "pending" | "processing" | "done" | "failed";
  resultPath?: string;
  error?: string;
  thumbPath?: string;
}

let batchItems: BatchItem[] = [];
let isBatchProcessing = false;

const SUPPORTED_EXTENSIONS = ["png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif", "gif"];

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

// `resetZoom` is defined further down the file; this indirection lets
// resetWorkspace() call it without depending on declaration order.
function resetZoomIfReady() {
  if (typeof resetZoom === "function") resetZoom();
}

function extOf(path: string): string {
  const dot = path.lastIndexOf(".");
  return dot === -1 ? "" : path.slice(dot + 1).toLowerCase();
}

function resetWorkspace() {
  resetZoomIfReady();
  exportPopover.classList.add("hidden");
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
  removeBgBtn.classList.remove("hidden");
  removeBgBtn.textContent = "Remove Background";
  cancelBtn.classList.add("hidden");
  setStatus("");

  workspace.classList.add("hidden");
  batchWorkspace.classList.add("hidden");
  dropzone.classList.remove("hidden");
}

function resetBatchWorkspace() {
  exportPopover.classList.add("hidden");
  batchItems = [];
  isBatchProcessing = false;
  batchList.innerHTML = "";
  batchExportBtn.disabled = true;
  batchStartBtn.disabled = false;
  batchStartBtn.classList.remove("hidden");
  batchCancelBtn.classList.add("hidden");
  setBatchStatus("");

  batchWorkspace.classList.add("hidden");
  workspace.classList.add("hidden");
  dropzone.classList.remove("hidden");
}

function setBatchStatus(message: string, kind: "" | "error" | "success" = "") {
  batchStatusText.textContent = message;
  batchStatusText.classList.remove("error", "success");
  if (kind) batchStatusText.classList.add(kind);
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
    resetZoomIfReady();

    // Load the original image directly from disk via Tauri's asset
    // protocol, instead of round-tripping it through Rust as a base64
    // string. The browser/WebView streams the file itself.
    // Some formats (TIFF in particular) decode fine in Rust but can't be
    // shown by the WebView, which only handles web image formats. If the
    // direct load fails, ask the backend for a downscaled PNG preview of the
    // same image and show that instead. Processing and export still use the
    // original full-resolution file either way.
    const loadedPath = info.path;
    originalImg.onerror = async () => {
      originalImg.onerror = null; // one fallback attempt only, never a loop
      if (currentImagePath !== loadedPath) return; // image was replaced or cleared
      try {
        const previewPath = await invoke<string>("generate_preview", { path: loadedPath });
        if (currentImagePath === loadedPath) originalImg.src = convertFileSrc(previewPath);
      } catch (err) {
        console.error("generate_preview failed:", err);
        showToast("Couldn't show a preview of this image, but you can still remove its background.", true);
      }
    };
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

// ---------- Batch mode ----------

function fileNameOf(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

function loadBatchFromPaths(paths: string[]) {
  const supported = paths.filter((p) => SUPPORTED_EXTENSIONS.includes(extOf(p)));
  const skipped = paths.length - supported.length;

  if (supported.length === 0) {
    showToast("No supported images found (PNG, JPG, or WebP).", true);
    return;
  }

  batchItems = supported.map((p) => ({
    sourcePath: p,
    fileName: fileNameOf(p),
    status: "pending",
  }));

  renderBatchList();
  void loadBatchThumbnails();
  batchSummary.textContent = `${supported.length} image${supported.length === 1 ? "" : "s"} selected`;
  if (skipped > 0) {
    showToast(`Skipped ${skipped} unsupported file${skipped === 1 ? "" : "s"}.`);
  }

  batchExportBtn.disabled = true;
  batchStartBtn.disabled = false;
  setBatchStatus("Ready.");

  dropzone.classList.add("hidden");
  workspace.classList.add("hidden");
  batchWorkspace.classList.remove("hidden");
}

// Thumbnails are generated one at a time, after the list already renders, so
// a large batch shows its filenames immediately rather than waiting on every
// thumbnail to finish decoding first. Each arrival updates just its own row.
async function loadBatchThumbnails() {
  const items = batchItems;
  for (const item of items) {
    if (batchItems !== items) return; // a new batch replaced this one
    try {
      const thumbPath = await invoke<string>("generate_thumbnail", { path: item.sourcePath });
      if (batchItems !== items) return;
      item.thumbPath = thumbPath;
      const row = batchList.querySelector<HTMLElement>(`[data-path="${CSS.escape(item.sourcePath)}"]`);
      const img = row?.querySelector<HTMLImageElement>(".batch-row-thumb");
      if (img) img.src = convertFileSrc(thumbPath);
    } catch (err) {
      console.warn("thumbnail generation failed for", item.sourcePath, err);
    }
  }
}

function renderBatchList() {
  batchList.innerHTML = "";
  for (const item of batchItems) {
    const row = document.createElement("div");
    row.className = "batch-row";
    row.dataset.path = item.sourcePath;

    const thumb = document.createElement("img");
    thumb.className = "batch-row-thumb";
    thumb.alt = "";
    if (item.thumbPath) thumb.src = convertFileSrc(item.thumbPath);

    const icon = document.createElement("span");
    icon.className = `batch-row-icon batch-row-icon-${item.status}`;
    icon.textContent =
      item.status === "done" ? "✓" : item.status === "failed" ? "!" : item.status === "processing" ? "" : "";

    const name = document.createElement("span");
    name.className = "batch-row-name";
    name.textContent = item.fileName;

    const detail = document.createElement("span");
    detail.className = "batch-row-detail";
    detail.textContent =
      item.status === "failed"
        ? item.error ?? "Failed"
        : item.status === "processing"
          ? "Processing…"
          : item.status === "done"
            ? "Done"
            : "Pending";

    row.append(thumb, icon, name, detail);
    batchList.appendChild(row);
  }
}

function updateBatchRow(sourcePath: string) {
  const item = batchItems.find((i) => i.sourcePath === sourcePath);
  const row = batchList.querySelector<HTMLElement>(`[data-path="${CSS.escape(sourcePath)}"]`);
  if (!item || !row) return;

  row.querySelector(".batch-row-icon")!.className = `batch-row-icon batch-row-icon-${item.status}`;
  row.querySelector(".batch-row-icon")!.textContent =
    item.status === "done" ? "✓" : item.status === "failed" ? "!" : "";
  row.querySelector(".batch-row-detail")!.textContent =
    item.status === "failed"
      ? item.error ?? "Failed"
      : item.status === "processing"
        ? "Processing…"
        : item.status === "done"
          ? "Done"
          : "Pending";
}

// Listens for per-file progress events emitted from Rust during a batch run,
// updating that file's row live instead of waiting for the whole batch to
// finish before showing anything.
listen<BatchItemResult>("batch-progress", (event) => {
  const payload = event.payload;
  const item = batchItems.find((i) => i.sourcePath === payload.sourcePath);
  if (!item) return;

  if (payload.status === "success") {
    item.status = "done";
    item.resultPath = payload.resultPath;
  } else {
    item.status = "failed";
    item.error = payload.error;
  }
  updateBatchRow(payload.sourcePath);

  const done = batchItems.filter((i) => i.status === "done" || i.status === "failed").length;
  setBatchStatus(`Processing ${done} of ${batchItems.length}…`);
});

batchStartBtn.addEventListener("click", async () => {
  if (isBatchProcessing || batchItems.length === 0) return;

  isBatchProcessing = true;
  batchStartBtn.disabled = true;
  batchStartBtn.classList.add("hidden");
  batchCancelBtn.classList.remove("hidden");
  batchExportBtn.disabled = true;
  batchItems.forEach((i) => (i.status = "pending"));
  renderBatchList();
  setBatchStatus(`Processing 0 of ${batchItems.length}…`);

  try {
    const results = await invoke<BatchItemResult[]>("remove_background_batch", {
      paths: batchItems.map((i) => i.sourcePath),
      model: batchModelSelect.value || null,
    });

    const succeeded = batchItems.filter((i) => i.status === "done").length;
    const failed = batchItems.filter((i) => i.status === "failed").length;
    // A cancelled batch stops Rust-side with `break`, so fewer results come
    // back than items were submitted — no exception is thrown for this case
    // (cancelling isn't an error), so detect it by the count mismatch
    // instead, matching the status wording used in the single-image flow.
    const wasCancelled = results.length < batchItems.length;

    if (wasCancelled) {
      setBatchStatus(`Cancelled after ${succeeded + failed} of ${batchItems.length}.`);
    } else {
      setBatchStatus(
        failed > 0 ? `Done — ${succeeded} succeeded, ${failed} failed.` : `Done — all ${succeeded} succeeded.`,
        failed > 0 ? "error" : "success",
      );
    }
    batchExportBtn.disabled = succeeded === 0;
  } catch (err) {
    console.error("remove_background_batch failed:", err);
    showToast(readableError(err), true);
    setBatchStatus("Batch processing failed.", "error");
  } finally {
    isBatchProcessing = false;
    batchStartBtn.disabled = false;
    batchStartBtn.classList.remove("hidden");
    batchCancelBtn.classList.add("hidden");
  }
});

batchCancelBtn.addEventListener("click", async () => {
  batchCancelBtn.disabled = true;
  setBatchStatus("Cancelling after the current image…");
  try {
    await invoke("cancel_processing");
  } catch (err) {
    console.error("cancel_processing failed:", err);
  } finally {
    batchCancelBtn.disabled = false;
  }
});

batchBackBtn.addEventListener("click", () => {
  resetBatchWorkspace();
});

batchExportBtn.addEventListener("click", async () => {
  const succeededItems = batchItems.filter(
    (i): i is BatchItem & { resultPath: string } => i.status === "done" && !!i.resultPath,
  );
  if (succeededItems.length === 0) return;

  try {
    const destinationDir = await open({
      directory: true,
      multiple: false,
      defaultPath: lastExportDir ?? undefined,
      title: "Choose a folder for exported images",
    });
    if (!destinationDir || typeof destinationDir !== "string") return;

    lastExportDir = destinationDir;
    setBatchStatus("Exporting…");

    const failures = await invoke<string[]>("export_batch", {
      items: succeededItems.map((i) => ({ sourcePath: i.sourcePath, resultPath: i.resultPath })),
      destinationDir,
      format: exportOptions.format,
      background: effectiveBackground(),
      backgroundImage: effectiveBackgroundImage(),
      edge: exportOptions.edge,
    });

    if (failures.length === 0) {
      setBatchStatus(`Exported ${succeededItems.length} image${succeededItems.length === 1 ? "" : "s"}.`, "success");
      showToast("Export complete.");
    } else {
      setBatchStatus(`Exported with ${failures.length} failure${failures.length === 1 ? "" : "s"}.`, "error");
    }

    try {
      await revealItemInDir(destinationDir);
    } catch (revealErr) {
      console.warn("Could not reveal export folder:", revealErr);
    }
  } catch (err) {
    console.error("export_batch failed:", err);
    showToast(readableError(err), true);
    setBatchStatus("Export failed.", "error");
  }
});

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

getCurrentWebview().onDragDropEvent((event) => {
  if (event.payload.type === "over") {
    dropzone.classList.add("drag-over");
  } else if (event.payload.type === "drop") {
    dropzone.classList.remove("drag-over");
    const paths = event.payload.paths;
    if (!paths || paths.length === 0) return;

    if (paths.length === 1) {
      loadImageFromPath(paths[0]);
    } else {
      loadBatchFromPaths(paths);
    }
  } else {
    dropzone.classList.remove("drag-over");
  }
});

// ---------- Browse buttons ----------

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

browseMultipleBtn.addEventListener("click", async () => {
  try {
    const selected = await open({
      multiple: true,
      filters: [{ name: "Images", extensions: SUPPORTED_EXTENSIONS }],
    });
    if (Array.isArray(selected) && selected.length > 0) {
      if (selected.length === 1) {
        await loadImageFromPath(selected[0]);
      } else {
        loadBatchFromPaths(selected);
      }
    }
  } catch (err) {
    console.error("open dialog failed:", err);
    showToast(readableError(err), true);
  }
});

browseFolderBtn.addEventListener("click", async () => {
  try {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected !== "string") return;

    setBatchStatus("Scanning folder…");
    const paths = await invoke<string[]>("scan_folder_for_images", { folderPath: selected });
    if (paths.length === 0) {
      showToast("No supported images found in that folder.", true);
      return;
    }
    loadBatchFromPaths(paths);
  } catch (err) {
    console.error("folder scan failed:", err);
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

imageBackBtn.addEventListener("click", () => {
  resetWorkspace();
});

// ---------- Remove background ----------

removeBgBtn.addEventListener("click", async () => {
  if (!currentImagePath || isProcessing) return;

  isProcessing = true;
  removeBgBtn.disabled = true;
  removeBgBtn.classList.add("hidden");
  cancelBtn.classList.remove("hidden");
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
      model: modelSelect.value || null,
    });

    currentResultPath = result.resultPath;
    // Cache-bust: if this session reprocesses the same source image twice,
    // the result filename could coincidentally repeat in edge cases: append
    // a query string so the WebView doesn't serve a stale cached copy.
    resultImg.src = `${convertFileSrc(result.resultPath)}?t=${Date.now()}`;
    resultImg.classList.remove("hidden");
    resultPlaceholder.classList.add("hidden");
    // If an edge setting is active, swap in the adjusted version of the result.
    if (exportOptions.edge !== 0) void refreshResultPreview();

    exportBtn.disabled = false;
    setStatus(`Done in ${(result.elapsedMs / 1000).toFixed(1)}s · ${result.width} × ${result.height}px`, "success");
  } catch (err) {
    const message = readableError(err);
    if (message === "Cancelled.") {
      setStatus("Cancelled.");
    } else {
      console.error("remove_background failed:", err);
      showToast(message, true);
      setStatus("Background removal failed.", "error");
    }
    resultPlaceholder.classList.remove("hidden");
  } finally {
    isProcessing = false;
    removeBgBtn.disabled = false;
    removeBgBtn.classList.remove("hidden");
    cancelBtn.classList.add("hidden");
    processingOverlay.classList.add("hidden");
    void start;
  }
});

cancelBtn.addEventListener("click", async () => {
  cancelBtn.disabled = true;
  processingLabel.textContent = "Cancelling…";
  try {
    await invoke("cancel_processing");
  } catch (err) {
    console.error("cancel_processing failed:", err);
  } finally {
    cancelBtn.disabled = false;
  }
});

// ---------- Export options (format + background color) ----------

type ExportFormat = "png" | "jpg" | "webp";
type BackgroundMode = "transparent" | "color" | "image";

interface ExportOptions {
  format: ExportFormat;
  bgMode: BackgroundMode;
  color: string; // always "#rrggbb", lowercase
  bgImagePath: string | null; // picture to put behind the cutout (Image mode)
  edge: number; // -100 (sharper) to 100 (softer), 0 = as the AI made it
}

interface Hsv {
  h: number; // 0-360, 0 at the top of the wheel, clockwise
  s: number; // 0-1, 0 at the centre, 1 at the edge
  v: number; // 0-1, brightness
}

const EXPORT_STORAGE_KEY = "cutout-export-options";
const FORMAT_LABELS: Record<ExportFormat, string> = { png: "PNG", jpg: "JPG", webp: "WebP" };
const FORMAT_DIALOG_NAMES: Record<ExportFormat, string> = {
  png: "PNG Image",
  jpg: "JPEG Image",
  webp: "WebP Image",
};
const WHEEL_RADIUS = 80; // matches the 160px .color-wheel in styles.css

const exportPopover = document.getElementById("exportPopover") as HTMLElement;
const formatToggle = document.getElementById("formatToggle") as HTMLElement;
const bgModeToggle = document.getElementById("bgModeToggle") as HTMLElement;
const colorPicker = document.getElementById("colorPicker") as HTMLElement;
const colorWheel = document.getElementById("colorWheel") as HTMLElement;
const colorWheelShade = document.getElementById("colorWheelShade") as HTMLElement;
const colorWheelThumb = document.getElementById("colorWheelThumb") as HTMLElement;
const valueSlider = document.getElementById("valueSlider") as HTMLInputElement;
const colorPreview = document.getElementById("colorPreview") as HTMLElement;
const hexInput = document.getElementById("hexInput") as HTMLInputElement;
const colorSwatches = document.getElementById("colorSwatches") as HTMLElement;
const exportHint = document.getElementById("exportHint") as HTMLElement;
const imagePicker = document.getElementById("imagePicker") as HTMLElement;
const chooseBgImageBtn = document.getElementById("chooseBgImageBtn") as HTMLButtonElement;
const bgImageName = document.getElementById("bgImageName") as HTMLElement;
const edgeSlider = document.getElementById("edgeSlider") as HTMLInputElement;
const edgeResetBtn = document.getElementById("edgeResetBtn") as HTMLButtonElement;
const appVersion = document.getElementById("appVersion") as HTMLElement;

/** Accepts "#rgb" or "#rrggbb" (the # is optional); returns "#rrggbb" or null. */
function normalizeHex(input: unknown): string | null {
  if (typeof input !== "string") return null;
  let hex = input.trim().replace(/^#/, "");
  if (/^[0-9a-fA-F]{3}$/.test(hex)) {
    hex = hex
      .split("")
      .map((c) => c + c)
      .join("");
  }
  return /^[0-9a-fA-F]{6}$/.test(hex) ? `#${hex.toLowerCase()}` : null;
}

function hexToHsv(hex: string): Hsv {
  const r = parseInt(hex.slice(1, 3), 16) / 255;
  const g = parseInt(hex.slice(3, 5), 16) / 255;
  const b = parseInt(hex.slice(5, 7), 16) / 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const d = max - min;

  let h = 0;
  if (d !== 0) {
    if (max === r) h = ((g - b) / d) % 6;
    else if (max === g) h = (b - r) / d + 2;
    else h = (r - g) / d + 4;
    h *= 60;
    if (h < 0) h += 360;
  }
  return { h, s: max === 0 ? 0 : d / max, v: max };
}

function hsvToHex({ h, s, v }: Hsv): string {
  const c = v * s;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = v - c;

  let r = 0;
  let g = 0;
  let b = 0;
  if (h < 60) [r, g, b] = [c, x, 0];
  else if (h < 120) [r, g, b] = [x, c, 0];
  else if (h < 180) [r, g, b] = [0, c, x];
  else if (h < 240) [r, g, b] = [0, x, c];
  else if (h < 300) [r, g, b] = [x, 0, c];
  else [r, g, b] = [c, 0, x];

  const part = (n: number) =>
    Math.round((n + m) * 255)
      .toString(16)
      .padStart(2, "0");
  return `#${part(r)}${part(g)}${part(b)}`;
}

function loadExportOptions(): ExportOptions {
  const fallback: ExportOptions = {
    format: "png",
    bgMode: "transparent",
    color: "#ffffff",
    bgImagePath: null,
    edge: 0,
  };
  try {
    const raw = window.localStorage.getItem(EXPORT_STORAGE_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw);
    return {
      format: parsed.format === "jpg" || parsed.format === "webp" ? parsed.format : "png",
      bgMode: parsed.bgMode === "color" || parsed.bgMode === "image" ? parsed.bgMode : "transparent",
      color: normalizeHex(parsed.color) ?? fallback.color,
      bgImagePath: typeof parsed.bgImagePath === "string" && parsed.bgImagePath ? parsed.bgImagePath : null,
      edge: typeof parsed.edge === "number" ? Math.max(-100, Math.min(100, Math.round(parsed.edge))) : 0,
    };
  } catch {
    return fallback;
  }
}

function saveExportOptions() {
  try {
    window.localStorage.setItem(EXPORT_STORAGE_KEY, JSON.stringify(exportOptions));
  } catch (err) {
    console.warn("couldn't save export options:", err);
  }
}

const exportOptions: ExportOptions = loadExportOptions();
let hsv: Hsv = hexToHsv(exportOptions.color);

/** The color sent to the backend, or null when no solid color is wanted. */
function effectiveBackground(): string | null {
  return exportOptions.bgMode === "color" ? exportOptions.color : null;
}

/** The picture sent to the backend, or null. Image mode with no picture chosen yet means none. */
function effectiveBackgroundImage(): string | null {
  return exportOptions.bgMode === "image" ? exportOptions.bgImagePath : null;
}

/** Solid color the user will get behind the cutout, or null for none. JPG can't be transparent, so it is white. */
function previewBackground(): string | null {
  if (exportOptions.bgMode === "color") return exportOptions.color;
  if (effectiveBackgroundImage()) return null; // the picture is shown instead
  return exportOptions.format === "jpg" ? "#ffffff" : null;
}

function renderColorPicker(updateHexField: boolean) {
  const rad = (hsv.h * Math.PI) / 180;
  colorWheelThumb.style.left = `${WHEEL_RADIUS + Math.sin(rad) * hsv.s * WHEEL_RADIUS}px`;
  colorWheelThumb.style.top = `${WHEEL_RADIUS - Math.cos(rad) * hsv.s * WHEEL_RADIUS}px`;
  colorWheelShade.style.opacity = String(1 - hsv.v);

  valueSlider.value = String(Math.round(hsv.v * 100));
  valueSlider.style.background = `linear-gradient(to right, #000000, ${hsvToHex({ h: hsv.h, s: hsv.s, v: 1 })})`;

  const hex = hsvToHex(hsv);
  colorPreview.style.backgroundColor = hex;
  if (updateHexField) hexInput.value = hex.toUpperCase();
}

/** Refreshes everything that depends on the export options. */
function updateExportUi(updateHexField = true) {
  for (const btn of formatToggle.querySelectorAll<HTMLButtonElement>(".theme-option")) {
    btn.classList.toggle("active", btn.dataset.format === exportOptions.format);
  }
  for (const btn of bgModeToggle.querySelectorAll<HTMLButtonElement>(".theme-option")) {
    btn.classList.toggle("active", btn.dataset.bgMode === exportOptions.bgMode);
  }

  const showPicker = exportOptions.bgMode === "color";
  colorPicker.classList.toggle("hidden", !showPicker);
  if (showPicker) renderColorPicker(updateHexField);

  const imageMode = exportOptions.bgMode === "image";
  imagePicker.classList.toggle("hidden", !imageMode);
  bgImageName.textContent = exportOptions.bgImagePath
    ? fileNameOf(exportOptions.bgImagePath)
    : "No image chosen";

  edgeSlider.value = String(exportOptions.edge);

  const jpgNeedsColor = exportOptions.format === "jpg" && exportOptions.bgMode === "transparent";
  const needsImage = imageMode && !exportOptions.bgImagePath;
  const hint = jpgNeedsColor
    ? "JPG can't be transparent, so the background will be white. Choose Color or Image to change it."
    : needsImage
      ? "Choose a picture to put behind the cutout. Until then the background stays transparent."
      : "";
  exportHint.classList.toggle("hidden", !hint);
  exportHint.textContent = hint;

  // Summary on both footer buttons (single image and batch).
  const backdropImage = effectiveBackgroundImage();
  const preview = previewBackground();
  const summaryColor = backdropImage
    ? "Image"
    : exportOptions.bgMode === "color"
      ? exportOptions.color.toUpperCase()
      : preview
        ? "White"
        : "Transparent";
  const summary = `${FORMAT_LABELS[exportOptions.format]} \u00b7 ${summaryColor}`;
  for (const btn of document.querySelectorAll<HTMLButtonElement>(".export-options-btn")) {
    const swatch = btn.querySelector<HTMLElement>(".export-swatch");
    const label = btn.querySelector<HTMLElement>(".export-summary-text");
    if (label) label.textContent = summary;
    if (swatch) {
      swatch.classList.toggle("is-transparent", !backdropImage && preview === null);
      swatch.style.backgroundColor = preview ?? "";
      swatch.style.backgroundImage = backdropImage ? `url("${convertFileSrc(backdropImage)}")` : "";
      swatch.style.backgroundSize = backdropImage ? "cover" : "";
    }
  }

  exportBtn.textContent = `Export ${FORMAT_LABELS[exportOptions.format]}`;

  // Show the chosen background behind the result so it's visible before
  // exporting. A picture is drawn on the <img> itself, so it covers exactly
  // the area the exported image will cover (and zooms along with it).
  resultFrame.classList.toggle("checkerboard", !backdropImage && preview === null);
  resultFrame.style.backgroundColor = backdropImage ? "" : (preview ?? "");
  resultImg.style.backgroundImage = backdropImage ? `url("${convertFileSrc(backdropImage)}")` : "";
  resultImg.style.backgroundSize = "cover";
  resultImg.style.backgroundPosition = "center";
}

function setColorFromHsv() {
  exportOptions.color = hsvToHex(hsv);
  exportOptions.bgMode = "color";
  saveExportOptions();
  updateExportUi(false);
}

function pickFromWheel(e: PointerEvent) {
  const rect = colorWheel.getBoundingClientRect();
  const radius = rect.width / 2;
  const dx = e.clientX - rect.left - radius;
  const dy = e.clientY - rect.top - radius;
  let h = (Math.atan2(dx, -dy) * 180) / Math.PI; // 0 at the top, clockwise
  if (h < 0) h += 360;
  hsv = { h, s: Math.min(1, Math.hypot(dx, dy) / radius), v: hsv.v };
  setColorFromHsv();
}

function setColorFromHex(hex: string) {
  hsv = hexToHsv(hex);
  exportOptions.color = hex;
  exportOptions.bgMode = "color";
  saveExportOptions();
  updateExportUi(false);
}

colorWheel.addEventListener("pointerdown", (e) => {
  colorWheel.setPointerCapture(e.pointerId);
  pickFromWheel(e);
});

colorWheel.addEventListener("pointermove", (e) => {
  if (colorWheel.hasPointerCapture(e.pointerId)) pickFromWheel(e);
});

valueSlider.addEventListener("input", () => {
  hsv = { ...hsv, v: Number(valueSlider.value) / 100 };
  setColorFromHsv();
});

// While typing, only a complete 6-digit value is applied, so a half-typed
// "#ff" doesn't flicker the preview. Short "#abc" forms are accepted on
// Enter or when the field loses focus.
hexInput.addEventListener("input", () => {
  const digits = hexInput.value.trim().replace(/^#/, "");
  if (digits.length !== 6) return;
  const hex = normalizeHex(digits);
  if (hex) setColorFromHex(hex);
});

hexInput.addEventListener("blur", () => {
  const hex = normalizeHex(hexInput.value);
  if (hex) setColorFromHex(hex);
  hexInput.value = exportOptions.color.toUpperCase();
});

hexInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") hexInput.blur();
});

colorSwatches.addEventListener("click", (e) => {
  const swatch = (e.target as HTMLElement).closest<HTMLButtonElement>(".color-swatch");
  const hex = normalizeHex(swatch?.dataset.color);
  if (hex) {
    setColorFromHex(hex);
    hexInput.value = hex.toUpperCase();
  }
});

formatToggle.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(".theme-option");
  const format = btn?.dataset.format;
  if (format !== "png" && format !== "jpg" && format !== "webp") return;
  exportOptions.format = format;
  saveExportOptions();
  updateExportUi();
});

bgModeToggle.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(".theme-option");
  const mode = btn?.dataset.bgMode;
  if (mode !== "transparent" && mode !== "color" && mode !== "image") return;
  exportOptions.bgMode = mode;
  saveExportOptions();
  updateExportUi();
});

chooseBgImageBtn.addEventListener("click", async () => {
  try {
    const selected = await open({
      multiple: false,
      title: "Choose a background picture",
      filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "webp", "bmp", "gif"] }],
    });
    if (typeof selected !== "string") return;
    exportOptions.bgImagePath = selected;
    exportOptions.bgMode = "image";
    saveExportOptions();
    updateExportUi();
  } catch (err) {
    console.error("background image dialog failed:", err);
    showToast(readableError(err), true);
  }
});

// Edge softness. The slider only changes how the exported file is made; the
// Result pane shows the effect through a backend-built preview, requested
// after the slider pauses so dragging stays smooth on large images.
let edgeTimer: number | undefined;
let edgeRequestId = 0;

async function refreshResultPreview() {
  const path = currentResultPath;
  if (!path) return;

  const requestId = ++edgeRequestId;
  try {
    const shown =
      exportOptions.edge === 0
        ? path
        : await invoke<string>("preview_edges", { resultPath: path, edge: exportOptions.edge });
    // A newer request or a different image has taken over since this started.
    if (requestId !== edgeRequestId || currentResultPath !== path) return;
    resultImg.src = `${convertFileSrc(shown)}?t=${Date.now()}`;
  } catch (err) {
    console.error("preview_edges failed:", err);
    showToast(readableError(err), true);
  }
}

edgeSlider.addEventListener("input", () => {
  exportOptions.edge = Number(edgeSlider.value);
  saveExportOptions();
  window.clearTimeout(edgeTimer);
  edgeTimer = window.setTimeout(() => void refreshResultPreview(), 250);
});

edgeResetBtn.addEventListener("click", () => {
  exportOptions.edge = 0;
  edgeSlider.value = "0";
  saveExportOptions();
  void refreshResultPreview();
});

function toggleExportPopover() {
  const opening = exportPopover.classList.contains("hidden");
  exportPopover.classList.toggle("hidden", !opening);
  if (opening) updateExportUi();
}

for (const btn of document.querySelectorAll<HTMLButtonElement>(".export-options-btn")) {
  btn.addEventListener("click", (e) => {
    e.stopPropagation(); // so the click-away handler below doesn't close it again
    toggleExportPopover();
  });
}

document.addEventListener("click", (e) => {
  if (exportPopover.classList.contains("hidden")) return;
  if (!exportPopover.contains(e.target as Node)) exportPopover.classList.add("hidden");
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
  const format = exportOptions.format;

  try {
    const destination = await save({
      defaultPath: lastExportDir ? `${lastExportDir}/${baseName}-cutout.${format}` : `${baseName}-cutout.${format}`,
      filters: [{ name: FORMAT_DIALOG_NAMES[format], extensions: [format] }],
    });
    if (!destination) return;

    // Remember the folder (not the exact filename) for next time.
    const dir = destination.slice(0, Math.max(destination.lastIndexOf("\\"), destination.lastIndexOf("/")));
    if (dir) lastExportDir = dir;

    setStatus("Exporting…");
    const info = await invoke<ExportInfo>("export_png", {
      originalPath: currentImagePath,
      resultPath: currentResultPath,
      destination,
      format,
      background: effectiveBackground(),
      backgroundImage: effectiveBackgroundImage(),
      edge: exportOptions.edge,
    });

    const exportedKb = Math.round(info.exportedSizeBytes / 1024);
    const sizeNote =
      info.originalSizeBytes != null
        ? `${exportedKb} KB (original was ${Math.round(info.originalSizeBytes / 1024)} KB)`
        : `${exportedKb} KB`;

    setStatus(`Saved to ${destination} \u00b7 ${sizeNote}`, "success");
    showToast(`Exported \u00b7 ${sizeNote}`);

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

// ---------- Clipboard paste ----------

window.addEventListener("paste", async (e) => {
  const items = e.clipboardData?.items;
  if (!items) return;

  for (const item of items) {
    if (!item.type.startsWith("image/")) continue;

    const blob = item.getAsFile();
    if (!blob) continue;

    try {
      // Sent as a raw ArrayBuffer request body rather than a JSON number
      // array — a pasted full-resolution image can be tens of millions of
      // bytes, and JSON-encoding that as `[12, 200, 4, ...]` is both far
      // slower to serialize and far larger over the wire than sending the
      // bytes directly. Tauri's invoke() supports this directly per its own
      // docs on sending a raw request body.
      const bytes = await blob.arrayBuffer();
      const path = await invoke<string>("save_pasted_image", bytes);
      await loadImageFromPath(path);
    } catch (err) {
      console.error("paste failed:", err);
      showToast(readableError(err), true);
    }
    return;
  }
});

// ---------- Keyboard shortcuts ----------

window.addEventListener("keydown", (e) => {
  // Never intercept keys while the user is typing in an input/textarea (none
  // currently exist in this UI, but this keeps the shortcut safe if one is
  // added later).
  const target = e.target as HTMLElement | null;
  if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;

  if (e.key === "Enter") {
    if (!workspace.classList.contains("hidden") && !removeBgBtn.disabled) {
      removeBgBtn.click();
    } else if (!batchWorkspace.classList.contains("hidden") && !batchStartBtn.disabled) {
      batchStartBtn.click();
    }
  } else if (e.key === "Escape") {
    if (!exportPopover.classList.contains("hidden")) {
      exportPopover.classList.add("hidden");
    } else if (!settingsOverlay.classList.contains("hidden")) {
      settingsOverlay.classList.add("hidden");
    } else if (isProcessing && !cancelBtn.classList.contains("hidden")) {
      // Mid-run, Esc cancels rather than resetting the screen out from under
      // a job the backend is still working on.
      cancelBtn.click();
    } else if (isBatchProcessing) {
      batchCancelBtn.click();
    } else if (!workspace.classList.contains("hidden")) {
      resetWorkspace();
    } else if (!batchWorkspace.classList.contains("hidden")) {
      resetBatchWorkspace();
    }
  }
});

// ---------- Theme ----------

type ThemeChoice = "dark" | "light" | "system";
const THEME_STORAGE_KEY = "cutout-theme";

function applyTheme(choice: ThemeChoice) {
  if (choice === "system") {
    document.documentElement.removeAttribute("data-theme");
  } else {
    document.documentElement.setAttribute("data-theme", choice);
  }

  for (const btn of themeToggle.querySelectorAll<HTMLButtonElement>(".theme-option")) {
    btn.classList.toggle("active", btn.dataset.themeChoice === choice);
  }
}

function loadStoredTheme(): ThemeChoice {
  const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
  return stored === "dark" || stored === "light" || stored === "system" ? stored : "system";
}

themeToggle.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(".theme-option");
  if (!btn) return;

  const choice = btn.dataset.themeChoice as ThemeChoice;
  applyTheme(choice);
  window.localStorage.setItem(THEME_STORAGE_KEY, choice);
});

settingsBtn.addEventListener("click", () => {
  settingsOverlay.classList.remove("hidden");
});

settingsCloseBtn.addEventListener("click", () => {
  settingsOverlay.classList.add("hidden");
});

settingsOverlay.addEventListener("click", (e) => {
  // Close when clicking the dimmed backdrop, not the panel itself.
  if (e.target === settingsOverlay) {
    settingsOverlay.classList.add("hidden");
  }
});

// Community links (Discord / feedback / GitHub). Opened through the opener
// plugin so they launch in the user's default browser, not inside the app.
for (const link of settingsOverlay.querySelectorAll<HTMLButtonElement>(".community-link")) {
  link.addEventListener("click", async () => {
    const url = link.dataset.url;
    if (!url) return;
    try {
      await openUrl(url);
    } catch (err) {
      console.error("openUrl failed:", err);
      showToast("Couldn't open the link in your browser.", true);
    }
  });
}

// ---------- Model picker ----------

const MODEL_NAMES: Record<string, string> = {
  fast: "Fast (U2Netp)",
  balanced: "Balanced (IS-Net)",
  best: "Best (BiRefNet Lite)",
  "best-plus": "Best+ (BiRefNet Full)",
};
const MODEL_STORAGE_KEY = "cutout-model";

function setModelSelectsLoading(loading: boolean) {
  for (const select of [modelSelect, batchModelSelect]) {
    select.disabled = loading;
    if (loading) {
      select.innerHTML = "";
      const opt = document.createElement("option");
      opt.textContent = "Loading models…";
      select.appendChild(opt);
    }
  }
}

async function initModelPicker() {
  setModelSelectsLoading(true);

  let models: ModelAvailability[] = [];
  try {
    models = await invoke<ModelAvailability[]>("list_available_models");
  } catch (err) {
    console.error("list_available_models failed:", err);
  }

  const usable = models.filter((m) => m.available);
  // If the backend call failed or nothing is found, fall back to the
  // original single model so the app still works exactly as before.
  const options: ModelAvailability[] =
    usable.length > 0 ? usable : [{ key: "best", available: true, sizeMb: null, description: "" }];

  const stored = window.localStorage.getItem(MODEL_STORAGE_KEY);
  const selected = options.some((m) => m.key === stored)
    ? stored!
    : options.some((m) => m.key === "best")
      ? "best"
      : options[0].key;

  for (const select of [modelSelect, batchModelSelect]) {
    select.disabled = false;
    select.innerHTML = "";
    for (const m of options) {
      const opt = document.createElement("option");
      opt.value = m.key;
      opt.textContent = MODEL_NAMES[m.key] ?? m.key;
      const sizeNote = m.sizeMb != null ? ` (${m.sizeMb} MB)` : "";
      opt.title = `${m.description}${sizeNote}`;
      select.appendChild(opt);
    }
    select.value = selected;
    // <select> itself doesn't reliably show the selected <option>'s title as
    // a hover tooltip across browsers/WebViews, so mirror it onto the
    // select element directly.
    select.title = options.find((m) => m.key === selected)?.description ?? "";
  }

  const onChange = (source: HTMLSelectElement) => {
    modelSelect.value = source.value;
    batchModelSelect.value = source.value;
    const desc = options.find((m) => m.key === source.value)?.description ?? "";
    modelSelect.title = desc;
    batchModelSelect.title = desc;
    window.localStorage.setItem(MODEL_STORAGE_KEY, source.value);
  };
  modelSelect.addEventListener("change", () => onChange(modelSelect));
  batchModelSelect.addEventListener("change", () => onChange(batchModelSelect));
}

// ---------- Preview zoom & pan ----------
// Both preview panes share one zoom level and pan offset so the original and
// the result always show the same region — the point of comparing them.

let zoom = 1;
let panX = 0;
let panY = 0;
const MIN_ZOOM = 1;
const MAX_ZOOM = 16;

function applyZoom() {
  const t = `translate(${panX}px, ${panY}px) scale(${zoom})`;
  originalImg.style.transform = t;
  resultImg.style.transform = t;
  zoomResetBtn.textContent = zoom === 1 ? "1:1" : `${Math.round(zoom * 100)}%`;
  originalFrame.classList.toggle("zoomed", zoom > 1);
  resultFrame.classList.toggle("zoomed", zoom > 1);
}

function setZoom(next: number, anchorX = 0, anchorY = 0) {
  const clamped = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, next));
  if (clamped === zoom) return;
  // Keep the point under the cursor fixed while zooming.
  const ratio = clamped / zoom;
  panX = anchorX - (anchorX - panX) * ratio;
  panY = anchorY - (anchorY - panY) * ratio;
  zoom = clamped;
  if (zoom === 1) {
    panX = 0;
    panY = 0;
  }
  applyZoom();
}

function resetZoom() {
  zoom = 1;
  panX = 0;
  panY = 0;
  applyZoom();
}

function frameCenterOffset(frame: HTMLElement, e: MouseEvent | WheelEvent) {
  const rect = frame.getBoundingClientRect();
  return { x: e.clientX - rect.left - rect.width / 2, y: e.clientY - rect.top - rect.height / 2 };
}

for (const frame of [originalFrame, resultFrame]) {
  frame.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const { x, y } = frameCenterOffset(frame, e);
      setZoom(zoom * (e.deltaY < 0 ? 1.15 : 1 / 1.15), x, y);
    },
    { passive: false },
  );

  frame.addEventListener("dblclick", () => resetZoom());

  let dragging = false;
  let startX = 0;
  let startY = 0;
  let startPanX = 0;
  let startPanY = 0;

  frame.addEventListener("mousedown", (e) => {
    if (zoom <= 1) return;
    dragging = true;
    startX = e.clientX;
    startY = e.clientY;
    startPanX = panX;
    startPanY = panY;
    frame.classList.add("panning");
    e.preventDefault();
  });

  window.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    panX = startPanX + (e.clientX - startX);
    panY = startPanY + (e.clientY - startY);
    applyZoom();
  });

  window.addEventListener("mouseup", () => {
    if (!dragging) return;
    dragging = false;
    frame.classList.remove("panning");
  });
}

zoomInBtn.addEventListener("click", () => setZoom(zoom * 1.5));
zoomOutBtn.addEventListener("click", () => setZoom(zoom / 1.5));
zoomResetBtn.addEventListener("click", () => resetZoom());

// ---------- Init ----------

applyTheme(loadStoredTheme());
resetWorkspace();
updateExportUi();

// Show the app version in Settings. If it can't be read, the line stays hidden.
getVersion()
  .then((version) => {
    appVersion.textContent = `Cutout v${version}`;
    appVersion.classList.remove("hidden");
  })
  .catch((err) => console.warn("couldn't read app version:", err));
void initModelPicker();
