// Tauri 2 globals: __TAURI__.core.invoke, __TAURI__.event.listen
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const qrHost = $("qr-host");
const qrEmpty = $("qr-empty");
const configText = $("config-text");
const errorBox = $("error");

// Bottom tab routing — the two `<main class="view">` sections are
// shown/hidden based on which tab is active. Cheaper than re-rendering;
// the image grid keeps its scroll position when switching back.
function setActiveTab(tab) {
  document.querySelectorAll(".tab-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.tab === tab);
  });
  document.querySelectorAll(".view").forEach((view) => {
    view.classList.toggle("hidden", view.id !== `view-${tab}`);
  });
}

function renderConfig(dto) {
  if (!dto) {
    configText.value = "";
    qrHost.innerHTML = "";
    qrHost.classList.add("hidden");
    qrEmpty.classList.remove("hidden");
    return;
  }
  configText.value = dto.json || "";
  if (dto.qr_svg) {
    qrHost.innerHTML = dto.qr_svg;
    qrHost.classList.remove("hidden");
    qrEmpty.classList.add("hidden");
  } else {
    qrHost.innerHTML = "";
    qrHost.classList.add("hidden");
    qrEmpty.classList.remove("hidden");
  }
}

function showError(msg) {
  if (!msg) {
    errorBox.classList.add("hidden");
    errorBox.textContent = "";
    return;
  }
  errorBox.textContent = msg;
  errorBox.classList.remove("hidden");
}

let toastTimer = null;
function toast(message) {
  const t = $("toast");
  t.textContent = message;
  t.classList.add("show");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove("show"), 2000);
}

const PILL_CLASSES = ["pill-neutral", "pill-info", "pill-ok", "pill-error"];
function setStatus(state) {
  const pill = $("status-pill");
  const label = $("status-label");
  PILL_CLASSES.forEach((c) => pill.classList.remove(c));
  let cls = "pill-neutral";
  let text = "等待启动";
  switch (state?.kind) {
    case "connecting":
      cls = "pill-info";
      text = "连接中…";
      break;
    case "connected":
      cls = "pill-ok";
      text = "已连接 · 同步中";
      break;
    case "disconnected":
      cls = "pill-neutral";
      text = "已断开,正在重连";
      break;
    case "error":
      cls = "pill-error";
      text = `连接出错:${state.message ?? ""}`;
      break;
    case "idle":
    default:
      cls = "pill-neutral";
      text = "等待启动";
      break;
  }
  pill.classList.add(cls);
  label.textContent = text;
}

// ─── Image tab ───

/** In-memory mirror of bridge::image_history. Bridge pushes via
 *  "image-event"; we also seed from cmd_recent_images on first show.
 *  Newest first. */
const imageHistory = [];

function formatSize(bytes) {
  const kb = Math.max(1, Math.round(bytes / 1024));
  if (kb >= 1024) return (kb / 1024).toFixed(1) + " MB";
  return kb + " KB";
}

function relativeTime(tsMillis) {
  const delta = Math.max(0, Date.now() - tsMillis);
  if (delta < 60_000) return Math.round(delta / 1000) + " 秒前";
  if (delta < 3_600_000) return Math.round(delta / 60_000) + " 分钟前";
  if (delta < 86_400_000) return Math.round(delta / 3_600_000) + " 小时前";
  return Math.round(delta / 86_400_000) + " 天前";
}

function renderImageHistory() {
  const received = imageHistory.filter((e) => e.direction === "received");
  const sent = imageHistory.filter((e) => e.direction === "sent");

  const renderInto = (gridId, emptyId, entries) => {
    const grid = $(gridId);
    const empty = $(emptyId);
    if (entries.length === 0) {
      grid.classList.add("hidden");
      empty.classList.remove("hidden");
      grid.innerHTML = "";
      return;
    }
    empty.classList.add("hidden");
    grid.classList.remove("hidden");
    grid.innerHTML = entries.map(cellHtml).join("");
    // Wire up the per-cell save buttons. innerHTML wipes any previously
    // attached listeners so this needs to run on every render.
    grid.querySelectorAll("button[data-save-id]").forEach((btn) => {
      btn.addEventListener("click", () => saveImage(btn.dataset.saveId));
    });
  };

  renderInto("received-grid", "received-empty", received);
  renderInto("sent-grid", "sent-empty", sent);
}

function cellHtml(entry) {
  const ts = entry.ts; // already millis from bridge
  return `
    <div class="image-cell">
      <div class="thumb">
        ${entry.thumbnail ? `<img src="${entry.thumbnail}" alt="" />` : "▥"}
      </div>
      <div class="meta">${entry.width}×${entry.height} · ${formatSize(entry.size_bytes)}</div>
      <div class="device" title="${escapeHtml(entry.device_name)}">${escapeHtml(entry.device_name)} · ${relativeTime(ts)}</div>
      <div class="cell-actions">
        <button data-save-id="${entry.id}">保存到本地…</button>
      </div>
    </div>
  `;
}

