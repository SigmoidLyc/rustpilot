import { spawn, spawnSync } from "node:child_process";
import { closeSync, existsSync, openSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const root = resolve(scriptDir, "..");
const tauriDir = join(root, "src-tauri");
const targetRoot = join(root, ".runtime", "cargo-target");
const targetDir = join(targetRoot, "debug");
const viteScript = join(root, "node_modules", "vite", "bin", "vite.js");
const appLauncher = join(root, "scripts", "launch-app.mjs");
const executable = join(targetDir, "rustpilot.exe");
const viteUrl = "http://127.0.0.1:1420/";

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
  for (let attempt = 0; attempt < 30; attempt += 1) {
    try {
      const response = await fetch(viteUrl);
      if (response.status >= 200 && response.status < 500) {
        return;
      }
    } catch {
      if (vite.exitCode !== null) {
        throw new Error("Vite exited before becoming ready.");
      }
    }
    await sleep(250);
  }
  throw new Error(`Vite did not become ready at ${viteUrl}.`);
}

function startWatcher(appPid, vitePid) {
  const watcher = spawn(process.execPath, [fileURLToPath(import.meta.url), "--watch", String(appPid), String(vitePid)], {
    cwd: root,
    detached: true,
    windowsHide: true,
    stdio: "ignore",
  });
  watcher.unref();
}

async function watch(appPid, vitePid) {
  const timer = setInterval(() => {
    try {
      process.kill(appPid, 0);
    } catch {
      try {
        process.kill(vitePid);
      } catch {
        // The dev server may already have exited.
      }
      clearInterval(timer);
    }
  }, 1000);
}

async function start() {
  const vite = spawn(process.execPath, [viteScript, "--host", "127.0.0.1", "--port", "1420"], {
    cwd: root,
    detached: true,
    windowsHide: true,
    stdio: "ignore",
  });
  vite.unref();

  try {
    await waitForVite(vite);
    stopExistingApplication(executable);
    await waitForExecutableRelease(executable);
    const cargo = process.platform === "win32" ? "cargo.exe" : "cargo";
    const build = spawnSync(cargo, ["build", "--locked", "--no-default-features", "--bin", "rustpilot"], {
      cwd: tauriDir,
      env: { ...process.env, CARGO_TARGET_DIR: targetRoot },
      stdio: "inherit",
    });
    if (build.error) {
      throw build.error;
    }
    if (build.status !== 0) {
      throw new Error(`Rust build failed with exit code ${build.status}.`);
    }

    const app = spawn(process.execPath, [appLauncher, executable, targetDir, String(vite.pid)], {
      cwd: root,
      detached: true,
      windowsHide: true,
      env: applicationEnvironment(),
      stdio: "ignore",
    });
    app.unref();
    await sleep(2000);
    startWatcher(app.pid, vite.pid);
    console.log(`RustPilot launch requested. Vite remains available at ${viteUrl}.`);
  } catch (error) {
    stopProcess(vite);
    throw error;
  }
}

if (process.argv[2] === "--watch") {
  await watch(Number(process.argv[3]), Number(process.argv[4]));
} else {
  await start();
}
