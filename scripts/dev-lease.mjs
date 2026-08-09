import { readFileSync, rmSync, writeFileSync } from "node:fs";

export function readViteLease(path) {
  try {
    const lease = JSON.parse(readFileSync(path, "utf8"));
    if (Number.isInteger(lease.pid) && lease.pid > 0 && typeof lease.token === "string" && lease.token) {
      return lease;
    }
  } catch {
    // Missing or partially written leases are not reusable.
  }
  return null;
}

export function writeViteLease(path, pid, token) {
  writeFileSync(path, JSON.stringify({ pid, token }), "utf8");
}

export function ownsViteLease(path, pid, token) {
  const lease = readViteLease(path);
  return lease?.pid === pid && lease.token === token;
}

export function releaseViteLease(path, pid, token) {
  if (!ownsViteLease(path, pid, token)) return false;
  try {
    process.kill(pid);
  } catch {
    // The server may already have exited.
  }
  if (ownsViteLease(path, pid, token)) {
    rmSync(path, { force: true });
  }
  return true;
}
