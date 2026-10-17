import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const release = process.argv.includes("--release");
const profile = release ? "release" : "debug";
const manifest = path.join(root, "runner-core", "Cargo.toml");
const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--manifest-path", manifest, "--format-version", "1", "--no-deps", "--locked"], { cwd: root, encoding: "utf8" }));
const built = path.join(metadata.target_directory, profile, "fabrica-worker.exe");
const resourceDirectory = path.join(root, "src-tauri", "resources");
const packaged = path.join(resourceDirectory, "fabrica-worker.exe");

execFileSync("cargo", ["build", "--manifest-path", manifest, "--bin", "fabrica-worker", "--locked", ...(release ? ["--release"] : [])], {
  cwd: root,
  stdio: "inherit",
});
mkdirSync(resourceDirectory, { recursive: true });
copyFileSync(built, packaged);
process.stdout.write(`Prepared ${profile} worker resource: ${path.relative(root, packaged)}\n`);
