// Read the Cast firewall command lines out of src-tauri/windows/installer.nsi.
// That file is the only copy. NSIS `$\"` becomes a plain quote, `$$` becomes `$`,
// then ${defines}, $INSTDIR, and $SYSDIR are substituted the way the installer does.
//
//   node scripts/cast-firewall-commands.mjs --check
//   node scripts/cast-firewall-commands.mjs --apply
//
// --check proves the runtime text has plain inner quotes and no \" .
// --apply (Windows, already elevated) runs those exact command lines.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const nsiPath = path.join(root, 'src-tauri', 'windows', 'installer.nsi');
const confPath = path.join(root, 'src-tauri', 'tauri.conf.json');

function fail(message) {
  throw new Error(message);
}

function parseDefines(text) {
  const defs = {};
  for (const line of text.split(/\r?\n/)) {
    const match = line.match(/^\s*!define\s+([A-Za-z0-9_]+)\s+"(.*)"\s*$/);
    if (match) defs[match[1]] = match[2];
  }
  return defs;
}

function functionBody(text, header) {
  const start = text.indexOf(header);
  if (start < 0) fail(`missing ${header} in installer.nsi`);
  const end = text.indexOf('FunctionEnd', start);
  if (end < 0) fail(`missing FunctionEnd after ${header}`);
  return text.slice(start, end);
}

function extractQuoted(body, label) {
  const marker = "StrCpy $R9 '";
  const at = body.indexOf(marker);
  if (at < 0) fail(`missing ${marker} in ${label}`);
  const from = at + marker.length;
  const end = body.indexOf("'", from);
  if (end < 0) fail(`unclosed StrCpy in ${label}`);
  return body.slice(from, end);
}

function mainBinaryName() {
  const conf = parseJsonStrict(fs.readFileSync(confPath, 'utf8'));
  const name = conf.mainBinaryName;
  if (!name) fail('tauri.conf.json has no mainBinaryName');
  return name;
}

function parseJsonStrict(text, label = 'tauri.conf.json') {
  const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
  let i = 0;
  function line() {
    let n = 1;
    for (let k = 0; k < i && k < src.length; k++) if (src[k] === '\n') n++;
    return n;
  }
  function failAt(message) {
    fail(`${label}:${line()} ${message}`);
  }
  function skipWs() {
    while (i < src.length && /\s/.test(src[i])) i++;
  }
  function parseString() {
    if (src[i] !== '"') failAt('expected a string');
    i++;
    let out = '';
    while (i < src.length) {
      const c = src[i++];
      if (c === '"') return out;
      if (c !== '\\') {
        out += c;
        continue;
      }
      const e = src[i++];
      if (e === '"' || e === '\\' || e === '/') out += e;
      else if (e === 'b') out += '\b';
      else if (e === 'f') out += '\f';
      else if (e === 'n') out += '\n';
      else if (e === 'r') out += '\r';
      else if (e === 't') out += '\t';
      else if (e === 'u') {
        const hex = src.slice(i, i + 4);
        if (!/^[0-9a-fA-F]{4}$/.test(hex)) failAt('bad unicode escape');
        out += String.fromCharCode(Number.parseInt(hex, 16));
        i += 4;
      } else failAt('bad escape');
    }
    failAt('unterminated string');
  }
  function parseLiteral() {
    const start = i;
    if (src.startsWith('true', i)) {
      i += 4;
      return true;
    }
    if (src.startsWith('false', i)) {
      i += 5;
      return false;
    }
    if (src.startsWith('null', i)) {
      i += 4;
      return null;
    }
    if (!/[-0-9]/.test(src[i] || '')) failAt(`unexpected ${JSON.stringify(src[i])}`);
    i++;
    while (i < src.length && /[0-9.eE+-]/.test(src[i])) i++;
    const num = Number(src.slice(start, i));
    if (!Number.isFinite(num)) failAt('bad number');
    return num;
  }
  function parseArray() {
    i++;
    const arr = [];
    skipWs();
    if (src[i] === ']') {
      i++;
      return arr;
    }
    while (true) {
      arr.push(parseValue());
      skipWs();
      if (src[i] === ',') {
        i++;
        continue;
      }
      if (src[i] === ']') {
        i++;
        return arr;
      }
      failAt('expected comma or ]');
    }
  }
  function parseObject() {
    i++;
    const obj = {};
    const seen = new Set();
    skipWs();
    if (src[i] === '}') {
      i++;
      return obj;
    }
    while (true) {
      skipWs();
      if (src[i] !== '"') failAt('expected a key');
      const key = parseString();
      if (seen.has(key)) failAt(`duplicate key ${JSON.stringify(key)}`);
      seen.add(key);
      skipWs();
      if (src[i] !== ':') failAt('expected :');
      i++;
      obj[key] = parseValue();
      skipWs();
      if (src[i] === ',') {
        i++;
        continue;
      }
      if (src[i] === '}') {
        i++;
        return obj;
      }
      failAt('expected comma or }');
    }
  }
  function parseValue() {
    skipWs();
    if (i >= src.length) failAt('unexpected end');
    const c = src[i];
    if (c === '{') return parseObject();
    if (c === '[') return parseArray();
    if (c === '"') return parseString();
    return parseLiteral();
  }
  const value = parseValue();
  skipWs();
  if (i !== src.length) failAt('trailing data');
  return value;
}

