import { spawn } from "node:child_process";

const executable = process.argv[2];
const targetDir = process.argv[3];
const vitePid = Number(process.argv[4]);

const app = spawn(executable, [], {
  cwd: targetDir,
  detached: true,
  windowsHide: false,
  shell: true,
  stdio: "ignore",
});
app.unref();

const timer = setInterval(() => {
  if (app.exitCode !== null) {
    try {
      process.kill(vitePid);
    } catch {
      // The dev server may already have exited.
    }
    clearInterval(timer);
  }
}, 1000);
