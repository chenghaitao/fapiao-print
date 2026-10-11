#!/usr/bin/env node
/**
 * 一键全量构建脚本 — 产出 4 个产物:
 *   1. 发票酱_x64-setup.exe          轻量版安装包 (NSIS)
 *   2. 发票酱_x64_绿色版.exe         轻量版绿色便携 (单文件 exe，无需 zip)
 *   3. 发票酱_x64_OCR版-setup.exe    OCR 版安装包 (NSIS)
 *   4. 发票酱_x64_OCR绿色版.zip      OCR 版绿色便携 (exe + models/)
 *
 * 另在仓库根目录写 release-assets.json：4 个产物的「中文说明 / 中文文件名 /
 * Release 用的 ASCII 别名」清单（同源生成，CI 读取后复制别名上传并渲染对照表）。
 *
 * 用法: node scripts/build-all.js
 *        npm run build:all
 */

const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');

// ─── 配置 ───────────────────────────────────────────
const ROOT = path.resolve(__dirname, '..');
const pkg = require(path.join(ROOT, 'package.json'));
const VERSION = pkg.version;
const PRODUCT_NAME = '发票酱';
const EXE_NAME = 'ticketchan.exe';           // Cargo 编译出的二进制名
const ARCH = 'x64';

const TARGET_RELEASE = path.join(ROOT, 'src-tauri', 'target', 'release');
const BUNDLE_NSIS = path.join(TARGET_RELEASE, 'bundle', 'nsis');
const MODELS_SRC = path.join(ROOT, 'src-tauri', 'models');
const DIST = path.join(ROOT, 'dist');

// 最终产物文件名（统一放在 dist/ 根目录）
const FINAL_FILES = {
  lwInstaller:  `${PRODUCT_NAME}_${VERSION}_${ARCH}-setup.exe`,
  lwPortable:   `${PRODUCT_NAME}_${VERSION}_${ARCH}_绿色版.exe`,
  ocrInstaller: `${PRODUCT_NAME}_${VERSION}_${ARCH}_OCR版-setup.exe`,
  ocrPortable:  `${PRODUCT_NAME}_${VERSION}_${ARCH}_OCR绿色版.zip`,
};

// Release 上传用的 ASCII 别名。GitHub 的 Release 资产名不支持非 ASCII 字符
// （平台限制：服务端把中文替换成「.」，gh CLI issue #8580 已确认 webui 同样如此），
// 用中文名上传只会得到 `_2.6.x_x64-.exe` 这类残缺名，所以上传时改用 ASCII 名。
// 命名沿用 v2.6.5 的既定约定 `TicketChan_<版本>_x64[-_ocr][-_portable]`（见 CHANGELOG）——
// 「TicketChan」是内部代码名（Cargo 包 / 可执行文件都叫它），程序对用户的显示名仍是「发票酱」。
// 本地 dist/ 保留中文名供分发，两者关系写进 release-assets.json 与 Release 说明。
const ASCII_PRODUCT = 'TicketChan';
const ASCII_RULES = [
  [PRODUCT_NAME, ASCII_PRODUCT],   // 发票酱 → TicketChan
  ['OCR绿色版', 'ocr-portable'],
  ['OCR版', 'ocr'],
  ['绿色版', 'portable'],
];

/**
 * 把中文产物名转成等价的 ASCII 别名
 * 兜底剔除任何残留非 ASCII 字符，保证结果一定是纯 ASCII（CI 断言依赖这一点）
 * @param {string} name 中文产物文件名
 * @returns {string} ASCII 别名
 */
function toAsciiName(name) {
  let out = name;
  for (const [from, to] of ASCII_RULES) out = out.split(from).join(to);
  out = out.replace(/[^\x20-\x7e]/g, '');          // 兜底：剔除残留非 ASCII
  out = out.replace(/__+/g, '_').replace(/^[_\-.]+/, '');
  return out;
}