function assertDuplicateDetector() {
  let message = '';
  try {
    parseJsonStrict('{"resources":{"a":"b"},"resources":["c"]}', 'sample');
  } catch (error) {
    message = error.message;
  }
  if (!message.includes('duplicate key "resources"')) {
    fail(`duplicate key detector missed a repeated key: ${message}`);
  }
  const nested = parseJsonStrict('{"a":{"ext":"jpg"},"b":{"ext":"png"}}', 'sample');
  if (nested.a.ext !== 'jpg' || nested.b.ext !== 'png') fail('strict parser dropped nested keys');
}

function resourceRelpath(value) {
  const parts = [];
  for (const part of String(value).split(/[\\/]/)) {
    if (!part || part === '.') continue;
    parts.push(part === '..' ? '_up_' : part);
  }
  return parts.join('/');
}

function walkFiles(dir) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walkFiles(full));
    else if (entry.isFile()) out.push(full);
  }
  return out;
}

function resolveBundleResources(conf, tauriDir) {
  const resources = conf?.bundle?.resources;
  if (!resources || Array.isArray(resources) || typeof resources !== 'object') {
    fail('bundle.resources must be one map of source path to install target');
  }
  const resolved = [];
  for (const [pattern, destRaw] of Object.entries(resources)) {
    const dest = resourceRelpath(destRaw);
    if (pattern.includes('*')) {
      const matches = fs.globSync(pattern, { cwd: tauriDir });
      const files = matches.filter((rel) => fs.statSync(path.join(tauriDir, rel)).isFile());
      if (!files.length) fail(`resource glob matched nothing: ${pattern}`);
      for (const rel of files) {
        const name = path.basename(rel);
        resolved.push({
          source: rel.replaceAll('\\', '/'),
          target: dest ? `${dest}/${name}` : name,
        });
      }
    } else {
      const abs = path.resolve(tauriDir, pattern);
      if (!fs.existsSync(abs)) fail(`resource not found: ${pattern}`);
      if (fs.statSync(abs).isDirectory()) {
        for (const file of walkFiles(abs)) {
          const rel = path.relative(abs, file).replaceAll('\\', '/');
          resolved.push({
            source: path.relative(tauriDir, file).replaceAll('\\', '/'),
            target: dest ? `${dest}/${rel}` : rel,
          });
        }
      } else {
        resolved.push({
          source: pattern.replaceAll('\\', '/'),
          target: dest || path.basename(pattern),
        });
      }
    }
  }
  return resolved;
}

function assertTauriBundle() {
  assertDuplicateDetector();
  const conf = parseJsonStrict(fs.readFileSync(confPath, 'utf8'));
  const resolved = resolveBundleResources(conf, path.dirname(confPath));
  const targets = resolved.map((item) => item.target);
  if (!targets.includes('cast-fw-check.ps1')) {
    fail(`resolved bundle resources do not install cast-fw-check.ps1 beside the exe: ${targets.join(', ')}`);
  }
  const ps1 = resolved.find((item) => item.target === 'cast-fw-check.ps1');
  if (ps1.source !== 'windows/cast-fw-check.ps1') fail(`cast-fw-check.ps1 source is ${ps1.source}`);
  for (const name of ['sample-still.jpg', 'sample-video.mp4']) {
    const target = `resources/cast-spike/${name}`;
    if (!targets.includes(target)) fail(`resolved bundle resources missing ${target}`);
  }
  console.log(`CAST_FIREWALL_RESOURCES=${targets.join(',')}`);
  const nsi = fs.readFileSync(nsiPath, 'utf8');
  const fileLoops = nsi.split('File /a "/oname={{this.[1]}}"').length - 1;
  if (fileLoops !== 1) fail(`expected 1 resources File loop, found ${fileLoops}`);
  if (!nsi.includes('Delete "$INSTDIR\\\\{{this.[1]}}"')) {
    fail('uninstaller resources loop does not delete bundled files');
  }
  const fileAt = nsi.indexOf('File /a "/oname={{this.[1]}}"');
  const callAt = nsi.indexOf('Call CastFirewallInstall');
  if (fileAt < 0 || callAt < 0 || fileAt > callAt) {
    fail('resources File loop does not run before CastFirewallInstall');
  }
}

