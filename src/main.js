// Tauri 2 global API (requires `app.withGlobalTauri: true` in tauri.conf.json).
// If the bridge is missing, every UI button silently fails — fail loudly instead.
if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
  document.body.innerHTML =
    '<pre style="padding:20px;color:#b32424;background:#fff;font-family:monospace">' +
    'Tauri bridge not available.\n\n' +
    'window.__TAURI__ = ' + JSON.stringify(window.__TAURI__) + '\n\n' +
    'Make sure tauri.conf.json has `"app": { "withGlobalTauri": true }`, then rebuild.' +
    '</pre>';
  throw new Error("Tauri bridge missing");
}
const invoke = window.__TAURI__.core.invoke;

window.addEventListener("error", (e) => console.error("[ui]", e.error || e.message));
window.addEventListener("unhandledrejection", (e) => console.error("[ui] promise", e.reason));

// ============================================================================
// state
// ============================================================================

const state = {
  graphs: [],
  currentGraph: null,
  nodes: [],
  edges: [],
  selectedNodeId: null,
  searchHits: null,    // null = not searching, [] = no hits, [id,...] = matching node ids (tree order)
  searchQuery: "",
  searchCursor: -1,    // index into searchHits for Enter-cycling (browser-find style)
  collapsed: new Set(),// node ids whose reply-children are hidden (outline collapse)
  index: null,         // last buildTreeIndex() result (for scroll / ancestor expansion)
};

// Persisted UI flags
const LS = {
  sidebar() { return localStorage.getItem("ui.sidebar") !== "0"; },
  setSidebar(v) { localStorage.setItem("ui.sidebar", v ? "1" : "0"); },
  pathpane() { return localStorage.getItem("ui.pathpane") !== "0"; },
  setPathpane(v) { localStorage.setItem("ui.pathpane", v ? "1" : "0"); },
};

// ============================================================================
// utilities
// ============================================================================

const $ = (sel) => document.querySelector(sel);

function fmtDate(s) { try { return new Date(s).toLocaleString(); } catch { return s; } }

