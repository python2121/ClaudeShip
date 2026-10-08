// ClaudeShip web app: the project directory and the browser terminal.
// Plain JS, no build step. Everything shown comes from /api/state (polled)
// and is put on the page with textContent/DOM nodes — never innerHTML —
// because conversation titles and paths are text the page does not control.
(() => {
  'use strict';

  // Must match PROTOCOL in the hub (hub/src/frame.rs). The hub outlives installs, so
  // this page can be newer than the hub serving it.
  const PROTOCOL = 3;

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
    minimize: 'M6 12h12',
    window: 'M8 8V5h11v11h-3M5 8h11v11H5z',
    maximize: 'M5 5h14v14H5z',
    close: 'M6 6l12 12M18 6 6 18',
    popout: 'M14 5h5v5M19 5l-8 8M10 5H5v14h14v-5',
    popin: 'M19 10h-5V5M14 10l5-5M10 5H5v14h14v-5',
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
  // Each host in the swarm has its own clock: a view carries the offset of
  // the host whose timestamps are being read.
  const now = (view) => Date.now() + (view ? view.offset : 0);

  function age(ms, view) {
    const s = Math.max(0, Math.floor((now(view) - ms) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m`;
    if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
    return `${Math.floor(s / 86400)}d`;
  }

  function ago(ms, view) {
    const s = Math.max(0, Math.floor((now(view) - ms) / 1000));
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.floor(s / 60)}m ago`;
    if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
    if (s < 86400 * 14) return `${Math.floor(s / 86400)}d ago`;
    return new Date(ms).toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' });
  }

  const tilde = (path, data) =>
    data && data.root && path.startsWith(data.root) ? data.rootDisplay + path.slice(data.root.length) : path;

  // ── State ────────────────────────────────────────────────

  const ui = {
    data: null,
    error: null,       // the poll's own trouble; cleared by the next good poll
    notice: null,      // a failed action; stays until dismissed or it times out
    unpaired: false,   // the hub wants its pairing link opened in this browser first
    filter: '',
    open: new Set(),   // project paths whose recent conversations are showing
    paintedAt: 0,
    menu: null,        // 'settings', 'computers', or the project path whose mode menu is open
    peers: null,       // GET /api/swarm/peers while the Computers panel is open
    peersError: null,
    removeArmed: null, // the peer id whose Remove is waiting for its confirming click
    removing: null,
    launching: false,
    answering: new Set(),  // approval ids / session ids with a request in flight
    signature: '',
    views: [],         // one per host in the swarm (a single one on an older hub)
  };

  /** One host's slice of /api/state, shaped like the top-level fields the
      rendering code reads, plus the host's identity and clock. hostId is
      null for the hub serving this page (requests omit it). */
  function buildViews(data) {
    const local = {
      hostId: null, name: data.host, local: true, reachable: true, down: false, lastSeen: null,
      protocol: data.protocol, offset: data.now - Date.now(),
      root: data.root, rootDisplay: data.rootDisplay, home: data.home,
      defaultPermissionMode: data.defaultPermissionMode, permissionModes: data.permissionModes,
      approvalsSupported: Boolean(data.approvalsSupported),
      projects: data.projects, elsewhere: data.elsewhere,
    };
    if (!Array.isArray(data.hosts) || !data.hosts.length) return [local];
    const views = data.hosts.map((entry) => {
      if (entry.local) return { ...local, name: entry.name || data.host };
      const reachable = entry.reachable !== false;
      return {
        hostId: String(entry.id), name: entry.name || String(entry.id), local: false,
        reachable, down: !reachable, lastSeen: Number.isFinite(entry.lastSeen) ? entry.lastSeen : null,
        protocol: entry.protocol,
        offset: (Number.isFinite(entry.now) ? entry.now : data.now) - Date.now(),
        root: entry.root, rootDisplay: entry.rootDisplay || entry.root, home: entry.home,
        defaultPermissionMode: entry.defaultPermissionMode, permissionModes: data.permissionModes,
        approvalsSupported: Boolean(entry.approvalsSupported),
        projects: Array.isArray(entry.projects) ? entry.projects : [],
        elsewhere: Array.isArray(entry.elsewhere) ? entry.elsewhere : [],
      };
    });
    if (!views.some((v) => v.local)) views.unshift(local);
    return views;
  }

  const localView = () => ui.views.find((v) => v.local) || null;
  const viewFor = (hostId) => ui.views.find((v) => v.hostId === (hostId || null)) || null;
  const withHost = (view, body) => (view && view.hostId ? { ...body, host: view.hostId } : body);
  // Per-host keys: project paths and session keys repeat across machines.
  const okey = (view, path) => `${view.hostId || ''}|${path}`;
  const sessionsOf = (view) => view.projects.flatMap((p) => p.sessions).concat(view.elsewhere);
  const isWaiting = (view, s) =>
    s.status === 'waiting' || (view.approvalsSupported && s.approvals && s.approvals.length);

  /** A readable line for a failed action, naming the host when the hub
      refused to proxy to it. */
  function failure(error, view) {
    const body = error.body || {};
    const named = (body.host && viewFor(body.host)) || view;
    const name = named ? named.name : 'that machine';
    if (error.status === 409 && body.error === 'protocol mismatch') {
      const versions = body.theirs != null && body.ours != null ? ` (protocol ${body.theirs}, this hub ${body.ours})` : '';
      return `${name} is running a different ClaudeShip build${versions}. `
        + 'Restart it when its sessions can end: claudeship hub stop, then claudeship hub start.';
    }
    if (error.status === 502 && body.error === 'unreachable') {
      return `${name} is unreachable right now. Try again when it is back on the network.`;
    }
    return error.message;
  }

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
    h('p', null, 'On the Mac, run ', h('code', null, 'claudeship hub link'),
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

  // The quick "+": a session in the home directory, in auto mode, for
  // work that isn't about any one project. Same look as New session,
  // without the words or the mode menu.
  const quickEl = h('button', {
    class: 'btn primary quick', 'aria-label': 'New session in your home folder', title: 'New session in your home folder (auto mode)',
    hidden: true,
    onclick: () => { const v = localView(); if (v && v.home) launch(v, v.home, 'auto'); },
  }, icon('plus'));

  const directoryEl = h('div', { class: 'directory' },
    h('header', { class: 'top' },
      h('div', { class: 'top-inner' },
        h('div', { class: 'brand' },
          h('img', { class: 'brand-mark', src: '/icon.svg', alt: '' }),
          h('span', { class: 'brand-name' }, 'ClaudeShip'),
          hostEl),
        tallyEl,
        h('div', { class: 'tools' }, searchEl, quickEl, settingsEl))),
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
    if (!response.ok) {
      const error = new Error(result.error || `request failed (${response.status})`);
      error.status = response.status;
      error.body = result;
      throw error;
    }
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
        if (fullTerminal()) location.replace('#/');
      } else {
        if (!response.ok) throw new Error(String(response.status));
        const data = await response.json();
        // An older hub may not send everything this page reads.
        data.projects = data.projects || [];
        data.elsewhere = data.elsewhere || [];
        data.permissionModes = data.permissionModes || [];
        ui.data = data;
        ui.views = buildViews(data);
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
    if (ui.menu === 'computers' && !(ui.data && Array.isArray(ui.data.hosts))) ui.menu = null;
    if (ui.menu === 'computers') loadPeers();
    quickEl.hidden = !(localView() && localView().home);
    quickEl.disabled = ui.launching;
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

  async function launch(view, path, mode, resume) {
    if (view.down) return;
    if (ui.launching) return;
    ui.launching = true;
    ui.menu = null;
    render(true);
    try {
      const body = { path };
      // Every launch from the page is auto; the mode is changed inside the
      // session (Mode in the terminal bar cycles it, as shift+tab does).
      body.permissionMode = mode || 'auto';
      if (resume) body.resume = resume;
      const { id } = await post('/api/launch', withHost(view, body));
      location.hash = termHash(id, view.hostId);
      poll(true);
    } catch (error) {
      notify(`Couldn't start a session: ${failure(error, view)}`);
    } finally {
      ui.launching = false;
      render(true);
    }
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
      data && { ...data, now: 0, hosts: data.hosts && data.hosts.map((x) => ({ ...x, now: 0 })) }, ui.error, ui.notice, ui.unpaired, ui.filter, [...ui.open], ui.menu, ui.projectsTab,
      ui.launching, [...ui.answering], data ? Math.floor(Date.now() / 15000) : 0,
    ]);
    // A routine repaint waits for an open menu or a text selection in the
    // list — but not forever, or the page would quietly go stale.
    const selection = window.getSelection();
    const held = ui.menu
      || (selection && !selection.isCollapsed && pageEl.contains(selection.anchorNode));
    if (!force && (signature === ui.signature || (held && Date.now() - ui.paintedAt < 20000))) return;
    // Under an open terminal the directory is display:none; building its
    // DOM every poll is work nobody sees. route() repaints it on the way
    // back (render(true)), and the signature stays unset so that repaint
    // isn't skipped.
    if (!force && fullTerminal()) { ui.signature = ''; return; }
    ui.signature = signature;
    ui.paintedAt = Date.now();

    const views = ui.views;
    const multi = views.length > 1;
    const local = localView() || views[0];
    const protocolBanner = (view, named) => view.protocol !== PROTOCOL && h('div', { class: 'banner' },
      named ? `${view.name} is running a different build than this page, so some things may not work. `
        : 'The hub on the Mac is running a different build than this page, so some things may not work. ',
      'Restart it when its sessions can end: ', h('code', null, 'claudeship hub stop'), ', then ',
      h('code', null, 'claudeship hub start'), '.');
    const banners = [
      ui.notice && h('button', {
        class: 'banner', 'data-key': 'notice', title: 'Dismiss',
        onclick: () => { ui.notice = null; render(true); },
      }, ui.notice),
      ui.error && h('div', { class: 'banner' }, ui.error),
      !multi && data && protocolBanner(local, false),
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

    // An unreachable host's sessions are last-known, not live: not counted.
    const counted = views.filter((v) => !v.down).map((v) => [v, sessionsOf(v)]);
    const total = counted.reduce((n, [, list]) => n + list.length, 0);
    const waiting = counted.reduce((n, [v, list]) => n + list.filter((s) => isWaiting(v, s)).length, 0);
    const busy = counted.reduce((n, [, list]) => n + list.filter((s) => s.status === 'busy').length, 0);
    updateTitle();
    hostEl.textContent = data.host;
    tallyEl.replaceChildren(
      waiting ? h('span', { class: 'pill waiting' }, h('span', { class: 'glyph waiting' }), `${waiting} need${waiting === 1 ? 's' : ''} you`) : '',
      busy ? h('span', { class: 'pill busy' }, h('span', { class: 'glyph busy' }), `${busy} working`) : '',
      h('span', { class: 'pill' }, `${total} session${total === 1 ? '' : 's'}`));
    if (ui.menu === 'computers' && settingsEl.contains(computersEl)) {
      // Moving the panel would drop the link field's focus (and a phone's
      // keyboard): refresh its list in place instead.
      paintComputers();
    } else {
      repaint(settingsEl, settings(local, multi));
    }

    const parts = [...banners];
    if (!multi) {
      const visible = local.projects.filter(matches);
      parts.push(...hostBody(local, visible));
      if (!visible.length && ui.filter) {
        parts.push(h('div', { class: 'empty' }, `No project matches “${ui.filter}”.`));
      }
    } else {
      // Several machines: everything running on top, one segment per
      // machine (this hub's first); below it the idle projects, one tab
      // per machine, so the page doesn't grow by a whole list per member.
      const ordered = [...views].sort((a, b) => Number(b.local) - Number(a.local));
      parts.push(runningAll(ordered, local), projectTabs(ordered, local));
    }
    repaint(pageEl, parts);
  }

  /** A machine's name line: the hub's own marked, an unreachable one dated. */
  function hostHead(view, local) {
    return h('div', { class: 'host-head' },
      h('h2', null, view.name),
      view.local && h('span', { class: 'tag' }, 'This hub'),
      view.down && h('span', { class: 'host-state' },
        view.lastSeen ? `Unreachable since ${ago(view.lastSeen, local)}` : 'Unreachable'));
  }

  /** A machine's version trouble, live or remembered. */
  function hostBanners(view) {
    if (view.protocol === PROTOCOL) return [];
    return [view.down
      ? h('div', { class: 'banner' }, `${view.name} was running a different build than this page (protocol ${view.protocol}).`)
      : h('div', { class: 'banner' },
          `${view.name} is running a different build than this page, so some things may not work. `,
          'Restart it when its sessions can end: ', h('code', null, 'claudeship hub stop'), ', then ',
          h('code', null, 'claudeship hub start'), '.')];
  }

  /** Every machine's running sessions, one segment per machine. */
  function runningAll(views, local) {
    let total = 0;
    const segments = [];
    for (const view of views) {
      const active = view.projects.filter(matches).filter((p) => p.sessions.length);
      const elsewhere = ui.filter ? [] : view.elsewhere;
      // Searching: a machine with nothing that matches stays out of the way.
      if (ui.filter && !active.length) continue;
      total += active.length + (elsewhere.length ? 1 : 0);
      const tiles = active.map((p) => card(p, view));
      if (elsewhere.length) {
        tiles.push(h('article', { class: `card${elsewhere.some((s) => s.status === 'waiting') ? ' attention' : ''}` },
          h('div', { class: 'card-head' },
            h('div', { class: 'card-title' }, h('h3', null, 'Elsewhere')),
            h('div', { class: 'card-path' }, `Outside ${view.rootDisplay || 'the projects folder'}`)),
          h('div', { class: 'sessions' }, elsewhere.map((s) => sessionRow(s, null, view)))));
      }
      segments.push(h('section', { class: `host running-host${view.down ? ' down' : ''}`, 'aria-label': `Running on ${view.name}` },
        hostHead(view, local),
        ...hostBanners(view),
        tiles.length
          ? h('div', { class: 'cards' }, tiles)
          : h('div', { class: 'host-idle' }, view.down ? 'Nothing was running when it was last seen.' : 'Nothing is running.')));
    }
    return h('section', { class: 'running-all' },
      h('div', { class: 'section-head' },
        h('h2', null, 'Running'),
        h('span', { class: 'count' }, String(total))),
      segments.length
        ? segments
        : h('div', { class: 'empty' }, ui.filter ? `Nothing running matches “${ui.filter}”.` : 'Nothing is running.'));
  }

  const TAB_KEY = 'claudeship.projectsTab';
  function selectedTab(views) {
    if (ui.projectsTab === undefined) {
      try { ui.projectsTab = localStorage.getItem(TAB_KEY) || ''; } catch { ui.projectsTab = ''; }
    }
    return views.find((v) => (v.hostId || '') === ui.projectsTab) || views[0];
  }
  function chooseTab(view) {
    ui.projectsTab = view.hostId || '';
    try { localStorage.setItem(TAB_KEY, ui.projectsTab); } catch { /* per-browser nicety only */ }
    render(true);
  }

  /** The idle projects, one tab per machine. */
  function projectTabs(views, local) {
    const chosen = selectedTab(views);
    const idle = (view) => view.projects.filter(matches).filter((p) => !p.sessions.length);
    const tabs = h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Computers' },
      views.map((view) => {
        const on = view === chosen;
        return h('button', {
          class: `tab${on ? ' on' : ''}${view.down ? ' down' : ''}`, role: 'tab', 'aria-selected': String(on),
          'data-key': `tab:${view.hostId || ''}`, onclick: () => chooseTab(view),
        }, h('span', { class: 'tab-name' }, view.name), h('span', { class: 'count' }, String(idle(view).length)));
      }));
    const rest = idle(chosen);
    return h('section', { class: 'projects-all' },
      h('div', { class: 'section-head' },
        h('h2', null, 'Projects'),
        h('span', { class: 'where' }, chosen.rootDisplay || '')),
      tabs,
      h('section', { class: `host projects-host${chosen.down ? ' down' : ''}`, role: 'tabpanel', 'aria-label': `Projects on ${chosen.name}` },
        chosen.down && h('div', { class: 'host-head' }, h('span', { class: 'host-state' },
          chosen.lastSeen ? `${chosen.name} unreachable since ${ago(chosen.lastSeen, local)}` : `${chosen.name} is unreachable`)),
        rest.length
          ? h('div', { class: 'list' }, rest.map((p) => row(p, chosen)))
          : h('div', { class: 'empty' }, ui.filter ? `No project on ${chosen.name} matches “${ui.filter}”.` : `Every project on ${chosen.name} is running.`),
        chosen.rootDisplay && h('div', { class: 'host-foot' },
          'Projects are the folders in ', h('code', null, chosen.rootDisplay), ' on ', h('code', null, chosen.name), '.')));
  }

  /** Running, Running elsewhere, and Projects for one host. */
  function hostBody(view, visible) {
    const active = visible.filter((p) => p.sessions.length);
    const rest = visible.filter((p) => !p.sessions.length);
    const parts = [];
    if (active.length || !ui.filter) {
      parts.push(h('section', null,
        h('div', { class: 'section-head' },
          h('h2', null, 'Running'),
          h('span', { class: 'count' }, String(active.length))),
        active.length
          ? h('div', { class: 'cards' }, active.map((p) => card(p, view)))
          : h('div', { class: 'empty' },
              'Nothing is running. Start a session below, or run ', h('code', null, 'claudeship'),
              view.local ? ' in a terminal on the Mac.' : ` in a terminal on ${view.name}.`)));
    }
    // Sessions running outside the projects folder: shown while they run,
    // never tracked otherwise, right under the projects they sit beside.
    if (view.elsewhere.length && !ui.filter) {
      parts.push(h('section', null,
        h('div', { class: 'section-head' },
          h('h2', null, 'Running elsewhere'),
          h('span', { class: 'count' }, String(view.elsewhere.length))),
        h('div', { class: 'cards' },
          h('article', { class: `card${view.elsewhere.some((s) => s.status === 'waiting') ? ' attention' : ''}` },
            h('div', { class: 'sessions' }, view.elsewhere.map((s) => sessionRow(s, null, view)))))));
    }
    if (rest.length) {
      parts.push(h('section', null,
        h('div', { class: 'section-head' },
          h('h2', null, 'Projects'),
          h('span', { class: 'count' }, String(rest.length)),
          h('span', { class: 'where' }, view.rootDisplay)),
        h('div', { class: 'list' }, rest.map((p) => row(p, view)))));
    }
    return parts;
  }

  function branchTag(name) {
    return h('span', { class: 'branch', title: name }, icon('branch'), h('span', null, name));
  }

  function card(project, data) {
    const count = project.sessions.length;
    const open = ui.open.has(okey(data, project.path));
    return h('article', { class: `card${project.sessions.some((s) => s.status === 'waiting') ? ' attention' : ''}` },
      h('div', { class: 'card-head' },
        h('div', { class: 'card-title' },
          h('h3', null, project.name),
          count > 1 && h('span', { class: 'instances' }, `${count} running`),
          project.branch && branchTag(project.branch),
          plusButton(project, data)),
        h('div', { class: 'card-path' }, tilde(project.path, data))),
      h('div', { class: 'sessions' }, project.sessions.map((s) => sessionRow(s, project, data))),
      project.recent.length > 0 && h('div', { class: 'card-foot' },
        h('span', { class: 'spacer' }),
        h('button', {
          class: 'btn quiet', 'aria-expanded': String(open), 'data-key': `earlier:${okey(data, project.path)}`,
          onclick: () => { toggleOpen(okey(data, project.path)); },
        }, 'Earlier', icon(open ? 'down' : 'right'))),
      open && recentList(project, data));
  }

  /** The "+" at the end of a running project's title: another session there. */
  function plusButton(project, data) {
    return h('button', {
      class: 'btn small plus', disabled: ui.launching || data.down, 'data-key': `new:${okey(data, project.path)}`,
      'aria-label': `New session in ${project.name}`, title: 'New session here (auto mode)',
      onclick: (event) => { event.stopPropagation(); launch(data, project.path); },
    }, icon('plus'));
  }

  function sessionRow(session, project, data) {
    const untitled = !session.title;
    const title = session.title || (session.status === 'starting' ? 'Starting…' : 'New conversation');
    const approvals = data && !data.down && data.approvalsSupported && Array.isArray(session.approvals) ? session.approvals : [];
    const meta = [h('span', { class: session.status }, STATUS[session.status] || session.status)];
    if (session.status === 'waiting' && session.waitingFor) meta.push(` — ${session.waitingFor}`);
    const extras = [];
    if (session.since) extras.push(age(session.since, data));
    if (!session.attachable) extras.push(session.background ? 'Background' : 'Terminal only');
    if (!project) extras.push(tilde(session.cwd, data));
    else if (session.sub) extras.push(session.sub);
    if (session.branch && (!project || session.branch !== project.branch)) extras.push(session.branch);
    if (session.viewers > 0) extras.push(`${session.viewers} attached`);
    for (const extra of extras) meta.push(h('span', { class: 'sep' }, '·'), extra);

    const bolt = data && data.approvalsSupported && autoApproveActive(session.autoApprove, data)
      ? h('span', { class: 'bolt', title: autoApproveText(session.autoApprove, data), 'aria-label': autoApproveText(session.autoApprove, data) }, '⚡')
      : null;
    const body = [
      h('span', { class: `glyph ${session.status}` }),
      h('span', { class: `session-title${untitled ? ' untitled' : ''}` }, title, bolt),
      approvals.length ? null : h('span', { class: 'session-meta' }, meta),
    ];
    if (approvals.length) {
      // Buttons can't nest in buttons: the row is a plain box. Open is its own
      // button beside Approve / Deny, and the title area opens the session too.
      const go = () => { location.hash = termHash(session.hubId, data.hostId); };
      const titleEl = body[1];
      if (session.attachable) {
        titleEl.setAttribute('role', 'button');
        titleEl.setAttribute('tabindex', '0');
        titleEl.setAttribute('title', 'Open this session');
        titleEl.classList.add('openable');
        titleEl.addEventListener('click', go);
        titleEl.addEventListener('keydown', (event) => { if (event.key === 'Enter') { event.preventDefault(); go(); } });
      }
      const open = session.attachable
        ? h('button', {
            class: 'btn small open', 'data-key': `session:${data.hostId || ''}:${session.key}`,
            title: 'Open this session\'s terminal',
            onclick: (event) => { event.stopPropagation(); go(); },
          }, 'Open', icon('right'))
        : null;
      body[2] = approvalBox(session, approvals, data, open);
      return h('div', { class: 'session approving' }, body);
    }
    if (session.attachable && data.down) {
      return h('div', { class: 'session external', title: `${data.name} is unreachable.` }, body,
        h('span', { class: 'session-go' }, 'Unreachable'));
    }
    if (session.attachable) {
      return h('button', {
        class: 'session', 'data-key': `session:${data.hostId || ''}:${session.key}`,
        onclick: () => { location.hash = termHash(session.hubId, data.hostId); },
      }, body, h('span', { class: 'session-go' }, 'Open', icon('right')));
    }
    const why = session.background
      ? 'A background session run by Claude Code itself.'
      : 'Started directly in a terminal, so it can only be used there. Sessions started here or with claudeship can be opened from anywhere.';
    return h('div', { class: 'session external', title: why }, body);
  }

  const autoApproveActive = (rule, view) =>
    Boolean(rule && (rule.session === true || (Number.isFinite(rule.until) && rule.until > now(view))));
  const autoApproveText = (rule, view) => rule && rule.session === true
    ? 'Approving everything for this session'
    : `Approving everything until ${new Date(rule.until - view.offset).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}`;

  /** Approve / Deny / ⋯ and the command summary, in place of the status text. */
  function approvalBox(session, approvals, view, openBtn) {
    const first = approvals[0];
    const menuId = `approve:${view.hostId || ''}:${session.key}`;
    const open = ui.menu === menuId;
    const busy = ui.answering.has(first.id) || ui.answering.has(session.sessionId);
    const extra = approvals.length > 1 ? ` (+${approvals.length - 1} more)` : '';
    // The hub's summary and detail already start with the tool's name.
    const tip = first.detail || first.summary || first.tool || '';
    const rule = (name, label, about) => h('button', {
      class: 'menu-item', role: 'menuitem', 'data-key': `rule:${session.key}:${name}`,
      onclick: () => setAutoApprove(view, session.sessionId, name),
    }, h('b', null, label), about && h('small', null, about));
    return h('div', { class: 'approval', title: tip || null },
      h('div', { class: 'approval-actions' },
        h('button', {
          class: 'btn primary small', disabled: busy, 'data-key': `allow:${session.key}`,
          onclick: () => answerApproval(view, first.id, true),
        }, 'Approve'),
        h('button', {
          class: 'btn small', disabled: busy, 'data-key': `deny:${session.key}`,
          onclick: () => answerApproval(view, first.id, false),
        }, 'Deny'),
        session.sessionId && h('div', { class: 'launch', onclick: (event) => event.stopPropagation() },
          h('button', {
            class: 'btn small more', 'aria-label': 'More approval options', 'aria-expanded': String(open),
            'data-key': `more:${session.key}`,
            onclick: () => { ui.menu = open ? null : menuId; render(true); },
          }, '⋯'),
          open && h('div', { class: 'menu right', role: 'menu' },
            rule('5m', 'Approve all for 5 minutes'),
            rule('session', 'Approve all for this session', 'Until the session ends or the hub restarts.'),
            autoApproveActive(session.autoApprove, view) && rule('off', 'Stop approving'))),
        openBtn),
      h('div', { class: 'approval-summary' }, (first.summary || first.tool || 'Permission request') + extra));
  }

  async function answerApproval(view, id, allow) {
    if (ui.answering.has(id)) return;
    ui.answering.add(id);
    render(true);
    try {
      await post('/api/approve', withHost(view, { id, allow }));
    } catch (error) {
      // 404: the terminal (or another screen) answered first, so there is
      // nothing left to answer.
      if (error.status !== 404) {
        notify(`Couldn't send the answer: ${failure(error, view)}`);
      }
    } finally {
      ui.answering.delete(id);
      poll(true);
    }
  }

  async function setAutoApprove(view, sessionId, rule) {
    ui.menu = null;
    ui.answering.add(sessionId);
    render(true);
    try {
      await post('/api/auto-approve', withHost(view, { sessionId, rule }));
    } catch (error) {
      notify(`Couldn't change the standing approval: ${failure(error, view)}`);
    } finally {
      ui.answering.delete(sessionId);
      poll(true);
    }
  }

  /** "New session" in an idle project's row. Always auto: the mode is
   *  changed inside the session, not chosen at launch. */
  function launchControl(project, data, primary) {
    const mkey = okey(data, project.path);
    return h('div', { class: 'launch', onclick: (event) => event.stopPropagation() },
      h('button', {
        class: `btn main${primary ? ' primary' : ''}`, disabled: ui.launching || data.down, 'data-key': `new:${mkey}`,
        title: 'New session here (auto mode)',
        onclick: () => launch(data, project.path),
      }, icon('plus'), 'New session'));
  }

  function recentList(project, data) {
    return h('div', { class: 'recent' },
      project.recent.length
        ? project.recent.map((conversation) => h('button', {
            class: 'recent-item', title: 'Resume this conversation in a new session', disabled: data.down,
            'data-key': `resume:${data.hostId || ''}:${conversation.sessionId}`,
            onclick: () => launch(data, project.path, null, conversation.sessionId),
          },
            h('span', { class: 'title' }, conversation.title),
            h('span', { class: 'when' }, ago(conversation.at, data)),
            h('span', { class: 'resume' }, 'Resume')))
        : h('div', { class: 'recent-none' }, 'No earlier conversations to resume.'));
  }

  function row(project, data) {
    const open = ui.open.has(okey(data, project.path));
    const toggle = () => toggleOpen(okey(data, project.path));
    return h('div', { class: `row${open ? ' open' : ''}` },
      h('div', {
        class: 'row-main', role: 'button', tabindex: '0', 'aria-expanded': String(open),
        'data-key': `row:${okey(data, project.path)}`,
        onclick: toggle,
        onkeydown: (event) => {
          if (event.target === event.currentTarget && (event.key === 'Enter' || event.key === ' ')) {
            event.preventDefault();
            toggle();
          }
        },
      },
        h('div', { class: 'row-name' }, h('b', null, project.name), project.branch && branchTag(project.branch)),
        h('div', { class: 'row-when' }, project.lastActivity ? ago(project.lastActivity, data) : ''),
        launchControl(project, data, false)),
      open && recentList(project, data));
  }

  function settings(data, multi) {
    // An older hub has no swarm: no Computers panel.
    const swarm = Boolean(ui.data && Array.isArray(ui.data.hosts));
    const computers = swarm && ui.menu === 'computers';
    const open = ui.menu === 'settings' || computers;
    if (computers) paintComputers();
    return [
      h('button', {
        class: 'icon-btn', 'aria-label': 'Settings', 'aria-expanded': String(open), 'data-key': 'gear',
        onclick: () => { ui.menu = open ? null : 'settings'; render(true); },
      }, icon('gear')),
      computers && computersEl,
      ui.menu === 'settings' && h('div', { class: 'menu right settings' },
        swarm && h('button', {
          class: 'menu-item computers-entry', 'data-key': 'computers',
          onclick: () => openComputers(),
        }, h('b', null, 'Computers…'), h('small', null, 'The machines whose sessions this page shows, and adding another')),
        // With several hosts each section carries its own footer line.
        !multi && h('div', { class: 'foot' },
          'Projects are the folders in ', h('code', null, data.rootDisplay), ' on ', h('code', null, data.name), '.')),
    ];
  }

  // ── Computers (the swarm) ────────────────────────────────
  //
  // A browser paired with this hub can't post to another one (its Origin
  // check refuses us, on purpose), so the page hands the other machine's
  // pairing link to this hub, which pairs with it and brings it into the
  // swarm (POST /api/swarm/invite). The field and the note are built once
  // and kept, like the pairing form: a repaint must not wipe a pasted link.

  const computersListEl = h('div', { class: 'computers-list' });
  const computersNoteEl = h('p', { class: 'computers-note', role: 'status', hidden: true });
  const inviteFieldEl = h('input', {
    class: 'search', name: 'link', type: 'text', placeholder: 'http://100.x.y.z:7433/auth?k=…',
    'aria-label': "The other computer's pairing link", 'data-key': 'invite-link',
    autocomplete: 'off', autocapitalize: 'off', spellcheck: 'false',
  });
  const inviteButtonEl = h('button', { class: 'btn primary', type: 'submit' }, 'Add');
  const computersEl = h('div', { class: 'menu right settings computers' },
    h('h4', null, 'Computers'),
    h('p', null, 'This page shows the sessions of every machine in this hub\'s swarm.'),
    computersListEl,
    h('h4', { class: 'computers-add' }, 'Add a computer'),
    h('p', null, 'On the other computer run ', h('code', null, 'claudeship hub link'), ' and paste the link here.'),
    h('form', { class: 'pair-form invite-form', onsubmit: (event) => { event.preventDefault(); invite(); } },
      inviteFieldEl, inviteButtonEl),
    computersNoteEl);

  function computersNote(text, isError) {
    computersNoteEl.textContent = text || '';
    computersNoteEl.hidden = !text;
    computersNoteEl.classList.toggle('error', Boolean(isError));
  }

  function openComputers() {
    ui.menu = 'computers';
    ui.removeArmed = null;
    computersNote('');
    loadPeers();
    render(true);
  }

  async function loadPeers() {
    try {
      const response = await fetch('/api/swarm/peers', { cache: 'no-store' });
      if (!response.ok) throw new Error(String(response.status));
      ui.peers = await response.json();
      ui.peersError = null;
    } catch (error) {
      ui.peersError = error.message === '404'
        ? 'This hub is an older build that cannot list its swarm here; see claudeship hub peers.'
        : "Couldn't load the list of computers.";
    }
    if (ui.menu === 'computers') paintComputers();
  }

  function addressLine(record) {
    const list = Array.isArray(record.addresses) ? record.addresses : [];
    return h('div', { class: 'computer-addr' }, list.length ? list.join('  ') : 'no address');
  }

  function protocolMark(record) {
    return record.protocol !== PROTOCOL && h('span', {
      class: 'tag warn', title: 'A different ClaudeShip build than this page: restart it when its sessions can end.',
    }, `protocol ${record.protocol}`);
  }

  function paintComputers() {
    const peers = ui.peers;
    const items = [];
    if (ui.peersError) items.push(h('div', { class: 'computer-empty' }, ui.peersError));
    else if (!peers) items.push(h('div', { class: 'computer-empty' }, 'Loading…'));
    else {
      const me = peers.self || {};
      items.push(h('div', { class: 'computer' },
        h('div', { class: 'computer-head' },
          h('b', null, me.name || 'This computer'), h('span', { class: 'tag' }, 'This hub'), protocolMark(me)),
        addressLine(me)));
      const list = (Array.isArray(peers.peers) ? peers.peers : [])
        .filter((p) => p && p.record && p.record.tombstone == null)
        .sort((a, b) => String(a.record.name).localeCompare(String(b.record.name)));
      for (const peer of list) items.push(computerRow(peer));
      if (!list.length) items.push(h('div', { class: 'computer-empty' }, 'No other computers yet.'));
    }
    repaint(computersListEl, items);
  }

  function computerRow(peer) {
    const record = peer.record;
    const id = String(record.id);
    const name = record.name || id;
    let state;
    if (peer.reachable) state = 'Reachable';
    else if (peer.refused) state = 'Refuses this swarm (it must pair again)';
    else if (Number.isFinite(record.lastSeen) && record.lastSeen > 0) state = `Unreachable since ${ago(record.lastSeen, localView())}`;
    else state = 'Not reached yet';
    const armed = ui.removeArmed === id;
    return h('div', { class: `computer${peer.reachable ? '' : ' down'}` },
      h('div', { class: 'computer-head' },
        h('b', null, name), protocolMark(record),
        h('button', {
          class: `btn quiet remove${armed ? ' confirm' : ''}`, 'data-key': `remove:${id}`,
          disabled: ui.removing === id,
          title: armed ? `Click again to remove ${name} from every computer in the swarm` : `Remove ${name}`,
          onclick: () => removeComputer(id, name),
        }, armed ? 'Remove?' : 'Remove')),
      addressLine(record),
      h('div', { class: 'computer-state' }, state));
  }

  let removeTimer = null;
  async function removeComputer(id, name) {
    if (ui.removing) return;
    if (ui.removeArmed !== id) {
      ui.removeArmed = id;
      clearTimeout(removeTimer);
      removeTimer = setTimeout(() => { ui.removeArmed = null; if (ui.menu === 'computers') paintComputers(); }, 3000);
      paintComputers();
      return;
    }
    clearTimeout(removeTimer);
    ui.removeArmed = null;
    ui.removing = id;
    paintComputers();
    try {
      await post('/api/swarm/unpair', { id });
      computersNote(`Removed ${name}. The other computers drop it within a few seconds.`, false);
    } catch (error) {
      computersNote(`Couldn't remove ${name}: ${error.message}`, true);
    }
    ui.removing = null;
    await loadPeers();
    poll(true);
  }

  async function invite() {
    const link = inviteFieldEl.value.trim();
    if (!link) {
      computersNote("Paste the other computer's pairing link first.", true);
      return;
    }
    if (inviteButtonEl.disabled) return;
    inviteButtonEl.disabled = true;
    computersNote('Adding…', false);
    try {
      const result = await post('/api/swarm/invite', { link });
      inviteFieldEl.value = '';
      computersNote(`Added ${result.name || 'the computer'}.`, false);
      await loadPeers();
      poll(true);
    } catch (error) {
      computersNote(error.status ? error.message : "Can't reach the hub on this computer.", true);
    } finally {
      inviteButtonEl.disabled = false;
    }
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
    if (event.key === 'Escape' && ui.menu && !fullTerminal()) {
      ui.menu = null;
      render(true);
    }
  });

  // ── Terminals ────────────────────────────────────────────
  //
  // Any number of sessions can be open at once, each its own xterm and
  // WebSocket. A terminal is in one of four places: `full` (over the whole
  // page — how a session opens, and the only mode on a phone), `float` (a
  // window over the directory: dragged by its bar, resized by its corner,
  // raised by a click), `min` (a chip in the dock along the bottom), or
  // `popped` (its own browser window, with a placeholder chip here). The
  // hub's size rules are per window, so the one that was last typed in,
  // resized, or made full owns the session's size, as before.

  const encoder = new TextEncoder();
  const coarse = window.matchMedia('(pointer: coarse)').matches;
  const terminals = new Map();  // terminal key (hub id, host-qualified) → terminal
  // This page is a popped-out window for one session (#/s/<id>/pop).
  const popped = location.hash.match(/^#\/s\/([0-9a-f]+)(?:@([0-9A-Za-z_-]+))?\/pop$/);
  // A terminal is named by its hub id and, off this hub, the host's id:
  // `#/s/<id>` is local, `#/s/<id>@<host>` another machine in the swarm.
  const termKey = (id, host) => (host ? `${host}:${id}` : id);
  const termHash = (id, host) => `#/s/${id}${host ? `@${host}` : ''}`;
  const popups = new Map();     // terminal key → the window this page popped it into
  let zTop = 100;
  const LAYOUT_KEY = 'claudeship.windows';

  const THEME = {
    background: '#151413', foreground: '#e9e5de', cursor: '#e9e5de', cursorAccent: '#151413',
    selectionBackground: 'rgba(223, 125, 90, 0.38)',
    black: '#1d1c1a', red: '#e06c66', green: '#7fc58b', yellow: '#e5c07b',
    blue: '#6ea8e8', magenta: '#c58fd6', cyan: '#63c3c6', white: '#d5d0c8',
    brightBlack: '#77716a', brightRed: '#f08a84', brightGreen: '#98d9a3', brightYellow: '#f0d290',
    brightBlue: '#8ebcf2', brightMagenta: '#d7a9e3', brightCyan: '#84d6d8', brightWhite: '#f5f2ec',
  };

  const fullTerminal = () => [...terminals.values()].find((t) => t.mode === 'full') || null;
  const sameGrid = (a, b) => Boolean(a && b && a.rows === b.rows && a.cols === b.cols);

  /** Build a terminal for a session and connect it. `mode` is where it goes. */
  function createTerminal(id, mode, geometry, hostId) {
    hostId = hostId || null;  // null: the hub serving this page
    const key = termKey(id, hostId);
    const existing = terminals.get(key);
    if (existing) { setMode(existing, mode); return existing; }

    const host = h('div', { class: 'term-host' });
    const note = h('div', { class: 'term-note', hidden: true });
    const over = h('div', { class: 'term-over', hidden: true });
    const project = h('div', { class: 'term-project' }, 'Session');
    const title = h('div', { class: 'term-title' });
    const glyph = h('span', { class: 'glyph' });
    const state = h('span', { class: 'term-state' });
    const end = h('button', { class: 'btn', onclick: () => endSession() }, 'End');
    // Claude Code cycles its permission mode on shift+tab; this is that
    // key with a name, beside End.
    const modeBtn = h('button', {
      class: 'btn mode', title: 'Cycle the permission mode (shift+tab)', 'aria-label': 'Cycle the permission mode',
      onpointerdown: (event) => event.preventDefault(),
      onclick: () => { send(encoder.encode('\x1b[Z')); t.term.focus(); },
    }, 'Mode');
    const keys = h('div', { class: 'keys' }, [
      ['esc', '\x1b'], ['tab', '\t'], ['⇧tab', '\x1b[Z'], ['^C', '\x03'],
      ['↑', 'A'], ['↓', 'B'], ['←', 'D'], ['→', 'C'],
      ['pgup', '\x1b[5~'], ['pgdn', '\x1b[6~'], ['⏎', '\r'],
    ].map(([label, sequence]) => h('button', {
      class: 'key', 'aria-label': label,
      // Keep focus (and the on-screen keyboard) on the terminal.
      onpointerdown: (event) => event.preventDefault(),
      onclick: () => {
        const arrow = sequence.length === 1 && 'ABCD'.includes(sequence);
        send(encoder.encode(arrow
          ? (t.term.modes.applicationCursorKeysMode ? '\x1bO' : '\x1b[') + sequence
          : sequence));
        t.term.focus();
      },
    }, label)));
    const control = (name, label, action) => h('button', {
      class: `win-btn ${name}`, 'aria-label': label, title: label,
      onpointerdown: (event) => event.stopPropagation(),  // not a drag
      onclick: action,
    }, icon(name));
    const back = h('button', { class: 'icon-btn back', 'aria-label': 'Back to projects', onclick: () => leaveTerminal(t) }, icon('left'));
    const controls = h('div', { class: 'win-controls' },
      popped ? [] : [
        control('minimize', 'Minimize', () => setMode(t, 'min')),
        control('window', 'Window', () => setMode(t, 'float')),
        control('maximize', 'Full screen', () => setMode(t, 'full')),
        control('popout', 'Pop out into its own window', () => popOut(t)),
      ],
      popped && window.opener ? control('popin', 'Pop back into the main window', () => popIn()) : [],
      control('close', 'Close (the session keeps running)', () => leaveTerminal(t)));
    const bar = h('div', { class: 'term-bar' },
      back,
      h('div', { class: 'term-id' }, glyph, h('div', { class: 'term-names' }, project, title)),
      state, modeBtn, end, controls);
    const page = h('div', { class: 'term-page', 'data-id': id },
      bar,
      h('div', { class: 'term-body' }, host, note, over),
      keys);
    document.body.append(page);

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

    const t = {
      id, hostId, key, term, fit, page, bar, host, note, over, project, title, glyph, state, end,
      mode: null,
      geometry: geometry || null,  // {x, y, w, h} of the floating window
      socket: null, ended: false,
      own: null,      // the grid this window fits
      sent: null,     // the grid the hub was last told this window fits
      size: null,     // the grid the session actually has
      owner: false,   // whether the session is sized for this screen (the hub says)
      attempts: 0, refusals: 0, connected: false, timer: null, noteTimer: null, endArmed: null,
      heard: Date.now(), pulse: null, resizeTimer: null, observer: null,
    };
    terminals.set(key, t);
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

    // Floating: a click anywhere raises the window; the bar drags it.
    page.addEventListener('pointerdown', () => raise(t), { capture: true });
    bar.addEventListener('pointerdown', (event) => beginDrag(t, event));
    // The corner grip (CSS resize) changes the box without a window
    // resize event; the observer is how we hear about it.
    t.observer = new ResizeObserver(() => {
      if (t.mode !== 'float' || t.ended) return;
      const rect = page.getBoundingClientRect();
      if (!t.geometry || (rect.width === t.geometry.w && rect.height === t.geometry.h)) return;
      t.geometry = { ...t.geometry, w: Math.round(rect.width), h: Math.round(rect.height) };
      saveLayout();
      clearTimeout(t.resizeTimer);
      // A person resizing the window is taking the size for it.
      t.resizeTimer = setTimeout(() => { measure(); claim(); }, 120);
    });
    t.observer.observe(page);

    setMode(t, mode);
    connect();
    updateTerminalBar();

    // The first measurement can run before the font has been measured;
    // take it again once layout and fonts have settled.
    const settle = () => {
      if (terminals.get(key) !== t || t.ended) return;
      measure();
      syncFit();
    };
    requestAnimationFrame(settle);
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(settle);

    // A socket can die without saying so (a phone waking up on another
    // network). Ask for a pong now and then, and start over if none comes.
    t.pulse = setInterval(() => {
      if (terminals.get(key) !== t || t.ended || document.hidden) return;
      if (!t.socket || t.socket.readyState !== WebSocket.OPEN) return;
      if (Date.now() - t.heard > 45000) reconnectNow();
      else t.socket.send(JSON.stringify({ type: 'ping' }));
    }, 15000);

    // ── Everything below closes over `t` ──

    /** Drop the current socket, whatever state it thinks it is in, and dial again. */
    function reconnectNow() {
      if (t.ended) return;
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

    /** The grid this window fits, whatever size the session currently is. */
    function measure() {
      if (t.mode === 'min') return;  // nothing to measure while hidden
      const proposed = t.fit.proposeDimensions();
      // A sliver (a phone on its side with the keyboard up) is not a size
      // worth imposing on the session; keep the last real one.
      if (proposed && proposed.rows >= 6 && proposed.cols >= 20) {
        t.own = { rows: proposed.rows, cols: proposed.cols };
      } else if (!t.own) {
        t.own = { rows: 24, cols: 80 };
      }
    }

    function connect() {
      if (t.ended) return;
      if (!t.own) measure();
      const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
      // Opening a session sizes it for this window. Coming back after a
      // dropped connection does not: the session may be in use on another
      // screen by now, and typing here takes the size back soon enough. (If
      // nobody else is attached, the hub hands it back anyway.)
      const claiming = !t.connected && !document.hidden && t.mode !== 'min';
      t.owner = claiming;  // until the hub's size message says how it went
      t.sent = { ...t.own };
      const socket = new WebSocket(
        `${scheme}://${location.host}/ws/term?id=${encodeURIComponent(t.id)}`
        + (t.hostId ? `&host=${encodeURIComponent(t.hostId)}` : '')
        + `&rows=${t.sent.rows}&cols=${t.sent.cols}&claim=${claiming ? 1 : 0}`);
      socket.binaryType = 'arraybuffer';
      t.socket = socket;
      t.heard = Date.now();
      let opened = false;
      socket.onopen = () => {
        if (terminals.get(key) !== t) return;
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
        if (terminals.get(key) !== t) return;
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
        if (terminals.get(key) !== t || t.ended || t.socket !== socket) return;
        // Another machine's terminal that never opened: the home hub refuses
        // (409) to relay to a peer on a different protocol. A browser can't
        // read the refusal, but the state says why: stop and say so.
        const remote = !opened && t.hostId ? viewFor(t.hostId) : null;
        const home = localView();
        if (remote && home && remote.protocol != null && remote.protocol !== home.protocol) {
          setNote(failure({ status: 409, body: { error: 'protocol mismatch', host: t.hostId, theirs: remote.protocol, ours: home.protocol } }, remote),
            h('button', {
              class: 'btn', onclick: () => { t.refusals = 0; t.attempts = 0; setNote('Reconnecting…'); connect(); },
            }, 'Retry'));
          return;
        }
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

    /** The session is over (ended here, ended elsewhere, or gone): the
        window goes, with a word about what happened. */
    function finished(heading, detail) {
      if (t.ended) return;
      t.ended = true;
      setNote(null);
      const wasFull = t.mode === 'full';
      disposeTerminal(t);
      if (wasFull) leaveFull();
      notify(`${heading}. ${detail}`);
      poll(true);
    }

    function setNote(text, action) {
      t.note.hidden = !text;
      delete t.note.dataset.kind;
      t.note.className = `term-note${action ? '' : ' plain'}`;
      t.note.replaceChildren(text || '', action || '');
    }

    function send(bytes) {
      if (t.ended || !t.socket || t.socket.readyState !== WebSocket.OPEN) return;
      // The hub decides whether this makes us the screen the session is
      // sized for: keystrokes do, the terminal's own replies and focus
      // reports don't.
      t.socket.send(bytes);
    }

    /** Bring the hub up to date with what this window fits: resizing the
        session if it is sized for this screen, just noting it otherwise (so
        a later keystroke here claims the right size). */
    function syncFit() {
      if (t.ended || t.mode === 'min') return;
      if (!sameGrid(t.own, t.sent) && t.socket && t.socket.readyState === WebSocket.OPEN) {
        t.sent = { ...t.own };
        t.socket.send(JSON.stringify({ type: t.owner ? 'resize' : 'fit', rows: t.own.rows, cols: t.own.cols }));
      }
      applySize();
    }

    /** Take the session's size for this window. The hub always answers. */
    function claim() {
      if (t.ended || t.mode === 'min' || !t.socket || t.socket.readyState !== WebSocket.OPEN) return;
      measure();
      t.sent = { ...t.own };
      t.socket.send(JSON.stringify({ type: 'resize', rows: t.own.rows, cols: t.own.cols }));
    }

    /** Show the session at its real size: ours, or another screen's scaled to fit. */
    function applySize() {
      if (!t.size) return;
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
        if (t.ended || t.owner || sameGrid(t.size, t.own)) return;
        if (!t.socket || t.socket.readyState !== WebSocket.OPEN) return;
        if (!t.note.hidden && t.note.dataset.kind !== 'size') return;
        setNote(`Sized for another screen (${cols}×${rows})`,
          h('button', { class: 'btn', onclick: () => { claim(); t.term.focus(); } }, 'Fit here'));
        t.note.dataset.kind = 'size';
      }, 500);
    }

    function rescale() {
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
      if (t.ended) return;
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
        await post('/api/kill', t.hostId ? { id: t.id, host: t.hostId } : { id: t.id });
      } catch (error) {
        if (terminals.get(key) !== t) return;
        t.end.disabled = false;
        t.end.textContent = 'End';
        t.end.classList.remove('confirm');
        setNote(error.status === 409 || error.status === 502
          ? failure(error, viewFor(t.hostId)) : "Couldn't end the session");
      }
    }

    /** The page is back (online, visible): make sure the socket is really alive. */
    function revive() {
      if (t.ended) return;
      if (!t.socket || t.socket.readyState !== WebSocket.OPEN) {
        reconnectNow();
      } else {
        // Time spent in the background doesn't count against the socket —
        // but a socket that died while we were away still says OPEN, so
        // give it one ping's worth of benefit of the doubt, not a minute's.
        const asked = Date.now();
        t.heard = asked;
        t.socket.send(JSON.stringify({ type: 'ping' }));
        setTimeout(() => { if (terminals.get(key) === t && !t.ended && t.heard <= asked) reconnectNow(); }, 5000);
      }
    }

    /** The window changed shape by itself (browser resize, mode change). */
    function refit(claiming) {
      if (t.ended || t.mode === 'min') return;
      measure();
      if (claiming) claim(); else syncFit();
    }

    t.api = { measure, syncFit, claim, applySize, rescale, revive, refit, reconnectNow };
    return t;
  }

  /** Detach and remove a terminal; the session keeps running. */
  function disposeTerminal(t) {
    if (terminals.get(t.key) !== t) return;
    terminals.delete(t.key);
    clearTimeout(t.timer);
    clearTimeout(t.noteTimer);
    clearTimeout(t.endArmed);
    clearTimeout(t.resizeTimer);
    clearInterval(t.pulse);
    if (t.observer) t.observer.disconnect();
    if (t.socket) { t.socket.onclose = null; t.socket.close(); }
    t.term.dispose();
    t.page.remove();
    saveLayout();
    renderDock();
    updateTitle();
  }

  // ── Window modes ──

  function setMode(t, mode) {
    if (t.ended) return;
    if (mode === 'float' && !floatingAllowed()) mode = 'full';
    if (mode === 'full' && !t.geometry) t.geometry = nextGeometry();
    const previous = t.mode;
    if (mode === 'full' && previous !== 'full') t.before = previous;  // what Back returns it to
    if (mode === previous) { if (mode === 'float') raise(t); return; }
    const other = fullTerminal();
    if (mode === 'full' && other && other !== t) setMode(other, 'float');
    t.mode = mode;
    t.page.className = `term-page ${mode}`;
    t.page.hidden = mode === 'min';
    if (mode === 'float') {
      if (!t.geometry) t.geometry = nextGeometry();
      placeWindow(t);
      raise(t);
    } else {
      t.page.style.cssText = '';
    }
    if (mode === 'full') {
      layoutViewport();
      t.page.style.zIndex = 1000;
      if (location.hash !== termHash(t.id, t.hostId) && !popped) {
        // Stamp the entry with how it was reached, as route() does.
        history.pushState({ fromDirectory: directoryShown }, '', termHash(t.id, t.hostId));
      }
      directoryEl.hidden = true;
    } else if (previous === 'full' || !previous) {
      leaveFull();
    }
    saveLayout();
    renderDock();
    updateTitle();
    if (mode !== 'min') {
      // Entering a mode is the person choosing this window: it takes the
      // size. (On a minimized window there is nothing to size for.)
      requestAnimationFrame(() => { t.api.refit(true); if (!coarse) t.term.focus(); });
    }
  }

  /** The full-screen terminal is gone: the directory is the page again
      (or, in a popped-out window, nothing is — close it). */
  function leaveFull() {
    if (fullTerminal()) return;
    if (popped) { window.close(); }
    directoryEl.hidden = false;
    if (location.hash.startsWith('#/s/') && !popped) location.replace('#/');
    render(true);
    updateTitle();
  }

  function raise(t) {
    if (t.mode !== 'float') return;
    zTop += 1;
    t.page.style.zIndex = zTop;
    t.z = zTop;
    renderDock();
  }

  /** The floating window in front: the one raised last. */
  function focusedTerminal() {
    let top = null;
    for (const t of terminals.values()) {
      if (t.mode === 'float' && (!top || (t.z || 0) > (top.z || 0))) top = t;
    }
    return top;
  }

  function placeWindow(t) {
    const g = t.geometry;
    const maxW = Math.max(320, window.innerWidth - 16);
    const maxH = Math.max(200, window.innerHeight - 16);
    g.w = Math.min(Math.max(320, g.w), maxW);
    g.h = Math.min(Math.max(200, g.h), maxH);
    g.x = Math.min(Math.max(0, g.x), window.innerWidth - g.w);
    g.y = Math.min(Math.max(0, g.y), window.innerHeight - g.h);
    t.page.style.left = `${g.x}px`;
    t.page.style.top = `${g.y}px`;
    t.page.style.width = `${g.w}px`;
    t.page.style.height = `${g.h}px`;
  }

  /** Where a new window lands: cascaded from the top-left, within the page. */
  function nextGeometry() {
    const n = [...terminals.values()].filter((t) => t.mode === 'float').length;
    const width = Math.min(760, Math.max(320, window.innerWidth - 80));
    const height = Math.min(520, Math.max(200, window.innerHeight - 120));
    return { x: 40 + 28 * (n % 8), y: 72 + 28 * (n % 8), w: width, h: height };
  }

  /** Floating windows need a pointer and room; a phone gets full screen. */
  const floatingAllowed = () => !coarse && window.innerWidth >= 720;

  function beginDrag(t, event) {
    if (t.mode !== 'float' || event.button !== 0) return;
    if (event.target.closest('button, .term-state')) return;
    const start = { x: event.clientX, y: event.clientY, gx: t.geometry.x, gy: t.geometry.y };
    const bar = t.bar;
    bar.setPointerCapture(event.pointerId);
    bar.classList.add('dragging');
    const move = (e) => {
      t.geometry.x = start.gx + (e.clientX - start.x);
      t.geometry.y = start.gy + (e.clientY - start.y);
      placeWindow(t);
    };
    const stop = () => {
      bar.classList.remove('dragging');
      bar.removeEventListener('pointermove', move);
      bar.removeEventListener('pointerup', stop);
      bar.removeEventListener('pointercancel', stop);
      saveLayout();
      t.term.focus();
    };
    bar.addEventListener('pointermove', move);
    bar.addEventListener('pointerup', stop);
    bar.addEventListener('pointercancel', stop);
    event.preventDefault();
  }

  /** Close this window: back to the directory if it was full. The session runs on. */
  function leaveTerminal(t) {
    const wasFull = t.mode === 'full';
    disposeTerminal(t);
    if (!wasFull) return;
    // Only when this history entry was reached straight from the
    // directory is "back" known to be the directory.
    if (popped) { window.close(); leaveFull(); return; }
    if (history.state && history.state.fromDirectory && history.length > 1) history.back();
    else leaveFull();
  }

  // ── Pop out / in ──

  function popOut(t) {
    const g = t.geometry || nextGeometry();
    const features = `popup=yes,width=${g.w},height=${g.h + 44},left=${window.screenX + g.x},top=${window.screenY + g.y}`;
    const url = `${location.origin}${location.pathname}${termHash(t.id, t.hostId)}/pop`;
    const popup = window.open(url, `claudeship-${t.key}`, features);
    if (!popup) { notify('The browser blocked the window. Allow pop-ups for this page and try again.'); return; }
    const id = t.key;
    const wasFull = t.mode === 'full';
    disposeTerminal(t);
    popups.set(id, { id: t.id, host: t.hostId, win: popup, geometry: g, title: t.title.textContent, project: t.project.textContent });
    if (wasFull) leaveFull();
    renderDock();
    // A popup closed by hand (the session keeps running) leaves no trace
    // but its chip; notice and drop it.
    const watch = setInterval(() => {
      const entry = popups.get(id);
      if (!entry || entry.win !== popup) { clearInterval(watch); return; }
      if (popup.closed) { clearInterval(watch); popups.delete(id); renderDock(); }
    }, 1000);
  }

  /** Bring a popped-out session back as a floating window here. */
  function popBack(id) {
    const entry = popups.get(id);
    if (!entry) return;
    popups.delete(id);
    try { entry.win.close(); } catch { /* already gone */ }
    createTerminal(entry.id, 'float', entry.geometry, entry.host);
  }

  /** From inside a popped-out window: ask the opener to take the session back. */
  function popIn() {
    const t = fullTerminal();
    if (!t || !window.opener) return;
    window.opener.postMessage({ type: 'claudeship-popin', id: t.key }, location.origin);
    disposeTerminal(t);
    window.close();
  }

  window.addEventListener('message', (event) => {
    if (event.origin !== location.origin || !event.data || event.data.type !== 'claudeship-popin') return;
    const entry = popups.get(event.data.id);
    if (!entry || entry.win !== event.source) return;
    popBack(event.data.id);
  });

  // ── Dock ──

  const dockEl = h('div', { class: 'dock', hidden: true });
  document.body.append(dockEl);

  function renderDock() {
    const chips = [];
    const front = focusedTerminal();
    for (const t of terminals.values()) {
      if (t.mode === 'full') continue;
      // Three looks: in front, behind another window, minimized.
      const look = t.mode === 'min' ? 'min' : t === front ? 'active' : 'behind';
      chips.push(h('button', {
        class: `chip ${look}`, 'data-key': `dock:${t.key}`,
        title: t.mode === 'min' ? 'Restore' : t === front ? 'In front' : 'Bring to front',
        onclick: () => { setMode(t, 'float'); },
      },
        h('span', { class: `glyph ${t.glyph.className.replace('glyph', '').trim()}` }),
        h('span', { class: 'chip-name' }, t.project.textContent),
        t.title.textContent ? h('span', { class: 'chip-title' }, t.title.textContent) : '',
        h('span', {
          class: 'chip-x', role: 'button', 'aria-label': 'Close',
          onclick: (event) => { event.stopPropagation(); leaveTerminal(t); },
        }, icon('close'))));
    }
    for (const [id, entry] of popups) {
      chips.push(h('button', {
        class: 'chip popped', 'data-key': `dock:${id}`, title: 'Popped out — click to bring it back here',
        onclick: () => popBack(id),
      },
        icon('popout'),
        h('span', { class: 'chip-name' }, entry.project),
        entry.title ? h('span', { class: 'chip-title' }, entry.title) : ''));
    }
    dockEl.hidden = chips.length === 0 || Boolean(fullTerminal());
    document.body.classList.toggle('has-dock', !dockEl.hidden);
    repaint(dockEl, chips);
  }

  // ── Layout persistence ──

  function saveLayout() {
    // Never from a screen without windows: it would overwrite the layout
    // a wide one will want back.
    if (popped || !floatingAllowed()) return;
    const windows = [...terminals.values()]
      .filter((t) => t.mode === 'float' || t.mode === 'min')
      .map((t) => ({ id: t.id, host: t.hostId, mode: t.mode, ...t.geometry }));
    try { localStorage.setItem(LAYOUT_KEY, JSON.stringify(windows)); } catch { /* private mode */ }
  }

  function restoreLayout() {
    // A phone (or a narrow window) has no windows to restore into; the
    // saved layout waits for a screen that does.
    if (popped || !floatingAllowed()) return;
    let windows = [];
    try { windows = JSON.parse(localStorage.getItem(LAYOUT_KEY) || '[]'); } catch { return; }
    for (const w of windows) {
      if (typeof w.id !== 'string' || !/^[0-9a-f]+$/.test(w.id)) continue;
      const geometry = [w.x, w.y, w.w, w.h].every(Number.isFinite) ? { x: w.x, y: w.y, w: w.w, h: w.h } : null;
      // Entries saved before the swarm have no host: the local hub.
      const host = typeof w.host === 'string' && /^[0-9A-Za-z_-]+$/.test(w.host) ? w.host : null;
      createTerminal(w.id, w.mode === 'min' ? 'min' : 'float', geometry, host);
    }
  }

  // ── Title bar text, page title ──

  function updateTerminalBar() {
    if (!ui.data) return;
    for (const t of terminals.values()) {
      let found = null;
      let owner = null;
      const view = viewFor(t.hostId);
      if (!view) continue;
      for (const project of view.projects) {
        for (const session of project.sessions) {
          if (session.hubId === t.id) { found = session; owner = project; }
        }
      }
      if (!found) found = view.elsewhere.find((s) => s.hubId === t.id) || null;
      if (!found) continue;
      t.project.textContent = owner ? owner.name : tilde(found.cwd, view);
      t.title.textContent = found.title || '';
      t.title.hidden = !found.title;
      t.glyph.className = `glyph ${found.status}`;
      t.state.className = `term-state ${found.status}`;
      t.state.replaceChildren(STATUS[found.status] || found.status,
        found.since ? h('span', { class: 'long' }, ` · ${age(found.since, view)}`) : '');
      t.status = found.status;
    }
    renderDock();
    updateTitle();
  }

  function updateTitle() {
    const t = fullTerminal();
    if (t) {
      document.title = `${t.status === 'waiting' ? '(!) ' : ''}${t.title.textContent || t.project.textContent} — ClaudeShip`;
    } else if (ui.data) {
      const waiting = ui.views.filter((v) => !v.down)
        .reduce((n, v) => n + sessionsOf(v).filter((s) => isWaiting(v, s)).length, 0);
      document.title = `${waiting ? `(${waiting}) ` : ''}ClaudeShip — ${ui.data.host}`;
    }
  }

  window.addEventListener('online', () => reviveTerminals());
  function reviveTerminals() {
    for (const t of terminals.values()) t.api.revive();
  }

  // ── Viewport, routing, polling ───────────────────────────

  /** Keep the full-screen terminal inside what's actually visible — on a
      phone, the part of the screen the keyboard leaves. */
  function layoutViewport() {
    const viewport = window.visualViewport;
    const height = viewport ? viewport.height : window.innerHeight;
    document.documentElement.style.setProperty('--vh', `${height}px`);
    const t = fullTerminal();
    if (t) {
      t.page.style.top = `${viewport ? viewport.offsetTop : 0}px`;
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
      for (const t of terminals.values()) {
        // Too narrow for windows now: they go to the dock, not full screen.
        if (t.mode === 'float' && !floatingAllowed()) { setMode(t, 'min'); continue; }
        if (t.mode === 'float') placeWindow(t);
        t.api.refit(false);
      }
      renderDock();
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
    const match = location.hash.match(/^#\/s\/([0-9a-f]+)(?:@([0-9A-Za-z_-]+))?(\/pop)?$/);
    if (match) {
      // Stamp a new history entry with how it was reached; an entry come
      // back to (Back/Forward, reload) keeps the stamp it has.
      if (history.state == null) {
        history.replaceState({ fromDirectory: directoryShown && !fullTerminal() }, '');
      }
      createTerminal(match[1], 'full', null, match[2]);
    } else {
      directoryShown = true;
      const t = fullTerminal();
      // Back from a window made full returns it to its window, and from
      // a dock chip made full (the phone's way) back to the dock: it was
      // kept on purpose. From a session opened full, Back closes it.
      if (t && t.before === 'min') setMode(t, 'min');
      else if (t && t.before === 'float' && floatingAllowed()) setMode(t, 'float');
      else if (t) disposeTerminal(t);
      directoryEl.hidden = false;
      render(true);
      updateTitle();
    }
  }
  window.addEventListener('hashchange', route);

  // With a terminal over the page only its status bar needs the directory,
  // and a slower poll keeps the hub's scan off the phone's radio.
  let pollTick = 0;
  setInterval(() => {
    if (document.hidden) return;
    pollTick += 1;
    if (fullTerminal() && pollTick % 3 !== 0) return;
    poll(false);
  }, 2000);
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) return;
    poll(false);
    reviveTerminals();
  });

  if (popped) document.body.classList.add('popped');
  layoutViewport();
  restoreLayout();
  route();
  poll(true);
})();
