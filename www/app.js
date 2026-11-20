// Licensed under the Apache-2.0 license
//
// The page: pick a scenario, boot its images in the emulator, and show the
// host driver's steps and the UART output as it runs.
//
// build.sh writes what this loads: scenarios.json and images/ (the firmware
// of each scenario), pkg/ (the emulator, with the host drivers built in) and
// build-info.json.
//
// URL parameters: ?scenario=<id> selects a scenario, and &run starts it.

import init, { Emulator } from "./pkg/openprot_wasm.js";

const $ = (id) => document.getElementById(id);
const scenarioSelect = $("scenario");
const runButton = $("run");
const pauseButton = $("pause");
const statusEl = $("status");
const statsEl = $("stats");
const descriptionEl = $("description");
const imageEl = $("image");
const errorEl = $("error");
const stepsEl = $("steps");
const stepsEmptyEl = $("steps-empty");
const uartEl = $("uart");
const imagesBody = $("images").tBodies[0];

// The emulator runs in the page's thread, a batch of cycles per animation
// frame. The batch is sized so that a frame takes about this long.
const FRAME_BUDGET_MS = 12;
const MIN_BATCH = 5_000;
const MAX_BATCH = 2_000_000;

// The UART pane keeps this many characters.
const MAX_UART_CHARS = 400_000;

// www/scenarios.json and www/build-info.json.
let manifest = null;
let buildInfo = null;
let scenarios = [];
let emulator = null;
let running = false;
let finished = false;
let batch = 50_000;
let currentStep = null;
let startedAt = 0;
let runId = 0;

const imageCache = new Map();

