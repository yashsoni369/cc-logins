#!/usr/bin/env node
// Runs as tauri.conf.json's `beforeBundleCommand`, between compiling and
// bundling.
//
// Tauri bundles every Cargo binary of the package, including the `claude`
// shim (src-tauri/src/bin/cc-logins-shim.rs). For a universal macOS build,
// though, tauri-cli lipos only the main binary into
// target/universal-apple-darwin/release and then fails looking for the shim
// there. This lipos the shim's two slices into the same place. Everywhere
// else it does nothing.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const target = join(import.meta.dirname, "..", "src-tauri", "target");
const slices = ["aarch64-apple-darwin", "x86_64-apple-darwin"].map((triple) =>
  join(target, triple, "release", "cc-logins-shim"),
);
const universalDir = join(target, "universal-apple-darwin", "release");
const universal = join(universalDir, "cc-logins-shim");

const isUniversalBuild =
  process.platform === "darwin" &&
  existsSync(universalDir) &&
  slices.every((slice) => existsSync(slice));

if (!isUniversalBuild) {
  process.exit(0);
}

mkdirSync(universalDir, { recursive: true });
execFileSync("lipo", ["-create", "-output", universal, ...slices], { stdio: "inherit" });
console.log(`bundle-shim: universal cc-logins-shim written to ${universal}`);