function nsisRuntime(raw, vars) {
  let out = '';
  for (let i = 0; i < raw.length; i++) {
    if (raw.startsWith('$\\"', i)) {
      out += '"';
      i += 2;
      continue;
    }
    out += raw[i];
  }
  if (out.includes('\\"')) fail(`runtime command still contains \\" : ${out}`);
  out = out.replace(/\$\{([A-Za-z0-9_]+)\}/g, (all, name) => {
    if (!Object.prototype.hasOwnProperty.call(vars, name)) fail(`undefined NSIS define \${${name}}`);
    return vars[name];
  });
  out = out.replaceAll('$INSTDIR', vars.INSTDIR);
  out = out.replaceAll('$SYSDIR', vars.SYSDIR);
  if (out.includes('${') || out.includes('$INSTDIR') || out.includes('$SYSDIR') || out.includes('$\\')) {
    fail(`unresolved NSIS token in: ${out}`);
  }
  out = out.replaceAll('$$', '$');
  return out;
}

function loadCommands(instDir, sysDir) {
  const text = fs.readFileSync(nsiPath, 'utf8');
  const defs = parseDefines(text);
  const binary = mainBinaryName();
  const definedBinary = defs.MAINBINARYNAME;
  if (definedBinary && !definedBinary.includes('{{') && definedBinary !== binary) {
    fail(`MAINBINARYNAME ${definedBinary} does not match tauri.conf.json ${binary}`);
  }
  const vars = {
    ...defs,
    MAINBINARYNAME: binary,
    INSTDIR: instDir,
    SYSDIR: sysDir,
  };
  const installParams = nsisRuntime(extractQuoted(functionBody(text, 'Function CastFirewallInstall'), 'install'), vars);
  const uninstallParams = nsisRuntime(
    extractQuoted(functionBody(text, 'Function un.CastFirewallUninstall'), 'uninstall'),
    vars,
  );
  const installBody = functionBody(text, 'Function CastFirewallInstall').replace(/\r\n/g, '\n');
  const prechecks = [...installBody.matchAll(/nsExec::ExecToStack '([^']*)'/g)].map((match) =>
    nsisRuntime(match[1], vars),
  );
  const matchAt = installBody.indexOf('status=match');
  const runasAt = installBody.indexOf('ExecShell "runas"');
  const between = matchAt >= 0 && runasAt > matchAt ? installBody.slice(matchAt, runasAt) : '';
  const returnAt = between.search(/\bReturn\b/);
  return {
    defs,
    binary,
    installParams,
    uninstallParams,
    prechecks,
    sysDir,
    instDir,
    skipsElevation: matchAt >= 0 && runasAt > matchAt && returnAt >= 0,
  };
}

function assertPlainQuotes(label, params, instDir, binary) {
  if (!params.startsWith('/c "')) fail(`${label} does not start with /c "`);
  if (!params.endsWith('"')) fail(`${label} does not end with the outer quote`);
  if (params.includes('\\"')) fail(`${label} still has a backslash-quote`);
  const command = params.slice('/c "'.length, -1);
  if (command.startsWith('"')) {
    fail(`${label} has an extra quote after the /c opener: ${params}`);
  }
  const quotes = [...command].filter((ch) => ch === '"').length;
  if (quotes % 2 !== 0) fail(`${label} has unbalanced inner quotes (${quotes})`);
  const exe = `${instDir}\\${binary}.exe`;
  const result = `${instDir}\\cast-firewall.new`;
  for (const piece of [
    'name="SlideX Cast media (Private)"',
    'name="SlideX Cast mDNS (Private)"',
    `program="${exe}"`,
  ]) {
    if (label === 'uninstall' && piece.startsWith('program=')) continue;
    if (!params.includes(piece)) fail(`${label} missing ${piece}`);
  }
  if (label === 'install') {
    if (!params.includes('localport=47200-47215')) fail('install missing TCP range');
    if (!params.includes('localport=5353')) fail('install missing UDP port');
    if (!params.includes('profile=private')) fail('install missing private profile');
    const localSubnet = params.split('remoteip=localsubnet').length - 1;
    if (localSubnet !== 2) fail(`install should set remoteip=localsubnet on both rules, found ${localSubnet}`);
    if (!params.includes(`(echo status=added>"${result}")`)) fail('install missing status=added redirect');
    if (!params.includes(`(echo status=failed>"${result}")`)) fail('install missing status=failed redirect');
  } else {
    if (!params.includes(`echo status=removed>"${result}"`)) fail('uninstall missing status=removed redirect');
  }
  return command;
}

