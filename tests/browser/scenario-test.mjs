#!/usr/bin/env node
// Licensed under the Apache-2.0 license
//
// Runs the scenarios on the page in a real (headless) browser.
//
// Serves www/ on a local port, launches headless Firefox, drives it over
// WebDriver BiDi (see harness.mjs), and for each scenario in
// www/scenarios.json selects it and clicks Run, as a user would. The page
// must report that the scenario passed.
//
// Usage:
//   node tests/browser/scenario-test.mjs [--scenario ID]... [--timeout SECS] [--out DIR]
//
// With --out, each run's UART output, final page state and a full-page
// screenshot are written to DIR/<id>-uart.txt, DIR/<id>-state.json and
// DIR/<id>.png.
//
// See harness.mjs for the environment variables that pick the browser.
// Requires Node 22+ (global WebSocket).

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { WWW, sleep, tail, withBrowser } from "./harness.mjs";

function parseArgs(argv) {
  const out = { scenarios: [], timeout: 600, outDir: null };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--scenario") out.scenarios.push(argv[++i]);
    else if (argv[i] === "--timeout") out.timeout = Number(argv[++i]);
    else if (argv[i] === "--out") out.outDir = argv[++i];
    else if (argv[i] === "-h" || argv[i] === "--help") {
      console.log("usage: scenario-test.mjs [--scenario ID]... [--timeout SECS] [--out DIR]");
      process.exit(0);
    } else throw new Error(`unknown argument: ${argv[i]}`);
  }
  return out;
}

const PAGE_STATE = `({
  state: document.getElementById("status").dataset.state,
  status: document.getElementById("status").textContent,
  stats: document.getElementById("stats").textContent,
  error: document.getElementById("error").hidden ? "" : document.getElementById("error").textContent,
  runDisabled: document.getElementById("run").disabled,
  selected: document.getElementById("scenario").value,
  steps: [...document.querySelectorAll("#steps .step")].map((step) => ({
    name: step.querySelector(".step-name").textContent,
    state: step.dataset.state,
    messages: [...step.querySelectorAll(".message-head")].map((m) => m.textContent),
  })),
  uart: document.getElementById("uart").textContent,
  images: document.getElementById("images").tBodies[0].innerText,
  coloredSpans: document.querySelectorAll('#uart span[class*="ansi-fg-"]').length,
})`;

async function runScenario(bidi, context, baseUrl, pageErrors, scenario, args) {
  const id = scenario.id;
  console.log(`\n=== ${id}: ${scenario.title} ===`);
  pageErrors.length = 0;
  await bidi.send("browsingContext.navigate", {
    context,
    url: `${baseUrl}?scenario=${encodeURIComponent(id)}`,
    wait: "complete",
  });

  // Wait for the WASM module and scenarios.json.
  let st;
  const readyBy = Date.now() + 60_000;
  for (;;) {
    st = await bidi.eval(context, PAGE_STATE);
    if (!st.runDisabled) break;
    if (st.state === "failed" || Date.now() > readyBy) {
      throw new Error(`${id}: the page did not become ready: ${st.status} ${st.error}`);
    }
    await sleep(250);
  }
  if (st.selected !== id) throw new Error(`${id}: the page selected ${st.selected}`);

  await bidi.eval(context, `(document.getElementById("run").click(), true)`);

  const save = async () => {
    if (!args.outDir) return;
    mkdirSync(args.outDir, { recursive: true });
    const { uart, ...rest } = st;
    writeFileSync(join(args.outDir, `${id}-uart.txt`), uart);
    writeFileSync(join(args.outDir, `${id}-state.json`), JSON.stringify({ ...rest, pageErrors }, null, 2));
    try {
      const shot = await bidi.send("browsingContext.captureScreenshot", { context, origin: "document" });
      writeFileSync(join(args.outDir, `${id}.png`), Buffer.from(shot.data, "base64"));
    } catch (e) {
      console.warn(`screenshot failed: ${e.message}`);
    }
  };
  const fail = async (why) => {
    await save();
    throw new Error(
      `${id}: ${why}\nstatus: ${st.status}\nstats: ${st.stats}\n` +
        `steps:\n${st.steps.map((s) => `  [${s.state}] ${s.name}`).join("\n")}\n` +
        (pageErrors.length ? `page errors:\n${pageErrors.join("\n")}\n` : "") +
        `UART tail:\n${tail(st.uart)}`,
    );
  };

  const started = Date.now();
  let shown = 0;
  for (;;) {
    await sleep(1000);
    st = await bidi.eval(context, PAGE_STATE);
    for (const step of st.steps.slice(shown)) console.log(`  step: ${step.name}`);
    shown = st.steps.length;
    if (st.state === "passed") break;
    if (st.state === "failed") await fail(`the scenario failed: ${st.error}`);
    if (pageErrors.length) await fail("the page logged an error");
    if (Date.now() - started > args.timeout * 1000) await fail(`not finished after ${args.timeout} s`);
  }

  if (st.steps.length === 0) await fail("passed without reporting a step");
  const notPassed = st.steps.filter((s) => s.state !== "passed");
  if (notPassed.length) await fail(`steps not marked passed: ${notPassed.map((s) => s.name).join(", ")}`);
  if (!st.uart.trim()) await fail("no UART output");
  if (!st.images.includes(scenario.image)) await fail("the image's label is not listed");
  // OpenPRoT's log lines color the level; the escape sequences must be
  // rendered, not printed.
  if (st.coloredSpans === 0) await fail("no colored text in the UART pane");
  if (/\x1b|\[\d+(;\d+)*m/.test(st.uart)) await fail("an escape sequence was printed in the UART pane");
  if (pageErrors.length) await fail("the page logged an error");
  await save();
  const seconds = ((Date.now() - started) / 1000).toFixed(0);
  console.log(`PASS ${id}: ${st.steps.length} steps in ${seconds} s (${st.stats})`);
}

const args = parseArgs(process.argv.slice(2));
const manifest = JSON.parse(readFileSync(join(WWW, "scenarios.json"), "utf8"));
let scenarios = manifest.scenarios;
if (args.scenarios.length) {
  const unknown = args.scenarios.filter((id) => !scenarios.some((s) => s.id === id));
  if (unknown.length) throw new Error(`not in www/scenarios.json: ${unknown.join(", ")}`);
  scenarios = scenarios.filter((s) => args.scenarios.includes(s.id));
}

await withBrowser(async (bidi, context, baseUrl, pageErrors) => {
  for (const scenario of scenarios) {
    await runScenario(bidi, context, baseUrl, pageErrors, scenario, args);
  }
});
console.log(`\n${scenarios.length} scenario(s) passed`);