function escapeHtml(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function appendOrReplaceEntry(entry) {
  // Same id (sha256 of bytes) → de-dup by replacing in place. Otherwise
  // insert at the front so newest is first.
  const idx = imageHistory.findIndex((e) => e.id === entry.id);
  if (idx >= 0) imageHistory.splice(idx, 1);
  imageHistory.unshift(entry);
  // Cap matches bridge::HISTORY_LIMIT.
  if (imageHistory.length > 24) imageHistory.length = 24;
  renderImageHistory();
}

async function pickAndSendImages() {
  const input = $("file-input");
  // Reset value so re-selecting the same file fires `change` again.
  input.value = "";
  input.click();
}

async function handlePickedFiles(files) {
  for (const file of files) {
    try {
      const buf = await file.arrayBuffer();
      const bytes = Array.from(new Uint8Array(buf));
      const entry = await invoke("cmd_send_image_bytes", { bytes });
      appendOrReplaceEntry(entry);
      toast(`已发送 ${file.name}`);
    } catch (e) {
      toast(`发送失败:${e}`);
    }
  }
}

async function sendFromClipboard() {
  // The bridge already polls and publishes clipboard images automatically.
  // This button just nudges the user — we trust the listener to fire on
  // the next clipboard change. If they want an explicit "force send now"
  // path we can add a Rust command later.
  toast("剪切板图片会自动同步, 请直接复制图片");
}

async function saveImage(id) {
  const entry = imageHistory.find((e) => e.id === id);
  if (!entry) {
    toast("图片已过期");
    return;
  }
  const stamp = new Date(entry.ts).toISOString().replace(/[:.]/g, "-").slice(0, 19);
  const defaultName = `ClipBridge-${stamp}.png`;
  try {
    const path = await invoke("cmd_save_image_to_file", {
      id,
      defaultName,
    });
    if (path) toast(`已保存到 ${path}`);
  } catch (e) {
    toast(`保存失败:${e}`);
  }
}

async function init() {
  // Tab switching.
  document.querySelectorAll(".tab-btn").forEach((btn) => {
    btn.addEventListener("click", () => setActiveTab(btn.dataset.tab));
  });

  // Live state updates from the bridge.
  await listen("connection-state", (evt) => setStatus(evt.payload));
  setStatus(await invoke("cmd_current_state"));

  // Image events stream.
  await listen("image-event", (evt) => appendOrReplaceEntry(evt.payload));
  try {
    const recent = await invoke("cmd_recent_images");
    if (Array.isArray(recent)) {
      // Bridge returns newest-first too — replace the in-memory array
      // wholesale since this runs once on init.
      imageHistory.splice(0, imageHistory.length, ...recent);
      renderImageHistory();
    }
  } catch (e) {
    console.warn("recent images:", e);
  }

  // Load existing pairing if any.
  try {
    const dto = await invoke("cmd_load_pairing");
    if (dto) renderConfig(dto);
  } catch (e) {
    console.warn("load pairing:", e);
  }

  // Autostart toggle. Plugin commands are namespaced as
  // `plugin:autostart|<name>` and registered with the same permissions
  // listed in capabilities/default.json.
  const chk = $("chk-autostart");
  try {
    chk.checked = await invoke("plugin:autostart|is_enabled");
  } catch (e) {
    console.warn("autostart status:", e);
  }
  chk.addEventListener("change", async () => {
    try {
      if (chk.checked) {
        await invoke("plugin:autostart|enable");
        toast("已开启开机自启");
      } else {
        await invoke("plugin:autostart|disable");
        toast("已关闭开机自启");
      }
    } catch (e) {
      // Restore the checkbox to whatever the OS thinks the state is, so the
      // UI doesn't lie if the call failed (eg. permission revoked manually).
      try {
        chk.checked = await invoke("plugin:autostart|is_enabled");
      } catch (_) {}
      showError(`修改开机自启失败:${e}`);
    }
  });

  $("btn-generate").addEventListener("click", async () => {
    showError(null);
    try {
      const dto = await invoke("cmd_generate_pairing");
      renderConfig(dto);
      toast("已生成新配对");
    } catch (e) {
      showError(String(e));
      toast("生成失败");
    }
  });

  $("btn-save").addEventListener("click", async () => {
    showError(null);
    const json = configText.value.trim();
    if (!json) {
      showError("请先粘贴或生成配对 JSON");
      return;
    }
    try {
      const dto = await invoke("cmd_save_pairing", { json });
      renderConfig(dto);
      toast("已保存,开始同步");
    } catch (e) {
      showError(String(e));
      toast("保存失败");
    }
  });

  $("btn-copy").addEventListener("click", async () => {
    const text = configText.value;
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      toast("已复制 JSON");
    } catch (e) {
      // Fallback: select the textarea so the user can copy manually.
      configText.focus();
      configText.select();
    }
  });

  $("btn-reset").addEventListener("click", async () => {
    if (!confirm("确认重置?所有已配对设备都需要重新配对。")) return;
    try {
      await invoke("cmd_clear_pairing");
      renderConfig(null);
      showError(null);
      toast("已重置配对");
    } catch (e) {
      showError(String(e));
    }
  });

  $("btn-pick-image").addEventListener("click", () => pickAndSendImages());
  $("btn-send-clipboard").addEventListener("click", () => sendFromClipboard());
  $("file-input").addEventListener("change", (e) => {
    handlePickedFiles(Array.from(e.target.files || []));
  });
}

init();
