import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync } from "node:fs";
import { createConnection } from "node:net";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import { readViteLease, releaseViteLease, writeViteLease } from "./dev-lease.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const root = resolve(scriptDir, "..");
const tauriDir = join(root, "src-tauri");
const targetRoot = join(root, ".runtime", "cargo-target");
const targetDir = join(targetRoot, "debug");
const viteScript = join(root, "node_modules", "vite", "bin", "vite.js");
const appLauncher = join(root, "scripts", "launch-app.mjs");
const executable = join(targetDir, "rustpilot.exe");
const leasePath = join(root, ".runtime", "vite-lease.json");
const viteUrl = "http://127.0.0.1:1420/";
const vitePort = 1420;

const sleep = (milliseconds) => new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));

function applicationEnvironment() {
  const allowedSystemKeys = [
    "PATH",
    "SystemRoot",
    "SystemDrive",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "PROGRAMDATA",
    "WINDIR",
  ];
  const environment = {};
  const sourceKeys = Object.keys(process.env);
  const copyKey = (key) => {
    const sourceKey = sourceKeys.find((candidate) => candidate.toLowerCase() === key.toLowerCase());
    if (sourceKey && process.env[sourceKey] !== undefined) {
      environment[key] = process.env[sourceKey];
    }
  };
  for (const key of allowedSystemKeys) {
    copyKey(key);
  }
  for (const sourceKey of sourceKeys) {
    if (/^(RUSTPILOT_|OPENAI_API_KEY$|HTTP_PROXY$|HTTPS_PROXY$|ALL_PROXY$|NO_PROXY$|WEBVIEW2_)/i.test(sourceKey)) {
      const normalizedKey = sourceKey.toUpperCase();
      if (!(normalizedKey in environment)) {
        environment[normalizedKey] = process.env[sourceKey];
      }
    }
  }
  return environment;
}

function stopProcess(processHandle) {
  if (processHandle && processHandle.exitCode === null) {
    processHandle.kill();
  }
}

function buildApplication(cargo) {
  return new Promise((resolvePromise, rejectPromise) => {
    const build = spawn(cargo, ["build", "--locked", "--no-default-features", "--bin", "rustpilot"], {
      cwd: tauriDir,
      env: { ...process.env, CARGO_TARGET_DIR: targetRoot },
      stdio: "inherit"
    });
    build.once("error", rejectPromise);
    build.once("exit", (code, signal) => {
      if (signal) rejectPromise(new Error(`Rust build stopped by ${signal}.`));
      else if (code !== 0) rejectPromise(new Error(`Rust build failed with exit code ${code}.`));
      else resolvePromise();
    });
  });
}

function isPortOpen(port) {
  return new Promise((resolvePromise) => {
    const socket = createConnection({ host: "127.0.0.1", port });
    let settled = false;
    const settle = (open) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      resolvePromise(open);
    };
    socket.setTimeout(200);
    socket.once("connect", () => settle(true));
    socket.once("timeout", () => settle(false));
    socket.once("error", () => settle(false));
  });
}

async function waitForVitePortRelease() {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (!(await isPortOpen(vitePort))) return;
    await sleep(100);
  }
  throw new Error(`The previous Vite server still owns ${viteUrl}.`);
}

async function fetchWithTimeout(url, timeoutMilliseconds) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMilliseconds);
  try {
    const response = await fetch(url, { cache: "no-store", signal: controller.signal });
    await response.arrayBuffer();
    return response;
  } finally {
    clearTimeout(timeout);
  }
}

function processExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function frontendReady(timeoutMilliseconds) {
  try {
    const responses = await Promise.all([
      fetchWithTimeout(viteUrl, timeoutMilliseconds),
      fetchWithTimeout(new URL("src/main.ts", viteUrl), timeoutMilliseconds),
      fetchWithTimeout(new URL("src/App.svelte", viteUrl), timeoutMilliseconds),
    ]);
    return responses.every((response) => response.status >= 200 && response.status < 500);
  } catch {
    return false;
  }
}

