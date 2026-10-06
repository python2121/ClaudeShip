// Claude Hub web app: the project directory and the browser terminal.
// Plain JS, no build step. Everything shown comes from /api/state (polled)
// and is put on the page with textContent/DOM nodes — never innerHTML —
// because conversation titles and paths are text the page does not control.
(() => {
  'use strict';

  // Must match HubFrame.version in the hub. The hub outlives installs, so
  // this page can be newer than the hub serving it.
  const PROTOCOL = 2;

  const MODES = {
    auto: ['Auto', "Works without asking, behind Claude's own safety checks."],
    acceptEdits: ['Accept edits', 'Edits files without asking. Still asks before running commands.'],
    plan: ['Plan', 'Reads and plans; changes nothing until you approve.'],
    manual: ['Manual', 'Asks before every edit and command.'],
    bypassPermissions: ['Bypass permissions', 'Never asks. Everything is allowed.'],
  };
  const STATUS = {
    busy: 'Working',
    waiting: 'Needs you',
    idle: 'Idle',
    shell: 'Running a shell command',
    starting: 'Starting',
  };
  const ICONS = {
    gear: 'M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Zm7.4-3.5c0-.5 0-.9-.1-1.3l2-1.6-2-3.4-2.4 1a7.6 7.6 0 0 0-2.2-1.3L14.3 3h-4l-.4 2.4c-.8.3-1.5.7-2.2 1.3l-2.4-1-2 3.4 2 1.6a7.7 7.7 0 0 0 0 2.6l-2 1.6 2 3.4 2.4-1c.7.6 1.4 1 2.2 1.3l.4 2.4h4l.4-2.4c.8-.3 1.5-.7 2.2-1.3l2.4 1 2-3.4-2-1.6c.1-.4.1-.8.1-1.3Z',
    plus: 'M12 5v14M5 12h14',
    down: 'm6 9 6 6 6-6',
    right: 'm9 6 6 6-6 6',
    left: 'm15 6-6 6 6 6',
    branch: 'M7 4v10m0 0a3 3 0 1 0 0 6 3 3 0 0 0 0-6Zm10-4a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm0 0c0 4-10 2-10 6',
  };

  // ── DOM helpers ──────────────────────────────────────────

  function h(tag, props, ...kids) {
    const el = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
      if (value == null || value === false) continue;
      if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
      else if (key === 'class') el.className = value;
      else el.setAttribute(key, value === true ? '' : String(value));
    }
    for (const kid of kids.flat()) {
      if (kid == null || kid === false || kid === '') continue;
      el.append(kid);
    }
    return el;
  }

  function icon(name) {
    const ns = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', name === 'gear' ? '1.6' : '2.2');
    svg.setAttribute('stroke-linecap', 'round');
    svg.setAttribute('stroke-linejoin', 'round');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(ns, 'path');
    path.setAttribute('d', ICONS[name]);
    svg.append(path);
    return svg;
  }

  /** Swap a container's children, keeping keyboard focus on whatever
      element plays the same role (same data-key) in the new tree. */
  function repaint(container, nodes) {
    const active = document.activeElement;
    const key = active && container.contains(active) ? active.dataset.key : null;
    container.replaceChildren(...nodes.filter(Boolean));
    if (!key) return;
    const again = container.querySelector(`[data-key="${CSS.escape(key)}"]`);
    if (again) again.focus({ preventScroll: true });
  }

  // ── Formatting ───────────────────────────────────────────

  // The hub's clock, not this device's: ages are differences against
  // timestamps the Mac wrote, and a phone's clock can be off.
  let clockOffset = 0;
  const now = () => Date.now() + clockOffset;

  function age(ms) {
    const s = Math.max(0, Math.floor((now() - ms) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m`;
    if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
    return `${Math.floor(s / 86400)}d`;
  }

  function ago(ms) {
    const s = Math.max(0, Math.floor((now() - ms) / 1000));
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.floor(s / 60)}m ago`;
    if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
    if (s < 86400 * 14) return `${Math.floor(s / 86400)}d ago`;
    return new Date(ms).toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' });
  }

  const tilde = (path, data) =>
    data && path.startsWith(data.root) ? data.rootDisplay + path.slice(data.root.length) : path;

  // ── State ────────────────────────────────────────────────

  const ui = {
    data: null,
    error: null,       // the poll's own trouble; cleared by the next good poll
    notice: null,      // a failed action; stays until dismissed or it times out
    unpaired: false,   // the hub wants its pairing link opened in this browser first
    filter: '',
    open: new Set(),   // project paths whose recent conversations are showing
    paintedAt: 0,
    menu: null,        // 'settings', or the project path whose mode menu is open
    launching: false,
    signature: '',
  };

  const app = document.getElementById('app');
  const tallyEl = h('div', { class: 'tally' });
  // Clicks inside stay inside: a repaint detaches the clicked node, and the
  // outside-click handler below would otherwise take it for a click elsewhere.
  const settingsEl = h('div', { class: 'settings-wrap', onclick: (event) => event.stopPropagation() });
  const hostEl = h('span', { class: 'brand-host' });
  const pageEl = h('main', { class: 'page' });
  const searchEl = h('input', {
    class: 'search', type: 'search', placeholder: 'Filter projects', 'aria-label': 'Filter projects',
    autocomplete: 'off', spellcheck: 'false',
    oninput: (event) => { ui.filter = event.target.value.trim().toLowerCase(); render(true); },
  });
  // Built once and kept: a repaint must not wipe a half-typed link or drop
  // the phone's keyboard. A home-screen app keeps its own cookies and has
  // no address bar, so this field is its only way to pair.
  const bannerEl = h('div');
  const pairEl = h('div', { class: 'empty pairing' },
    h('h3', null, "This browser isn't paired with the hub yet"),
    h('p', null, 'On the Mac, run ', h('code', null, 'claudeandrew hub link'),
      ' and open the link it prints in this browser — or scan the QR code it shows with this phone.'),
    h('form', {
      class: 'pair-form',
      onsubmit: async (event) => {
        event.preventDefault();
        const field = event.target.elements.link;
        const text = field.value.trim();
        const token = (text.match(/[?&]k=([0-9a-f]{64})(?![0-9a-f])/) || text.match(/^([0-9a-f]{64})$/) || [])[1];
        if (!token) {
          notify("That doesn't look like a pairing link.");
          return;
        }
        // Pair in place rather than navigating: a refused link would leave
        // a home-screen app on an error page with no way back.
        const response = await fetch(`/auth?k=${token}`, { cache: 'no-store' }).catch(() => null);
        if (response && response.ok) {
          field.value = '';
          poll(true);
        } else {
          notify(response ? 'That pairing link is not valid for this hub.' : "Can't reach the hub on the Mac.");
        }
      },
    },
      h('input', {
        class: 'search', name: 'link', type: 'text', placeholder: 'Or paste the pairing link here',
        'aria-label': 'Pairing link', autocomplete: 'off', autocapitalize: 'off', spellcheck: 'false',
      }),
      h('button', { class: 'btn primary', type: 'submit' }, 'Pair')));

  const directoryEl = h('div', { class: 'directory' },
    h('header', { class: 'top' },
      h('div', { class: 'top-inner' },
        h('div', { class: 'brand' },
          h('img', { class: 'brand-mark', src: '/icon.svg', alt: '' }),
          h('span', { class: 'brand-name' }, 'Claude Hub'),
          hostEl),
        tallyEl,
        h('div', { class: 'tools' }, searchEl, settingsEl))),
    pageEl);
  app.append(directoryEl);

  // ── API ──────────────────────────────────────────────────

  async function post(path, body) {
    const response = await fetch(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const result = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(result.error || `request failed (${response.status})`);
    return result;
  }

  let polling = false;
  let pollAgain = false;
  async function poll(force) {
    // A poll asked for while one is in flight (e.g. right after pairing)
    // runs as soon as that one finishes, rather than being dropped.
    if (polling) { pollAgain = true; return; }
    polling = true;
    try {
      const response = await fetch('/api/state', { cache: 'no-store' });
      if (response.status === 401) {
        ui.unpaired = true;
        ui.data = null;
        ui.error = null;
        // Nothing to attach to without pairing; show why instead.
        if (terminal) location.replace('#/');
      } else {
        if (!response.ok) throw new Error(String(response.status));
        const data = await response.json();
        // An older hub may not send everything this page reads.
        data.projects = data.projects || [];
        data.elsewhere = data.elsewhere || [];
        data.permissionModes = data.permissionModes || [];
        clockOffset = data.now - Date.now();
        ui.data = data;
        ui.unpaired = false;
        ui.error = null;
      }
    } catch {
      // Unreachable is not the same as unpaired, whatever came before.
      ui.unpaired = false;
      ui.error = "Can't reach the hub on the Mac. Retrying…";
    } finally {
      polling = false;
    }
    if (!ui.data) ui.menu = null;  // nothing for a menu to act on; don't let it hold the page
    render(force);
    updateTerminalBar();
    if (pollAgain) {
      pollAgain = false;
      poll(true);
    }
  }

  let noticeTimer = null;
  function notify(text) {
    ui.notice = text;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => { ui.notice = null; render(true); }, 10000);
    render(true);
  }

  async function launch(path, mode, resume) {
    if (ui.launching) return;
    ui.launching = true;
    ui.menu = null;
    render(true);
    try {
      const body = { path };
      if (mode) body.permissionMode = mode;
      if (resume) body.resume = resume;
      const { id } = await post('/api/launch', body);
      location.hash = `#/s/${id}`;
      poll(true);
    } catch (error) {
      notify(`Couldn't start a session: ${error.message}`);
    } finally {
      ui.launching = false;
      render(true);
    }
  }

  async function setDefaultMode(mode) {
    try {
      await post('/api/settings', { defaultPermissionMode: mode });
      if (ui.data) ui.data.defaultPermissionMode = mode;
    } catch (error) {
      notify(`Couldn't save the setting: ${error.message}`);
    }
    render(true);
  }

  // ── Directory rendering ──────────────────────────────────

  function matches(project) {
    if (!ui.filter) return true;
    const haystack = [
      project.name, project.branch,
      ...project.sessions.map((s) => s.title), ...project.recent.map((r) => r.title),
    ].filter(Boolean).join('\n').toLowerCase();
    return haystack.includes(ui.filter);
  }

  function render(force) {
    const data = ui.data;
    // Ages tick over a few times a minute; otherwise only repaint when
    // something changed, and never under an open menu.
    const signature = JSON.stringify([
      data && { ...data, now: 0 }, ui.error, ui.notice, ui.unpaired, ui.filter, [...ui.open], ui.menu,
      ui.launching, data ? Math.floor(Date.now() / 15000) : 0,
    ]);
    // A routine repaint waits for an open menu or a text selection in the
    // list — but not forever, or the page would quietly go stale.
    const selection = window.getSelection();
    const held = ui.menu
      || (selection && !selection.isCollapsed && pageEl.contains(selection.anchorNode));
    if (!force && (signature === ui.signature || (held && Date.now() - ui.paintedAt < 20000))) return;
    ui.signature = signature;
    ui.paintedAt = Date.now();

    const banners = [
      ui.notice && h('button', {
        class: 'banner', 'data-key': 'notice', title: 'Dismiss',
        onclick: () => { ui.notice = null; render(true); },
      }, ui.notice),
      ui.error && h('div', { class: 'banner' }, ui.error),
      data && data.protocol !== PROTOCOL && h('div', { class: 'banner' },
        'The hub on the Mac is running a different build than this page, so some things may not work. ',
        'Restart it when its sessions can end: ', h('code', null, 'claudeandrew hub stop'), ', then ',
        h('code', null, 'claudeandrew hub start'), '.'),
    ];
    if (!data) {
      // No state to show: don't leave the last one's header standing.
      hostEl.textContent = '';
      tallyEl.replaceChildren();
      settingsEl.replaceChildren();
      bannerEl.replaceChildren(...banners.filter(Boolean));
      const wanted = ui.unpaired ? [bannerEl, pairEl] : [bannerEl];
      const same = pageEl.children.length === wanted.length && wanted.every((node, i) => pageEl.children[i] === node);
      if (!same) pageEl.replaceChildren(...wanted);
      return;
    }

    const all = data.projects.flatMap((p) => p.sessions).concat(data.elsewhere);
    const waiting = all.filter((s) => s.status === 'waiting').length;
    const busy = all.filter((s) => s.status === 'busy').length;
    if (!terminal) document.title = `${waiting ? `(${waiting}) ` : ''}Claude Hub — ${data.host}`;
    hostEl.textContent = data.host;
    tallyEl.replaceChildren(
      waiting ? h('span', { class: 'pill waiting' }, h('span', { class: 'glyph waiting' }), `${waiting} need${waiting === 1 ? 's' : ''} you`) : '',
      busy ? h('span', { class: 'pill busy' }, h('span', { class: 'glyph busy' }), `${busy} working`) : '',
      h('span', { class: 'pill' }, `${all.length} session${all.length === 1 ? '' : 's'}`));
    repaint(settingsEl, settings(data));

    const visible = data.projects.filter(matches);
    const active = visible.filter((p) => p.sessions.length);
    const rest = visible.filter((p) => !p.sessions.length);
    const parts = [...banners];

    if (active.length || !ui.filter) {
      parts.push(h('section', null,
        h('div', { class: 'section-head' },
          h('h2', null, 'Running'),
          h('span', { class: 'count' }, String(active.length))),
        active.length
          ? h('div', { class: 'cards' }, active.map((p) => card(p, data)))
          : h('div', { class: 'empty' },
              'Nothing is running. Start a session below, or run ', h('code', null, 'claudeandrew'),
              ' in a terminal on the Mac.')));
    }
    // Sessions running outside the projects folder: shown while they run,
    // never tracked otherwise, right under the projects they sit beside.
    if (data.elsewhere.length && !ui.filter) {
      parts.push(h('section', null,
        h('div', { class: 'section-head' },
          h('h2', null, 'Running elsewhere'),
          h('span', { class: 'count' }, String(data.elsewhere.length))),
        h('div', { class: 'cards' },
          h('article', { class: `card${data.elsewhere.some((s) => s.status === 'waiting') ? ' attention' : ''}` },
            h('div', { class: 'sessions' }, data.elsewhere.map((s) => sessionRow(s, null, data)))))));
    }
    if (rest.length) {
      parts.push(h('section', null,
        h('div', { class: 'section-head' },
          h('h2', null, 'Projects'),
          h('span', { class: 'count' }, String(rest.length)),
          h('span', { class: 'where' }, data.rootDisplay)),
        h('div', { class: 'list' }, rest.map((p) => row(p, data)))));
    }
    if (!visible.length && ui.filter) {
      parts.push(h('div', { class: 'empty' }, `No project matches “${ui.filter}”.`));
    }
    repaint(pageEl, parts);
  }

  function branchTag(name) {
    return h('span', { class: 'branch', title: name }, icon('branch'), h('span', null, name));
  }

  function card(project, data) {
    const count = project.sessions.length;
    const open = ui.open.has(project.path);
    return h('article', { class: `card${project.sessions.some((s) => s.status === 'waiting') ? ' attention' : ''}` },
      h('div', { class: 'card-head' },
        h('div', { class: 'card-title' },
          h('h3', null, project.name),
          count > 1 && h('span', { class: 'instances' }, `${count} running`),
          project.branch && branchTag(project.branch)),
        h('div', { class: 'card-path' }, tilde(project.path, data))),
      h('div', { class: 'sessions' }, project.sessions.map((s) => sessionRow(s, project, data))),
      h('div', { class: 'card-foot' },
        launchControl(project, data, true),
        h('span', { class: 'spacer' }),
        project.recent.length > 0 && h('button', {
          class: 'btn quiet', 'aria-expanded': String(open), 'data-key': `earlier:${project.path}`,
          onclick: () => { toggleOpen(project.path); },
        }, 'Earlier', icon(open ? 'down' : 'right'))),
      open && recentList(project));
  }

  function sessionRow(session, project, data) {
    const untitled = !session.title;
    const title = session.title || (session.status === 'starting' ? 'Starting…' : 'New conversation');
    const meta = [h('span', { class: session.status }, STATUS[session.status] || session.status)];
    if (session.status === 'waiting' && session.waitingFor) meta.push(` — ${session.waitingFor}`);
    const extras = [];
    if (session.since) extras.push(age(session.since));
    if (!project) extras.push(tilde(session.cwd, data));
    else if (session.sub) extras.push(session.sub);
    if (session.branch && (!project || session.branch !== project.branch)) extras.push(session.branch);
    if (session.viewers > 0) extras.push(`${session.viewers} attached`);
    for (const extra of extras) meta.push(h('span', { class: 'sep' }, '·'), extra);

    const body = [
      h('span', { class: `glyph ${session.status}` }),
      h('span', { class: `session-title${untitled ? ' untitled' : ''}` }, title),
      h('span', { class: 'session-meta' }, meta),
    ];
    if (session.attachable) {
      return h('button', {
        class: 'session', 'data-key': `session:${session.key}`,
        onclick: () => { location.hash = `#/s/${session.hubId}`; },
      }, body, h('span', { class: 'session-go' }, 'Open', icon('right')));
    }
    const why = session.background
      ? 'A background session run by Claude Code itself.'
      : 'Started directly in a terminal, so it can only be used there. Sessions started here or with claudeandrew can be opened from anywhere.';
    return h('div', { class: 'session external', title: why }, body,
      h('span', { class: 'session-go' }, session.background ? 'Background' : 'Terminal only'));
  }

  function launchControl(project, data, primary) {
    const open = ui.menu === project.path;
    const tone = primary ? ' primary' : '';
    return h('div', { class: 'launch', onclick: (event) => event.stopPropagation() },
      h('button', {
        class: `btn main${tone}`, disabled: ui.launching, 'data-key': `new:${project.path}`,
        title: `Start in ${MODES[data.defaultPermissionMode]?.[0] || data.defaultPermissionMode} mode`,
        onclick: () => launch(project.path),
      }, icon('plus'), 'New session'),
      h('button', {
        class: `btn caret${tone}`, 'aria-label': 'Choose a permission mode', 'aria-expanded': String(open),
        'data-key': `caret:${project.path}`,
        onclick: () => { ui.menu = open ? null : project.path; render(true); },
      }, icon('down')),
      open && h('div', { class: `menu${primary ? '' : ' right'}`, role: 'menu' },
        h('div', { class: 'menu-label' }, 'Start in'),
        Object.entries(MODES).filter(([mode]) => data.permissionModes.includes(mode)).map(([mode, [name, about]]) =>
          h('button', {
            class: `menu-item${mode === 'bypassPermissions' ? ' danger' : ''}`, role: 'menuitem',
            'data-key': `mode:${project.path}:${mode}`,
            onclick: () => launch(project.path, mode),
          }, h('b', null, name), mode === data.defaultPermissionMode && h('span', { class: 'tag' }, 'Default'),
            h('small', null, about)))));
  }

  function recentList(project) {
    return h('div', { class: 'recent' },
      project.recent.length
        ? project.recent.map((conversation) => h('button', {
            class: 'recent-item', title: 'Resume this conversation in a new session',
            'data-key': `resume:${conversation.sessionId}`,
            onclick: () => launch(project.path, null, conversation.sessionId),
          },
            h('span', { class: 'title' }, conversation.title),
            h('span', { class: 'when' }, ago(conversation.at)),
            h('span', { class: 'resume' }, 'Resume')))
        : h('div', { class: 'recent-none' }, 'No earlier conversations to resume.'));
  }

  function row(project, data) {
    const open = ui.open.has(project.path);
    const last = project.recent[0];
    const toggle = () => toggleOpen(project.path);
    return h('div', { class: `row${open ? ' open' : ''}` },
      h('div', {
        class: 'row-main', role: 'button', tabindex: '0', 'aria-expanded': String(open),
        'data-key': `row:${project.path}`,
        onclick: toggle,
        onkeydown: (event) => {
          if (event.target === event.currentTarget && (event.key === 'Enter' || event.key === ' ')) {
            event.preventDefault();
            toggle();
          }
        },
      },
        h('div', { class: 'row-name' }, h('b', null, project.name), project.branch && branchTag(project.branch)),
        h('div', { class: `row-last${last ? '' : ' none'}` },
          last ? last.title : project.lastActivity ? 'No summarized conversations' : 'Not used with Claude yet'),
        h('div', { class: 'row-when' }, project.lastActivity ? ago(project.lastActivity) : ''),
        launchControl(project, data, false)),
      open && recentList(project));
  }

  function settings(data) {
    const open = ui.menu === 'settings';
    return [
      h('button', {
        class: 'icon-btn', 'aria-label': 'Settings', 'aria-expanded': String(open), 'data-key': 'gear',
        onclick: () => { ui.menu = open ? null : 'settings'; render(true); },
      }, icon('gear')),
      open && h('div', { class: 'menu right settings' },
        h('h4', null, 'New sessions start in'),
        h('p', null, 'Used by New session and Resume on this page. The arrow beside New session picks a different mode for one launch.'),
        h('div', { class: 'options', role: 'radiogroup' },
          Object.entries(MODES).filter(([mode]) => data.permissionModes.includes(mode)).map(([mode, [name, about]]) =>
            h('button', {
              class: 'option', role: 'radio', 'aria-checked': String(mode === data.defaultPermissionMode),
              'data-key': `default:${mode}`,
              onclick: () => setDefaultMode(mode),
            }, h('span', { class: 'dot' }), h('b', null, name), h('small', null, about)))),
        h('div', { class: 'foot' },
          'Projects are the folders in ', h('code', null, data.rootDisplay), ' on ', h('code', null, data.host), '.')),
    ];
  }

  function toggleOpen(path) {
    if (ui.open.has(path)) ui.open.delete(path);
    else ui.open.add(path);
    render(true);
  }

  document.addEventListener('click', (event) => {
    if (ui.menu && !event.target.closest('.launch, .settings-wrap')) {
      ui.menu = null;
      render(true);
    }
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && ui.menu && !terminal) {
      ui.menu = null;
      render(true);
    }
  });

  // ── Terminal ─────────────────────────────────────────────

  const encoder = new TextEncoder();
  const coarse = window.matchMedia('(pointer: coarse)').matches;
  let terminal = null;

  const THEME = {
    background: '#151413', foreground: '#e9e5de', cursor: '#e9e5de', cursorAccent: '#151413',
    selectionBackground: 'rgba(223, 125, 90, 0.38)',
    black: '#1d1c1a', red: '#e06c66', green: '#7fc58b', yellow: '#e5c07b',
    blue: '#6ea8e8', magenta: '#c58fd6', cyan: '#63c3c6', white: '#d5d0c8',
    brightBlack: '#77716a', brightRed: '#f08a84', brightGreen: '#98d9a3', brightYellow: '#f0d290',
    brightBlue: '#8ebcf2', brightMagenta: '#d7a9e3', brightCyan: '#84d6d8', brightWhite: '#f5f2ec',
  };

  function openTerminal(id) {
    closeTerminal();
    const host = h('div', { class: 'term-host' });
    const note = h('div', { class: 'term-note', hidden: true });
    const over = h('div', { class: 'term-over', hidden: true });
    const project = h('div', { class: 'term-project' }, 'Session');
    const title = h('div', { class: 'term-title' });
    const glyph = h('span', { class: 'glyph' });
    const state = h('span', { class: 'term-state' });
    const end = h('button', { class: 'btn', onclick: () => endSession() }, 'End');
    const keys = h('div', { class: 'keys' }, [
      ['esc', '\x1b'], ['tab', '\t'], ['⇧tab', '\x1b[Z'], ['^C', '\x03'],
      ['↑', 'A'], ['↓', 'B'], ['←', 'D'], ['→', 'C'],
      ['pgup', '\x1b[5~'], ['pgdn', '\x1b[6~'], ['⏎', '\r'],
    ].map(([label, sequence]) => h('button', {
      class: 'key', 'aria-label': label,
      // Keep focus (and the on-screen keyboard) on the terminal.
      onpointerdown: (event) => event.preventDefault(),
      onclick: () => {
        const t = terminal;
        if (!t) return;
        const arrow = sequence.length === 1 && 'ABCD'.includes(sequence);
        send(encoder.encode(arrow
          ? (t.term.modes.applicationCursorKeysMode ? '\x1bO' : '\x1b[') + sequence
          : sequence));
        t.term.focus();
      },
    }, label)));
    const page = h('div', { class: 'term-page' },
      h('div', { class: 'term-bar' },
        h('button', { class: 'icon-btn', 'aria-label': 'Back to projects', onclick: leaveTerminal }, icon('left')),
        h('div', { class: 'term-id' }, glyph, h('div', { class: 'term-names' }, project, title)),
        state, end),
      h('div', { class: 'term-body' }, host, note, over),
      keys);
    document.body.append(page);
    directoryEl.hidden = true;

    const term = new Terminal({
      fontFamily: 'ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace',
      fontSize: coarse ? 12 : 13,
      lineHeight: 1.15,
      cursorBlink: true,
      scrollback: 10000,
      theme: THEME,
      macOptionIsMeta: false,
      allowProposedApi: false,
    });
    const fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(host);

    terminal = {
      id, term, fit, page, host, note, over, project, title, glyph, state, end,
      socket: null, ended: false,
      own: null,      // the grid this window fits
      sent: null,     // the grid the hub was last told this window fits
      size: null,     // the grid the session actually has
      owner: false,   // whether the session is sized for this screen (the hub says)
      attempts: 0, refusals: 0, connected: false, timer: null, noteTimer: null, endArmed: null,
      heard: Date.now(), pulse: null,
    };
    const t = terminal;
    term.onData((text) => send(encoder.encode(text)));
    term.onBinary((text) => send(Uint8Array.from(text, (c) => c.charCodeAt(0))));

    // A fullscreen TUI turns on mouse reporting, which leaves a finger with
    // no way to scroll — so translate vertical drags into wheel reports.
    let lastY = 0;
    let travelled = 0;
    host.addEventListener('touchstart', (event) => {
      if (event.touches.length === 1) { lastY = event.touches[0].clientY; travelled = 0; }
    }, { passive: true });
    host.addEventListener('touchmove', (event) => {
      if (event.touches.length !== 1 || term.modes.mouseTrackingMode === 'none') return;
      const y = event.touches[0].clientY;
      travelled += y - lastY;
      lastY = y;
      const column = Math.max(1, Math.floor(term.cols / 2));
      const line = Math.max(1, Math.floor(term.rows / 2));
      while (Math.abs(travelled) >= 16) {
        const up = travelled > 0;
        travelled -= up ? 16 : -16;
        send(encoder.encode(`\x1b[<${up ? 64 : 65};${column};${line}M`));
      }
      event.preventDefault();
    }, { passive: false });

    layoutViewport();
    measure();
    connect();
    updateTerminalBar();
    if (!coarse) term.focus();

    // The first measurement can run before the font has been measured;
    // take it again once layout and fonts have settled.
    const settle = () => {
      if (terminal !== t || t.ended) return;
      measure();
      syncFit();
    };
    requestAnimationFrame(settle);
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(settle);

    // A socket can die without saying so (a phone waking up on another
    // network). Ask for a pong now and then, and start over if none comes.
    t.pulse = setInterval(() => {
      if (terminal !== t || t.ended || document.hidden) return;
      if (!t.socket || t.socket.readyState !== WebSocket.OPEN) return;
      if (Date.now() - t.heard > 45000) reconnectNow();
      else t.socket.send(JSON.stringify({ type: 'ping' }));
    }, 15000);
  }

  /** Back to the directory, without leaving the session one Back away. */
  function leaveTerminal() {
    // Only when this history entry was reached straight from the
    // directory is "back" known to be the directory.
    if (history.state && history.state.fromDirectory && history.length > 1) history.back();
    else location.replace('#/');
  }

  /** Drop the current socket, whatever state it thinks it is in, and dial again. */
  function reconnectNow() {
    const t = terminal;
    if (!t || t.ended) return;
    clearTimeout(t.timer);
    if (t.socket) {
      t.socket.onclose = null;
      t.socket.onmessage = null;
      try { t.socket.close(); } catch { /* already gone */ }
    }
    t.attempts = 0;
    t.refusals = 0;
    setNote('Reconnecting…');
    connect();
  }

  function closeTerminal() {
    const t = terminal;
    if (!t) return;
    terminal = null;
    clearTimeout(t.timer);
    clearTimeout(t.noteTimer);
    clearTimeout(t.endArmed);
    clearInterval(t.pulse);
    if (t.socket) t.socket.close();
    t.term.dispose();
    t.page.remove();
    directoryEl.hidden = false;
  }

  /** The grid this window fits, whatever size the session currently is. */
  function measure() {
    const t = terminal;
    if (!t) return;
    const proposed = t.fit.proposeDimensions();
    // A sliver (a phone on its side with the keyboard up) is not a size
    // worth imposing on the session; keep the last real one.
    if (proposed && proposed.rows >= 6 && proposed.cols >= 20) {
      t.own = { rows: proposed.rows, cols: proposed.cols };
    } else if (!t.own) {
      t.own = { rows: 24, cols: 80 };
    }
  }

  const sameGrid = (a, b) => Boolean(a && b && a.rows === b.rows && a.cols === b.cols);

  function connect() {
    const t = terminal;
    if (!t || t.ended) return;
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    // Opening a session sizes it for this window. Coming back after a
    // dropped connection does not: the session may be in use on another
    // screen by now, and typing here takes the size back soon enough. (If
    // nobody else is attached, the hub hands it back anyway.)
    const claiming = !t.connected && !document.hidden;
    t.owner = claiming;  // until the hub's size message says how it went
    t.sent = { ...t.own };
    const socket = new WebSocket(
      `${scheme}://${location.host}/ws/term?id=${encodeURIComponent(t.id)}`
      + `&rows=${t.sent.rows}&cols=${t.sent.cols}&claim=${claiming ? 1 : 0}`);
    socket.binaryType = 'arraybuffer';
    t.socket = socket;
    t.heard = Date.now();
    let opened = false;
    socket.onopen = () => {
      if (terminal !== t) return;
      opened = true;
      t.attempts = 0;
      t.refusals = 0;
      // A reconnect gets the whole replay again; start from a blank screen.
      if (t.connected) t.term.reset();
      t.connected = true;
      setNote(null);
      // The window may have changed (or only now been measurable) while
      // the socket was still connecting, when nothing could be sent.
      measure();
      syncFit();
    };
    socket.onmessage = (event) => {
      if (terminal !== t) return;
      t.heard = Date.now();
      if (typeof event.data !== 'string') {
        t.term.write(new Uint8Array(event.data));
        return;
      }
      let message;
      try { message = JSON.parse(event.data); } catch { return; }
      if (message.type === 'size') {
        t.size = { rows: message.rows, cols: message.cols };
        if (typeof message.owner === 'boolean') t.owner = message.owner;
        applySize();
      } else if (message.type === 'exit') {
        finished('Session ended', message.code ? `Claude exited with code ${message.code}.` : 'Claude exited.');
      } else if (message.type === 'gone') {
        finished('Session not found', 'It has ended, or the hub was restarted.');
      }
    };
    socket.onclose = () => {
      if (terminal !== t || t.ended || t.socket !== socket) return;
      // Never even opening, again and again, while the hub is otherwise
      // answering is the hub saying no (not a flaky network): stop and say
      // so rather than retry forever.
      if (!opened && ui.data && !ui.error && ++t.refusals >= 5) {
        setNote("Can't connect to this session", h('button', {
          class: 'btn', onclick: () => { t.refusals = 0; t.attempts = 0; setNote('Reconnecting…'); connect(); },
        }, 'Retry'));
        return;
      }
      setNote('Reconnecting…');
      t.timer = setTimeout(connect, Math.min(5000, 400 * 2 ** t.attempts++));
    };
  }

  /** The session is over (ended here, ended elsewhere, or gone): back to
      the directory, with a word about what happened. */
  function finished(heading, detail) {
    const t = terminal;
    if (!t) return;
    t.ended = true;
    setNote(null);
    leaveTerminal();
    notify(`${heading}. ${detail}`);
    poll(true);
  }

  function setNote(text, action) {
    const t = terminal;
    if (!t) return;
    t.note.hidden = !text;
    delete t.note.dataset.kind;
    t.note.className = `term-note${action ? '' : ' plain'}`;
    t.note.replaceChildren(text || '', action || '');
  }

  function send(bytes) {
    const t = terminal;
    if (!t || t.ended || !t.socket || t.socket.readyState !== WebSocket.OPEN) return;
    // The hub decides whether this makes us the screen the session is
    // sized for: keystrokes do, the terminal's own replies and focus
    // reports don't.
    t.socket.send(bytes);
  }

  /** Bring the hub up to date with what this window fits: resizing the
      session if it is sized for this screen, just noting it otherwise (so
      a later keystroke here claims the right size). */
  function syncFit() {
    const t = terminal;
    if (!t || t.ended) return;
    if (!sameGrid(t.own, t.sent) && t.socket && t.socket.readyState === WebSocket.OPEN) {
      t.sent = { ...t.own };
      t.socket.send(JSON.stringify({ type: t.owner ? 'resize' : 'fit', rows: t.own.rows, cols: t.own.cols }));
    }
    applySize();
  }

  /** Take the session's size for this screen. The hub always answers. */
  function claim() {
    const t = terminal;
    if (!t || t.ended || !t.socket || t.socket.readyState !== WebSocket.OPEN) return;
    measure();
    t.sent = { ...t.own };
    t.socket.send(JSON.stringify({ type: 'resize', rows: t.own.rows, cols: t.own.cols }));
  }

  /** Show the session at its real size: ours, or another screen's scaled to fit. */
  function applySize() {
    const t = terminal;
    if (!t || !t.size) return;
    const { rows, cols } = t.size;
    if (t.term.rows !== rows || t.term.cols !== cols) t.term.resize(cols, rows);
    requestAnimationFrame(rescale);
    clearTimeout(t.noteTimer);
    const foreign = !t.owner && !sameGrid(t.size, t.own);
    if (!foreign) {
      if (!t.note.hidden && t.note.dataset.kind === 'size') setNote(null);
      return;
    }
    // After a beat, so a size in flight doesn't flash the note — and only
    // over a live connection, never over "Reconnecting…" or a give-up.
    t.noteTimer = setTimeout(() => {
      if (terminal !== t || t.ended || t.owner || sameGrid(t.size, t.own)) return;
      if (!t.socket || t.socket.readyState !== WebSocket.OPEN) return;
      if (!t.note.hidden && t.note.dataset.kind !== 'size') return;
      setNote(`Sized for another screen (${cols}×${rows})`,
        h('button', { class: 'btn', onclick: () => { claim(); t.term.focus(); } }, 'Fit here'));
      t.note.dataset.kind = 'size';
    }, 500);
  }

  function rescale() {
    const t = terminal;
    if (!t) return;
    t.host.style.transform = '';
    // Measured, not inferred from the grids: a window too small to be worth
    // sizing the session for (a sliver beside a phone keyboard) still has
    // to show the whole screen, prompt included.
    const screen = t.host.querySelector('.xterm-screen');
    if (!screen || !screen.offsetWidth || !screen.offsetHeight) return;
    const scale = Math.min(1, t.host.clientWidth / screen.offsetWidth, t.host.clientHeight / screen.offsetHeight);
    if (scale < 1) t.host.style.transform = `scale(${scale})`;
  }

  async function endSession() {
    const t = terminal;
    if (!t || t.ended) return;
    if (!t.endArmed) {
      t.end.textContent = 'End session?';
      t.end.classList.add('confirm');
      t.endArmed = setTimeout(() => {
        t.endArmed = null;
        t.end.textContent = 'End';
        t.end.classList.remove('confirm');
      }, 3000);
      return;
    }
    clearTimeout(t.endArmed);
    t.end.disabled = true;
    t.endArmed = null;
    try {
      await post('/api/kill', { id: t.id });
    } catch {
      if (terminal !== t) return;
      t.end.disabled = false;
      t.end.textContent = 'End';
      t.end.classList.remove('confirm');
      setNote("Couldn't end the session");
    }
  }

  function updateTerminalBar() {
    const t = terminal;
    if (!t || !ui.data) return;
    let found = null;
    let owner = null;
    for (const project of ui.data.projects) {
      for (const session of project.sessions) {
        if (session.hubId === t.id) { found = session; owner = project; }
      }
    }
    if (!found) found = ui.data.elsewhere.find((s) => s.hubId === t.id) || null;
    if (!found) return;
    t.project.textContent = owner ? owner.name : tilde(found.cwd, ui.data);
    t.title.textContent = found.title || '';
    t.title.hidden = !found.title;
    t.glyph.className = `glyph ${found.status}`;
    t.state.className = `term-state ${found.status}`;
    t.state.replaceChildren(STATUS[found.status] || found.status,
      found.since ? h('span', { class: 'long' }, ` · ${age(found.since)}`) : '');
    document.title = `${found.status === 'waiting' ? '(!) ' : ''}${found.title || t.project.textContent} — Claude Hub`;
  }

  window.addEventListener('online', () => reviveTerminal());
  function reviveTerminal() {
    const t = terminal;
    if (!t || t.ended) return;
    if (!t.socket || t.socket.readyState !== WebSocket.OPEN) {
      reconnectNow();
    } else {
      // Time spent in the background doesn't count against the socket —
      // but a socket that died while we were away still says OPEN, so
      // give it one ping's worth of benefit of the doubt, not a minute's.
      const asked = Date.now();
      t.heard = asked;
      t.socket.send(JSON.stringify({ type: 'ping' }));
      setTimeout(() => { if (terminal === t && !t.ended && t.heard <= asked) reconnectNow(); }, 5000);
    }
  }

  // ── Viewport, routing, polling ───────────────────────────

  /** Keep the terminal inside what's actually visible — on a phone, the
      part of the screen the keyboard leaves. */
  function layoutViewport() {
    const viewport = window.visualViewport;
    const height = viewport ? viewport.height : window.innerHeight;
    document.documentElement.style.setProperty('--vh', `${height}px`);
    if (terminal) {
      terminal.page.style.top = `${viewport ? viewport.offsetTop : 0}px`;
      window.scrollTo(0, 0);
    }
  }

  let resizeTimer = null;
  function onResize() {
    // Pinch-zoom shrinks the visual viewport too; that is the user looking
    // closer, not the screen changing size.
    if (window.visualViewport && Math.abs(window.visualViewport.scale - 1) > 0.01) return;
    layoutViewport();
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      const t = terminal;
      if (!t || t.ended) return;
      measure();
      syncFit();
    }, 90);
  }
  window.addEventListener('resize', onResize);
  if (window.visualViewport) {
    window.visualViewport.addEventListener('resize', onResize);
    window.visualViewport.addEventListener('scroll', layoutViewport);
  }

  // Whether the directory has been on screen in this page's history — if
  // so, the terminal's Back can simply go back to it.
  let directoryShown = false;
  function route() {
    const match = location.hash.match(/^#\/s\/([0-9a-f]+)$/);
    if (match) {
      // Stamp a new history entry with how it was reached; an entry come
      // back to (Back/Forward, reload) keeps the stamp it has.
      if (history.state == null) {
        history.replaceState({ fromDirectory: directoryShown && !terminal }, '');
      }
      if (!terminal || terminal.id !== match[1]) openTerminal(match[1]);
    } else {
      directoryShown = true;
      closeTerminal();
      render(true);
    }
  }
  window.addEventListener('hashchange', route);

  setInterval(() => { if (!document.hidden) poll(false); }, 2000);
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) return;
    poll(false);
    reviveTerminal();
  });

  layoutViewport();
  route();
  poll(true);
})();
