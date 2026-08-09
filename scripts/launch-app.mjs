import { spawn } from "node:child_process";
import { releaseViteLease } from "./dev-lease.mjs";

const executable = process.argv[2];
const targetDir = process.argv[3];
const vitePid = Number(process.argv[4]);
const leasePath = process.argv[5];
const leaseToken = process.argv[6];

const app = spawn(executable, [], {
  cwd: targetDir,
  detached: true,
  windowsHide: false,
  stdio: "ignore",
});
app.unref();

const timer = setInterval(() => {
  if (app.exitCode !== null) {
    releaseViteLease(leasePath, vitePid, leaseToken);
    clearInterval(timer);
  }
}, 1000);