function escapeHtml(s) {
  return (s || "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

// Highlight FTS5 search terms inside arbitrary text. Splits on whitespace —
// matches the implicit-AND semantics ("foo bar" = AND of foo and bar).
function highlight(text, query) {
  if (!query || !text) return escapeHtml(text);
  const terms = query.trim().split(/\s+/).filter(Boolean);
  if (!terms.length) return escapeHtml(text);
  const esc = escapeHtml(text);
  let out = esc;
  for (const t of terms) {
    const re = new RegExp("(" + t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "gi");
    out = out.replace(re, '<mark class="hl">$1</mark>');
  }
  return out;
}

// 10-char base62 id. 62^10 ≈ 8e17 — collisions are astronomically unlikely.
function genAppId() {
  const alpha = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  const bytes = crypto.getRandomValues(new Uint8Array(10));
  let s = "";
  for (let i = 0; i < 10; i++) s += alpha[bytes[i] % alpha.length];
  return s;
}

// Create a node with auto-generated app_id; retry on the (astronomically rare) collision.
async function createNodeAuto(graphId, content, parentNodeId) {
  let lastErr;
  for (let attempt = 0; attempt < 5; attempt++) {
    const appId = genAppId();
    try {
      return await invoke("create_node", { graphId, appId, content, parentNodeId });
    } catch (e) {
      lastErr = e;
      const msg = String(e || "");
      if (!msg.includes("already exists")) throw e;   // unrelated error → bail
    }
  }
  throw new Error("Failed to allocate unique app_id after 5 attempts: " + lastErr);
}

function debounce(fn, ms) {
  let t;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

// ============================================================================
// modal
// ============================================================================

function modal({ title, body, okLabel = "OK", onValidate }) {
  return new Promise((resolve) => {
    const m = $("#modal");
    $("#modal-title").textContent = title;
    const bodyEl = $("#modal-body");
    bodyEl.className = "modal-body";
    bodyEl.innerHTML = "";
    bodyEl.appendChild(body);
    $("#modal-ok").textContent = okLabel;
    m.classList.remove("hidden");

    const firstField = bodyEl.querySelector("textarea, input, button");
    if (firstField && firstField.tagName !== "BUTTON") firstField.focus();

    function cleanup(result) {
      m.classList.add("hidden");
      $("#modal-ok").onclick = null;
      $("#modal-cancel").onclick = null;
      bodyEl.innerHTML = "";
      resolve(result);
    }
    $("#modal-ok").onclick = () => {
      const v = onValidate ? onValidate() : true;
      if (v === false) return;
      cleanup(v);
    };
    $("#modal-cancel").onclick = () => cleanup(null);
  });
}

// Modal that takes only a content textarea. Used for both new comment & reply.
async function contentModal({ title, placeholder = "", initial = "", okLabel = "OK" }) {
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Content
      <textarea id="cm-content" rows="6" placeholder="${escapeHtml(placeholder)}">${escapeHtml(initial)}</textarea>
    </label>
    <small class="muted">app_id is generated automatically.</small>
  `;
  return modal({
    title,
    body: div,
    okLabel,
    onValidate() {
      const v = div.querySelector("#cm-content").value;
      if (!v.trim()) return false;
      return { content: v };
    },
  });
}

// Modal for picking the *target* of a reference edge — a list of existing nodes
// (since users no longer remember app_ids).
async function refTargetModal(fromNode) {
  const candidates = state.nodes.filter((n) => n.id !== fromNode.id);
  if (!candidates.length) { alert("This graph has no other nodes to reference yet."); return null; }
  const div = document.createElement("div");
  div.innerHTML = `
    <p class="muted">Pick the target node — adds a <b>ref</b> edge from <code>${escapeHtml(fromNode.app_id)}</code> to it. This may close a cycle.</p>
    <input id="cm-filter" type="search" placeholder="Filter…" autofocus />
    <ul id="cm-list" class="picker-list"></ul>
    <label>Edge label (optional)
      <input id="cm-label" placeholder="e.g. depends-on, contradicts" />
    </label>
  `;
  let picked = null;
  function render(filter = "") {
    const f = filter.toLowerCase();
    const ul = div.querySelector("#cm-list");
    const items = candidates.filter(
      (n) => n.app_id.toLowerCase().includes(f) || n.content.toLowerCase().includes(f),
    );
    ul.innerHTML = items.map((n) =>
      `<li data-id="${n.id}">
         <span class="app-id">${escapeHtml(n.app_id)}</span>
         <span>${escapeHtml(n.content.length > 120 ? n.content.slice(0, 120) + "…" : n.content)}</span>
       </li>`).join("");
    for (const li of ul.querySelectorAll("li")) {
      li.onclick = () => {
        picked = Number(li.dataset.id);
        ul.querySelectorAll("li").forEach((x) => x.classList.remove("picked"));
        li.classList.add("picked");
      };
    }
  }
  render();
  div.querySelector("#cm-filter").oninput = (e) => render(e.target.value);
  return modal({
    title: "Reference an existing node (⟲)",
    body: div,
    okLabel: "Add reference",
    onValidate() {
      if (!picked) { alert("Select a target node first."); return false; }
      const target = candidates.find((n) => n.id === picked);
      const label = div.querySelector("#cm-label").value || "";
      return { target, label };
    },
  });
}

// ============================================================================
// graphs
// ============================================================================

async function loadGraphs() {
  state.graphs = await invoke("list_graphs");
  renderGraphList();
}

function renderGraphList() {
  const ul = $("#graph-list");
  ul.innerHTML = "";
  for (const g of state.graphs) {
    const li = document.createElement("li");
    if (state.currentGraph?.id === g.id) li.classList.add("active");
    li.innerHTML = `<div class="gname">${escapeHtml(g.name)}</div><div class="gdate">${fmtDate(g.updated_at)}</div>`;
    li.onclick = () => selectGraph(g.id);
    ul.appendChild(li);
  }
}

async function selectGraph(id) {
  state.currentGraph = state.graphs.find((g) => g.id === id) || null;
  state.selectedNodeId = null;
  state.searchHits = null;
  state.searchQuery = "";
  state.searchCursor = -1;
  state.collapsed = new Set();
  $("#content-search").value = "";
  $("#search-count").textContent = "";
  $("#from-input").value = "";
  $("#to-input").value = "";
  $("#path-results").innerHTML = "";
  renderGraphList();
  if (!state.currentGraph) return;
  $("#graph-title").textContent = state.currentGraph.name;
  $("#graph-desc").textContent = state.currentGraph.description || "";
  await reloadNodesAndEdges();
}

async function reloadNodesAndEdges() {
  if (!state.currentGraph) return;
  const [nodes, edges] = await Promise.all([
    invoke("list_nodes", { graphId: state.currentGraph.id }),
    invoke("list_edges", { graphId: state.currentGraph.id }),
  ]);
  state.nodes = nodes;
  state.edges = edges;
  renderTree();
}

// ============================================================================
// tree (unified notes + replies with inline actions)
// ============================================================================

function buildTreeIndex() {
  const replyParent = new Map();          // child id -> parent id
  const outgoingByFrom = new Map();        // node id -> [edge]
  for (const e of state.edges) {
    if (e.kind === "reply") replyParent.set(e.to_node_id, e.from_node_id);
    if (!outgoingByFrom.has(e.from_node_id)) outgoingByFrom.set(e.from_node_id, []);
    outgoingByFrom.get(e.from_node_id).push(e);
  }
  const children = new Map();
  for (const n of state.nodes) {
    const p = replyParent.get(n.id);
    if (p) (children.get(p) || children.set(p, []).get(p)).push(n);
  }
  const roots = state.nodes.filter((n) => !replyParent.has(n.id));
  return { roots, children, outgoingByFrom, replyParent };
}

// Walk reply-parents upward from a node (nearest ancestor first).
function ancestorsOf(nodeId, replyParent) {
  const out = [];
  let p = replyParent.get(nodeId);
  while (p != null) { out.push(p); p = replyParent.get(p); }
  return out;
}

// DFS visit order of node ids, exactly as renderTree lays them out. Used so
// Enter-cycling through search hits walks top-to-bottom like browser find.
function treeOrder(roots, children) {
  const order = [];
  const visit = (n) => {
    order.push(n.id);
    for (const k of children.get(n.id) || []) visit(k);
  };
  for (const r of roots) visit(r);
  return order;
}

function renderTree() {
  const ul = $("#nodes-tree");
  ul.innerHTML = "";
  if (state.nodes.length === 0) {
    ul.innerHTML = `<li class="muted" style="padding:10px">No nodes yet. Click <b>+ New comment</b> to start.</li>`;
    return;
  }

  const idx = buildTreeIndex();
  state.index = idx;
  const { roots, children, outgoingByFrom } = idx;
  const matchIds = new Set(state.searchHits || []);
  const currentMatch = (state.searchHits && state.searchCursor >= 0)
    ? state.searchHits[state.searchCursor] : null;
  const nodeById = new Map(state.nodes.map((n) => [n.id, n]));

  function nodeLi(n) {
    const li = document.createElement("li");
    const card = document.createElement("div");
    card.className = "node-card";
    if (state.selectedNodeId === n.id) card.classList.add("selected");
    if (matchIds.has(n.id)) card.classList.add("match");
    if (currentMatch === n.id) card.classList.add("match-current");

    const contentHtml = state.searchQuery
      ? highlight(n.content, state.searchQuery)
      : escapeHtml(n.content);

    const kids = children.get(n.id) || [];
    const isCollapsed = state.collapsed.has(n.id);
    // Outline disclosure triangle: ▼ expanded, ▶ collapsed, blank spacer for leaves.
    const toggleHtml = kids.length
      ? `<button class="tree-toggle" data-node="${n.id}" title="Collapse / expand">${isCollapsed ? "▶" : "▼"}</button>`
      : `<span class="tree-toggle leaf"></span>`;

    const outgoing = outgoingByFrom.get(n.id) || [];
    const edgesHtml = outgoing.length === 0 ? "" : `
      <div class="outgoing-edges">
        ${outgoing.map((e) => {
          const target = nodeById.get(e.to_node_id);
          const tlabel = target ? target.app_id : "?";
          return `<span class="edge-pill ${e.kind}">
                    ${e.kind === "ref" ? "⟲" : "↳"} ${escapeHtml(tlabel)}${e.label ? " [" + escapeHtml(e.label) + "]" : ""}
                    <button data-edge="${e.id}" title="Delete edge">×</button>
                  </span>`;
        }).join("")}
      </div>`;

    card.innerHTML = `
      <div class="node-row">
        ${toggleHtml}
        <span class="app-id">${escapeHtml(n.app_id)}</span>
        <div class="content-preview collapsed" data-node="${n.id}">${contentHtml}</div>
        <div class="row-actions">
          <button class="icon-btn act-reply" data-node="${n.id}" title="Reply (↳)">↳</button>
          <button class="icon-btn act-ref"   data-node="${n.id}" title="Reference (⟲) — adds a cycle">⟲</button>
          <button class="icon-btn act-edit"  data-node="${n.id}" title="Edit content">✎</button>
          <button class="icon-btn act-del danger" data-node="${n.id}" title="Delete node">✕</button>
        </div>
      </div>
      ${edgesHtml}
    `;

    // wire actions
    const toggleBtn = card.querySelector(".tree-toggle");
    if (toggleBtn && kids.length) {
      toggleBtn.onclick = (e) => {
        e.stopPropagation();
        if (state.collapsed.has(n.id)) state.collapsed.delete(n.id);
        else state.collapsed.add(n.id);
        renderTree();
      };
    }
    card.querySelector(".act-reply").onclick = (e) => { e.stopPropagation(); doReply(n); };
    card.querySelector(".act-ref").onclick   = (e) => { e.stopPropagation(); doAddRef(n); };
    card.querySelector(".act-edit").onclick  = (e) => { e.stopPropagation(); doEdit(n); };
    card.querySelector(".act-del").onclick   = (e) => { e.stopPropagation(); doDelete(n); };
    for (const btn of card.querySelectorAll("button[data-edge]")) {
      btn.onclick = async (ev) => {
        ev.stopPropagation();
        await invoke("delete_edge", { edgeId: Number(btn.dataset.edge) });
        await reloadNodesAndEdges();
      };
    }

    // click on content toggles expand/collapse + selection
    const cp = card.querySelector(".content-preview");
    cp.onclick = () => {
      state.selectedNodeId = n.id;
      cp.classList.toggle("collapsed");
      renderTree();
    };

    li.appendChild(card);
    if (kids.length && !isCollapsed) {
      const sub = document.createElement("ul");
      for (const k of kids) sub.appendChild(nodeLi(k));
      li.appendChild(sub);
    }
    return li;
  }

  for (const r of roots) ul.appendChild(nodeLi(r));
}

function scrollToNode(nodeId) {
  state.selectedNodeId = nodeId;
  // Expand every collapsed ancestor so the target is actually rendered.
  const idx = state.index || buildTreeIndex();
  for (const a of ancestorsOf(nodeId, idx.replyParent)) state.collapsed.delete(a);
  renderTree();
  // expand the selected card's content
  setTimeout(() => {
    const el = document.querySelector(`#nodes-tree .content-preview[data-node="${nodeId}"]`);
    if (el) {
      el.classList.remove("collapsed");
      el.closest(".node-card")?.scrollIntoView({ behavior: "smooth", block: "center" });
    }
  }, 0);
}

// ============================================================================
// per-node actions
// ============================================================================

async function doReply(node) {
  const r = await contentModal({
    title: `Reply to ${node.app_id}`,
    placeholder: "Your reply…",
    okLabel: "Reply",
  });
  if (!r) return;
  try {
    const created = await createNodeAuto(state.currentGraph.id, r.content, node.id);
    await reloadNodesAndEdges();
    scrollToNode(created.id);
  } catch (e) { alert(e); }
}

async function doAddRef(node) {
  const r = await refTargetModal(node);
  if (!r) return;
  try {
    await invoke("add_ref_edge", {
      graphId: state.currentGraph.id,
      fromNodeId: node.id,
      toAppId: r.target.app_id,
      label: r.label,
    });
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

async function doEdit(node) {
  const r = await contentModal({
    title: `Edit ${node.app_id}`,
    initial: node.content,
    okLabel: "Save",
  });
  if (!r) return;
  try {
    await invoke("update_node", { nodeId: node.id, content: r.content });
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

async function doDelete(node) {
  if (!confirm(`Delete node "${node.app_id}"? Replies under it will also be deleted.`)) return;
  try {
    await invoke("delete_node", { nodeId: node.id });
    if (state.selectedNodeId === node.id) state.selectedNodeId = null;
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

// ============================================================================
// top-level actions
// ============================================================================

$("#new-graph-btn").onclick = async () => {
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Name <input id="cm-name" placeholder="e.g. Q2 strategy" /></label>
    <label>Description <input id="cm-desc" placeholder="Optional" /></label>
  `;
  const r = await modal({
    title: "New graph",
    body: div,
    okLabel: "Create",
    onValidate() {
      const name = div.querySelector("#cm-name").value.trim();
      if (!name) return false;
      return { name, desc: div.querySelector("#cm-desc").value };
    },
  });
  if (!r) return;
  const g = await invoke("create_graph", { name: r.name, description: r.desc });
  await loadGraphs();
  await selectGraph(g.id);
};

$("#rename-graph").onclick = async () => {
  if (!state.currentGraph) return;
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Name <input id="cm-name" value="${escapeHtml(state.currentGraph.name)}" /></label>
    <label>Description <input id="cm-desc" value="${escapeHtml(state.currentGraph.description)}" /></label>
  `;
  const r = await modal({
    title: "Rename graph",
    body: div,
    okLabel: "Save",
    onValidate() {
      const name = div.querySelector("#cm-name").value.trim();
      if (!name) return false;
      return { name, desc: div.querySelector("#cm-desc").value };
    },
  });
  if (!r) return;
  await invoke("rename_graph", { id: state.currentGraph.id, name: r.name, description: r.desc });
  await loadGraphs();
  await selectGraph(state.currentGraph.id);
};

$("#delete-graph").onclick = async () => {
  if (!state.currentGraph) return;
  if (!confirm(`Delete graph "${state.currentGraph.name}"? This removes all nodes and edges.`)) return;
  await invoke("delete_graph", { id: state.currentGraph.id });
  state.currentGraph = null;
  $("#graph-title").textContent = "Pick or create a graph";
  $("#graph-desc").textContent = "";
  $("#nodes-tree").innerHTML = "";
  $("#path-results").innerHTML = "";
  await loadGraphs();
};

$("#new-root").onclick = async () => {
  if (!state.currentGraph) { alert("Pick or create a graph first."); return; }
  const r = await contentModal({
    title: "New top-level comment",
    placeholder: "Your thought…",
    okLabel: "Create",
  });
  if (!r) return;
  try {
    const created = await createNodeAuto(state.currentGraph.id, r.content, null);
    await reloadNodesAndEdges();
    scrollToNode(created.id);
  } catch (e) { alert(e); }
};

// ============================================================================
// import outline (Markdown headings + indented bullets → node tree)
// ============================================================================
//
// Parses an outline like:
//   # Title
//   _Article: foo_
//   - > "quote"
//     - child a
//       - grandchild
// into a tree of nodes linked by `reply` edges. Indentation depth defines
// nesting; `#` headings become top-level roots that bullets nest under.

function parseOutline(text) {
  // Phase 1: raw items with an "effective indent". Headings get indent -1 so any
  // following content (indent ≥ 0) nests beneath the most recent heading.
  const raw = [];
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    let m;
    if ((m = line.match(/^\s*(#+)\s+(.*)$/))) {
      raw.push({ indent: -1, text: m[2].trim() });
    } else if ((m = line.match(/^(\s*)[-*+]\s+(.*)$/))) {
      let t = m[2].trim().replace(/^>\s*/, "");   // drop a leading blockquote marker
      raw.push({ indent: m[1].length, text: t });
    } else {
      const lead = line.match(/^(\s*)/)[1].length;
      raw.push({ indent: lead, text: line.trim() });
    }
  }
  // Phase 2: assign parents via an indent stack (nearest shallower ancestor).
  const items = [];           // { text, parentIndex }
  const stack = [];           // { indent, itemIndex }
  for (const r of raw) {
    while (stack.length && stack[stack.length - 1].indent >= r.indent) stack.pop();
    const parentIndex = stack.length ? stack[stack.length - 1].itemIndex : null;
    const itemIndex = items.length;
    items.push({ text: r.text, parentIndex });
    stack.push({ indent: r.indent, itemIndex });
  }
  return items;
}

async function importOutlineText(text) {
  const items = parseOutline(text);
  if (!items.length) { alert("Nothing to import — the outline is empty."); return; }
  const createdIds = [];      // parallel to items: itemIndex → created node id
  let first = null;
  for (const it of items) {
    const parentId = it.parentIndex == null ? null : createdIds[it.parentIndex];
    const node = await createNodeAuto(state.currentGraph.id, it.text, parentId ?? null);
    createdIds.push(node.id);
    if (first == null) first = node.id;
  }
  await reloadNodesAndEdges();
  if (first != null) scrollToNode(first);
}

$("#import-outline").onclick = async () => {
  if (!state.currentGraph) { alert("Pick or create a graph first."); return; }
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Outline (Markdown — headings &amp; indented bullets)
      <textarea id="cm-outline" rows="14" placeholder="# Title&#10;- top level&#10;  - child&#10;    - grandchild"></textarea>
    </label>
    <small class="muted">Indentation defines nesting; each line becomes a node linked by reply edges.</small>
  `;
  const r = await modal({
    title: "Import outline",
    body: div,
    okLabel: "Import",
    onValidate() {
      const v = div.querySelector("#cm-outline").value;
      if (!v.trim()) return false;
      return { text: v };
    },
  });
  if (!r) return;
  try { await importOutlineText(r.text); }
  catch (e) { alert(e); }
};

// ============================================================================
// content search (client-side substring, space = AND)
// ============================================================================

// In-tree search is done entirely client-side with case-insensitive substring
// matching (space = AND). This keeps the hit count, the yellow highlight, and
// Enter-navigation perfectly in sync — the FTS5 path used to disagree with the
// substring highlighter (e.g. CJK runs tokenized as one word reported 0 matches
// while text was still highlighted).
function computeMatches(q) {
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
  if (!terms.length) return [];
  const idx = state.index || buildTreeIndex();
  const order = treeOrder(idx.roots, idx.children);
  const nodeById = new Map(state.nodes.map((n) => [n.id, n]));
  const hits = [];
  for (const id of order) {
    const n = nodeById.get(id);
    if (!n) continue;
    const hay = (n.app_id + " " + n.content).toLowerCase();
    if (terms.every((t) => hay.includes(t))) hits.push(id);
  }
  return hits;
}

function updateSearchCount() {
  const el = $("#search-count");
  const hits = state.searchHits;
  if (!hits) { el.textContent = ""; return; }
  if (!hits.length) { el.textContent = "0 matches"; return; }
  const pos = state.searchCursor >= 0 ? `${state.searchCursor + 1}/` : "";
  el.textContent = `${pos}${hits.length} match${hits.length === 1 ? "" : "es"}`;
}

const runSearch = debounce(() => {
  if (!state.currentGraph) return;
  const q = $("#content-search").value.trim();
  state.searchQuery = q;
  if (!q) {
    state.searchHits = null;
    state.searchCursor = -1;
    updateSearchCount();
    renderTree();
    return;
  }
  const hits = computeMatches(q);
  state.searchHits = hits;
  state.searchCursor = hits.length ? 0 : -1;
  updateSearchCount();
  renderTree();
  if (hits.length) scrollToNode(hits[0]);   // jump to first match (browser-find style)
}, 150);

$("#content-search").oninput = runSearch;

// Enter cycles to the next match (Shift+Enter to the previous), wrapping around
// — like pressing Enter repeatedly in a browser's in-page find.
$("#content-search").addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;
  e.preventDefault();
  const hits = state.searchHits;
  if (!hits || !hits.length) return;
  const step = e.shiftKey ? -1 : 1;
  state.searchCursor = (state.searchCursor + step + hits.length) % hits.length;
  updateSearchCount();
  scrollToNode(hits[state.searchCursor]);
});

// ============================================================================
// export / render
// ============================================================================

$("#export-gv").onclick = async () => {
  if (!state.currentGraph) return;
  try {
    const path = await invoke("export_gv", { graphId: state.currentGraph.id });
    alert(`Exported:\n${path}`);
  } catch (e) { alert(e); }
};
$("#render-pdf").onclick = async () => {
  if (!state.currentGraph) return;
  try { await invoke("render_and_open", { graphId: state.currentGraph.id, format: "pdf" }); }
  catch (e) { alert(e); }
};
$("#open-graphviz").onclick = async () => {
  if (!state.currentGraph) return;
  try { await invoke("open_in_graphviz_app", { graphId: state.currentGraph.id }); }
  catch (e) { alert(e); }
};

// ============================================================================
// pane toggles
// ============================================================================

function applyPaneState() {
  $("#app").classList.toggle("no-sidebar", !LS.sidebar());
  document.querySelector(".panes").classList.toggle("no-search", !LS.pathpane());
  $("#toggle-sidebar").textContent = LS.sidebar() ? "◀" : "▶";
  $("#toggle-pathpane").textContent = LS.pathpane() ? "▶" : "◀";
}
$("#toggle-sidebar").onclick = () => { LS.setSidebar(!LS.sidebar()); applyPaneState(); };
$("#toggle-pathpane").onclick = () => { LS.setPathpane(!LS.pathpane()); applyPaneState(); };

// ============================================================================
// path search (keyword DFS)
// ============================================================================
//
// Mirrors parse_gv_and_search_dfs_all_paths_save_gv.go: take two keywords,
// enumerate every directed simple path from any node matching the From keyword
// to any node matching the To keyword.

const MAX_PATHS = 50;

$("#search-paths").onclick = async () => {
  if (!state.currentGraph) { alert("Pick or create a graph first."); return; }
  const fromKw = $("#from-input").value.trim();
  const toKw = $("#to-input").value.trim();
  if (!fromKw) { alert("Enter a From keyword."); return; }
  if (!toKw)   { alert("Enter a To keyword."); return; }
  try {
    const hits = await invoke("find_paths_by_keyword", {
      graphId: state.currentGraph.id,
      fromKeyword: fromKw,
      toKeyword: toKw,
      maxPaths: MAX_PATHS,
    });
    renderPathResults({ fromKw, toKw, hits });
  } catch (e) {
    $("#path-results").innerHTML = `<p class="muted" style="padding:8px">${escapeHtml(String(e))}</p>`;
  }
};

function renderPathResults({ fromKw, toKw, hits }) {
  const box = $("#path-results");
  const header = `<div class="resolved-note" style="display:flex; align-items:center; justify-content:space-between; gap:8px">
       <span><code>${escapeHtml(fromKw)}</code> → <code>${escapeHtml(toKw)}</code> · ${hits.length} path${hits.length === 1 ? "" : "s"}</span>
       ${hits.length ? `<button id="open-paths-gv" class="primary" style="padding:4px 8px; font-size:11px">📈 Open Graphviz</button>` : ""}
     </div>`;
  if (!hits.length) {
    box.innerHTML = header + `<p class="muted" style="padding:8px">No path found.</p>`;
    return;
  }
  box.innerHTML = header + hits.map((h, i) => {
    const parts = [];
    for (let j = 0; j < h.nodes.length; j++) {
      if (j > 0) {
        const step = h.steps[j - 1];
        const glyph = step.kind === "ref" ? "⟲" : "↳";
        const cls = step.kind === "ref" ? "ref" : "reply";
        parts.push(`<span class="arrow ${cls}" title="${step.kind}${step.label ? " · " + escapeHtml(step.label) : ""}"> ${glyph} </span>`);
      }
      const n = h.nodes[j];
      const preview = (n.content || "").replace(/\s+/g, " ").slice(0, 60);
      parts.push(`<span class="step" data-node="${n.id}" title="${escapeHtml(n.content.slice(0, 200))}"><span class="app-id">${escapeHtml(n.app_id)}</span> <span class="step-preview">${escapeHtml(preview)}</span></span>`);
    }
    return `<div class="path-item"><b>Path ${i + 1}</b> · ${h.nodes.length - 1} step(s)<br/>${parts.join("")}</div>`;
  }).join("");

  for (const s of box.querySelectorAll(".step[data-node]")) {
    s.style.cursor = "pointer";
    s.onclick = () => scrollToNode(Number(s.dataset.node));
  }

  const openBtn = box.querySelector("#open-paths-gv");
  if (openBtn) {
    openBtn.onclick = async () => {
      openBtn.disabled = true;
      const origText = openBtn.textContent;
      openBtn.textContent = "Rendering…";
      try {
        await invoke("render_paths_by_keyword_and_open", {
          graphId: state.currentGraph.id,
          fromKeyword: fromKw,
          toKeyword: toKw,
          maxPaths: MAX_PATHS,
          format: "pdf",
        });
      } catch (e) { alert(e); }
      finally { openBtn.disabled = false; openBtn.textContent = origText; }
    };
  }
}

// Enter in either input triggers the search.
for (const id of ["#from-input", "#to-input"]) {
  $(id).addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); $("#search-paths").click(); }
  });
}

// ============================================================================
// backup & versions
// ============================================================================
//
// "Backup now" writes every graph as an outline file into
// ~/Documents/my-thoughtgraph and git-commits it. Restoring a version
// re-imports its outlines as NEW graphs (never overwrites existing ones).

let backupSelectedHash = null;

function setBackupStatus(msg, cls = "") {
  const el = $("#backup-status");
  el.textContent = msg || "";
  el.className = "backup-status muted" + (cls ? " " + cls : "");
}

function openBackup() {
  $("#backup-overlay").classList.remove("hidden");
  setBackupStatus("");
  refreshBackupHistory();
}
function closeBackup() {
  $("#backup-overlay").classList.add("hidden");
}

async function refreshBackupHistory(selectFirst = false) {
  const ul = $("#backup-history");
  let commits = [];
  try {
    commits = await invoke("backup_history", { limit: 100 });
  } catch (e) {
    ul.innerHTML = `<li class="empty">${escapeHtml(String(e))}</li>`;
    return;
  }
  if (!commits.length) {
    ul.innerHTML = `<li class="empty">No backups yet. Click <b>Backup now</b>.</li>`;
    $("#backup-detail").innerHTML = `<p class="muted" style="padding:12px">No versions yet.</p>`;
    return;
  }
  ul.innerHTML = commits.map((c) => `
    <li data-hash="${c.hash}">
      <div class="bk-date">${escapeHtml(fmtDate(c.date))}</div>
      <div class="bk-msg">${escapeHtml(c.message)}</div>
    </li>`).join("");
  for (const li of ul.querySelectorAll("li[data-hash]")) {
    li.onclick = () => {
      ul.querySelectorAll("li").forEach((x) => x.classList.remove("active"));
      li.classList.add("active");
      showBackupCommit(li.dataset.hash);
    };
  }
  // auto-open the most recent (or keep current selection)
  const target = (selectFirst || !backupSelectedHash)
    ? commits[0].hash
    : (commits.find((c) => c.hash === backupSelectedHash)?.hash || commits[0].hash);
  const targetLi = ul.querySelector(`li[data-hash="${target}"]`);
  if (targetLi) { targetLi.classList.add("active"); showBackupCommit(target); }
}

async function showBackupCommit(hash) {
  backupSelectedHash = hash;
  const box = $("#backup-detail");
  box.innerHTML = `<p class="muted" style="padding:12px">Loading…</p>`;
  let detail;
  try {
    detail = await invoke("backup_commit_detail", { hash });
  } catch (e) {
    box.innerHTML = `<p class="muted" style="padding:12px">${escapeHtml(String(e))}</p>`;
    return;
  }
  const filesHtml = detail.files.length
    ? detail.files.map((f) =>
        `<div class="bk-file-name">${escapeHtml(f.path)}</div><pre>${escapeHtml(f.content)}</pre>`).join("")
    : `<p class="muted">This version has no graph files.</p>`;
  box.innerHTML = `
    <div class="bk-detail-head">
      <div class="bk-stat">${escapeHtml(detail.files.length + " graph file(s) in this version")}</div>
      <button id="bk-restore" class="primary">↺ Restore as new graphs</button>
    </div>
    ${filesHtml}
  `;
  const btn = box.querySelector("#bk-restore");
  if (btn) btn.onclick = () => restoreBackupVersion(detail);
}

// Build a brand-new graph from one outline file. The `# heading` becomes the
// graph name; its descendants become the node tree. Non-destructive.
async function createGraphFromOutline(text, suffix) {
  const items = parseOutline(text);
  if (!items.length) return null;
  const titleIdx = items.findIndex((it) => it.parentIndex == null);
  const baseName = titleIdx >= 0 ? items[titleIdx].text : "Restored";
  const name = `${baseName} ${suffix}`.trim();
  const g = await invoke("create_graph", { name, description: "" });
  const createdIds = [];            // item index → node id (null for the title)
  for (let i = 0; i < items.length; i++) {
    if (i === titleIdx) { createdIds.push(null); continue; }
    const it = items[i];
    // children of the title become roots; everything else keeps its parent
    const parentId = (it.parentIndex == null || it.parentIndex === titleIdx)
      ? null : createdIds[it.parentIndex];
    const node = await createNodeAuto(g.id, it.text, parentId ?? null);
    createdIds.push(node.id);
  }
  return g;
}

async function restoreBackupVersion(detail) {
  const files = (detail.files || []).filter((f) => f.path.endsWith(".md"));
  if (!files.length) { setBackupStatus("Nothing to restore in this version.", "err"); return; }
  if (!confirm(`Restore ${files.length} graph(s) from this version as NEW graphs? Your current graphs stay untouched.`)) return;
  const suffix = `(restored ${new Date().toLocaleDateString()})`;
  setBackupStatus("Restoring…");
  let last = null, count = 0;
  try {
    for (const f of files) {
      const g = await createGraphFromOutline(f.content, suffix);
      if (g) { last = g; count++; }
    }
  } catch (e) { setBackupStatus(String(e), "err"); return; }
  await loadGraphs();
  closeBackup();
  if (last) await selectGraph(last.id);
  setBackupStatus("");
  alert(`Restored ${count} graph(s) as new graphs.`);
}

$("#open-backup").onclick = openBackup;
$("#backup-close").onclick = closeBackup;
$("#backup-overlay").addEventListener("click", (e) => {
  if (e.target.id === "backup-overlay") closeBackup();   // click backdrop to close
});
$("#backup-open-folder").onclick = async () => {
  try { await invoke("open_backup_dir"); }
  catch (e) { setBackupStatus(String(e), "err"); }
};
$("#backup-now").onclick = async () => {
  const btn = $("#backup-now");
  btn.disabled = true;
  setBackupStatus("Backing up…");
  try {
    const r = await invoke("backup_now");
    setBackupStatus(
      r.committed
        ? `✓ ${r.message}  ·  ${r.path}`
        : `${r.message}  ·  ${r.path}`,
      "ok",
    );
    backupSelectedHash = r.hash || backupSelectedHash;
    await refreshBackupHistory(true);
  } catch (e) {
    setBackupStatus(String(e), "err");
  } finally {
    btn.disabled = false;
  }
};

// ============================================================================
// boot
// ============================================================================

applyPaneState();
loadGraphs();