function printCommand(label, sysDir, params) {
  const line = `${sysDir}\\cmd.exe ${params}`;
  console.log(`CAST_FIREWALL_${label}_CMDLINE=${line}`);
  return line;
}

function runCmd(sysDir, params) {
  const exe = path.join(sysDir, 'cmd.exe');
  const result = spawnSync(exe, [params], {
    windowsVerbatimArguments: true,
    stdio: 'inherit',
  });
  if (result.error) fail(result.error.message);
  if (result.status !== 0) fail(`cmd exited ${result.status}`);
}

const ps1Path = path.join(root, 'src-tauri', 'windows', 'cast-fw-check.ps1');

function assertLineLimit(text, binaryName) {
  const defs = parseDefines(text);
  defs.MAINBINARYNAME = binaryName;
  const lines = text.split(/\r?\n/);
  let max = 0;
  lines.forEach((line, index) => {
    let resolved = line;
    for (let pass = 0; pass < 4; pass++) {
      resolved = resolved.replace(/\$\{([A-Za-z0-9_]+)\}/g, (all, name) => {
        if (!Object.prototype.hasOwnProperty.call(defs, name)) return all;
        const value = defs[name] === '{{main_binary_name}}' ? binaryName : defs[name];
        return value;
      });
    }
    resolved = resolved.replaceAll('{{main_binary_name}}', binaryName);
    if (resolved.length > max) max = resolved.length;
    if (resolved.length > 1000) {
      fail(
        `installer.nsi:${index + 1} is ${resolved.length} chars after !define substitution (limit 1000): ${resolved.slice(0, 180)}`,
      );
    }
  });
  console.log(`CAST_FIREWALL_NSIS_MAX_LINE=${max}`);
}

function assertFlagOrder(label, text) {
  const flags = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File';
  if (!text.includes(flags)) fail(`${label} missing ${flags}`);
  const ps = text.toLowerCase();
  if (!ps.includes('\\system32\\windowspowershell\\v1.0\\powershell.exe')) {
    fail(`${label} does not use the System32 PowerShell path`);
  }
  if (/(^|[^\\\w])powershell\.exe/i.test(text.replace(/\\System32\\WindowsPowerShell\\v1\.0\\powershell\.exe/gi, ''))) {
    fail(`${label} also invokes powershell.exe from PATH`);
  }
}

function assertAppCallSite() {
  const cast = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'cast.rs'), 'utf8');
  const start = cast.indexOf('fn firewall_powershell_file_args');
  if (start < 0) fail('cast.rs is missing firewall_powershell_file_args');
  const end = cast.indexOf('\nfn ', start + 10);
  const body = cast.slice(start, end);
  const order = ['"-NoProfile"', '"-NonInteractive"', '"-ExecutionPolicy"', '"Bypass"', '"-File"', '"-ExePath"'];
  let pos = 0;
  for (const token of order) {
    const at = body.indexOf(token, pos);
    if (at < 0) fail(`app firewall args missing ${token} after the previous flag`);
    pos = at + token.length;
  }
  if (body.includes('"-Command"')) fail('app firewall check still uses -Command');
  if (body.includes(".replace('\\''")) {
    fail('app firewall check still interpolates the exe path into quotes');
  }
  if (!cast.includes('fn powershell_exe_under_windows_dir')) {
    fail('app does not resolve System32 powershell.exe');
  }
  if (!cast.includes("r\"C:\\O'Brien\\slideshowpro.exe\"") && !cast.includes('O\'Brien')) {
    fail('app firewall test does not pass an apostrophe path as its own argument');
  }
}

function splitPrecheck(commandLine) {
  const match = commandLine.match(
    /^"([^"]+)"\s+-NoProfile\s+-NonInteractive\s+-ExecutionPolicy\s+Bypass\s+-File\s+"([^"]+)"\s+-ExePath\s+"([^"]+)"([\s\S]*)$/,
  );
  if (!match) {
    fail(`pre-check is not a full-path PowerShell -File -ExecutionPolicy Bypass command: ${commandLine}`);
  }
  const exe = match[1];
  if (!exe.toLowerCase().endsWith('\\system32\\windowspowershell\\v1.0\\powershell.exe')) {
    fail(`pre-check exe is not System32 PowerShell: ${exe}`);
  }
  return { exe, script: match[2], exePath: match[3], rest: match[4].trim() };
}