async function fetchJson(path) {
  const response = await fetch(path, { cache: "no-cache" });
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}`);
  return response.json();
}

async function fetchImage(info) {
  if (!imageCache.has(info.path)) {
    const response = await fetch(info.path);
    if (!response.ok) throw new Error(`${info.path}: HTTP ${response.status}`);
    imageCache.set(info.path, new Uint8Array(await response.arrayBuffer()));
  }
  return imageCache.get(info.path);
}

function selectedScenario() {
  return scenarios.find((s) => s.id === scenarioSelect.value);
}

function setStatus(state, label) {
  statusEl.dataset.state = state;
  statusEl.textContent = label;
}

function showError(message) {
  errorEl.textContent = message;
  errorEl.hidden = !message;
}

function showScenario() {
  const scenario = selectedScenario();
  descriptionEl.textContent = scenario.description;
  imageEl.replaceChildren("Host driver ", code(scenario.driver), ".");
  showImages();
  const url = new URL(location.href);
  url.searchParams.set("scenario", scenario.id);
  history.replaceState(null, "", url);
}

function code(text) {
  const el = document.createElement("code");
  el.textContent = text;
  return el;
}

function link(href, text) {
  const el = document.createElement("a");
  el.href = href;
  el.textContent = text;
  return el;
}

// -------------------------------------------------------- images and versions

// A link to a commit, labelled with its short hash.
function commit(source) {
  return link(`${source.repo}/commit/${source.rev}`, source.rev.slice(0, 8));
}

function imageRow(name, source, sourceVersion) {
  const row = imagesBody.insertRow();
  const head = document.createElement("th");
  head.scope = "row";
  head.textContent = name;
  row.append(head);
  row.insertCell().append(...source);
  row.insertCell().textContent = sourceVersion;
}

// Lists the images of the selected scenario, with the sources they were
// built from.
function showImages() {
  const scenario = selectedScenario();
  const { caliptra, openprot } = manifest;
  imagesBody.replaceChildren();

  const stages = { rom: "Caliptra ROM", fmc: "Caliptra FMC", runtime: "Caliptra runtime" };
  for (const [stage, name] of Object.entries(stages)) {
    const version = caliptra.caliptra_sw.source_versions?.[stage] ?? "Not known";
    imageRow(name, ["caliptra-sw ", commit(caliptra.caliptra_sw)], version);
  }

  const mcuRom = ["caliptra-mcu-sw ", commit(caliptra.caliptra_mcu_sw)];
  if (caliptra.caliptra_mcu_sw.release_tag) mcuRom.push(", tag ", code(caliptra.caliptra_mcu_sw.release_tag));
  imageRow("MCU ROM", mcuRom, "Has none");

  const firmware = ["OpenPRoT ", commit(openprot)];
  if (openprot.rev !== openprot.pinned_rev) firmware.push(" (not the pinned revision)");
  if (openprot.modified) firmware.push(" with local changes");
  firmware.push(", ", code(scenario.image), `, ${scenario.images.mcu_fw.size.toLocaleString()} bytes`);
  imageRow("MCU firmware", firmware, "Has none");

  if (scenario.update) {
    const size = scenario.update.mcu_fw.size.toLocaleString();
    const updated = ["OpenPRoT ", commit(openprot), ", ", code(scenario.update.image), `, ${size} bytes`];
    imageRow("MCU firmware of the update", updated, "Has none");
  }
}

function showEmulatorSources() {
  if (!buildInfo) return;
  $("emulator-sources").replaceChildren(
    "The emulator that runs them is built from caliptra-mcu-sw ",
    commit(buildInfo.caliptra_mcu_sw),
    " and caliptra-sw ",
    commit(buildInfo.caliptra_sw),
    ".",
  );
}

// ---------------------------------------------------------------- host steps

function setStepState(step, state) {
  step.dataset.state = state;
  step.querySelector(".step-mark").textContent = { running: "…", passed: "✓", failed: "✗" }[state];
}

function addStep(event) {
  if (currentStep) setStepState(currentStep, "passed");
  const step = document.createElement("li");
  step.className = "step";
  step.innerHTML =
    '<div class="step-head"><span class="step-mark" aria-hidden="true"></span>' +
    '<span class="step-name"></span><span class="step-cycle"></span></div>' +
    '<div class="step-body"></div>';
  step.querySelector(".step-name").textContent = event.name;
  step.querySelector(".step-cycle").textContent = `cycle ${event.cycle.toLocaleString()}`;
  setStepState(step, "running");
  stepsEl.append(step);
  currentStep = step;
  stepsEmptyEl.hidden = true;
}

function addToStep(el) {
  if (currentStep) currentStep.querySelector(".step-body").append(el);
}

function addNote(text) {
  const p = document.createElement("p");
  p.textContent = text;
  addToStep(p);
}

// A message of more bytes than MAX_SHOWN_BYTES is collapsed to its first
// SUMMARY_BYTES.
const MAX_SHOWN_BYTES = 64;
const SUMMARY_BYTES = 16;

// Hex digits as bytes separated by spaces.
function spacedHex(hex) {
  return hex.replace(/../g, "$& ").trim();
}

function addMessage(event) {
  const message = document.createElement("div");
  message.className = "message";
  message.dataset.direction = event.direction;
  const head = document.createElement("div");
  head.className = "message-head";
  const arrow = event.direction === "tx" ? "→" : "←";
  const length = event.bytes.length / 2;
  head.textContent = `${arrow} ${event.protocol} ${event.name} (${length} bytes)`;
  message.append(head);
  if (length <= MAX_SHOWN_BYTES) {
    message.append(code(spacedHex(event.bytes)));
  } else {
    // A long message shows its first bytes and opens to show all of them.
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = `${spacedHex(event.bytes.slice(0, 2 * SUMMARY_BYTES))} …`;
    details.append(summary, code(spacedHex(event.bytes)));
    message.append(details);
  }
  addToStep(message);
}

// An element with a class and children (nodes or strings).
function el(tag, className, ...children) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.append(...children);
  return node;
}

// A definition list of [label, value] rows. A value is a node, a string or
// an array of them.
function fieldList(rows, className = "fields") {
  const list = el("dl", className);
  for (const [label, value] of rows) list.append(el("dt", "", label), el("dd", "", ...[value].flat()));
  return list;
}

// A subject or issuer name: the common name, then the other attributes.
function nameNode(attributes) {
  return el(
    "div",
    "",
    ...attributes.map(({ label, value }) =>
      label === "CN" ? el("div", "cert-cn", value) : el("div", "cert-attr", `${label} `, code(value)),
    ),
  );
}

// "2023-01-01T00:00:00Z" as "2023-01-01 00:00:00 UTC".
function utc(time) {
  return time.replace("T", " ").replace("Z", " UTC");
}

// A certificate with this end date has no expiry date (RFC 5280).
const NO_EXPIRY = "9999-12-31T23:59:59Z";

// A data URL of the PEM encoding of a certificate, from its DER in hex.
function pemUrl(derHex) {
  const der = String.fromCharCode(...derHex.match(/../g).map((byte) => parseInt(byte, 16)));
  const lines = btoa(der).match(/.{1,64}/g).join("\n");
  const pem = `-----BEGIN CERTIFICATE-----\n${lines}\n-----END CERTIFICATE-----\n`;
  return `data:application/x-pem-file;base64,${btoa(pem)}`;
}

function extensionNode(extension) {
  const head = el("div", "cert-ext-head", extension.name);
  if (extension.critical) head.append(" ", el("span", "cert-tag", "critical"));
  if (extension.name !== extension.oid) head.append(" ", el("span", "cert-oid", extension.oid));
  return el(
    "div",
    "cert-ext",
    head,
    ...extension.groups.map((group) =>
      fieldList(
        group.map(({ label, value }) => [label, code(value)]),
        "fields stacked",
      ),
    ),
  );
}

// A card for a certificate: its names, validity, key and signature, with the
// rest of its contents behind "Details".
function addCertificate(event) {
  const certificate = event.certificate;
  const title = `${event.name[0].toUpperCase()}${event.name.slice(1)} certificate`;
  const valid =
    certificate.not_after === NO_EXPIRY
      ? `From ${utc(certificate.not_before)}, with no expiry date`
      : `${utc(certificate.not_before)} to ${utc(certificate.not_after)}`;
  const check = el("span", "cert-check", certificate.signature_check.text);
  check.dataset.verified = certificate.signature_check.verified;

  const details = el(
    "details",
    "cert-details",
    el("summary", "", "Details"),
    fieldList(
      [
        ["Serial number", code(certificate.serial)],
        ["SHA-256 fingerprint", code(certificate.sha256)],
        ["Public key", code(certificate.public_key.bytes)],
      ],
      "fields stacked",
    ),
    el("div", "cert-ext-title", "Extensions"),
    ...certificate.extensions.map(extensionNode),
  );
  const download = el("a", "cert-download", "Download PEM");
  download.href = pemUrl(certificate.der);
  download.download = `${event.name.toLowerCase().replace(/[^a-z0-9]+/g, "-")}.pem`;

  addToStep(
    el(
      "div",
      "certificate",
      el(
        "div",
        "cert-head",
        el("span", "cert-title", title),
        el("span", "cert-size", `${certificate.der.length / 2} bytes`),
      ),
      fieldList([
        ["Subject", nameNode(certificate.subject)],
        ["Issuer", nameNode(certificate.issuer)],
        ["Valid", valid],
        ["Key", certificate.public_key.algorithm],
        ["Signature", [`${certificate.signature_algorithm}. `, check]],
      ]),
      details,
      download,
    ),
  );
}

// A bar for how far a transfer is. A later event with the same name in the
// same step moves the bar that the first one added.
function showProgress(event) {
  if (!currentStep) return;
  const bars = [...currentStep.querySelectorAll(".progress")];
  let bar = bars.find((node) => node.dataset.name === event.name);
  if (!bar) {
    bar = el("div", "progress", el("div", "progress-label"), document.createElement("progress"));
    bar.dataset.name = event.name;
    addToStep(bar);
  }
  const done = event.done.toLocaleString();
  const total = event.total.toLocaleString();
  bar.querySelector(".progress-label").textContent = `${event.name}: ${done} of ${total} bytes`;
  const meter = bar.querySelector("progress");
  meter.max = event.total;
  meter.value = event.done;
}

function showEvents(events) {
  for (const event of events) {
    if (event.kind === "step") addStep(event);
    else if (event.kind === "note") addNote(event.text);
    else if (event.kind === "progress") showProgress(event);
    else if (event.kind === "message") addMessage(event);
    else if (event.kind === "certificate") addCertificate(event);
  }
}

// ---------------------------------------------------------------------- UART

// OpenPRoT's log lines carry ANSI SGR escape sequences (pw_log colors the
// level: dim, bold and the eight foreground colors). The pane renders those,
// plus the bright and background colors, as classes on spans; style.css has
// the palette. Other escape sequences are dropped.

// The end of the last chunk of UART output, if it stopped inside an escape
// sequence.
let uartPartial = "";
// The style that the escape sequences so far have selected.
let uartStyle = { bold: false, dim: false, fg: null, bg: null };
// How many characters the pane holds.
let uartChars = 0;

function resetUart() {
  uartEl.replaceChildren();
  uartPartial = "";
  uartStyle = { bold: false, dim: false, fg: null, bg: null };
  uartChars = 0;
}

// Applies the parameters of one SGR sequence ("2;36" of ESC[2;36m).
function applySgr(params) {
  const codes = params === "" ? [0] : params.split(";").map(Number);
  for (let i = 0; i < codes.length; i++) {
    const code = codes[i];
    if (code === 0) uartStyle = { bold: false, dim: false, fg: null, bg: null };
    else if (code === 1) uartStyle.bold = true;
    else if (code === 2) uartStyle.dim = true;
    else if (code === 22) uartStyle.bold = uartStyle.dim = false;
    else if (code >= 30 && code <= 37) uartStyle.fg = code - 30;
    else if (code >= 90 && code <= 97) uartStyle.fg = code - 90 + 8;
    else if (code === 39) uartStyle.fg = null;
    else if (code >= 40 && code <= 47) uartStyle.bg = code - 40;
    else if (code >= 100 && code <= 107) uartStyle.bg = code - 100 + 8;
    else if (code === 49) uartStyle.bg = null;
    // 256-color and RGB colors are not rendered; skip their arguments.
    else if (code === 38 || code === 48) i += codes[i + 1] === 5 ? 2 : codes[i + 1] === 2 ? 4 : 0;
  }
}

function uartClass() {
  const classes = [];
  if (uartStyle.bold) classes.push("ansi-bold");
  if (uartStyle.dim) classes.push("ansi-dim");
  if (uartStyle.fg !== null) classes.push(`ansi-fg-${uartStyle.fg}`);
  if (uartStyle.bg !== null) classes.push(`ansi-bg-${uartStyle.bg}`);
  return classes.join(" ");
}

// Adds text to the pane in the current style.
function appendUartText(text) {
  // Keep printable text: the firmware's exit status arrives as a raw byte.
  text = text.replace(/[^\x20-\x7e\n\t]/g, "");
  if (!text) return;
  const className = uartClass();
  let span = uartEl.lastElementChild;
  if (!span || span.className !== className) {
    span = document.createElement("span");
    span.className = className;
    uartEl.append(span);
  }
  span.append(text);
  uartChars += text.length;
}

function appendUart(text) {
  if (!text) return;
  text = uartPartial + text;
  uartPartial = "";
  const partial = text.match(/\x1b(\[[0-9;]*)?$/);
  if (partial) {
    uartPartial = partial[0];
    text = text.slice(0, partial.index);
  }
  const atBottom = uartEl.scrollHeight - uartEl.scrollTop - uartEl.clientHeight < 40;

  let last = 0;
  for (const sequence of text.matchAll(/\x1b\[([0-9;]*)([A-Za-z])/g)) {
    appendUartText(text.slice(last, sequence.index));
    if (sequence[2] === "m") applySgr(sequence[1]);
    last = sequence.index + sequence[0].length;
  }
  appendUartText(text.slice(last));

  // Drop the oldest output beyond what the pane keeps.
  while (uartChars > MAX_UART_CHARS) {
    const first = uartEl.firstElementChild;
    const excess = uartChars - MAX_UART_CHARS;
    const length = first.textContent.length;
    if (length <= excess) {
      first.remove();
      uartChars -= length;
    } else {
      first.textContent = first.textContent.slice(excess);
      uartChars -= excess;
    }
  }
  if (atBottom) uartEl.scrollTop = uartEl.scrollHeight;
}

// ------------------------------------------------------------------ run loop

function drain() {
  appendUart(emulator.take_uart());
  showEvents(JSON.parse(emulator.take_events()));
  return JSON.parse(emulator.status());
}

function showStats(status) {
  const seconds = (performance.now() - startedAt) / 1000;
  const rate = seconds > 0 ? status.cycles / seconds / 1e6 : 0;
  statsEl.textContent = `${status.cycles.toLocaleString()} cycles, ${rate.toFixed(2)} M cycles/s`;
}

function finish(status) {
  finished = true;
  running = false;
  pauseButton.textContent = "Keep running";
  if (status.state === "passed") {
    if (currentStep) setStepState(currentStep, "passed");
    setStatus("passed", "Passed");
  } else {
    if (currentStep) setStepState(currentStep, "failed");
    setStatus("failed", "Failed");
    showError(status.error);
  }
}

function fail(message) {
  running = false;
  finished = true;
  pauseButton.disabled = true;
  if (currentStep) setStepState(currentStep, "failed");
  setStatus("failed", "Failed");
  showError(message);
}

function frame(id) {
  if (!running || id !== runId) return;
  let status;
  try {
    const before = performance.now();
    emulator.run(batch);
    const elapsed = performance.now() - before;
    if (elapsed > 0) {
      batch = Math.round(Math.min(MAX_BATCH, Math.max(MIN_BATCH, (batch * FRAME_BUDGET_MS) / elapsed)));
    }
    status = drain();
  } catch (e) {
    // A panic in the emulator leaves the module unusable.
    emulator = null;
    fail(`The emulator stopped: ${e}`);
    return;
  }
  showStats(status);
  if (!finished && status.state !== "running") {
    finish(status);
    return;
  }
  requestAnimationFrame(() => frame(id));
}

async function start() {
  const id = ++runId;
  running = false;
  finished = false;
  if (emulator) emulator.free();
  emulator = null;
  currentStep = null;
  stepsEl.replaceChildren();
  stepsEmptyEl.hidden = false;
  resetUart();
  statsEl.textContent = "";
  showError("");
  setStatus("running", "Loading");
  runButton.textContent = "Restart";
  pauseButton.disabled = true;
  pauseButton.textContent = "Pause";

  const scenario = selectedScenario();
  try {
    const names = ["caliptra_rom", "caliptra_fw", "soc_manifest", "mcu_rom", "mcu_fw"];
    // A scenario with a firmware update also has the SoC manifest and the
    // MCU firmware that its driver sends to the device.
    const update = scenario.update ? [scenario.update.soc_manifest, scenario.update.mcu_fw] : [];
    const [images, updateImages] = await Promise.all([
      Promise.all(names.map((name) => fetchImage(scenario.images[name]))),
      Promise.all(update.map(fetchImage)),
    ]);
    if (id !== runId) return;
    emulator = new Emulator(...images, scenario.vendor_pk_hash, scenario.driver, ...updateImages);
  } catch (e) {
    if (id === runId) fail(`Could not start the scenario: ${e}`);
    return;
  }
  setStatus("running", "Running");
  pauseButton.disabled = false;
  running = true;
  startedAt = performance.now();
  showEvents(JSON.parse(emulator.take_events()));
  requestAnimationFrame(() => frame(id));
}

function togglePause() {
  if (!emulator) return;
  running = !running;
  if (finished) {
    pauseButton.textContent = running ? "Pause" : "Keep running";
  } else {
    pauseButton.textContent = running ? "Pause" : "Resume";
    setStatus("running", running ? "Running" : "Paused");
  }
  if (running) requestAnimationFrame(() => frame(runId));
}

// --------------------------------------------------------------------- setup

async function setup() {
  try {
    [manifest] = await Promise.all([fetchJson("scenarios.json"), init()]);
    buildInfo = await fetchJson("build-info.json").catch(() => null);
  } catch (e) {
    setStatus("failed", "Not built");
    showError(`Could not load the site's build output (run ./build.sh): ${e}`);
    return;
  }
  scenarios = manifest.scenarios;
  for (const scenario of scenarios) {
    const option = document.createElement("option");
    option.value = scenario.id;
    option.textContent = scenario.title;
    scenarioSelect.append(option);
  }
  const params = new URLSearchParams(location.search);
  if (scenarios.some((s) => s.id === params.get("scenario"))) {
    scenarioSelect.value = params.get("scenario");
  }
  showScenario();
  showEmulatorSources();

  scenarioSelect.addEventListener("change", () => {
    showScenario();
    runButton.textContent = "Run";
  });
  runButton.addEventListener("click", start);
  pauseButton.addEventListener("click", togglePause);
  runButton.disabled = false;
  setStatus("idle", "Ready");
  if (params.has("run")) start();
}

setup();