// 产物清单（唯一来源）：构建汇总、release-assets.json、CI 上传都从这里取
const ARTIFACTS = [
  { label: '轻量版安装包',   file: FINAL_FILES.lwInstaller },
  { label: '轻量版绿色便携', file: FINAL_FILES.lwPortable },
  { label: 'OCR 版安装包',   file: FINAL_FILES.ocrInstaller },
  { label: 'OCR 版绿色便携', file: FINAL_FILES.ocrPortable },
].map(a => ({ ...a, ascii: toAsciiName(a.file) }));

// ─── 工具函数 ───────────────────────────────────────
function run(cmd, opts = {}) {
  console.log(`\n\x1b[36m▶ ${cmd}\x1b[0m`);
  execSync(cmd, { stdio: 'inherit', cwd: ROOT, ...opts });
}

function sizeMB(filepath) {
  const stat = fs.statSync(filepath);
  return (stat.size / 1024 / 1024).toFixed(1);
}

function copyFile(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
  console.log(`  ✓ ${path.basename(dest)} (${sizeMB(dest)} MB)`);
}

/**
 * 清空 NSIS bundle 目录，避免旧构建残留的安装包被误选
 */
function cleanNsisBundle() {
  if (fs.existsSync(BUNDLE_NSIS)) {
    for (const f of fs.readdirSync(BUNDLE_NSIS)) {
      fs.unlinkSync(path.join(BUNDLE_NSIS, f));
    }
  }
}

/**
 * 在 NSIS bundle 目录中查找安装包文件
 * 优先匹配当前版本号，兜底取第一个
 * CI 环境可能因编码问题丢失中文，所以用 glob 而非硬编码文件名
 * @returns {string} 找到的安装包完整路径
 */
function findNsisInstaller(label) {
  if (!fs.existsSync(BUNDLE_NSIS)) {
    throw new Error(`NSIS bundle 目录不存在: ${BUNDLE_NSIS}`);
  }
  const files = fs.readdirSync(BUNDLE_NSIS).filter(f => f.endsWith('-setup.exe'));
  if (files.length === 0) {
    throw new Error(`在 ${BUNDLE_NSIS} 中未找到 *-setup.exe (${label})`);
  }
  // 优先选文件名包含当前版本号的
  let found;
  const versionMatch = files.filter(f => f.includes(VERSION));
  if (versionMatch.length > 0) {
    found = versionMatch[0];
  } else {
    found = files[0];
    if (files.length > 1) {
      console.log(`  ⚠ 未找到包含 v${VERSION} 的安装包，使用第一个: ${found}`);
    }
  }
  const fullPath = path.join(BUNDLE_NSIS, found);
  // 如果文件名不含中文（CI 编码问题），提示用户
  if (!found.includes(PRODUCT_NAME)) {
    console.log(`  ⚠ NSIS 产物文件名含中文丢失: ${found}`);
    console.log(`  → 将重命名为: ${FINAL_FILES[label]}`);
  } else {
    console.log(`  ✓ 找到 ${label} 安装包: ${found} (${sizeMB(fullPath)} MB)`);
  }
  return fullPath;
}

/**
 * 检查 OCR 模型文件是否存在
 */
function verifyModels() {
  const requiredModels = [
    'PP-OCRv6_small_det.mnn',
    'PP-OCRv6_small_rec.mnn',
    'ppocr_keys_v6_small.txt',
  ];
  if (!fs.existsSync(MODELS_SRC)) {
    throw new Error(`OCR 模型目录不存在: ${MODELS_SRC}`);
  }
  for (const model of requiredModels) {
    const p = path.join(MODELS_SRC, model);
    if (!fs.existsSync(p)) {
      throw new Error(`OCR 模型文件缺失: ${p}`);
    }
  }
  const totalSize = requiredModels.reduce((sum, m) => {
    return sum + fs.statSync(path.join(MODELS_SRC, m)).size;
  }, 0);
  console.log(`  ✓ OCR 模型验证通过 (${(totalSize / 1024 / 1024).toFixed(1)} MB)`);
}

