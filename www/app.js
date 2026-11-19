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

// The emulator runs in the page's thread, a batch of cycles per animation
// frame. The batch is sized so that a frame takes about this long.
const FRAME_BUDGET_MS = 12;
const MIN_BATCH = 5_000;
const MAX_BATCH = 2_000_000;

// The UART pane keeps this many characters.
const MAX_UART_CHARS = 400_000;

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
  imageEl.replaceChildren(
    "Image ",
    code(scenario.image),
    `, ${scenario.images.mcu_fw.size.toLocaleString()} bytes. Host driver `,
    code(scenario.driver),
    ".",
  );
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

function addMessage(event) {
  const message = document.createElement("div");
  message.className = "message";
  message.dataset.direction = event.direction;
  const head = document.createElement("div");
  head.className = "message-head";
  const arrow = event.direction === "tx" ? "→" : "←";
  head.textContent = `${arrow} ${event.protocol} ${event.name} (${event.bytes.length / 2} bytes)`;
  message.append(head, code(event.bytes.replace(/../g, "$& ").trim()));
  addToStep(message);
}

function showEvents(events) {
  for (const event of events) {
    if (event.kind === "step") addStep(event);
    else if (event.kind === "note") addNote(event.text);
    else if (event.kind === "message") addMessage(event);
  }
}

// ---------------------------------------------------------------------- UART

// The end of the last chunk of UART output, if it stopped inside an escape
// sequence.
let uartPartial = "";

function appendUart(text) {
  if (!text) return;
  text = uartPartial + text;
  uartPartial = "";
  const partial = text.match(/\x1b(\[[0-9;]*)?$/);
  if (partial) {
    uartPartial = partial[0];
    text = text.slice(0, partial.index);
  }
  // Drop the log's color codes, then keep printable text: the firmware's
  // exit status arrives as a raw byte.
  text = text.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "").replace(/[^\x20-\x7e\n\t]/g, "");
  const atBottom = uartEl.scrollHeight - uartEl.scrollTop - uartEl.clientHeight < 40;
  uartEl.append(text);
  uartEl.normalize();
  const excess = uartEl.textContent.length - MAX_UART_CHARS;
  if (excess > 0) uartEl.textContent = uartEl.textContent.slice(excess);
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
  uartEl.textContent = "";
  uartPartial = "";
  statsEl.textContent = "";
  showError("");
  setStatus("running", "Loading");
  runButton.textContent = "Restart";
  pauseButton.disabled = true;
  pauseButton.textContent = "Pause";

  const scenario = selectedScenario();
  try {
    const names = ["caliptra_rom", "caliptra_fw", "soc_manifest", "mcu_rom", "mcu_fw"];
    const images = await Promise.all(names.map((name) => fetchImage(scenario.images[name])));
    if (id !== runId) return;
    emulator = new Emulator(...images, scenario.vendor_pk_hash, scenario.driver);
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

function showBuildInfo(openprot, emulatorInfo) {
  const footer = $("build-info");
  const short = (rev) => rev.slice(0, 8);
  const commit = (source) => link(`${source.repo}/commit/${source.rev}`, short(source.rev));
  footer.replaceChildren("Firmware: OpenPRoT ", commit(openprot));
  if (openprot.rev !== openprot.pinned_rev) footer.append(" (not the pinned revision)");
  if (openprot.modified) footer.append(" with local changes");
  if (emulatorInfo) {
    footer.append(
      ". Emulator: caliptra-mcu-sw ",
      commit(emulatorInfo.caliptra_mcu_sw),
      " and caliptra-sw ",
      commit(emulatorInfo.caliptra_sw),
    );
  }
  footer.append(".");
}

async function setup() {
  let manifest;
  try {
    [manifest] = await Promise.all([fetchJson("scenarios.json"), init()]);
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
  showBuildInfo(manifest.openprot, await fetchJson("build-info.json").catch(() => null));

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
