"use strict";

// Renders the state from the Rust side (get_state) and refetches it on every
// `changed` event. Each region is only rewritten when its HTML changes, so
// buttons keep hover and focus, and the QR code doesn't flicker.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const icons = {
  phone: '<svg viewBox="0 0 24 24"><rect x="6" y="2.5" width="12" height="19" rx="2.5"/><path d="M11 18h2"/></svg>',
  wifi: '<svg viewBox="0 0 24 24"><path d="M2 8.8a15 15 0 0 1 20 0"/><path d="M5 12.6a10 10 0 0 1 14 0"/><path d="M8.5 16.4a5 5 0 0 1 7 0"/><path d="M12 20h.01"/></svg>',
  wifiOff: '<svg viewBox="0 0 24 24"><path d="M2 2l20 20"/><path d="M8.5 16.4a5 5 0 0 1 7 0"/><path d="M5 12.6a10 10 0 0 1 5.2-2.7"/><path d="M13.8 9.9A10 10 0 0 1 19 12.6"/><path d="M2 8.8a15 15 0 0 1 4.2-2.9"/><path d="M10.7 5.1A15 15 0 0 1 22 8.8"/><path d="M12 20h.01"/></svg>',
  check: '<svg viewBox="0 0 24 24"><path d="M20 6 9 17l-5-5"/></svg>',
  alert: '<svg viewBox="0 0 24 24"><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4"/><path d="M12 17h.01"/></svg>',
  info: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/></svg>',
  x: '<svg viewBox="0 0 24 24"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>',
  clock: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></svg>',
  qr: '<svg viewBox="0 0 24 24"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><path d="M14 14h3v3h-3zM20 14v.01M14 20h.01M17 20h4v-3"/></svg>',
  plug: '<svg viewBox="0 0 24 24"><path d="M9 2v6M15 2v6"/><path d="M6 8h12v4a6 6 0 0 1-12 0z"/><path d="M12 18v4"/></svg>',
  folder: '<svg viewBox="0 0 24 24"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>',
  restart: '<svg viewBox="0 0 24 24"><path d="M3 12a9 9 0 1 0 2.6-6.4L3 8"/><path d="M3 3v5h5"/></svg>',
};

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

/** Sets an element's HTML only if it changed. */
function put(el, html) {
  if (el.__html !== html) {
    el.__html = html;
    el.innerHTML = html;
  }
}

// ---------- local UI state ----------

let state = null;
let editingAdb = false;
let timeoutSecs = 120;
let deadline = null; // performance.now() when the QR code expires
let deadlineTotal = 0;
let logOpen = false;
let logSeen = 0;
let logLines = [];
let lastQr = ""; // shown faded once the QR code expires

/** The timeout hint ("...Check that:\n- a\n- b") as a bulleted callout. */
function checklist(text) {
  const items = text
    .split("\n")
    .filter((l) => l.startsWith("- "))
    .map((l) => {
      const help = firewallHelpAvailable() && l.includes("firewall")
        ? ` <button class="inline-link" data-action="firewall-help">How?</button>`
        : "";
      return `<li>${esc(l.slice(2))}${help}</li>`;
    });
  if (!items.length) return `<div class="callout info">${icons.info}<span>${esc(text)}</span></div>`;
  return `<div class="callout info">${icons.info}<div><b>If you scanned it, check that:</b><ul>${items.join("")}</ul></div></div>`;
}
let timeoutLoaded = false;

// ---------- adb bar ----------