async function claimReusableVite() {
  const lease = readViteLease(leasePath);
  if (!lease || !processExists(lease.pid) || !(await frontendReady(500))) return null;
  const token = randomUUID();
  writeViteLease(leasePath, lease.pid, token);
  return { pid: lease.pid, token, process: null, reused: true };
}

function stopExistingApplication(executable) {
  if (process.platform !== "win32") {
    return;
  }

  const escapedExecutable = executable.replaceAll("'", "''");
  const command = [
    `$target = '${escapedExecutable}';`,
    "$processes = Get-Process -Name rustpilot -ErrorAction SilentlyContinue | Where-Object {",
    "  try { $_.Path -and ([System.IO.Path]::GetFullPath($_.Path) -ieq $target) } catch { $false }",
    "};",
    "$processes | Stop-Process -Force -ErrorAction SilentlyContinue;",
  ].join(" ");
  const result = spawnSync("powershell.exe", [
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    command,
  ], { stdio: "ignore" });
  if (result.error) {
    throw result.error;
  }
}

async function waitForExecutableRelease(executable) {
  if (!existsSync(executable)) {
    return;
  }
  for (let attempt = 0; attempt < 20; attempt += 1) {
    try {
      const handle = openSync(executable, "r+");
      closeSync(handle);
      return;
    } catch {
      await sleep(250);
    }
  }
  throw new Error(`The previous RustPilot process still holds ${executable}.`);
}

async function waitForVite(vite) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    const exitCode = vite.process?.exitCode;
    if (!processExists(vite.pid) || (exitCode !== null && exitCode !== undefined)) {
      throw new Error(`Vite exited before becoming ready${vite.process ? ` (code ${exitCode})` : ""}.`);
    }
    if (await frontendReady(1000)) return;
    await sleep(250);
  }
  throw new Error(`Vite did not become ready at ${viteUrl}.`);
}

function startWatcher(appPid, vitePid, leaseToken) {
  const watcher = spawn(
    process.execPath,
    [fileURLToPath(import.meta.url), "--watch", String(appPid), String(vitePid), leaseToken],
    {
      cwd: root,
      detached: true,
      windowsHide: true,
      stdio: "ignore",
    },
  );
  watcher.unref();
}

async function watch(appPid, vitePid, leaseToken) {
  const timer = setInterval(() => {
    try {
      process.kill(appPid, 0);
    } catch {
      releaseViteLease(leasePath, vitePid, leaseToken);
      clearInterval(timer);
    }
  }, 1000);
}

async function start() {
  mkdirSync(dirname(leasePath), { recursive: true });
  let vite;
  let viteProcess;

  try {
    vite = await claimReusableVite();
    stopExistingApplication(executable);
    if (vite) {
      await waitForExecutableRelease(executable);
      console.log(`Reusing Vite server at ${viteUrl}.`);
    } else {
      await Promise.all([waitForExecutableRelease(executable), waitForVitePortRelease()]);
      viteProcess = spawn(process.execPath, [viteScript, "--host", "127.0.0.1", "--port", String(vitePort)], {
        cwd: root,
        detached: true,
        windowsHide: true,
        stdio: "ignore",
      });
      viteProcess.unref();
      const token = randomUUID();
      writeViteLease(leasePath, viteProcess.pid, token);
      vite = { pid: viteProcess.pid, token, process: viteProcess, reused: false };
    }

    const cargo = process.platform === "win32" ? "cargo.exe" : "cargo";
    await Promise.all([waitForVite(vite), buildApplication(cargo)]);

    const app = spawn(process.execPath, [appLauncher, executable, root, String(vite.pid), leasePath, vite.token], {
      cwd: root,
      detached: true,
      windowsHide: true,
      env: applicationEnvironment(),
      stdio: "ignore",
    });
    app.unref();
    startWatcher(app.pid, vite.pid, vite.token);
    console.log(`RustPilot launch requested. Vite remains available at ${viteUrl}.`);
  } catch (error) {
    if (vite) releaseViteLease(leasePath, vite.pid, vite.token);
    else stopProcess(viteProcess);
    throw error;
  }
}

if (process.argv[2] === "--watch") {
  await watch(Number(process.argv[3]), Number(process.argv[4]), process.argv[5]);
} else {
  await start();
}
