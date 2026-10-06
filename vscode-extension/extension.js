// ClaudeShip ⇄ VS Code bridge.
//
// VS Code has no scripting interface for terminals, so the ClaudeShip
// menubar app can't select an integrated-terminal tab on its own. This
// extension is the missing half: when a session row is clicked, the app drops
// a request file; every VS Code window runs this extension and watches that
// file; the one window that owns a terminal whose shell pid is in the request
// focuses it (`terminal.show()` selects the exact tab or split) and answers
// with its workspace path so the app can raise the right window via the
// editor's own CLI (`code <folder>` focuses an already-open window).
//
// Each window's extension host only sees its own terminals, which is exactly
// why a shared file works across windows: every window looks, one matches.
//
// Files (owned by the app, under its Application Support directory):
//   request.json  {ts, nonce, pids: [..], cwd, sessionId}   written atomically
//   reply.json    {nonce, workspace, terminal, ts}           written atomically
//   windows/<env.sessionId>.json                             this window's state:
//                 {ts, name, workspace, terminals: [{pid, name}]}
// The windows/ file is how the app groups its session rows by VS Code window
// without asking: it's rewritten when terminals open or close, on a 30 s
// heartbeat (so a crashed window's file goes stale and is ignored), and
// removed on deactivate. Requests older than 15 s or already handled (by
// nonce) are ignored, so a late activation or a double click can't fire
// twice. This extension executes nothing and reads nothing else.

const vscode = require('vscode');
const fs = require('fs');
const os = require('os');
const path = require('path');

const DIR = path.join(os.homedir(), 'Library', 'Application Support', 'ClaudeShip', 'vscode-focus');
const REQUEST = path.join(DIR, 'request.json');
const REPLY = path.join(DIR, 'reply.json');
const WINDOWS_DIR = path.join(DIR, 'windows');
const MAX_AGE_MS = 15000;
const SEEN_LIMIT = 200;
const HEARTBEAT_MS = 30000;

const seen = [];

function remember(nonce) {
  seen.push(nonce);
  if (seen.length > SEEN_LIMIT) seen.shift();
}

function readRequest() {
  try {
    const data = JSON.parse(fs.readFileSync(REQUEST, 'utf8'));
    if (!data || typeof data.nonce !== 'string' || !Array.isArray(data.pids)) return null;
    if (typeof data.ts !== 'number' || Date.now() - data.ts > MAX_AGE_MS) return null;
    return data;
  } catch (e) {
    return null; // missing, partial, or malformed — never ours to act on
  }
}

// The terminal whose shell pid (Terminal.processId) is in the request's
// ancestor chain, if this window has one.
async function ownedTerminal(pids) {
  const wanted = new Set(pids.map(Number));
  const terminals = vscode.window.terminals;
  const ids = await Promise.all(
    terminals.map((t) => Promise.resolve(t.processId).catch(() => undefined))
  );
  const i = ids.findIndex((pid) => pid !== undefined && wanted.has(pid));
  return i >= 0 ? terminals[i] : undefined;
}

// What `code <path>` needs to raise this window: the .code-workspace file for
// a multi-root workspace, else the first folder, else null (untitled window).
function workspacePath() {
  const file = vscode.workspace.workspaceFile;
  if (file && file.scheme === 'file') return file.fsPath;
  const folders = vscode.workspace.workspaceFolders;
  if (folders && folders.length && folders[0].uri.scheme === 'file') return folders[0].uri.fsPath;
  return null;
}

function writeAtomically(file, obj) {
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(obj));
  fs.renameSync(tmp, file);
}

function writeReply(obj) {
  writeAtomically(REPLY, obj);
}

// One file per window, named by env.sessionId (unique per window, per launch).
function stateFile() {
  const id = String(vscode.env.sessionId || process.pid).replace(/[^A-Za-z0-9._-]/g, '_');
  return path.join(WINDOWS_DIR, `${id}.json`);
}

async function publishState() {
  try {
    const terminals = vscode.window.terminals;
    const pids = await Promise.all(
      terminals.map((t) => Promise.resolve(t.processId).catch(() => undefined))
    );
    const list = [];
    terminals.forEach((t, i) => {
      if (pids[i] !== undefined) list.push({ pid: pids[i], name: t.name });
    });
    fs.mkdirSync(WINDOWS_DIR, { recursive: true });
    writeAtomically(stateFile(), {
      ts: Date.now(),
      name: vscode.workspace.name || null,
      workspace: workspacePath(),
      terminals: list,
    });
  } catch (e) {
    // Best effort; grouping degrades to "VS Code" without a window label.
  }
}

async function handleRequest() {
  const req = readRequest();
  if (!req || seen.includes(req.nonce)) return;
  remember(req.nonce);
  const terminal = await ownedTerminal(req.pids);
  if (!terminal) return; // another window's session (or none) — stay quiet
  terminal.show(false);
  writeReply({ nonce: req.nonce, workspace: workspacePath(), terminal: terminal.name, ts: Date.now() });
}

function activate(context) {
  try {
    fs.mkdirSync(DIR, { recursive: true });
    const watcher = fs.watch(DIR, (_event, fname) => {
      if (!fname || fname === path.basename(REQUEST)) handleRequest();
    });
    context.subscriptions.push({ dispose: () => watcher.close() });
    handleRequest(); // a request that landed just before activation
  } catch (e) {
    // No bridge directory and no permission to make one: nothing to do.
  }

  // Window state for row grouping. The pid resolves a moment after a terminal
  // opens, so publish again shortly after the open event.
  publishState();
  context.subscriptions.push(
    vscode.window.onDidOpenTerminal(() => { publishState(); setTimeout(publishState, 1500); }),
    vscode.window.onDidCloseTerminal(() => publishState()),
    vscode.workspace.onDidChangeWorkspaceFolders(() => publishState())
  );
  const heartbeat = setInterval(publishState, HEARTBEAT_MS);
  context.subscriptions.push({ dispose: () => clearInterval(heartbeat) });
}

function deactivate() {
  try { fs.unlinkSync(stateFile()); } catch (e) { /* already gone */ }
}

module.exports = { activate, deactivate };