function renderAdb() {
  const s = state;
  const editing = editingAdb || !s.adbPath;
  if (!editing) {
    put($("adb"), `
      <div class="adb-chip" title="${esc(s.adbPath)}">
        <span class="ok">${icons.check}</span>
        <span class="label">adb</span>
        <span class="path mono">&lrm;${esc(s.adbPath)}&lrm;</span>
        <button class="btn small ghost" data-action="edit-adb">Change</button>
      </div>`);
    return;
  }
  const html = `
    <div style="display:flex;flex-direction:column;gap:4px;flex:1;max-width:680px">
      <div class="adb-edit">
        <input id="adb-input" type="text" spellcheck="false" placeholder="Path to adb or the platform-tools folder">
        <button class="btn small primary" data-action="use-adb">Use</button>
        <button class="btn small" data-action="detect-adb">Auto-detect</button>
        ${s.adbPath ? '<button class="btn small ghost" data-action="cancel-adb">Cancel</button>' : ""}
      </div>
      ${s.adbError ? `<div class="adb-error">${esc(s.adbError)}</div>` : ""}
    </div>`;
  if ($("adb").__html === html) return;
  // Keep what was typed when the error line appears or changes.
  const typed = $("adb-input")?.value;
  put($("adb"), html);
  const input = $("adb-input");
  input.value = typed ?? s.adbPath ?? "";
  input.addEventListener("keydown", (e) => e.key === "Enter" && useAdb());
}

async function useAdb(path) {
  const input = $("adb-input");
  const value = path ?? (input ? input.value : "");
  try {
    await invoke("set_adb", { path: value });
    editingAdb = false;
  } catch (_) {
    editingAdb = true; // the error is shown from the state
  }
  pull();
}

// ---------- server banner ----------

function renderBanner() {
  const sv = state.server;
  let html = "";
  if (sv.restarting) {
    html += `<div class="banner info"><span class="spinner"></span><span class="text">Restarting the adb server...</span></div>`;
  } else if (sv.hung) {
    html += `<div class="banner warn">${icons.alert}
      <span class="text">The adb server isn't responding, so nothing that uses adb works, Android Studio included.</span>
      <button class="btn small" data-action="restart-server" title="Stops the adb server, forcibly if needed, and starts a new one. Android Studio and other adb tools lose their device connections for a moment and reconnect on their own.">${icons.restart} Restart adb server</button>
    </div>`;
  }
  if (sv.notice) {
    html += `<div class="banner ${sv.notice.error ? "error" : "ok"}">${sv.notice.error ? icons.alert : icons.check}
      <span class="text">${esc(sv.notice.text)}</span>
      <button class="icon-btn" data-action="dismiss" data-which="server" title="Dismiss">${icons.x}</button>
    </div>`;
  }
  put($("banner"), html);
}

// ---------- pairing ----------

const steps = `
  <ol class="steps">
    <li><span>On the phone, open <b>Developer options</b> &rsaquo; <b>Wireless debugging</b>.</span></li>
    <li><span>Tap <b>Pair device with QR code</b>.</span></li>
    <li><span>Scan this code. The phone and this PC must be on the same Wi-Fi.</span></li>
  </ol>`;

function timeoutField() {
  return `<label class="field" title="How long each QR code stays valid">Scan timeout
    <input id="timeout" type="number" min="10" max="900" step="10"> s</label>`;
}

