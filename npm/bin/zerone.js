#!/usr/bin/env node

import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const platformKeys = {
  "win32-x64": "win32-x64",
  "linux-x64": "linux-x64",
  "darwin-x64": "darwin-x64",
  "darwin-arm64": "darwin-arm64",
};

const runtime = `${process.platform}-${process.arch}`;
const platformKey = platformKeys[runtime];
if (!platformKey) {
  console.error(`Zerone does not support ${runtime}.`);
  process.exit(1);
}

const executable = process.platform === "win32" ? "zerone.exe" : "zerone";
const binary = fileURLToPath(
  new URL(`../vendor/${platformKey}/${executable}`, import.meta.url),
);

if (!existsSync(binary)) {
  console.error(
    `The installed Zerone package does not contain a binary for ${runtime}.\n` +
      "Reinstall the package or use a release that supports this platform.",
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: false,
});

if (result.error) {
  console.error(`Failed to start Zerone: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