/**
 * 用 PowerShell Compress-Archive 创建 zip
 * 内部目录名使用 ASCII 安全名（避免 CI 编码问题），zip 文件名使用中文
 * @param {string} zipPath       输出 zip 路径（中文文件名）
 * @param {string[]} items       要打包的文件/目录路径
 * @param {string} innerDirName  zip 内一级目录名（ASCII 安全）
 */
function createZip(zipPath, items, innerDirName) {
  // 先删掉已存在的 zip（Compress-Archive 不允许覆盖）
  if (fs.existsSync(zipPath)) fs.unlinkSync(zipPath);

  // 在临时目录中组装好结构，再整目录打包
  const tmpDir = path.join(DIST, `_zip_tmp_${Date.now()}`);
  const contentDir = path.join(tmpDir, innerDirName);
  fs.mkdirSync(contentDir, { recursive: true });

  for (const item of items) {
    const basename = path.basename(item);
    const dest = path.join(contentDir, basename);
    if (fs.statSync(item).isDirectory()) {
      copyDirRecursive(item, dest);
    } else {
      fs.copyFileSync(item, dest);
    }
  }

  // PowerShell Compress-Archive
  // 设置 UTF-8 编码确保中文路径在 CI 环境也正确
  const psCmd = `chcp 65001 | Out-Null; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Compress-Archive -Path '${contentDir}' -DestinationPath '${zipPath}' -Force`;
  run(`powershell -NoProfile -Command "${psCmd}"`, { cwd: ROOT });

  // 清理临时目录
  fs.rmSync(tmpDir, { recursive: true, force: true });
  console.log(`  ✓ ${path.basename(zipPath)} (${sizeMB(zipPath)} MB)`);
}

function copyDirRecursive(src, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const srcPath = path.join(src, entry.name);
    const destPath = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      copyDirRecursive(srcPath, destPath);
    } else {
      fs.copyFileSync(srcPath, destPath);
    }
  }
}