function renderPairing() {
  const p = state.pairing;
  const attempt = $("attempt");
  if (p && p.attempt && !p.outcome) {
    attempt.hidden = false;
    attempt.textContent = `Attempt ${p.attempt} of ${p.maxAttempts}`;
  } else {
    attempt.hidden = true;
  }

  if (p && p.expiresInMs != null && p.qrSvg) {
    const next = performance.now() + p.expiresInMs;
    // Ignore jitter from transport delay so the bar doesn't jump.
    if (deadline == null || Math.abs(next - deadline) > 1500) deadline = next;
    deadlineTotal = state.timeoutSecs * 1000;
  } else {
    deadline = null;
  }

  let box, side;
  if (!p) {
    box = `<div class="qr-box placeholder"><div class="qr-overlay"><div class="big-icon idle">${icons.qr}</div></div></div>`;
    side = `${steps}
      <div class="actions"><button class="btn primary big" data-action="pair" ${state.adbPath ? "" : "disabled"}>Show pairing QR code</button></div>
      <div class="actions">${timeoutField()}</div>`;
  } else if (p.outcome && p.outcome.ok) {
    box = `<div class="qr-box placeholder"><div class="qr-overlay"><div class="big-icon ok">${icons.check}</div></div></div>`;
    side = `<div><p class="headline ok">Connected</p><p class="sub">${esc(p.outcome.text)}</p></div>
      <p class="sub">The phone stays paired with this PC. Next time, use <b>Reconnect</b> under <b>Available on Wi-Fi</b>; no QR code needed.</p>
      <div class="actions"><button class="btn primary" data-action="pair">Pair another phone</button></div>`;
  } else if (p.outcome && p.expired) {
    box = `<div class="qr-box expired">${lastQr}<div class="qr-overlay"><div class="big-icon muted">${icons.clock}</div><b>QR code expired</b></div></div>`;
    side = `<p class="headline">The QR code expired</p>
      <div class="actions"><button class="btn primary big" data-action="pair">Show new QR code</button></div>
      <div class="actions">${timeoutField()}</div>
      ${checklist(p.outcome.text)}`;
  } else if (p.outcome) {
    box = `<div class="qr-box placeholder"><div class="qr-overlay"><div class="big-icon error">${icons.alert}</div></div></div>`;
    side = `<div><p class="headline">Couldn't connect</p></div>
      <div class="callout error">${icons.alert}<span>${esc(p.outcome.text)}</span></div>
      <div class="actions">${p.help === "restartServer" ? restartButton : ""}<button class="btn primary" data-action="pair">Try again</button><button class="btn" data-action="cancel">Close</button></div>`;
  } else {
    if (p.qrSvg) {
      box = `<div class="qr-box">${p.qrSvg}</div>`;
    } else {
      box = `<div class="qr-box placeholder"><div class="qr-overlay"><span class="spinner large"></span></div></div>`;
    }
    const timer = p.qrSvg
      ? `<div class="timer">
           <div class="timer-row"><span>${p.expiresInMs == null ? "Starting adb..." : "Expires in"}</span><b id="countdown"></b></div>
           <div class="bar ${p.expiresInMs == null ? "indeterminate" : ""}"><i id="countdown-bar"></i></div>
         </div>`
      : "";
    side = `${p.qrSvg ? steps : ""}
      ${timer}
      <div class="status"><span class="spinner"></span><span>${esc(p.status || "Starting...")}</span></div>
      ${warningCallout(p)}
      <div class="actions"><button class="btn" data-action="cancel">Cancel</button></div>`;
  }
  if (p && p.qrSvg) lastQr = p.qrSvg;
  put($("pair"), `${box}<div class="pair-side">${side}</div>`);
  // Filled in here rather than in the HTML, so editing it doesn't re-render the panel.
  const timeout = $("timeout");
  if (timeout && timeout.value === "" && document.activeElement !== timeout) timeout.value = timeoutSecs;
  tick();
}

const restartButton = `<button class="btn small" data-action="restart-server" title="Stops the adb server, forcibly if needed, and starts a new one. Android Studio and other adb tools lose their device connections for a moment and reconnect on their own.">${icons.restart} Restart adb server</button>`;

/** The pairing warning, with the help it comes with: firewall steps, or restarting adb. */
function warningCallout(p) {
  if (!p.warning) return "";
  if (p.help === "restartServer") {
    return `<div class="callout warn">${icons.alert}<div><span>${esc(p.warning)}</span>
      <div class="actions">${restartButton}</div></div></div>`;
  }
  if (p.help === "firewall" && firewallHelpAvailable()) {
    return `<button class="callout warn clickable" data-action="firewall-help" title="Show how to let adb through Windows Defender Firewall">
      ${icons.alert}<span>${esc(p.warning)}<span class="more">How to allow adb ›</span></span></button>`;
  }
  return `<div class="callout warn">${icons.alert}<span>${esc(p.warning)}</span></div>`;
}

