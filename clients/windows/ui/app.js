// Tauri 2 globals: __TAURI__.core.invoke, __TAURI__.event.listen
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const qrHost = $("qr-host");
const qrEmpty = $("qr-empty");
const configText = $("config-text");
const errorBox = $("error");

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

async function init() {
  // Live state updates from the bridge.
  await listen("connection-state", (evt) => setStatus(evt.payload));
  setStatus(await invoke("cmd_current_state"));

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
}

init();
