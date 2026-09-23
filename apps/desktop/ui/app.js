// UnionDesk desktop UI.
//
// The engine is the single source of truth: it pushes a complete snapshot
// whenever anything changes and this file simply redraws it. Commands are
// fire-and-forget, so a slow handshake or a large transfer never blocks the
// interface.

const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;
const listen = tauri?.event?.listen;

const el = (id) => document.getElementById(id);

const state = {
  snapshot: null,
  tab: "devices",
  settingsDraft: null,
  pendingFiles: [],
  pairingPeer: null,
  dropCount: 0,
};

// ------------------------------------------------------------------ helpers

function bytes(value) {
  if (!value) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let index = 0;
  let size = value;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }
  return `${size < 10 && index > 0 ? size.toFixed(1) : Math.round(size)} ${units[index]}`;
}

function relativeTime(unixSeconds) {
  if (!unixSeconds) return "never";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function osLabel(os) {
  switch (os) {
    case "windows":
      return "Windows";
    case "macos":
      return "macOS";
    case "linux":
      return "Linux";
    default:
      return "Unknown";
  }
}

function osGlyph(os) {
  switch (os) {
    case "macos":
      return "🍎";
    case "windows":
      return "🪟";
    default:
      return "💻";
  }
}

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function field(label, value, className) {
  const wrapper = node("div", "field");
  wrapper.append(node("span", "label", label));
  wrapper.append(node("span", className ? `value ${className}` : "value", value));
  return wrapper;
}

async function call(command, args) {
  if (!invoke) return;
  try {
    return await invoke(command, args);
  } catch (error) {
    toast(String(error), "error");
    return undefined;
  }
}

function toast(message, level = "info") {
  const item = node("div", `toast toast-${level}`, message);
  el("toasts").append(item);
  setTimeout(() => {
    item.style.opacity = "0";
    item.style.transition = "opacity 0.25s ease";
    setTimeout(() => item.remove(), 260);
  }, 4200);
}

// ------------------------------------------------------------------- render

function render() {
  const snapshot = state.snapshot;
  if (!snapshot) return;

  el("device-summary").textContent =
    `${snapshot.device.name} · ${osLabel(snapshot.device.os)} · port ` +
    `${snapshot.listening_port} · ${snapshot.platform.displays.length} display(s)`;

  renderControls(snapshot);
  renderSelf(snapshot);
  renderDevices(snapshot);
  renderTransfers(snapshot);
  renderSettings(snapshot);
  renderPairing(snapshot);
  renderPermission(snapshot);
}

function renderControls(snapshot) {
  const badge = el("control-badge");
  const mode = snapshot.control.mode;
  badge.classList.remove("badge-idle", "badge-controlling");
  if (mode.mode === "controlling") {
    badge.textContent = `Controlling ${mode.name}`;
    badge.classList.add("badge-controlling");
  } else if (mode.mode === "controlled") {
    badge.textContent = `${mode.name} is controlling this machine`;
    badge.classList.add("badge-controlling");
  } else {
    badge.textContent = "Local input";
    badge.classList.add("badge-idle");
  }

  el("release-control").classList.toggle(
    "hidden",
    mode.mode === "local",
  );

  const toggle = el("sharing-toggle");
  const enabled = snapshot.control.sharing_enabled;
  toggle.textContent = enabled ? "Sharing on" : "Sharing off";
  toggle.classList.toggle("on", enabled);
  toggle.setAttribute("aria-pressed", String(enabled));
  toggle.disabled = !snapshot.input.backend_available;
  toggle.title = enabled
    ? `Move the cursor to a screen edge to control another machine. Panic shortcut: ${snapshot.control.release_hotkey}.`
    : "Turn on to let this machine drive, or be driven by, another one.";
}

function renderSelf(snapshot) {
  const card = el("self-card");
  card.replaceChildren();
  card.append(field("Name", snapshot.device.name));
  card.append(field("System", osLabel(snapshot.device.os)));
  card.append(field("Version", snapshot.device.version));
  card.append(
    field("Identity", snapshot.device.fingerprint, "fingerprint"),
  );
  card.append(field("Desktop", `${snapshot.platform.desktop.width} × ${snapshot.platform.desktop.height}`));
  card.append(field("Clipboard", snapshot.clipboard.enabled ? "sharing" : "off"));
}

function renderDevices(snapshot) {
  const list = el("device-list");
  list.replaceChildren();
  el("device-empty").classList.toggle("hidden", snapshot.peers.length > 0);

  for (const peer of snapshot.peers) {
    const card = node("div", "device");
    if (peer.connection.state === "connected") card.classList.add("connected");

    const head = node("div", "device-head");
    head.append(node("div", "device-icon", osGlyph(peer.os)));
    const title = node("div", "device-title");
    title.append(node("strong", null, peer.name));
    title.append(
      node(
        "span",
        null,
        [osLabel(peer.os), peer.trusted ? "paired" : "not paired", peer.discovered_via]
          .filter(Boolean)
          .join(" · "),
      ),
    );
    head.append(title);
    head.append(connectionBadge(peer.connection));
    card.append(head);

    const meta = node("div", "device-meta");
    if (peer.address) meta.append(node("span", null, `Address ${peer.address}`));
    if (peer.display_summary) meta.append(node("span", null, peer.display_summary));
    meta.append(node("span", null, `Seen ${relativeTime(peer.last_seen)}`));
    meta.append(node("span", "fingerprint", peer.fingerprint));
    card.append(meta);

    card.append(deviceActions(peer));
    list.append(card);
  }
}

function connectionBadge(connection) {
  const badge = node("span", "badge", "");
  switch (connection.state) {
    case "connected":
      badge.textContent = "Connected";
      badge.classList.add("badge-connected");
      break;
    case "pairing":
      badge.textContent = "Pairing";
      badge.classList.add("badge-pairing");
      break;
    case "connecting":
      badge.textContent = "Connecting";
      break;
    case "failed":
      badge.textContent = "Failed";
      badge.classList.add("badge-failed");
      badge.title = connection.reason ?? "";
      break;
    default:
      badge.textContent = "Offline";
      break;
  }
  return badge;
}

function deviceActions(peer) {
  const actions = node("div", "device-actions");
  const connected = peer.connection.state === "connected";
  const pending = peer.connection.state === "pairing";

  if (connected) {
    actions.append(
      button("Send files", "button-quiet", async () => {
        const paths = await call("pick_files", { multiple: true });
        if (paths && paths.length) {
          await call("send_files", { deviceId: peer.device_id, paths });
        }
      }),
    );
    actions.append(button("Disconnect", "button-quiet", () => call("disconnect", { deviceId: peer.device_id })));
    actions.append(edgePicker(peer));
  } else if (pending) {
    actions.append(
      button("Pair now", "button-quiet", () => {
        state.pairingPeer = peer.device_id;
        renderPairing(state.snapshot, true);
      }),
    );
    actions.append(button("Cancel", "button-quiet", () => call("disconnect", { deviceId: peer.device_id })));
  } else {
    const connectButton = button("Connect", "button-primary", () =>
      call("connect", { deviceId: peer.device_id }),
    );
    connectButton.disabled = !peer.address;
    actions.append(connectButton);
    if (peer.trusted) {
      actions.append(button("Forget", "button-quiet", () => call("forget", { deviceId: peer.device_id })));
    }
  }
  return actions;
}

function edgePicker(peer) {
  const group = node("div", "edge-group");
  group.append(node("span", "edge-label", "Screen edge"));
  const current = peer.link?.local_side ?? null;
  for (const side of ["left", "right", "top", "bottom"]) {
    const target = button(
      side[0].toUpperCase() + side.slice(1),
      current === side ? "active" : "",
      () => call("set_link", { deviceId: peer.device_id, name: peer.name, side }),
    );
    group.append(target);
  }
  return group;
}

function button(label, className, handler) {
  const element = node("button", `button ${className}`.trim(), label);
  element.type = "button";
  element.addEventListener("click", handler);
  return element;
}

function renderTransfers(snapshot) {
  const list = el("transfer-list");
  list.replaceChildren();
  const active = snapshot.transfers.filter(
    (transfer) => transfer.state === "active" || transfer.state === "pending",
  ).length;
  const counter = el("transfer-count");
  counter.textContent = String(active);
  counter.classList.toggle("hidden", active === 0);
  el("transfer-empty").classList.toggle("hidden", snapshot.transfers.length > 0);

  for (const transfer of snapshot.transfers) {
    const card = node("div", "transfer");
    const head = node("div", "transfer-head");
    const sending = transfer.direction === "sending";
    head.append(
      node(
        "strong",
        null,
        `${sending ? "To" : "From"} ${transfer.peer_name} · ${transfer.files.length} item(s)`,
      ),
    );
    head.append(node("span", "muted", stateLabel(transfer.state)));
    head.append(
      node(
        "span",
        "muted",
        `${bytes(transfer.done_bytes)} / ${bytes(transfer.total_bytes)}` +
          (transfer.bytes_per_second ? ` · ${bytes(transfer.bytes_per_second)}/s` : ""),
      ),
    );
    card.append(head);

    const progress = node("div", "progress");
    if (transfer.state === "completed") progress.classList.add("done");
    if (transfer.state === "failed") progress.classList.add("failed");
    const bar = node("span");
    bar.style.width = `${Math.round(progressFraction(transfer) * 100)}%`;
    progress.append(bar);
    card.append(progress);

    const files = node("div", "transfer-files");
    for (const file of transfer.files.slice(0, 40)) {
      files.append(node("div", null, file.is_dir ? `${file.relative_path}/` : file.relative_path));
    }
    if (transfer.files.length > 40) {
      files.append(node("div", null, `… and ${transfer.files.length - 40} more`));
    }
    card.append(files);

    if (transfer.destination) {
      card.append(node("div", "muted", `Saved to ${transfer.destination}`));
    }
    if (transfer.error) {
      card.append(node("div", "muted", transfer.error));
    }

    const actions = node("div", "transfer-actions");
    if (transfer.state === "pending" && transfer.direction === "receiving") {
      actions.append(
        button("Accept", "button-primary", () => call("accept_transfer", { id: transfer.id })),
      );
    }
    if (transfer.state === "active" || transfer.state === "pending") {
      actions.append(button("Cancel", "button-quiet", () => call("cancel_transfer", { id: transfer.id })));
    }
    if (actions.childElementCount) card.append(actions);
    list.append(card);
  }
}

function progressFraction(transfer) {
  if (!transfer.total_bytes) {
    return transfer.state === "completed" ? 1 : 0;
  }
  return Math.min(1, transfer.done_bytes / transfer.total_bytes);
}

function stateLabel(value) {
  switch (value) {
    case "pending":
      return "Waiting";
    case "active":
      return "In progress";
    case "completed":
      return "Completed";
    case "cancelled":
      return "Cancelled";
    case "failed":
      return "Failed";
    default:
      return value;
  }
}

// ----------------------------------------------------------------- settings

function renderSettings(snapshot) {
  if (!state.settingsDraft) {
    state.settingsDraft = structuredClone(snapshot.settings);
  }
  const draft = state.settingsDraft;
  const form = el("settings-form");
  if (form.dataset.rendered === "yes") return;
  form.dataset.rendered = "yes";
  form.replaceChildren();

  const device = group("This machine");
  device.append(text("Device name", draft.device_name, (value) => (draft.device_name = value)));
  device.append(
    number("Listen port", draft.port, 1, 65535, (value) => (draft.port = value)),
  );
  device.append(toggle("Visible to other machines", draft.discoverable, (value) => (draft.discoverable = value)));
  form.append(device);

  const input = group("Keyboard & mouse");
  input.append(
    toggle("Share keyboard and mouse", draft.input.enabled, (value) => (draft.input.enabled = value)),
  );
  input.append(
    toggle("Also send keystrokes", draft.input.relay_keyboard, (value) => (draft.input.relay_keyboard = value)),
  );
  input.append(
    range("Pointer speed", draft.input.mouse_speed, 0.25, 3, 0.05, (value) => (draft.input.mouse_speed = value)),
  );
  input.append(
    number(
      "Edge trigger distance (px)",
      draft.input.edge_armed_pixels,
      1,
      60,
      (value) => (draft.input.edge_armed_pixels = value),
    ),
  );
  input.append(
    select(
      "Release shortcut",
      draft.input.release_hotkey,
      [
        ["scroll_lock_twice", "Scroll Lock twice"],
        ["ctrl_alt_escape", "Ctrl + Alt + Esc"],
        ["ctrl_alt_cmd_escape", "Ctrl + Alt + Cmd + Esc"],
        ["disabled", "Disabled"],
      ],
      (value) => (draft.input.release_hotkey = value),
    ),
  );
  form.append(input);

  const clipboard = group("Clipboard");
  clipboard.append(
    toggle("Sync the clipboard", draft.clipboard.enabled, (value) => (draft.clipboard.enabled = value)),
  );
  clipboard.append(toggle("Text", draft.clipboard.sync_text, (value) => (draft.clipboard.sync_text = value)));
  clipboard.append(
    toggle("Images", draft.clipboard.sync_images, (value) => (draft.clipboard.sync_images = value)),
  );
  clipboard.append(
    toggle("Sync even when idle", draft.clipboard.always, (value) => (draft.clipboard.always = value)),
  );
  clipboard.append(
    number("Poll interval (ms)", draft.clipboard.poll_interval_ms, 100, 5000, (value) => (draft.clipboard.poll_interval_ms = value)),
  );
  form.append(clipboard);

  const transfer = group("File transfer");
  transfer.append(
    toggle("Accept file transfers", draft.transfer.enabled, (value) => (draft.transfer.enabled = value)),
  );
  transfer.append(
    toggle("Auto accept from paired machines", draft.transfer.auto_accept_trusted, (value) => (draft.transfer.auto_accept_trusted = value)),
  );
  transfer.append(
    toggle("Overwrite existing files", draft.transfer.overwrite_existing, (value) => (draft.transfer.overwrite_existing = value)),
  );
  transfer.append(
    folderRow("Save received files to", draft.transfer.download_dir, async () => {
      const chosen = await call("pick_download_dir");
      if (chosen) {
        draft.transfer.download_dir = chosen;
        form.dataset.rendered = "no";
        renderSettings(state.snapshot);
      }
    }),
  );
  form.append(transfer);
}

function group(title) {
  const wrapper = node("div", "settings-group");
  wrapper.append(node("h3", null, title));
  return wrapper;
}

function row(label, description, control) {
  const wrapper = node("div", "row");
  const holder = node("label");
  holder.append(node("span", null, label));
  if (description) holder.append(node("span", null, description));
  wrapper.append(holder);
  wrapper.append(control);
  return wrapper;
}

function text(label, value, onChange) {
  const input = node("input", "input");
  input.type = "text";
  input.value = value ?? "";
  input.addEventListener("change", () => onChange(input.value.trim()));
  return row(label, null, input);
}

function number(label, value, min, max, onChange) {
  const input = node("input", "input");
  input.type = "number";
  input.min = String(min);
  input.max = String(max);
  input.value = String(value ?? "");
  input.addEventListener("change", () => {
    const parsed = Number(input.value);
    if (Number.isFinite(parsed)) onChange(Math.min(max, Math.max(min, parsed)));
  });
  return row(label, null, input);
}

function range(label, value, min, max, step, onChange) {
  const input = node("input");
  input.type = "range";
  input.min = String(min);
  input.max = String(max);
  input.step = String(step);
  input.value = String(value ?? 1);
  const readout = node("span", "muted", `${Number(input.value).toFixed(2)}×`);
  input.addEventListener("input", () => {
    readout.textContent = `${Number(input.value).toFixed(2)}×`;
    onChange(Number(input.value));
  });
  const holder = node("div", "row");
  const label_ = node("label");
  label_.append(node("span", null, label));
  label_.append(readout);
  holder.append(label_, input);
  return holder;
}

function toggle(label, value, onChange) {
  const input = node("input");
  input.type = "checkbox";
  input.checked = Boolean(value);
  input.addEventListener("change", () => onChange(input.checked));
  return row(label, null, input);
}

function select(label, value, options, onChange) {
  const element = node("select", "select");
  for (const [key, text] of options) {
    const option = node("option", null, text);
    option.value = key;
    element.append(option);
  }
  element.value = value;
  element.addEventListener("change", () => onChange(element.value));
  return row(label, null, element);
}

function folderRow(label, value, onBrowse) {
  const holder = node("div");
  holder.append(node("div", "muted", value ?? ""));
  holder.append(button("Change…", "button", onBrowse));
  const wrapper = node("div", "row");
  const label_ = node("label");
  label_.append(node("span", null, label));
  wrapper.append(label_, holder);
  return wrapper;
}

// ------------------------------------------------------------------ pairing

function renderPairing(snapshot, force) {
  const modal = el("pairing-modal");
  const candidate = snapshot.peers.find((peer) => peer.pairing);
  if (!candidate) {
    if (!force) {
      modal.classList.add("hidden");
      state.pairingPeer = null;
    }
    return;
  }
  state.pairingPeer = candidate.device_id;
  const pairing = candidate.pairing;

  modal.classList.remove("hidden");
  el("pairing-code-block").classList.toggle("hidden", !pairing.code_to_share);
  el("pairing-input").classList.toggle("hidden", !pairing.awaiting_code);
  el("pairing-code").textContent = pairing.code_to_share
    ? pairing.code_to_share.replace(/(\d{3})(\d{3})/, "$1 $2")
    : "";
  el("pairing-fingerprint").textContent = `Key ${pairing.remote_fingerprint}`;
  el("pairing-title").textContent = `Pair with ${candidate.name}`;

  if (pairing.code_to_share) {
    el("pairing-text").textContent =
      "Type this code on the other machine. If the two codes do not match, the machines are not talking directly to each other.";
    el("pairing-accept").textContent = "Waiting…";
    el("pairing-accept").disabled = true;
  } else if (pairing.awaiting_code) {
    el("pairing-text").textContent =
      "Enter the six digit code shown on the other machine. Matching codes prove the two machines are talking to each other and not to something in the middle.";
    el("pairing-accept").textContent = "Confirm";
    el("pairing-accept").disabled = false;
    el("pairing-input").focus();
  }
}

function renderPermission(snapshot) {
  const callout = el("permission-callout");
  const hint = snapshot.input.permission_hint;
  callout.classList.toggle("visible", Boolean(hint));
  if (!hint) {
    callout.replaceChildren();
    return;
  }
  if (callout.dataset.hint === hint) return;
  callout.dataset.hint = hint;
  callout.replaceChildren();
  callout.append(node("span", "callout-text", hint));
  callout.append(
    button("Open System Settings", "button button-quiet", () =>
      call("open_permission_settings"),
    ),
  );
}

// --------------------------------------------------------------- wiring up

function bindTabs() {
  for (const tab of document.querySelectorAll(".tab")) {
    tab.addEventListener("click", () => {
      state.tab = tab.dataset.tab;
      for (const other of document.querySelectorAll(".tab")) {
        other.classList.toggle("active", other === tab);
      }
      for (const panel of document.querySelectorAll(".panel")) {
        panel.classList.toggle("active", panel.id === `panel-${state.tab}`);
      }
    });
  }
}

function bindActions() {
  el("sharing-toggle").addEventListener("click", () => {
    const enabled = state.snapshot?.control.sharing_enabled ?? false;
    call("set_sharing", { enabled: !enabled });
  });

  el("release-control").addEventListener("click", () => call("release_control"));
  el("rescan").addEventListener("click", () => call("refresh"));
  el("open-downloads").addEventListener("click", () => call("open_download_dir"));
  el("clear-transfers").addEventListener("click", () => call("clear_finished_transfers"));

  el("save-settings").addEventListener("click", async () => {
    if (!state.settingsDraft) return;
    await call("save_settings", { settings: state.settingsDraft });
    el("settings-status").textContent = "Saved.";
    setTimeout(() => (el("settings-status").textContent = ""), 2000);
  });

  el("add-by-address").addEventListener("click", () => {
    el("address-modal").classList.remove("hidden");
    el("address-input").focus();
  });
  el("address-cancel").addEventListener("click", () =>
    el("address-modal").classList.add("hidden"),
  );
  el("address-connect").addEventListener("click", async () => {
    const address = el("address-input").value.trim();
    if (!address) return;
    await call("connect_address", { address, name: null });
    el("address-modal").classList.add("hidden");
    el("address-input").value = "";
  });

  el("pairing-cancel").addEventListener("click", () => {
    if (!state.pairingPeer) return;
    call("answer_pairing", {
      deviceId: state.pairingPeer,
      code: null,
      accept: false,
    });
    el("pairing-modal").classList.add("hidden");
  });
  el("pairing-accept").addEventListener("click", () => {
    if (!state.pairingPeer) return;
    const code = el("pairing-input").value.trim();
    if (!/^\d{6}$/.test(code.replace(/\s+/g, ""))) {
      toast("Enter the six digit code shown on the other machine.", "warning");
      return;
    }
    call("answer_pairing", {
      deviceId: state.pairingPeer,
      code,
      accept: true,
    });
  });

  el("sender-cancel").addEventListener("click", () => {
    state.pendingFiles = [];
    el("sender-modal").classList.add("hidden");
  });
}

async function collectDroppedFiles() {
  const paths = await call("take_pending_drop");
  if (!paths || !paths.length) return;
  const connected = (state.snapshot?.peers ?? []).filter(
    (peer) => peer.connection.state === "connected",
  );
  if (!connected.length) {
    toast("Pair and connect a machine before sending files.", "warning");
    return;
  }
  if (connected.length === 1) {
    await call("send_files", { deviceId: connected[0].device_id, paths });
    return;
  }
  state.pendingFiles = paths;
  el("sender-summary").textContent = `${paths.length} item(s) ready to send.`;
  const targets = el("sender-targets");
  targets.replaceChildren();
  for (const peer of connected) {
    targets.append(
      button(`${peer.name} · ${osLabel(peer.os)}`, "button", async () => {
        await call("send_files", { deviceId: peer.device_id, paths: state.pendingFiles });
        state.pendingFiles = [];
        el("sender-modal").classList.add("hidden");
      }),
    );
  }
  el("sender-modal").classList.remove("hidden");
}

async function main() {
  bindTabs();
  bindActions();

  const notes = await call("platform_notes");
  const list = el("platform-notes");
  list.replaceChildren();
  for (const note of notes ?? []) {
    list.append(node("li", null, note));
  }

  const snapshot = await call("get_snapshot");
  if (snapshot) {
    state.snapshot = snapshot;
    render();
  }

  if (!listen) {
    toast("This page is open outside UnionDesk; live updates are unavailable.", "warning");
    return;
  }

  await listen("uniondesk://snapshot", (event) => {
    state.snapshot = event.payload;
    // Rebuild the settings form whenever the device list changes shape; the
    // draft is kept so unsaved edits survive unrelated updates.
    render();
  });

  await listen("uniondesk://notice", (event) => {
    const notice = event.payload;
    const level = notice.level === "error" ? "error" : notice.level === "warning" ? "warning" : "info";
    toast(notice.message, level);
  });

  await listen("uniondesk://dropped", () => {
    el("drop-overlay").classList.remove("hidden");
    setTimeout(() => el("drop-overlay").classList.add("hidden"), 900);
    setTimeout(collectDroppedFiles, 120);
  });

  call("refresh");
}

main();