// ---------- firewall help ----------

let fw = { open: false, busy: false, result: null, networks: null, networksError: null, manual: false };

function firewallHelpAvailable() {
  return state && state.os === "windows";
}

async function openFirewallHelp() {
  const dialog = $("fw-dialog");
  fw = { open: true, busy: false, result: null, networks: null, networksError: null, manual: false };
  renderFirewall();
  if (!dialog.open) dialog.showModal();
  loadNetworks();
}

async function loadNetworks() {
  try {
    fw.networks = await invoke("firewall_networks");
    fw.networksError = null;
  } catch (err) {
    fw.networks = [];
    fw.networksError = String(err);
  }
  renderFirewall();
}

function networkStatus() {
  if (fw.networks == null) {
    return `<div class="fw-net muted"><span class="spinner"></span><span>Checking your network...</span></div>`;
  }
  if (fw.networksError || !fw.networks.length) return "";
  const publicNets = fw.networks.filter((n) => n.category === "Public");
  if (!publicNets.length) {
    const names = fw.networks.map((n) => `<b>${esc(n.name)}</b>`).join(", ");
    return `<div class="fw-net ok">${icons.check}<span>${names} ${fw.networks.length > 1 ? "are" : "is"} a private network. Good.</span></div>`;
  }
  const names = publicNets.map((n) => `<b>${esc(n.name)}</b>`).join(", ");
  return `<div class="callout warn">${icons.alert}<div>
      ${names} ${publicNets.length > 1 ? "are" : "is"} set to <b>Public</b>. Windows blocks more on public networks, and the rule below
      only covers private ones. If this is your home or office Wi-Fi, set it to <b>Private</b>: open its properties and pick
      <i>Private network</i>.
      <div class="actions"><button class="btn small" data-action="open-settings" data-page="network">Open network settings</button></div>
    </div></div>`;
}

function codeBox(text) {
  return `<div class="code"><code>${esc(text)}</code><button class="btn small ghost" data-action="copy" data-copy="${esc(text)}">Copy</button></div>`;
}