// ─── 主流程 ─────────────────────────────────────────
async function main() {
  const t0 = Date.now();
  console.log(`\n${'═'.repeat(60)}`);
  console.log(`  ${PRODUCT_NAME} v${VERSION} — 全量构建`);
  console.log(`${'═'.repeat(60)}`);

  // 清理 dist
  fs.rmSync(DIST, { recursive: true, force: true });
  fs.mkdirSync(DIST, { recursive: true });

  // 临时 staging 目录（存放 exe 副本，打包后可删）
  const staging = path.join(DIST, '_staging');

  // ─── Step 1: 轻量版编译 ─────────────────────────
  console.log(`\n${'─'.repeat(60)}`);
  console.log('  [1/4] 编译轻量版 (无 OCR)...');
  console.log(`${'─'.repeat(60)}`);
  cleanNsisBundle();
  run('npx tauri build');

  // 发现 NSIS 安装包，复制到 dist/ 根目录并重命名为正确中文名
  console.log('\n📦 保存轻量版产物...');
  const lwInstallerSrc = findNsisInstaller('lwInstaller');
  copyFile(lwInstallerSrc, path.join(DIST, FINAL_FILES.lwInstaller));

  // 保存 exe 副本到 staging（供绿色版打包用）
  const lwExe = path.join(TARGET_RELEASE, EXE_NAME);
  fs.mkdirSync(staging, { recursive: true });
  const lwExeCopy = path.join(staging, 'lightweight.exe');
  fs.copyFileSync(lwExe, lwExeCopy);
  console.log(`  ✓ 轻量版 exe (${sizeMB(lwExe)} MB)`);

  // ─── Step 2: OCR 版编译 ─────────────────────────
  console.log(`\n${'─'.repeat(60)}`);
  console.log('  [2/4] 编译 OCR 版 (含 PP-OCRv6)...');
  console.log(`${'─'.repeat(60)}`);
  cleanNsisBundle();
  run('npx tauri build --features ocr --config src-tauri/tauri.ocr.conf.json');

  // 发现 NSIS 安装包，复制到 dist/ 根目录
  console.log('\n📦 保存 OCR 版产物...');
  const ocrInstallerSrc = findNsisInstaller('ocrInstaller');
  copyFile(ocrInstallerSrc, path.join(DIST, FINAL_FILES.ocrInstaller));

  // 保存 exe 副本到 staging
  const ocrExe = path.join(TARGET_RELEASE, EXE_NAME);
  const ocrExeCopy = path.join(staging, EXE_NAME);
  fs.copyFileSync(ocrExe, ocrExeCopy);
  console.log(`  ✓ OCR 版 exe (${sizeMB(ocrExe)} MB)`);

  // ─── Step 3: 绿色便携版打包 ─────────────────────
  console.log(`\n${'─'.repeat(60)}`);
  console.log('  [3/4] 打包绿色便携版...');
  console.log(`${'─'.repeat(60)}`);

  // 轻量版绿色便携: 直接用单文件 exe（无额外依赖，不需要打 zip）
  console.log('\n📦 轻量版绿色便携 (单文件 exe)...');
  copyFile(lwExeCopy, path.join(DIST, FINAL_FILES.lwPortable));

  // OCR 版绿色便携: exe + models/ (先验证模型)
  console.log('\n📦 OCR 版绿色便携 (exe + models/)...');
  verifyModels();
  createZip(
    path.join(DIST, FINAL_FILES.ocrPortable),
    [ocrExeCopy, MODELS_SRC],
    `${PRODUCT_NAME}_${VERSION}_OCR绿色版`
  );

  // 清理 staging
  fs.rmSync(staging, { recursive: true, force: true });

  // ─── Step 4: 汇总 ────────────────────────────────
  console.log(`\n${'─'.repeat(60)}`);
  console.log('  [4/4] 产物汇总');
  console.log(`${'─'.repeat(60)}\n`);

  const artifacts = ARTIFACTS;

  let allOk = true;
  for (const a of artifacts) {
    const fullPath = path.join(DIST, a.file);
    if (fs.existsSync(fullPath)) {
      console.log(`  ✅ ${a.label}: ${a.file} (${sizeMB(fullPath)} MB)`);
      console.log(`     └ Release 别名: ${a.ascii}`);
    } else {
      console.log(`  ❌ ${a.label}: 未找到! (${a.file})`);
      allOk = false;
    }
  }

  // 写 Release 清单：CI 据此把产物复制成 ASCII 别名上传，并渲染「说明 ↔ 文件名」对照表
  const manifestPath = path.join(ROOT, 'release-assets.json');
  fs.writeFileSync(manifestPath, JSON.stringify({
    version: VERSION,
    product: PRODUCT_NAME,
    generatedAt: new Date().toISOString(),
    assets: artifacts.map(a => ({
      label: a.label,
      file: a.file,
      ascii: a.ascii,
      size: fs.existsSync(path.join(DIST, a.file)) ? fs.statSync(path.join(DIST, a.file)).size : null,
    })),
  }, null, 2) + '\n', 'utf8');
  console.log(`\n📄 Release 清单: ${manifestPath}`);

  const elapsed = ((Date.now() - t0) / 1000 / 60).toFixed(1);
  console.log(`\n⏱  总耗时: ${elapsed} 分钟`);
  console.log(`\n产物目录: ${DIST}\n`);

  if (!allOk) {
    console.error('\n❌ 部分产物缺失，请检查构建日志!');
    process.exit(1);
  }
}

main().catch(err => {
  console.error('\n❌ 构建失败:', err.message);
  process.exit(1);
});
