// 主窗口前端 invoke 的 Tauri 命令必须在 ACL 里有权限。
//
// 教训（v0.0.85）：v0.0.83 加了 clear_webhome_data 命令、v0.0.84 又扩了
// 它的清理范围，但 capabilities/default.json 里一直没有
// allow-clear-webhome-data，真机上点「清理全部缓存」直接报
// "Command clear_webhome_data not allowed by ACL"，清理一次都没跑成。
// cargo check 与 tsc 都验不出来——权限白名单只在运行时由 Tauri 核对。
// 以后再加命令，跑这项检查就能发现漏白名单。
//
// 做法：扫 src/ 里所有 invoke("cmd") 字面量调用（pluginCommand 走的
// hpp_manage 是一次 invoke，不展开插件命令），把命令名映射成权限名
// （下划线变连字符、加 allow- 前缀；plugin:app|exit 这类核心插件命令
// 映射成 core:app:allow-exit），断言每个权限既在
// capabilities/default.json 的 permissions 里，又在
// permissions/app.toml 里有定义。webhome 窗口的桥命令
// （webhome_call / webhome_close）由远端页面经 fm 桥调、不在 src/ 里，
// 由 capabilities/webhome.json 覆盖，不在本检查范围。
//
// 跑法：node scripts/check-acl.mjs（package.json 的 check:acl）。
// 负向测试不用改真文件：CHECK_ACL_CAPABILITY / CHECK_ACL_PERMISSIONS
// 可指向临时文件，例如去掉 allow-clear-webhome-data 后的 default.json
// 副本，检查应失败并点名缺失的权限。
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const srcDir = process.env.CHECK_ACL_SRC || join(root, "src");
const capabilityPath =
  process.env.CHECK_ACL_CAPABILITY ||
  join(root, "src-tauri", "capabilities", "default.json");
const permissionsPath =
  process.env.CHECK_ACL_PERMISSIONS ||
  join(root, "src-tauri", "permissions", "app.toml");

function walk(dir, out) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p, out);
    else if (/\.(ts|tsx)$/.test(name)) out.push(p);
  }
  return out;
}

// invoke("cmd") / invoke('cmd') / invoke<T>("cmd")：只认字符串字面量。
const INVOKE_RE = /invoke\s*(?:<[^>]*>\s*)?\(\s*["']([^"']+)["']/g;
// 任何 invoke( 出现都必须能解析成字面量，否则是动态命令名、无法核对。
const INVOKE_ANY_RE = /invoke\s*\(/g;

const commands = new Map(); // command -> "file:line"
const unverifiable = [];
for (const file of walk(srcDir, [])) {
  const text = readFileSync(file, "utf-8");
  const lines = text.split("\n");
  const literalLines = new Set();
  for (const m of text.matchAll(INVOKE_RE)) {
    const line = text.slice(0, m.index).split("\n").length;
    literalLines.add(line);
    if (!commands.has(m[1])) commands.set(m[1], `${file}:${line}`);
  }
  for (const m of text.matchAll(INVOKE_ANY_RE)) {
    const line = text.slice(0, m.index).split("\n").length;
    if (!literalLines.has(line))
      unverifiable.push(`${file}:${line}: ${lines[line - 1].trim()}`);
  }
}

function permissionFor(cmd) {
  const plugin = cmd.match(/^plugin:([a-z]+)\|([a-z0-9_]+)$/);
  if (plugin) return `core:${plugin[1]}:allow-${plugin[2].replace(/_/g, "-")}`;
  return `allow-${cmd.replace(/_/g, "-")}`;
}

const capability = JSON.parse(readFileSync(capabilityPath, "utf-8"));
const allowed = new Set(capability.permissions || []);
const defined = new Set(
  [...readFileSync(permissionsPath, "utf-8").matchAll(/^identifier\s*=\s*"([^"]+)"/gm)].map(
    (m) => m[1],
  ),
);

const missing = [];
for (const [cmd, where] of [...commands.entries()].sort()) {
  const perm = permissionFor(cmd);
  const inCap = allowed.has(perm);
  // core:* 是 Tauri 核心插件自带的权限（core:app、core:window 等），
  // 不在 app.toml 里定义；只核对它在 default.json 里。
  const inDef = perm.startsWith("core:") || defined.has(perm);
  console.log(`${inCap && inDef ? "OK  " : "MISS"} ${cmd} -> ${perm} (${where})`);
  if (!inCap || !inDef)
    missing.push(`${cmd} -> ${perm} (${where}): ${[
      !inCap ? "default.json 缺" : "",
      !inDef ? "app.toml 缺定义" : "",
    ]
      .filter(Boolean)
      .join("、")}`);
}

let failed = false;
if (unverifiable.length) {
  failed = true;
  console.error("\n无法核对的动态 invoke 调用（命令名不是字符串字面量）：");
  for (const u of unverifiable) console.error(`  ${u}`);
}
if (missing.length) {
  failed = true;
  console.error("\n缺 ACL 权限：");
  for (const m of missing) console.error(`  ${m}`);
}
if (failed) {
  console.error(`\nFAIL: ${commands.size} 个命令，${missing.length} 个缺权限，${unverifiable.length} 个无法核对`);
  process.exit(1);
}
console.log(`\nPASS: ${commands.size} 个命令，权限全在 default.json 且 app.toml 有定义`);
