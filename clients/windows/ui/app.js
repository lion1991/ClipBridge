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
// the transfer grids keep their scroll position when switching back.
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
// Cached so the LAN-peer poll can re-render without losing the latest
// state.kind from the bridge stream.
let lastConnState = { kind: "idle" };
let lastLanPeerNames = [];
function setStatus(state) {
  lastConnState = state ?? { kind: "idle" };
  renderStatusPill();
}
function setLanPeerNames(names) {
  lastLanPeerNames = Array.isArray(names) ? names.slice().sort() : [];
  renderStatusPill();
}
function renderStatusPill() {
  const pill = $("status-pill");
  const label = $("status-label");
  PILL_CLASSES.forEach((c) => pill.classList.remove(c));
  let cls = "pill-neutral";
  let text = "等待启动";
  switch (lastConnState?.kind) {
    case "connecting":
      cls = "pill-info";
      text = "连接中…";
      break;
    case "connected":
      cls = "pill-ok";
      // Only annotate transport when actually connected — before that
      // the user cares about why the relay isn't up, not which lane
      // would have been used.
      text = lastLanPeerNames.length > 0
        ? `已连接 · 同步中 · 局域网 ${lastLanPeerNames.length} (${lastLanPeerNames.join(", ")})`
        : "已连接 · 同步中 · 仅中继";
      break;
    case "disconnected":
      cls = "pill-neutral";
      text = "已断开,正在重连";
      break;
    case "error":
      cls = "pill-error";
      text = `连接出错:${lastConnState.message ?? ""}`;
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

// ─── Transfer tab ───

/** In-memory mirror of bridge::image_history. Bridge pushes via
 *  "image-event"; we also seed from cmd_recent_images on first show.
 *  Newest first. */
const imageHistory = [];
const fileHistory = [];
let lanFilePeers = [];
const selectedFileTargets = new Set();

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

function renderFileTransfer() {
  const summary = $("file-target-summary");
  const list = $("file-peer-list");
  const selectAll = $("btn-file-select-all");
  const sendBtn = $("btn-pick-files");

  const validIds = new Set(lanFilePeers.map((p) => p.device_id));
  [...selectedFileTargets].forEach((id) => {
    if (!validIds.has(id)) selectedFileTargets.delete(id);
  });

  const selectedCount = [...selectedFileTargets].filter((id) => validIds.has(id)).length;
  if (lanFilePeers.length === 0) summary.textContent = "暂无可用 LAN 设备";
  else if (selectedCount === 0) summary.textContent = "未选择设备";
  else if (selectedCount === lanFilePeers.length) summary.textContent = `已选择全部 ${lanFilePeers.length} 台`;
  else summary.textContent = `已选择 ${selectedCount}/${lanFilePeers.length} 台`;

  selectAll.disabled = lanFilePeers.length === 0;
  selectAll.textContent = selectedCount === lanFilePeers.length && lanFilePeers.length > 0 ? "清空" : "全选";
  sendBtn.disabled = lanFilePeers.length === 0 || selectedCount === 0;

  if (lanFilePeers.length === 0) {
    list.innerHTML = `<div class="hint">等同组设备出现在局域网后可发送文件。</div>`;
  } else {
    list.innerHTML = lanFilePeers.map((peer) => `
      <label class="peer-row">
        <input type="checkbox" data-file-peer="${escapeHtml(peer.device_id)}" ${selectedFileTargets.has(peer.device_id) ? "checked" : ""} />
        <span class="peer-main">
          <span class="peer-name" title="${escapeHtml(peer.display_name)}">${escapeHtml(peer.display_name)}</span>
          <span class="peer-meta">${peer.candidate_count} 个地址</span>
        </span>
      </label>
    `).join("");
    list.querySelectorAll("input[data-file-peer]").forEach((input) => {
      input.addEventListener("change", () => {
        if (input.checked) selectedFileTargets.add(input.dataset.filePeer);
        else selectedFileTargets.delete(input.dataset.filePeer);
        renderFileTransfer();
      });
    });
  }

  renderFileHistory();
}

function renderFileHistory() {
  const list = $("file-history-list");
  const empty = $("file-history-empty");
  if (fileHistory.length === 0) {
    list.classList.add("hidden");
    empty.classList.remove("hidden");
    list.innerHTML = "";
    return;
  }
  empty.classList.add("hidden");
  list.classList.remove("hidden");
  list.innerHTML = fileHistory.map(fileRowHtml).join("");
  list.querySelectorAll("button[data-reveal-file]").forEach((btn) => {
    btn.addEventListener("click", () => revealFile(btn.dataset.revealFile));
  });
}

function fileRowHtml(entry) {
  const statusText = {
    sending: "发送中",
    sent: "已发送",
    received: "已接收",
    failed: "失败",
  }[entry.status] || entry.status || "";
  const reveal = entry.path && entry.status === "received"
    ? `<button data-reveal-file="${entry.id}">打开位置</button>`
    : "";
  return `
    <div class="file-row ${entry.status === "failed" ? "failed" : ""}">
      <div class="file-icon">□</div>
      <div class="file-main">
        <div class="file-name" title="${escapeHtml(entry.file_name)}">${escapeHtml(entry.file_name)}</div>
        <div class="file-meta">${escapeHtml(entry.device_name)} · ${formatSize(entry.size_bytes)} · ${statusText} · ${relativeTime(entry.ts)}</div>
        ${entry.message ? `<div class="file-error">${escapeHtml(entry.message)}</div>` : ""}
      </div>
      <div class="file-actions">${reveal}</div>
    </div>
  `;
}

function appendOrReplaceFileEntry(entry) {
  const idx = fileHistory.findIndex((e) => e.id === entry.id);
  if (idx >= 0) fileHistory.splice(idx, 1);
  fileHistory.unshift(entry);
  if (fileHistory.length > 24) fileHistory.length = 24;
  renderFileTransfer();
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
  const input = $("image-input");
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

async function pickAndSendFiles() {
  const targetDeviceIds = [...selectedFileTargets];
  if (targetDeviceIds.length === 0) {
    toast("先选择接收设备");
    return;
  }
  try {
    const entries = await invoke("cmd_pick_and_send_files", { targetDeviceIds });
    if (Array.isArray(entries)) {
      entries.forEach(appendOrReplaceFileEntry);
      if (entries.length > 0) toast(`已处理 ${entries.length} 个文件任务`);
    }
  } catch (e) {
    toast(`发送失败:${e}`);
  }
}

async function revealFile(id) {
  const entry = fileHistory.find((e) => e.id === id);
  if (!entry?.path) {
    toast("文件路径已过期");
    return;
  }
  try {
    await invoke("cmd_reveal_file", { path: entry.path });
  } catch (e) {
    toast(`打开失败:${e}`);
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

  // LAN peer names: poll every 2s. Cheap (atomic + small HashMap snapshot
  // on the Rust side) and the LAN topology doesn't change fast enough to
  // need event-driven plumbing here.
  const pollLanPeers = async () => {
    try {
      const names = await invoke("cmd_lan_peer_names");
      setLanPeerNames(names);
      const peers = await invoke("cmd_lan_file_peers");
      lanFilePeers = Array.isArray(peers) ? peers : [];
      if (lastLanPeerNames.length === 0 && lanFilePeers.length > 0) {
        setLanPeerNames(lanFilePeers.map((p) => p.display_name));
      }
      renderFileTransfer();
    } catch (e) {
      // Bridge not started yet — keep the previous value.
    }
  };
  await pollLanPeers();
  setInterval(pollLanPeers, 2000);

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

  await listen("file-event", (evt) => appendOrReplaceFileEntry(evt.payload));
  try {
    const receiveDir = await invoke("cmd_file_receive_dir");
    if (receiveDir) $("file-receive-dir").textContent = receiveDir;
  } catch (e) {
    console.warn("file receive dir:", e);
  }
  try {
    const recentFiles = await invoke("cmd_recent_file_transfers");
    if (Array.isArray(recentFiles)) {
      fileHistory.splice(0, fileHistory.length, ...recentFiles);
      renderFileTransfer();
    }
  } catch (e) {
    console.warn("recent files:", e);
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

  // Drop zone — clickable for the picker, plus HTML5 drag-drop targets.
  // Tauri 2 normally intercepts drag-drop at the OS layer; we set
  // dragDropEnabled=false in tauri.conf.json so the standard browser
  // events fire here with File objects on `dataTransfer.files`.
  const dropZone = $("drop-zone");
  dropZone.addEventListener("click", () => pickAndSendImages());
  $("btn-pick-files").addEventListener("click", () => pickAndSendFiles());
  $("btn-file-select-all").addEventListener("click", () => {
    if (selectedFileTargets.size === lanFilePeers.length && lanFilePeers.length > 0) {
      selectedFileTargets.clear();
    } else {
      selectedFileTargets.clear();
      lanFilePeers.forEach((peer) => selectedFileTargets.add(peer.device_id));
    }
    renderFileTransfer();
  });

  // dragenter / dragover both must call preventDefault, otherwise drop
  // never fires (browser defaults to "no drop allowed"). dragleave needs
  // to be careful because moving over child elements re-fires it; we
  // count enter/leave with a depth counter to only un-highlight on the
  // final leave.
  let dragDepth = 0;
  ["dragenter", "dragover"].forEach((evt) => {
    dropZone.addEventListener(evt, (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
      dragDepth++;
      dropZone.classList.add("dragging");
    });
  });
  dropZone.addEventListener("dragleave", (e) => {
    e.preventDefault();
    e.stopPropagation();
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dropZone.classList.remove("dragging");
  });
  dropZone.addEventListener("drop", (e) => {
    e.preventDefault();
    e.stopPropagation();
    dragDepth = 0;
    dropZone.classList.remove("dragging");
    const files = Array.from(e.dataTransfer?.files || []).filter((f) =>
      (f.type || "").startsWith("image/"),
    );
    if (files.length === 0) {
      toast("拖入的不是图片");
      return;
    }
    handlePickedFiles(files);
  });

  // Block drops on the rest of the document so browser doesn't navigate
  // to a dropped file's URI when the user misses the zone.
  ["dragover", "drop"].forEach((evt) => {
    window.addEventListener(evt, (e) => {
      // Only block when target isn't our intended drop zone or its
      // children — preserves textarea drag-paste behavior in 高级选项.
      if (!dropZone.contains(e.target)) e.preventDefault();
    });
  });

  $("image-input").addEventListener("change", (e) => {
    handlePickedFiles(Array.from(e.target.files || []));
  });

  renderFileTransfer();
}

init();