function renderFirewall() {
  const dialog = $("fw-dialog");
  if (!fw.open) return;
  const adb = state && state.adbPath ? state.adbPath : "";
  const psPath = adb.replace(/'/g, "''");
  const command =
    `New-NetFirewallRule -DisplayName 'adb (adbqr)' -Direction Inbound -Program '${psPath}' -Action Allow -Profile Private,Domain`;

  let result = "";
  if (fw.result) {
    result = fw.result.ok
      ? `<div class="callout ok">${icons.check}<span>${esc(fw.result.text)}\nIf the QR code is still up, scan it again or wait a few seconds; otherwise show a new one.</span></div>`
      : `<div class="callout error">${icons.alert}<span>${esc(fw.result.text)}</span></div>`;
  }

  put(dialog, `
    <div class="modal-head">
      <h2 id="fw-title">Let adb through Windows Defender Firewall</h2>
      <button class="icon-btn" data-action="close-firewall" title="Close">${icons.x}</button>
    </div>
    <div class="modal-body">
      <p class="sub">After you scan, the phone announces itself on the network over mDNS (UDP 5353), and adb listens for it.
        If Windows Defender Firewall blocks adb, the phone never shows up. That often happens when
        Windows asked whether to allow adb and the prompt was cancelled: Windows then saves rules that block it.</p>

      ${networkStatus()}

      <section class="fw-step">
        <h3>Fix it for me</h3>
        <p class="sub">Adds an inbound rule that allows this adb on private networks, and removes any rules that block it.
          Windows asks for administrator approval.</p>
        <div class="actions">
          <button class="btn primary" data-action="fix-firewall" ${fw.busy || !adb ? "disabled" : ""}>
            ${fw.busy ? '<span class="spinner"></span> Waiting for approval...' : "Allow adb through the firewall"}
          </button>
        </div>
        ${result}
      </section>

      <details class="fw-step" id="fw-manual" ${fw.manual ? "open" : ""}>
        <summary><h3>Or do it yourself</h3></summary>
        <ol class="fw-steps">
          <li>Make sure your Wi-Fi is a <b>Private</b> network: Settings › Network &amp; internet › Wi-Fi › your network › Network profile type.</li>
          <li>Open <b>Allow an app through firewall</b>
            <button class="inline-link" data-action="open-settings" data-page="firewall">Open it</button>,
            then click <b>Change settings</b>.</li>
          <li>Look for <b>adb</b> or <b>Android Debug Bridge</b>. If an entry is unticked, tick <b>Private</b> for it.
            If there's none, click <b>Allow another app...</b> › <b>Browse...</b>, pick this file, and tick <b>Private</b>:
            ${adb ? codeBox(adb) : '<span class="sub">(adb not found yet)</span>'}</li>
          <li>Still blocked? Block rules win over allow rules. In <b>Windows Defender Firewall with Advanced Security</b> ›
            Inbound Rules, delete any <b>Block</b> rule for adb.</li>
        </ol>
        <p class="sub">Or run this in PowerShell as administrator:</p>
        ${adb ? codeBox(command) : ""}
      </details>
    </div>
    <div class="modal-foot">
      <button class="btn" data-action="close-firewall">Close</button>
    </div>`);
}

/** Updates the countdown text and bar between state updates. */
function tick() {
  const label = $("countdown");
  const bar = $("countdown-bar");
  if (!label || !bar || deadline == null) return;
  const left = Math.max(0, deadline - performance.now());
  const secs = Math.ceil(left / 1000);
  label.textContent = secs >= 60 ? `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, "0")}` : `${secs}s`;
  const frac = deadlineTotal ? Math.min(1, left / deadlineTotal) : 0;
  bar.style.transform = `scaleX(${frac})`;
  bar.parentElement.classList.toggle("low", secs <= 15);
}
setInterval(tick, 250);

// ---------- devices ----------

function renderDevices() {
  const d = { ...state.devices };
  // The list is stale while adb is missing or failing; show only the problem.
  const usable = state.adbPath && d.loaded && !d.error;
  if (!usable) {
    d.wireless = [];
    d.available = [];
    d.usb = [];
    d.known = [];
    d.otherCount = 0;
  }
  const count = $("count");
  // A phone switched from USB is on both lists; count it once.
  const connected = d.wireless.length + d.usb.filter((u) => !u.onWifi).length;
  count.hidden = !connected;
  count.textContent = connected;

  let list = "";
  if (!state.adbPath) {
    list = `<div class="empty">${icons.folder}<span>adb wasn't found. Enter its path at the top.</span></div>`;
  } else if (d.error) {
    list = `<div class="empty error">${icons.alert}<span>${esc(d.error)}</span></div>`;
  } else if (!d.loaded) {
    list = `<div class="empty"><span class="spinner"></span><span>Looking for devices...</span></div>`;
  } else if (!d.wireless.length && !d.available.length) {
    const hint = d.known.length
      ? " Phones paired before are listed below; Find reconnects one without a QR code."
      : d.usb.length
        ? " A phone on USB can be switched to Wi-Fi below."
        : "";
    list = `<div class="empty">${icons.wifiOff}<span>No devices connected over Wi-Fi.${hint}</span></div>`;
  }

  if (usable) {
    if (d.wireless.length) {
      list += `<div class="section-label">Connected over Wi-Fi</div>`;
      for (const dev of d.wireless) {
        const tip = dev.mdns
          ? "Turns off Wireless debugging on the phone. adb reconnects paired phones on its own, so this is what keeps it disconnected. Turn it back on in Developer options to reconnect."
          : "Runs adb disconnect for this address.";
        const online = dev.state === "device";
        list += `<div class="device">
          <div class="device-icon">${icons.phone}<span class="dot ${online ? "" : "warn"}"></span></div>
          <div class="device-info">
            <div class="device-name">${esc(dev.model || "Unknown device")}${online ? "" : `<span class="tag">${esc(dev.state)}</span>`}</div>
            <div class="device-serial" title="${esc(dev.serial)}">${esc(dev.serial)}</div>
          </div>
          ${dev.busy ? '<span class="spinner"></span>' : `<button class="btn small danger" data-action="disconnect" data-target="${esc(dev.serial)}" title="${esc(tip)}">Disconnect</button>`}
        </div>`;
      }
    }
    if (d.available.length) {
      list += `<div class="section-label" title="Phones on this network with Wireless debugging on that adb isn't connected to.">Available on Wi-Fi</div>`;
      for (const s of d.available) {
        list += `<div class="device">
          <div class="device-icon">${icons.phone}<span class="dot off"></span></div>
          <div class="device-info">
            <div class="device-name" title="${esc(s.name)}">${esc(phoneName(s.name))}</div>
            <div class="device-serial">${esc(s.addr)}</div>
          </div>
          ${s.busy ? '<span class="spinner"></span>' : `<button class="btn small" data-action="reconnect" data-target="${esc(s.addr)}" title="Runs adb connect for this address. Works without a QR code if the phone was paired with this PC before.">Reconnect</button>`}
        </div>`;
      }
    }
    if (d.known.length) {
      list += `<div class="section-label" title="Phones paired with this PC that adb isn't connected to and doesn't see on the network right now.">Paired before</div>`;
      for (const k of d.known) {
        const findTip =
          "Looks for the phone and connects to it without a QR code: at its last address, over mDNS, by its MAC address if its IP changed, " +
          "then by scanning its IP for Wireless debugging's new port (takes up to 15 seconds). Wireless debugging must be on.";
        const action = k.progress
          ? '<span class="spinner"></span>'
          : `<button class="btn small" data-action="find" data-target="${esc(k.serialno)}" title="${esc(findTip)}">Find</button>
             <button class="icon-btn" data-action="forget" data-target="${esc(k.serialno)}" title="Forget this phone. It stays paired; it comes back here when it connects again.">${icons.x}</button>`;
        const tag = k.paused
          ? `<span class="tag" title="You disconnected it, so it isn't reconnected automatically. Find reconnects it.">Disconnected</span>`
          : "";
        list += `<div class="device">
          <div class="device-icon">${icons.phone}<span class="dot off"></span></div>
          <div class="device-info">
            <div class="device-name" title="${esc(k.serialno)}">${esc(k.model || k.serialno)}${tag}</div>
            <div class="device-serial" title="${esc(k.progress || knownDetail(k))}">${esc(k.progress || knownDetail(k))}</div>
          </div>
          ${action}
        </div>`;
      }
    }
    if (d.usb.length) {
      list += `<div class="section-label" title="Phones connected with a USB cable. Switch to Wi-Fi connects them over Wi-Fi without a QR code, since they already trust this PC over USB.">Connected over USB</div>`;
      for (const u of d.usb) {
        const online = u.state === "device";
        let action;
        if (u.busy) {
          action = '<span class="spinner"></span>';
        } else if (u.onWifi) {
          action = `<span class="tag ok" title="This phone is also connected over Wi-Fi, listed above. You can unplug the cable.">On Wi-Fi</span>`;
        } else {
          const tip = online
            ? "Connects this phone over Wi-Fi too, without a QR code. Uses Wireless debugging on Android 11+, or adb tcpip if that doesn't work. The phone must be on the same Wi-Fi as this PC."
            : u.state === "unauthorized"
              ? "Allow USB debugging on the phone first."
              : `The phone is ${u.state}.`;
          action = `<button class="btn small" data-action="switch-wifi" data-target="${esc(u.serial)}" title="${esc(tip)}" ${online ? "" : "disabled"}>${icons.wifi} Switch to Wi-Fi</button>`;
        }
        list += `<div class="device">
          <div class="device-icon">${icons.plug}<span class="dot ${online ? "" : "warn"}"></span></div>
          <div class="device-info">
            <div class="device-name">${esc(u.model || "Unknown device")}${online ? "" : `<span class="tag">${esc(u.state)}</span>`}</div>
            <div class="device-serial" title="${esc(u.progress || u.serial)}">${esc(u.progress || u.serial)}</div>
          </div>
          ${action}
        </div>`;
      }
    }
  }

  let foot = "";
  if (d.notice) {
    foot += `<div class="toast ${d.notice.error ? "error" : "ok"}"><span class="text">${esc(d.notice.text)}</span>
      <button class="icon-btn" data-action="dismiss" data-which="devices" title="Dismiss">${icons.x}</button></div>`;
  }
  const idle = d.wireless.filter((x) => !x.busy);
  if (d.wireless.length > 1) {
    foot += `<button class="btn danger" data-action="disconnect-all" ${idle.length ? "" : "disabled"}>Disconnect all</button>`;
  }
  if (usable && (d.known.length || d.wireless.length)) {
    foot += `<label class="field check" title="Every few seconds, tries to reconnect paired phones last seen on this network that dropped off, at their last address and over mDNS. Phones you disconnect are left alone.">
      <input type="checkbox" data-change="auto-reconnect" ${d.autoReconnect ? "checked" : ""}> Reconnect paired phones automatically</label>`;
  }
  if (d.otherCount > 0) {
    foot += `<div class="note">${d.otherCount} emulator${d.otherCount === 1 ? "" : "s"} not shown.</div>`;
  }

  put($("devices"), `<div class="device-scroll">${list}</div><div class="devices-foot">${foot}</div>`);
}

/** "192.168.1.20:37891 · 3 h ago", or why there's no address. */
function knownDetail(k) {
  if (!k.lastSeen) return k.paired ? "Paired; no address yet" : k.serialno;
  const parts = [];
  if (k.addr) parts.push(k.addr);
  parts.push(k.otherNetwork ? `on another network, ${ago(k.lastSeen)}` : ago(k.lastSeen));
  return parts.join(" · ");
}

function ago(unixSecs) {
  const s = Math.max(0, Date.now() / 1000 - unixSecs);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

/** `adb-R52W70XYZ12-Qm8kLp` (mDNS instance) -> `R52W70XYZ12` (the phone's serial). */
function phoneName(instance) {
  const m = /^adb-(.+)-[^-]+$/.exec(instance);
  return m ? m[1] : instance;
}

// ---------- log ----------

async function renderLog() {
  const label = $("log-label");
  label.textContent = logOpen ? "Hide adb log" : "Show adb log";
  $("log-text").hidden = !logOpen;
  document.querySelectorAll("[data-log-only]").forEach((b) => (b.hidden = !logOpen));
  if (!logOpen || state.logTotal <= logSeen) return;
  const chunk = await invoke("get_log", { from: logSeen });
  logSeen = chunk.total;
  if (!chunk.lines.length) return;
  logLines.push(...chunk.lines);
  if (logLines.length > 1000) logLines = logLines.slice(-1000);
  const pre = $("log-text");
  const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 30;
  pre.textContent = logLines.join("\n");
  if (atBottom) pre.scrollTop = pre.scrollHeight;
}

// ---------- events ----------

function readTimeout() {
  const input = $("timeout");
  if (input) {
    const v = parseInt(input.value, 10);
    if (v >= 10 && v <= 900) timeoutSecs = v;
  }
  return timeoutSecs;
}

document.addEventListener("click", async (e) => {
  const el = e.target.closest("[data-action]");
  if (!el || el.disabled) return;
  const target = el.dataset.target;
  switch (el.dataset.action) {
    case "edit-adb":
      editingAdb = true;
      render();
      $("adb-input")?.select();
      break;
    case "cancel-adb":
      editingAdb = false;
      render();
      break;
    case "use-adb":
      useAdb();
      break;
    case "detect-adb":
      useAdb("");
      break;
    case "pair":
      await invoke("start_pairing", { timeoutSecs: readTimeout() });
      break;
    case "cancel":
      await invoke("cancel_pairing");
      break;
    case "disconnect":
      await invoke("disconnect", { serials: [target] });
      break;
    case "disconnect-all":
      await invoke("disconnect", { serials: state.devices.wireless.filter((x) => !x.busy).map((x) => x.serial) });
      break;
    case "reconnect":
      await invoke("reconnect", { addrs: [target] });
      break;
    case "switch-wifi":
      await invoke("switch_to_wifi", { serials: [target] });
      break;
    case "find":
      await invoke("find_device", { serialno: target });
      break;
    case "forget":
      await invoke("forget_device", { serialno: target });
      break;
    case "refresh":
      el.classList.remove("spin");
      void el.offsetWidth;
      el.classList.add("spin");
      await invoke("refresh");
      break;
    case "restart-server":
      await invoke("restart_server");
      break;
    case "dismiss":
      await invoke("dismiss_notice", { which: el.dataset.which });
      break;
    case "toggle-log":
      logOpen = !logOpen;
      renderLog();
      if (logOpen) {
        const pre = $("log-text");
        pre.scrollTop = pre.scrollHeight;
      }
      break;
    case "clear-log":
      await invoke("clear_log");
      logLines = [];
      $("log-text").textContent = "";
      break;
    case "firewall-help":
      openFirewallHelp();
      return;
    case "close-firewall":
      $("fw-dialog").close();
      return;
    case "open-settings":
      try {
        await invoke("open_settings", { page: el.dataset.page });
      } catch (err) {
        console.error(err);
      }
      return;
    case "fix-firewall":
      fw.busy = true;
      fw.result = null;
      renderFirewall();
      try {
        fw.result = { ok: true, text: await invoke("fix_firewall") };
      } catch (err) {
        fw.result = { ok: false, text: String(err) };
      }
      fw.busy = false;
      renderFirewall();
      loadNetworks();
      break;
    case "copy":
      try {
        await navigator.clipboard.writeText(el.dataset.copy);
        el.textContent = "Copied";
        setTimeout(() => (el.textContent = "Copy"), 1200);
      } catch (_) {}
      return;
    case "copy-log":
      try {
        await navigator.clipboard.writeText(logLines.join("\n"));
        el.textContent = "Copied";
        setTimeout(() => (el.textContent = "Copy"), 1200);
      } catch (_) {}
      break;
  }
  pull();
});

document.addEventListener("change", async (e) => {
  if (e.target.id === "timeout") readTimeout();
  if (e.target.dataset.change === "auto-reconnect") {
    await invoke("set_auto_reconnect", { on: e.target.checked });
    pull();
  }
});

$("fw-dialog").addEventListener("close", () => (fw.open = false));
// Remember whether "Or do it yourself" is expanded, so a re-render keeps it.
$("fw-dialog").addEventListener("toggle", (e) => {
  if (e.target.id === "fw-manual") fw.manual = e.target.open;
}, true);

function render() {
  if (!state) return;
  renderAdb();
  renderBanner();
  renderPairing();
  renderDevices();
  renderLog();
  renderFirewall();
}

let fetching = false;
let again = false;

/** Fetches the state and renders it; calls made meanwhile are merged into one more fetch. */
async function pull() {
  if (fetching) {
    again = true;
    return;
  }
  fetching = true;
  try {
    do {
      again = false;
      state = await invoke("get_state");
      if (!timeoutLoaded) {
        timeoutLoaded = true;
        timeoutSecs = state.timeoutSecs;
      }
      render();
    } while (again);
  } catch (err) {
    console.error(err);
  } finally {
    fetching = false;
  }
}

listen("changed", pull);
pull();
// A fallback in case an event is missed.
setInterval(pull, 2000);