function powershellArgs(commandLine) {
  const parsed = splitPrecheck(commandLine);
  const rest = parsed.rest.length ? parsed.rest.split(/\s+(?=(?:[^"]*"[^"]*")*[^"]*$)/) : [];
  const args = [
    '-NoProfile',
    '-NonInteractive',
    '-ExecutionPolicy',
    'Bypass',
    '-File',
    parsed.script,
    '-ExePath',
    parsed.exePath,
    ...rest.map((part) => part.replace(/^"|"$/g, '')),
  ];
  return { exe: parsed.exe, args };
}

function runPrecheck(commandLine, extraArgs = []) {
  const { exe, args } = powershellArgs(commandLine);
  const result = spawnSync(exe, [...args, ...extraArgs].map(quoteCmd), {
    encoding: 'utf8',
    windowsVerbatimArguments: true,
  });
  return {
    status: result.status,
    text: `${result.stdout || ''}\n${result.stderr || ''}`,
    error: result.error,
  };
}

function quoteCmd(arg) {
  return `"${String(arg).replace(/"/g, '""')}"`;
}

function runFixture(sysDir, exePath, records) {
  const file = path.join(os.tmpdir(), `slidex-fw-${process.pid}-${Math.random().toString(16).slice(2)}.json`);
  fs.writeFileSync(file, JSON.stringify(records));
  try {
    const ps = path.join(sysDir, 'WindowsPowerShell', 'v1.0', 'powershell.exe');
    const result = spawnSync(
      ps,
      [
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        ps1Path,
        '-ExePath',
        exePath,
        '-MediaName',
        'SlideX Cast media (Private)',
        '-MdnsName',
        'SlideX Cast mDNS (Private)',
        '-TcpPorts',
        '47200-47215',
        '-UdpPort',
        '5353',
        '-Fixture',
        file,
      ].map(quoteCmd),
      { encoding: 'utf8', windowsVerbatimArguments: true },
    );
    if (result.error) fail(result.error.message);
    return `${result.stdout || ''}\n${result.stderr || ''}`;
  } finally {
    fs.rmSync(file, { force: true });
  }
}

function ruleRecord(program, profile, remote) {
  const row = (name, protocol, localport) => ({
    name,
    enabled: true,
    direction: 'Inbound',
    action: 'Allow',
    profile,
    program,
    remote,
    protocol,
    localport,
  });
  return [
    row('SlideX Cast media (Private)', 'TCP', '47200-47215'),
    row('SlideX Cast mDNS (Private)', 'UDP', '5353'),
  ];
}

function netshShow(sysDir, ruleName) {
  const exe = path.join(sysDir, 'netsh.exe');
  const result = spawnSync(
    exe,
    ['advfirewall', 'firewall', 'show', 'rule', `name=${ruleName}`, 'verbose'],
    { encoding: 'utf8' },
  );
  const text = `${result.stdout || ''}\n${result.stderr || ''}`;
  return { status: result.status, text };
}

function field(text, name) {
  const match = text.match(new RegExp(`^${name}:\\s*(.*)$`, 'im'));
  return match ? match[1].trim() : '';
}

function assertRule(text, ruleName, exePath, protocol, ports) {
  if (/no rules match/i.test(text)) fail(`rule missing: ${ruleName}\n${text}`);
  const gotName = field(text, 'Rule Name');
  if (gotName !== ruleName) fail(`rule name ${JSON.stringify(gotName)} != ${JSON.stringify(ruleName)}`);
  const profiles = field(text, 'Profiles');
  if (!/private/i.test(profiles) || /public/i.test(profiles)) {
    fail(`${ruleName} profile is ${JSON.stringify(profiles)}, want Private only`);
  }
  const proto = field(text, 'Protocol');
  if (proto.toUpperCase() !== protocol) fail(`${ruleName} protocol ${proto} != ${protocol}`);
  const localPort = field(text, 'LocalPort');
  if (localPort !== ports) fail(`${ruleName} port ${localPort} != ${ports}`);
  const program = field(text, 'Program');
  if (program.toLowerCase() !== exePath.toLowerCase()) {
    fail(`${ruleName} program ${program} != ${exePath}`);
  }
  const action = field(text, 'Action');
  if (!/allow/i.test(action)) fail(`${ruleName} action ${action}`);
  const remote = field(text, 'RemoteIP');
  if (!/local\s*subnet/i.test(remote)) {
    fail(`${ruleName} remote IP ${JSON.stringify(remote)}, want LocalSubnet`);
  }
}

function assertGone(text, ruleName) {
  if (!/no rules match/i.test(text)) fail(`rule still present: ${ruleName}\n${text}`);
}

function readStatus(file) {
  if (!fs.existsSync(file)) fail(`missing ${file}`);
  return fs.readFileSync(file, 'utf8');
}

// Same replace the installer does: if the live file exists, delete it, then
// rename cast-firewall.new onto cast-firewall.txt.
function commitFirewallStatus(dir) {
  const live = path.join(dir, 'cast-firewall.txt');
  const temp = path.join(dir, 'cast-firewall.new');
  if (!fs.existsSync(temp)) fail(`missing ${temp}`);
  if (fs.existsSync(live)) fs.rmSync(live);
  fs.renameSync(temp, live);
  return readStatus(live);
}

function assertSilentSkipsElevation(installBody) {
  const silentAt = installBody.indexOf('IfSilent');
  const elevateAt = installBody.indexOf('cast_fw_elevate:');
  const runasAt = installBody.indexOf('ExecShell "runas"');
  const endAt = installBody.indexOf('cast_fw_install_end:');
  const skippedAt = installBody.indexOf('status=skipped_silent');
  if (silentAt < 0 || elevateAt < 0 || runasAt < 0 || endAt < 0 || skippedAt < 0) {
    fail('silent install path is missing IfSilent, status=skipped_silent, or cast_fw_elevate');
  }
  if (!(silentAt < skippedAt && skippedAt < elevateAt && elevateAt <= runasAt && runasAt < endAt)) {
    fail('silent install must write status=skipped_silent before cast_fw_elevate, and ExecShell must stay after that label');
  }
  const silentLine = installBody.slice(silentAt, installBody.indexOf('\n', silentAt));
  if (!silentLine.includes('cast_fw_elevate')) {
    fail('IfSilent does not send an interactive install to cast_fw_elevate');
  }
  const silentSlice = installBody.slice(silentAt, elevateAt);
  if (/ExecShell|runas/i.test(silentSlice)) fail('silent branch contains an elevation attempt');
  if (!silentSlice.includes('status=nomatch')) fail('silent skip does not check status=nomatch');
  if (!silentSlice.includes('status=check_failed')) {
    fail('silent pre-check with neither match nor nomatch does not write status=check_failed');
  }
  if (!silentSlice.includes('Goto cast_fw_install_end')) {
    fail('/S with no rules can fall through to ExecShell runas');
  }
  const nsi = fs.readFileSync(nsiPath, 'utf8');
  const macroAt = nsi.indexOf('!macro CAST_FW_COMMIT');
  const macroEnd = nsi.indexOf('!macroend', macroAt);
  const macroBody = nsi.slice(macroAt, macroEnd < 0 ? nsi.length : macroEnd);
  const deleteAt = macroBody.indexOf('Delete "$1"');
  const renameAt = macroBody.indexOf('Rename /REBOOTOK "$R8" "$1"');
  if (macroAt < 0 || deleteAt < 0 || renameAt < 0 || !(deleteAt < renameAt) || !installBody.includes('!insertmacro CAST_FW_COMMIT')) {
    fail('CAST_FW_COMMIT must Delete "$1" before Rename /REBOOTOK "$R8" "$1"');
  }
  const head = installBody.slice(0, installBody.indexOf('nsExec::ExecToStack'));
  if (head.includes('Delete "$1"') || head.includes('Delete "$INSTDIR\\cast-firewall.txt"')) {
    fail('CastFirewallInstall deletes cast-firewall.txt before the new status exists');
  }
  console.log('CAST_FIREWALL_SILENT_NO_ELEVATION=ok');
}

// os.tmpdir() on the GitHub runner is an 8.3 path (C:\Users\RUNNER~1\...).
// netsh reports "The application name could not be resolved" for that form,
// and also for a zero-byte file. Use the long path and a real PE image.
function stripDevicePrefix(p) {
  if (p.startsWith('\\\\?\\UNC\\')) return `\\\\${p.slice('\\\\?\\UNC\\'.length)}`;
  if (p.startsWith('\\\\?\\')) return p.slice('\\\\?\\'.length);
  return p;
}

function prepareInstallDir() {
  const candidates = [];
  try {
    candidates.push(path.join(stripDevicePrefix(fs.realpathSync.native(os.tmpdir())), 'SlideX Cast CI'));
  } catch {
    // TEMP can be missing in a stripped environment. The drive root is the fallback.
  }
  candidates.push(`${process.env.SystemDrive || 'C:'}\\SlideX Cast CI`);
  const errors = [];
  for (const dir of candidates) {
    try {
      fs.rmSync(dir, { recursive: true, force: true });
      fs.mkdirSync(dir, { recursive: true });
      const resolved = stripDevicePrefix(fs.realpathSync.native(dir));
      if (!resolved.includes('~')) return resolved;
      errors.push(`${resolved} is still a short path`);
      fs.rmSync(resolved, { recursive: true, force: true });
    } catch (error) {
      errors.push(`${dir}: ${error.message}`);
    }
  }
  fail(`could not create an install dir netsh can resolve\n${errors.join('\n')}`);
}

const apply = process.argv.includes('--apply');
if (apply && process.platform !== 'win32') fail('--apply runs on Windows');
const sysDir = process.env.SystemRoot ? path.join(process.env.SystemRoot, 'System32') : 'C:\\Windows\\System32';
const instDir = apply ? prepareInstallDir() : 'C:\\SlideX Cast CI';

const loaded = loadCommands(instDir, sysDir);
assertPlainQuotes('install', loaded.installParams, instDir, loaded.binary);
assertPlainQuotes('uninstall', loaded.uninstallParams, instDir, loaded.binary);
assertLineLimit(fs.readFileSync(nsiPath, 'utf8'), loaded.binary);
if (!fs.existsSync(ps1Path)) fail(`missing ${ps1Path}`);
const ps1 = fs.readFileSync(ps1Path, 'utf8');
for (const piece of [
  'Get-NetFirewallRule',
  'Get-NetFirewallApplicationFilter',
  'Get-NetFirewallAddressFilter',
  'Get-NetFirewallPortFilter',
  'LocalSubnet',
  'status=match',
  'param(',
]) {
  if (!ps1.includes(piece)) fail(`cast-fw-check.ps1 missing ${piece}`);
}
if (!ps1.includes('return @((Get-Content -Raw -LiteralPath $Fixture | ConvertFrom-Json))')) {
  fail('cast-fw-check.ps1 must enumerate ConvertFrom-Json arrays for Windows PowerShell 5.1');
}
if (!loaded.skipsElevation) {
  fail('CastFirewallInstall does not return before ExecShell when the pre-check reports status=match');
}
assertSilentSkipsElevation(functionBody(fs.readFileSync(nsiPath, 'utf8'), 'Function CastFirewallInstall'));
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'slidex-fw-'));
  try {
    const live = path.join(dir, 'cast-firewall.txt');
    const temp = path.join(dir, 'cast-firewall.new');
    fs.writeFileSync(live, 'status=present\r\n');
    fs.writeFileSync(temp, 'status=check_failed\r\n');
    const body = commitFirewallStatus(dir);
    if (!body.includes('status=check_failed')) fail(`commit kept the old status:\n${body}`);
    if (fs.existsSync(temp)) fail('commit left cast-firewall.new in place');
    if (fs.existsSync(path.join(dir, 'cast-firewall.replacing'))) {
      fail('commit still stages cast-firewall.replacing');
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
if (!fs.readFileSync(nsiPath, 'utf8').includes('Delete "$INSTDIR\\cast-fw-check.ps1"')) {
  fail('uninstaller does not delete cast-fw-check.ps1');
}
if (loaded.prechecks.length !== 1) fail(`expected 1 unelevated PowerShell pre-check, found ${loaded.prechecks.length}`);
const precheck = loaded.prechecks[0];
const psExe = `${sysDir}\\WindowsPowerShell\\v1.0\\powershell.exe`;
if (!precheck.startsWith(`"${psExe}"`)) fail(`pre-check does not use the full PowerShell path: ${precheck}`);
assertFlagOrder('installer pre-check', precheck);
if (!precheck.includes('cast-fw-check.ps1')) fail('pre-check does not run cast-fw-check.ps1');
if (!precheck.includes('-ExePath')) fail('pre-check does not pass -ExePath');
if (/\bnetsh\b/i.test(precheck)) fail('pre-check still shells out to netsh');
if (precheck.includes('-Command')) fail('pre-check still uses -Command');
assertAppCallSite();
assertTauriBundle();
console.log(`CAST_FIREWALL_PRECHECK_CMDLINE=${precheck}`);

printCommand('INSTALL', sysDir, loaded.installParams);
printCommand('UNINSTALL', sysDir, loaded.uninstallParams);

splitPrecheck(precheck);

function assertFixture(label, text, expect) {
  if (!text.includes(expect)) fail(`${label}: expected ${expect} in\n${text}`);
}

if (process.platform === 'win32') {
  const apostrophe = "C:\\O'Brien\\slideshowpro.exe";
  assertFixture('apostrophe path', runFixture(sysDir, apostrophe, ruleRecord(apostrophe, 'Private', 'LocalSubnet')), 'status=match');
  assertFixture('wrong program', runFixture(sysDir, apostrophe, ruleRecord('C:\\Other\\slideshowpro.exe', 'Private', 'LocalSubnet')), 'status=nomatch');
  assertFixture('public only', runFixture(sysDir, apostrophe, ruleRecord(apostrophe, 'Public', 'LocalSubnet')), 'status=nomatch');
  assertFixture('missing localsubnet', runFixture(sysDir, apostrophe, ruleRecord(apostrophe, 'Private', 'Any')), 'status=nomatch');
  console.log('CAST_FIREWALL_PS1_FIXTURES=ok');
}

if (!apply) {
  console.log('cast firewall command check ok');
  process.exit(0);
}

const exePath = path.join(instDir, `${loaded.binary}.exe`);
fs.copyFileSync(path.join(sysDir, 'cmd.exe'), exePath);
fs.copyFileSync(ps1Path, path.join(instDir, 'cast-fw-check.ps1'));
const statusFile = path.join(instDir, 'cast-firewall.txt');

try {
  const first = runPrecheck(loaded.prechecks[0]);
  console.log('--- first install pre-check (no elevation) ---');
  console.log(first.text.trim());
  if (first.error) fail(first.error.message);
  if (!first.text.includes('status=nomatch') && !first.text.includes('status=added')) {
    fail(`first install pre-check should report nomatch before the rules exist:\n${first.text}`);
  }
  const firstState = first.text.includes('status=nomatch') ? 'nomatch' : 'added';
  console.log(`CAST_FIREWALL_FIRST_INSTALL=${firstState}`);
  if (firstState === 'nomatch') {
    fs.writeFileSync(statusFile, 'status=present\r\n');
    fs.writeFileSync(path.join(instDir, 'cast-firewall.new'), 'status=skipped_silent\r\n');
    if (!readStatus(statusFile).includes('status=present')) {
      fail('silent nomatch clobbered cast-firewall.txt before the commit');
    }
    const silentStatus = commitFirewallStatus(instDir);
    if (!silentStatus.includes('status=skipped_silent')) {
      fail(`silent install did not commit status=skipped_silent:\n${silentStatus}`);
    }
    console.log('CAST_FIREWALL_SILENT_INSTALL=skipped_silent');
  }

  fs.writeFileSync(statusFile, 'status=present\r\n');
  runCmd(sysDir, loaded.installParams);
  if (!readStatus(statusFile).includes('status=present')) {
    fail('elevated command replaced cast-firewall.txt before the installer commit');
  }
  const added = commitFirewallStatus(instDir);
  console.log(`CAST_FIREWALL_STATUS_AFTER_INSTALL=${JSON.stringify(added)}`);
  if (!added.includes('status=added')) fail(`cast-firewall.txt did not record status=added:\n${added}`);

  const mediaName = loaded.defs.CAST_FW_MEDIA_NAME;
  const mdnsName = loaded.defs.CAST_FW_MDNS_NAME;
  const media = netshShow(sysDir, mediaName);
  const mdns = netshShow(sysDir, mdnsName);
  console.log('--- show rule media verbose ---');
  console.log(media.text.trim());
  console.log('--- show rule mdns verbose ---');
  console.log(mdns.text.trim());
  assertRule(media.text, mediaName, exePath, 'TCP', loaded.defs.CAST_FW_TCP);
  assertRule(mdns.text, mdnsName, exePath, 'UDP', loaded.defs.CAST_FW_UDP);

  const second = runPrecheck(loaded.prechecks[0]);
  console.log('--- second install pre-check (no elevation) ---');
  console.log(second.text.trim());
  if (second.error) fail(second.error.message);
  if (!second.text.includes('status=match')) {
    fail(`second install would elevate; pre-check did not report status=match:\n${second.text}`);
  }
  console.log('CAST_FIREWALL_SECOND_INSTALL=skip-elevation');

  fs.writeFileSync(statusFile, 'status=added\r\n');
  runCmd(sysDir, loaded.uninstallParams);
  if (!readStatus(statusFile).includes('status=added')) {
    fail('elevated uninstall replaced cast-firewall.txt before the installer commit');
  }
  const removed = commitFirewallStatus(instDir);
  console.log(`CAST_FIREWALL_STATUS_AFTER_UNINSTALL=${JSON.stringify(removed)}`);
  if (!removed.includes('status=removed')) fail(`cast-firewall.txt did not record status=removed:\n${removed}`);

  const mediaGone = netshShow(sysDir, mediaName);
  const mdnsGone = netshShow(sysDir, mdnsName);
  console.log('--- show rule media after uninstall ---');
  console.log(mediaGone.text.trim());
  console.log('--- show rule mdns after uninstall ---');
  console.log(mdnsGone.text.trim());
  assertGone(mediaGone.text, mediaName);
  assertGone(mdnsGone.text, mdnsName);
  console.log('cast firewall install and uninstall ok');
} finally {
  try {
    const cleanup = loadCommands(instDir, sysDir);
    runCmd(sysDir, cleanup.uninstallParams);
  } catch {
    // The uninstall command already ran, or netsh was never installed.
  }
  fs.rmSync(instDir, { recursive: true, force: true });
}
