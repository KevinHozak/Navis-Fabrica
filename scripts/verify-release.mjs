import { readFileSync, statSync } from "node:fs";
import { resolve } from "node:path";

const root = process.cwd();
const readJson = (path) => JSON.parse(readFileSync(resolve(root, path), "utf8"));
const failures = [];
const requireValue = (condition, message) => {
  if (!condition) failures.push(message);
};

const appPackage = readJson("package.json");
const packageLock = readJson("package-lock.json");
const tauri = readJson("src-tauri/tauri.conf.json");
const cargoToml = readFileSync(resolve(root, "src-tauri/Cargo.toml"), "utf8");
const cargoLock = readFileSync(resolve(root, "src-tauri/Cargo.lock"), "utf8");
const settingsSource = readFileSync(resolve(root, "src-tauri/src/lib.rs"), "utf8");
const runsSource = readFileSync(resolve(root, "src-tauri/src/runs.rs"), "utf8");
const appCrate = cargoToml.match(/^\[package\]\s+.*?^name\s*=\s*"([^"]+)"/ms);
const cargoVersion = cargoToml.match(/^\[package\]\s+.*?^version\s*=\s*"([^"]+)"/ms);
const lockedApp = appCrate
  ? cargoLock.match(new RegExp(`^name = "${appCrate[1]}"\\r?\\nversion = "([^"]+)"`, "ms"))
  : null;
const declaredVersions = {
  "package.json": appPackage.version,
  "package-lock.json root": packageLock.version,
  "package-lock.json app package": packageLock.packages?.[""]?.version,
  "Cargo.toml": cargoVersion?.[1],
  "Cargo.lock app package": lockedApp?.[1],
  "tauri.conf.json": tauri.version,
};
const versions = new Set(Object.values(declaredVersions));
requireValue(versions.size === 1 && !versions.has(undefined), `Application versions differ: ${JSON.stringify(declaredVersions)}`);
requireValue(Object.values(declaredVersions).every((version) => /^\d+\.\d+\.\d+$/.test(version ?? "")), "Application version must be stable SemVer in every manifest.");

const bundle = tauri.bundle ?? {};
const targets = Array.isArray(bundle.targets) ? bundle.targets : [bundle.targets];
requireValue(bundle.active === true, "Tauri bundling must be enabled for release packaging.");
requireValue(targets.includes("nsis"), "The release target must include NSIS.");
requireValue(bundle.icon?.includes("icons/icon.ico") && bundle.icon?.includes("icons/icon.png"), "Both checked-in application icons must be bundled.");
requireValue(bundle.windows?.nsis?.installMode === "currentUser", "NSIS must default to a per-user installation.");
requireValue(bundle.windows?.webviewInstallMode?.type === "downloadBootstrapper", "WebView2 setup must use the documented bootstrapper mode.");
requireValue(bundle.windows?.webviewInstallMode?.silent === true, "WebView2 bootstrapper invocation must be silent.");
requireValue(tauri.app?.windows?.some((window) => window.title === `Navis-Fabrica · v${tauri.version}`), "The native window title must match the application release version.");
requireValue(bundle.createUpdaterArtifacts !== true, "Updater artifacts must remain disabled until the update strategy is separately approved.");
requireValue(bundle.windows?.certificateThumbprint == null && bundle.windows?.signCommand == null, "Release artifacts must not imply configured code signing.");
requireValue(/^default\s*=\s*\[[^\]]*"custom-protocol"/m.test(cargoToml) && /^custom-protocol\s*=\s*\[[^\]]*"tauri\/custom-protocol"/m.test(cargoToml), "Release builds must enable Tauri's custom protocol so standalone executables load bundled frontend assets.");
requireValue(tauri.identifier === "com.navis.fabrica", "Changing the application identifier would change the per-user data namespace.");
requireValue(tauri.app?.appDirectoriesOverride == null, "Release builds must keep the existing app data/config directory layout.");
requireValue(/fn settings_path\([\s\S]*?\.app_config_dir\(\)[\s\S]*?SETTINGS_FILE/.test(settingsSource), "Settings must continue to use Tauri's application config directory.");
requireValue(/fn history_path\([\s\S]*?\.app_data_dir\(\)[\s\S]*?HISTORY_FILE/.test(runsSource), "Run history must continue to use Tauri's application data directory.");
requireValue(/const SETTINGS_FILE: &str = "settings\.json"/.test(settingsSource), "The stable settings filename must remain unchanged.");
requireValue(/const HISTORY_FILE: &str = "workflow-runs\.sqlite3"/.test(runsSource), "The stable run-history filename must remain unchanged.");
requireValue(appPackage.scripts?.["release:verify"] === "node scripts/verify-release.mjs", "The release verification command is missing.");
requireValue(appPackage.scripts?.["release:windows"] === "powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release-windows.ps1", "The repeatable Windows release command is missing.");
requireValue(appPackage.packageManager === "npm@11.11.1", "The npm package manager version must be pinned.");
requireValue(appPackage.engines?.node === "24.14.x" && appPackage.engines?.npm === "11.11.x", "Supported Node.js and npm release toolchains must be declared.");
requireValue(readFileSync(resolve(root, ".node-version"), "utf8").trim() === "24.14.0", "The exact Node.js release version must be pinned.");
const rustToolchain = readFileSync(resolve(root, "rust-toolchain.toml"), "utf8");
requireValue(/^channel\s*=\s*"1\.96\.0"$/m.test(rustToolchain), "The exact Rust release toolchain must be pinned.");
requireValue(/^targets\s*=\s*\["x86_64-pc-windows-msvc"\]$/m.test(rustToolchain), "The Windows MSVC target must be pinned.");

const png = readFileSync(resolve(root, "src-tauri/icons/icon.png"));
requireValue(png.length > 24 && png.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])), "icons/icon.png is not a valid PNG file.");
if (png.length > 24) requireValue(png.readUInt32BE(16) === png.readUInt32BE(20), "icons/icon.png must be square.");

const icoPath = resolve(root, "src-tauri/icons/icon.ico");
const ico = readFileSync(icoPath);
requireValue(ico.length >= 22 && ico.readUInt16LE(0) === 0 && ico.readUInt16LE(2) === 1, "icons/icon.ico has an invalid ICO header.");
if (ico.length >= 22) {
  const count = ico.readUInt16LE(4);
  requireValue(count >= 4 && 6 + count * 16 <= ico.length, "icons/icon.ico must contain multiple valid icon sizes.");
  for (let index = 0; index < count; index += 1) {
    const offset = 6 + index * 16;
    const size = ico.readUInt32LE(offset + 8);
    const imageOffset = ico.readUInt32LE(offset + 12);
    requireValue(size > 0 && imageOffset + size <= ico.length, `icons/icon.ico entry ${index + 1} points outside the file.`);
  }
}
requireValue(statSync(icoPath).size === ico.length, "Could not read the complete Windows icon file.");

if (failures.length) {
  console.error("Release verification failed:");
  for (const failure of failures) console.error(`- ${failure}`);
  process.exitCode = 1;
} else {
  console.log(`Release source contract passed for v${tauri.version}.`);
  console.log("- Synchronized application versions");
  console.log("- NSIS per-user bundle and WebView2 bootstrapper setup");
  console.log("- Existing PNG and multi-resolution ICO assets");
  console.log("- Signing and updater artifact generation remain disabled");
}
