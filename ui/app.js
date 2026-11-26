// concord web UI. No build step, no framework. All text written by people or agents is set
// through textContent (never innerHTML): task text, notes and messages are untrusted.

const app = document.getElementById("app");
const state = { me: null };

// ---------- helpers ----------

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: { "content-type": "application/json", "x-concord-ui": "1" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) {
    const err = new Error((data && data.error && data.error.message) || `${res.status} ${res.statusText}`);
    err.status = res.status;
    err.data = data;
    throw err;
  }
  return data;
}

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") el.className = v;
    else if (k === "on") for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
    else if (k === "value") el.value = v;
    else if (k === "checked") el.checked = Boolean(v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid === null || kid === undefined || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

// replaceChildren() turns null into the text "null"; this drops empty slots first.
function fill(el, ...kids) {
  el.replaceChildren(...kids.flat(Infinity).filter((k) => k !== null && k !== undefined && k !== false));
}

const SVG = "http://www.w3.org/2000/svg";
function s(tag, attrs, ...kids) {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, String(v));
  for (const kid of kids) el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  return el;
}

function icon(name, cls = "") {
  return s("svg", { class: `icon ${cls}`, "aria-hidden": "true" }, s("use", { href: `/ui/icons.svg#i-${name}` }));
}

function brandMark() {
  return s("svg", { class: "brand-mark", viewBox: "0 0 32 32", "aria-hidden": "true" },
    s("circle", { cx: 16, cy: 16, r: 13, fill: "none", stroke: "currentColor", "stroke-width": 2 }),
    s("circle", { cx: 16, cy: 16, r: 8, fill: "none", stroke: "currentColor", "stroke-width": 2 }),
    s("path", { d: "M12 16.5l3 3 5-6", fill: "none", stroke: "currentColor", "stroke-width": 2, "stroke-linecap": "round", "stroke-linejoin": "round" }));
}

const enc = encodeURIComponent;

function ago(iso) {
  if (!iso) return "never";
  const sec = (Date.now() - Date.parse(iso)) / 1000;
  if (sec < 60) return "just now";
  if (sec < 3600) return `${Math.round(sec / 60)} min ago`;
  if (sec < 86400) return `${Math.round(sec / 3600)} h ago`;
  return `${Math.round(sec / 86400)} d ago`;
}

function until(iso) {
  const sec = (Date.parse(iso) - Date.now()) / 1000;
  if (sec <= 0) return "due now";
  if (sec < 3600) return `in ${Math.ceil(sec / 60)} min`;
  if (sec < 86400) return `in ${Math.round(sec / 3600)} h`;
  return `in ${Math.round(sec / 86400)} d`;
}

// Agents get exact RFC 3339 times; people get "Nov 26, 10:41 (in 30 min)".
function humanTimes(text) {
  return text.replace(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z/g, (iso) => `${stamp(iso)} (${until(iso)})`);
}

function stamp(iso) {
  return iso ? new Date(iso).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }) : "";
}

function dur(secs) {
  if (secs % 86400 === 0) return `${secs / 86400}d`;
  if (secs % 3600 === 0) return `${secs / 3600}h`;
  if (secs % 60 === 0) return `${secs / 60}m`;
  return `${secs}s`;
}

function toast(message, bad = false) {
  const t = h("div", { class: `toast${bad ? " bad" : ""}`, role: "status" }, message);
  document.body.append(t);
  setTimeout(() => t.remove(), 4200);
}

async function busy(btn, fn) {
  btn.disabled = true;
  try { return await fn(); } finally { btn.disabled = false; }
}

function copyButton(text, label = "Copy") {
  const b = h("button", { class: "btn btn-quiet btn-sm", type: "button" }, icon("copy"), label);
  b.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = h("textarea", { class: "hidden" });
      ta.value = text;
      document.body.append(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    b.replaceChildren(icon("check"), "Copied");
    setTimeout(() => b.replaceChildren(icon("copy"), label), 1500);
  });
  return b;
}

function openDialog({ title, body, actions, wide = false }) {
  const dlg = h("dialog", { class: wide ? "wide" : "" });
  const close = () => { if (dlg.open) dlg.close(); };
  dlg.append(
    h("div", { class: "dlg-head" },
      h("h2", {}, title),
      h("div", { class: "head-actions" },
        h("button", { class: "btn btn-quiet icon-btn", type: "button", "aria-label": "Close", on: { click: close } }, icon("x")))),
    h("div", { class: "dlg-body" }, body),
    actions ? h("div", { class: "dlg-foot" }, actions(close)) : null,
  );
  dlg.addEventListener("close", () => dlg.remove());
  document.body.append(dlg);
  dlg.showModal();
  return { dlg, close, body: dlg.querySelector(".dlg-body") };
}

function field(label, control, hint) {
  return h("label", { class: "field" }, h("span", {}, label), control, hint ? h("small", {}, hint) : null);
}

function input(attrs = {}) {
  return h("input", { class: "input", ...attrs });
}

function select(options, value, attrs = {}) {
  const el = h("select", { class: "input", ...attrs },
    options.map(([v, label]) => h("option", { value: v }, label)));
  if (value !== undefined && value !== null) el.value = value;
  return el;
}

const ROLES = [["viewer", "Viewer"], ["member", "Member"], ["lead", "Lead"], ["admin", "Admin"]];
const ROLE_HINT = "Viewer reads only. Member works on tasks. Lead also approves plan changes. Admin also manages people and connectors";

// ---------- seal: outcome stamp (the signature detail) ----------

const OUTCOMES = {
  agreed: ["A", "Agreed"],
  silence: ["S", "No objection before the deadline"],
  policy: ["R", "Decided by the rule"],
  status_quo: ["Q", "Nothing changed"],
  withdrawn: ["W", "Withdrawn"],
  stale: ["X", "No longer applied"],
  humans: ["H", "Decided by the humans"],
  admin: ["O", "Decided by the hub operator"],
  escalation_timeout: ["T", "Escalation timed out"],
  note: ["N", "Note"],
};

function seal(n, size = 28) {
  const closed = n.status === "closed";
  const cls = closed ? (n.outcome || "") : n.status;
  const letter = closed ? (OUTCOMES[n.outcome] || ["·"])[0] : n.status === "escalated" ? "!" : `R${n.round || 1}`;
  const label = closed ? (OUTCOMES[n.outcome] || ["", n.outcome])[1] : n.status === "escalated" ? "With the humans" : "Open";
  return s("svg", { class: `seal ${cls}`, width: size, height: size, viewBox: "0 0 32 32", role: "img", "aria-label": label },
    s("title", {}, label),
    s("circle", { class: "outer", cx: 16, cy: 16, r: 14.5, "stroke-width": 1.5 }),
    s("circle", { cx: 16, cy: 16, r: 11, "stroke-width": 0.75 }),
    s("text", { x: 16, y: 16.5 }, letter));
}

function passSeal(size = 56) {
  return s("svg", { class: "seal", width: size, height: size, viewBox: "0 0 32 32", "aria-hidden": "true" },
    s("circle", { class: "outer", cx: 16, cy: 16, r: 14.5, "stroke-width": 1.5 }),
    s("circle", { cx: 16, cy: 16, r: 11, "stroke-width": 0.75 }),
    s("path", { d: "M11.5 16.5l3 3 6-7", fill: "none", stroke: "currentColor", "stroke-width": 1.5, "stroke-linecap": "round", "stroke-linejoin": "round" }));
}

function describe(c) {
  switch (c.op) {
    case "accept_task": return `Accept ${c.task_id} into the plan`;
    case "update_task": return `Edit ${c.task_id}: ${Object.keys(c.patch || {}).join(", ")}`;
    case "assign": return `Give ${c.task_id} to ${c.to}`;
    case "split": return `Split ${c.task_id} into: ${(c.parts || []).map((p) => p.title + (p.assignee ? ` (${p.assignee})` : "")).join("; ")}`;
    case "cancel_task": return `Cancel ${c.task_id}`;
    case "set_goal": return "Replace the goal";
    case "add_milestone": return `Add milestone “${c.title}”`;
    case "update_milestone": return `Edit milestone ${c.milestone_id}`;
    case "remove_milestone": return `Remove milestone ${c.milestone_id}`;
    case "set_area": return c.shared ? `Make area ${c.name} shared` : c.owner ? `Give area ${c.name} to ${c.owner}` : `Edit area ${c.name}`;
    default: return c.op;
  }
}

// ---------- routing ----------

function route() {
  return location.hash.replace(/^#\/?/, "").split("/").filter(Boolean).map(decodeURIComponent);
}

window.addEventListener("hashchange", () => render());

async function boot() {
  try {
    state.me = await api("GET", "/api/v1/me");
  } catch (e) {
    state.me = null;
  }
  render();
}

// Each render gets a number; a slower, older render never overwrites a newer one.
let renderGen = 0;

async function render() {
  const gen = ++renderGen;
  if (!state.me) {
    renderSignin();
    return;
  }
  const [first, a, b, c] = route();
  let page;
  try {
    if (first === "ns" && a) page = await namespacePage(a, b || "overview", c);
    else if (first === "access") page = await accessPage();
    else page = await namespacesPage();
  } catch (e) {
    if (gen !== renderGen) return;
    if (e.status === 401) {
      state.me = null;
      renderSignin();
      return;
    }
    page = { node: h("div", { class: "content" }, h("div", { class: "empty" }, e.message)), title: "Something went wrong" };
  }
  if (gen !== renderGen) return;
  app.replaceChildren(shell(page));
}

function shell(page) {
  const [first, a] = route();
  const me = state.me;
  const navItem = (href, iconName, label, current, count) =>
    h("a", { class: "nav-item", href, "aria-current": current ? "page" : null },
      icon(iconName), h("span", { class: "label" }, label), count !== undefined ? h("span", { class: "count" }, count) : null);
  return h("div", { class: "shell" },
    h("nav", { class: "side", "aria-label": "Main" },
      h("a", { class: "brand", href: "#/" }, brandMark(), "concord"),
      h("div", { class: "nav-group" },
        h("div", { class: "nav-label" }, "Namespaces"),
        navItem("#/", "layers", "All namespaces", !first || first === "namespaces"),
        me.namespaces.map((n) => navItem(`#/ns/${enc(n.id)}`, "chevron-right", n.name, first === "ns" && a === n.id))),
      h("div", { class: "nav-group" },
        h("div", { class: "nav-label" }, "Hub"),
        navItem("#/access", "key-round", "People and tokens", first === "access")),
      h("div", { class: "side-foot" },
        h("div", { class: "who" },
          h("div", {}, me.display_name),
          h("div", { class: "mono muted" }, me.is_admin ? `${me.principal} · hub admin` : me.principal)),
        h("button", {
          class: "btn btn-quiet icon-btn", type: "button", "aria-label": "Sign out", title: "Sign out",
          on: { click: async () => { await api("POST", "/ui/logout"); state.me = null; location.hash = "#/"; render(); } },
        }, icon("log-out")))),
    h("main", { class: "main" }, page.node));
}

async function refreshMe() {
  state.me = await api("GET", "/api/v1/me");
}

// ---------- sign in ----------

function renderSignin() {
  const token = input({ type: "password", placeholder: "cnc_…", autocomplete: "off", "aria-label": "Access token", class: "input mono" });
  const note = h("p", { class: "note", role: "alert" });
  const submit = h("button", { class: "btn btn-primary", type: "submit" }, "Sign in");
  const form = h("form", { class: "signin-form" },
    h("a", { class: "brand", href: "#/" }, brandMark(), "concord"),
    h("h1", {}, "One plan for agents of different people"),
    h("p", {}, "Create namespaces, give people access, connect task trackers and follow how the agents split the work and settle disagreements"),
    field("Access token", token, "The hub prints an admin token on first start. Everyone else gets a token from an admin"),
    h("div", { class: "row" }, submit),
    note);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    note.textContent = "";
    await busy(submit, async () => {
      try {
        await api("POST", "/ui/login", { token: token.value.trim() });
        await refreshMe();
        location.hash = "#/";
        render();
      } catch (err) {
        note.className = "note bad";
        note.textContent = err.status === 401 ? "This token is not valid or was revoked" : err.message;
      }
    });
  });
  const sample = {
    id: "N-14", status: "closed", outcome: "agreed", risk: "safe", proposer: "alice", round: 2, max_rounds: 4, applied_round: 2,
    participants: ["alice", "bob"], awaiting: [], deadline: new Date(Date.now() + 9e5).toISOString(),
    explanation: "all participants accepted; applied round 2",
    rule: "",
    offers: [
      { round: 1, author: "alice", changes: [{ op: "assign", task_id: "T-31", to: "alice" }], note: { from: "alice", untrusted_text: "I know the payment API" } },
      { round: 2, author: "bob", changes: [{ op: "split", task_id: "T-31", parts: [{ title: "Checkout UI", assignee: "bob" }, { title: "Payment calls", assignee: "alice" }] }], note: { from: "bob", untrusted_text: "Let's split it: I keep the UI" } },
    ],
    stances: { alice: { on_current_round: "accept" }, bob: { on_current_round: "accept" } },
  };
  app.replaceChildren(h("div", { class: "signin" }, form,
    h("aside", { class: "signin-art", "aria-hidden": "true" }, negotiationPanel(sample, null, false))));
  token.focus();
}

// ---------- namespaces ----------

async function namespacesPage() {
  const data = await api("GET", "/api/v1/namespaces");
  const list = data.namespaces;
  const head = h("header", { class: "page-head" },
    h("div", {}, h("h1", { class: "page-title" }, "Namespaces"),
      h("div", { class: "page-sub" }, list.length === 1 ? "1 namespace you can reach" : `${list.length} namespaces you can reach`)),
    h("div", { class: "head-actions" },
      data.can_create ? h("button", { class: "btn btn-primary", type: "button", on: { click: newNamespaceDialog } }, icon("plus"), "New namespace") : null));
  const body = list.length === 0
    ? h("div", { class: "empty" }, data.can_create ? "No namespaces yet. Create the first one" : "You have no access to a namespace yet. Ask a hub admin")
    : h("table", { class: "table" },
      h("thead", {}, h("tr", {},
        h("th", {}, "Namespace"), h("th", {}, "Goal"), h("th", {}, "Your role"),
        h("th", { class: "num" }, "Members"), h("th", { class: "num" }, "Open tasks"), h("th", { class: "num" }, "Negotiations"), h("th", {}, "Last activity"))),
      h("tbody", {}, list.map((n) => h("tr", { class: "link", on: { click: () => { location.hash = `#/ns/${enc(n.id)}`; } } },
        h("td", {}, h("div", { class: "cell-main" }, h("span", {}, n.name), h("span", { class: "mono muted" }, n.id))),
        h("td", { class: "dim" }, h("div", { class: "clamp" }, n.goal || "No goal yet")),
        h("td", { class: "dim" }, n.role),
        h("td", { class: "num" }, n.members),
        h("td", { class: "num" }, n.open_tasks),
        h("td", { class: "num" }, n.open_negotiations),
        h("td", { class: "dim" }, ago(n.last_activity_at))))));
  return { node: h("div", {}, head, h("div", { class: "content" }, body)) };
}

function newNamespaceDialog() {
  const id = input({ placeholder: "shop", autocomplete: "off", class: "input mono" });
  const name = input({ placeholder: "Shop MVP" });
  const goal = h("textarea", { class: "input", placeholder: "What the people behind the agents want to build" });
  const turn = input({ value: "30m", class: "input mono" });
  const esc = input({ value: "24h", class: "input mono" });
  const lease = input({ value: "2h", class: "input mono" });
  const rounds = input({ value: "4", type: "number", min: "1", max: "20", class: "input mono" });
  const note = h("p", { class: "note" });
  openDialog({
    title: "New namespace",
    body: [
      h("div", { class: "form-grid" },
        field("Id", id, "Lowercase letters, digits and dashes. Part of the MCP address"),
        field("Name", name),
        h("div", { class: "wide" }, field("Goal", goal))),
      h("div", { class: "section-head" }, h("h3", { class: "section-title" }, "Negotiation rules")),
      h("div", { class: "form-grid" },
        field("Time to answer a proposal", turn), field("Time for humans on escalations", esc),
        field("Lease on a claimed task", lease), field("Rounds per negotiation", rounds)),
      note,
    ],
    actions: (close) => {
      const ok = h("button", { class: "btn btn-primary", type: "button" }, "Create namespace");
      ok.addEventListener("click", () => busy(ok, async () => {
        try {
          const r = await api("POST", "/api/v1/namespaces", {
            id: id.value.trim(), name: name.value.trim(), goal: goal.value, turn_timeout: turn.value, escalation_timeout: esc.value,
            lease: lease.value, max_rounds: Number(rounds.value) || 4,
          });
          close();
          await refreshMe();
          location.hash = `#/ns/${enc(r.created)}`;
          render();
        } catch (e) { note.className = "note bad"; note.textContent = e.message; }
      }));
      return [h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), ok];
    },
  });
  id.focus();
}

// ---------- one namespace ----------

async function namespacePage(id, tab, sub) {
  const plan = await api("GET", `/api/v1/namespaces/${enc(id)}`);
  const role = plan.your_role;
  const isAdmin = role === "admin";
  const negCount = plan.open_negotiations.length;
  const tabs = [["overview", "Overview"], ["negotiations", "Negotiations", negCount], ["members", "Members", plan.team.length], ["connectors", "Connectors"]];
  if (isAdmin) tabs.push(["settings", "Settings"]);
  const head = h("header", { class: "page-head" },
    h("div", {},
      h("h1", { class: "page-title" }, plan.namespace.name),
      h("div", { class: "page-sub mono" }, `${plan.namespace.id} · you are ${role}`)),
    h("div", { class: "head-actions" },
      h("div", { class: "copyline", title: "MCP address for agents" }, h("code", {}, plan.mcp_url), copyButton(plan.mcp_url))));
  const tabBar = h("nav", { class: "tabs", "aria-label": "Namespace" },
    tabs.map(([key, label, count]) => h("a", { class: "tab", href: `#/ns/${enc(id)}/${key}`, "aria-current": tab === key ? "page" : null },
      label, count ? h("span", { class: "count" }, count) : null)));
  let body;
  if (tab === "negotiations") body = await negotiationsTab(id, plan, sub);
  else if (tab === "members") body = await membersTab(id, plan);
  else if (tab === "connectors") body = await connectorsTab(id, plan);
  else if (tab === "settings" && isAdmin) body = settingsTab(id, plan);
  else body = overviewTab(id, plan);
  return { node: h("div", {}, head, tabBar, h("div", { class: "content" }, body)) };
}

function taskRow(t) {
  const closed = ["done", "cancelled", "rejected"].includes(t.status);
  return h("tr", { class: `task-row${closed ? " closed" : ""}` },
    h("td", {}, h("span", { class: `dot ${t.status}`, title: t.status })),
    h("td", { class: "mono muted" }, t.id),
    h("td", {}, h("div", { class: "cell-main" }, h("span", { class: "clamp" }, t.title))),
    h("td", { class: "dim" }, t.status.replace("_", " ")),
    h("td", { class: "dim" }, t.area || "shared"),
    h("td", { class: "dim" }, t.assignee || ""),
    h("td", { class: "num" }, t.priority));
}

function taskTable(tasks) {
  if (!tasks.length) return h("div", { class: "empty" }, "No tasks here yet");
  return h("table", { class: "table tasks" },
    h("thead", {}, h("tr", {},
      h("th", { class: "c-dot" }, ""), h("th", { class: "c-id" }, "Id"), h("th", {}, "Task"), h("th", { class: "c-status" }, "Status"),
      h("th", { class: "c-area" }, "Area"), h("th", { class: "c-who" }, "Assignee"), h("th", { class: "num c-prio" }, "Priority"))),
    h("tbody", {}, tasks.map(taskRow)));
}

function overviewTab(id, plan) {
  const c = plan.task_counts || {};
  const n = (k) => c[k] || 0;
  const canWrite = plan.your_role !== "viewer";
  const stat = (label, value, key = false) =>
    h("div", { class: "stat" }, h("div", { class: "stat-label" }, label), h("div", { class: `stat-value${key ? " key" : ""}` }, value));
  const groups = plan.milestones.map((m) => h("div", { class: "section" },
    h("div", { class: "section-head" }, h("h3", { class: "section-title" }, `${m.title}`), h("span", { class: "mono muted" }, `${m.id} · ${m.status}`)),
    taskTable(m.tasks)));
  if (plan.unscheduled_tasks.length || !plan.milestones.length) {
    groups.push(h("div", { class: "section" },
      h("div", { class: "section-head" }, h("h3", { class: "section-title" }, plan.milestones.length ? "Without a milestone" : "Tasks")),
      taskTable(plan.unscheduled_tasks)));
  }
  const decisions = plan.recent_decisions.slice(0, 6);
  return [
    h("section", { class: "section" }, h("p", { class: "goal" }, plan.goal || "No goal yet. Agents set it with update_plan, admins in Settings")),
    h("section", { class: "section stats" },
      stat("Open", n("open") + n("proposed")),
      stat("In progress", n("claimed") + n("in_progress") + n("blocked")),
      stat("Waiting for review", n("review")),
      stat("Open negotiations", plan.open_negotiations.length, true)),
    h("section", { class: "section" },
      h("div", { class: "section-head" },
        h("h2", { class: "section-title" }, "Plan"),
        canWrite ? h("div", { class: "head-actions" },
          h("button", { class: "btn", type: "button", on: { click: () => importDialog(id, plan) } }, icon("upload"), "Import tasks"),
          h("button", { class: "btn btn-primary", type: "button", on: { click: () => newTaskDialog(id, plan) } }, icon("plus"), "New task")) : null),
      groups),
    decisions.length ? h("section", { class: "section" },
      h("div", { class: "section-head" }, h("h2", { class: "section-title" }, "Recent decisions")),
      h("div", { class: "neg-list" }, decisions.map((d) => h("div", { class: "neg-item" },
        seal({ status: "closed", outcome: d.outcome }),
        h("div", {}, h("div", { class: "clamp" }, d.summary), h("div", { class: "mono muted" }, `${d.actor} · ${ago(d.created_at)}`)))))) : null,
  ];
}

function taskForm(plan) {
  const title = input({ placeholder: "Fix 500 on empty cart" });
  const description = h("textarea", { class: "input", placeholder: "What and why. Steps to reproduce for bugs" });
  const area = select([["", "Shared"], ...plan.areas.map((a) => [a.name, a.owner ? `${a.name} · ${a.owner}` : a.name])], "");
  const milestone = select([["", "None"], ...plan.milestones.map((m) => [m.id, `${m.id} · ${m.title}`])], "");
  const kind = select([["feature", "Feature"], ["bug", "Bug"], ["chore", "Chore"], ["followup", "Follow-up"]], "feature");
  const priority = select([["p0", "P0 urgent"], ["p1", "P1 high"], ["p2", "P2 normal"], ["p3", "P3 later"]], "p2");
  const assignee = select([["", "Anyone (open pool)"], ...plan.team.filter((m) => m.role !== "viewer").map((m) => [m.id, m.display_name])], "");
  const node = h("div", { class: "form-grid" },
    h("div", { class: "wide" }, field("Title", title)),
    h("div", { class: "wide" }, field("Description", description)),
    field("Area", area), field("Milestone", milestone), field("Kind", kind), field("Priority", priority),
    h("div", { class: "wide" }, field("Assignee", assignee)));
  const value = () => ({
    title: title.value.trim(), description: description.value, area: area.value || undefined, milestone: milestone.value || undefined,
    kind: kind.value, priority: priority.value, assignee: assignee.value || undefined,
  });
  return { node, value, focus: () => title.focus() };
}

function newTaskDialog(id, plan) {
  const form = taskForm(plan);
  const note = h("p", { class: "note" },
    ["lead", "admin"].includes(plan.your_role) ? "As a lead you add it straight to the plan, in any area" : "In someone else's area this becomes a proposal its owner answers");
  openDialog({
    title: "New task",
    body: [form.node, note],
    actions: (close) => {
      const ok = h("button", { class: "btn btn-primary", type: "button" }, "Add task");
      ok.addEventListener("click", () => busy(ok, async () => {
        try {
          const r = await api("POST", `/api/v1/namespaces/${enc(id)}/tasks`, form.value());
          close();
          toast(r.proposals.length ? `${r.created[0].id} filed as a proposal` : `${r.created[0].id} added`);
          render();
        } catch (e) { note.className = "note bad"; note.textContent = e.message; }
      }));
      return [h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), ok];
    },
  });
  form.focus();
}

function importDialog(id, plan) {
  const text = h("textarea", { class: "input code", placeholder: "- [ ] Checkout page #p1 @frontend\n  Cart summary and pay button\n- [ ] Rate limit login #bug @backend" });
  const file = input({ type: "file", accept: ".md,.markdown,.txt,.csv,.json" });
  const format = select([["auto", "Detect"], ["markdown", "Markdown list"], ["csv", "CSV"], ["json", "JSON"]], "auto");
  const preview = h("div", {});
  const note = h("p", { class: "note" }, "Markdown: one task per top-level item, indented lines are the description, tags #bug #p1 @area !M-2. CSV needs a title column");
  file.addEventListener("change", async () => {
    const f = file.files[0];
    if (!f) return;
    text.value = await f.text();
    if (f.name.endsWith(".csv")) format.value = "csv";
    else if (f.name.endsWith(".json")) format.value = "json";
  });
  let parsed = null;
  openDialog({
    title: "Import tasks",
    wide: true,
    body: [h("div", { class: "row" }, field("File", file), field("Format", format)), field("Or paste the list", text), note, preview],
    actions: (close) => {
      const go = h("button", { class: "btn btn-primary", type: "button", disabled: true }, "Create tasks");
      const check = h("button", { class: "btn", type: "button" }, "Preview");
      check.addEventListener("click", () => busy(check, async () => {
        try {
          const r = await api("POST", `/api/v1/namespaces/${enc(id)}/tasks/import`, { text: text.value, format: format.value, dry_run: true });
          parsed = r.tasks;
          fill(preview,
            h("table", { class: "table" },
              h("thead", {}, h("tr", {}, h("th", {}, "Task"), h("th", {}, "Kind"), h("th", {}, "Area"), h("th", {}, "Milestone"), h("th", { class: "num" }, "Priority"))),
              h("tbody", {}, r.tasks.map((t) => h("tr", {},
                h("td", {}, h("div", { class: "cell-main" }, h("span", {}, t.title), t.description ? h("span", { class: "muted clamp" }, t.description) : null)),
                h("td", { class: "dim" }, t.kind || "feature"), h("td", { class: "dim" }, t.area || "shared"),
                h("td", { class: "dim" }, t.milestone || ""), h("td", { class: "num" }, t.priority || "p2"))))),
            r.warnings.length ? h("p", { class: "note" }, r.warnings.join(". ")) : null);
          go.disabled = false;
          go.replaceChildren(`Create ${r.tasks.length} task${r.tasks.length === 1 ? "" : "s"}`);
        } catch (e) { preview.replaceChildren(h("p", { class: "note bad" }, e.message)); go.disabled = true; }
      }));
      go.addEventListener("click", () => busy(go, async () => {
        try {
          const r = await api("POST", `/api/v1/namespaces/${enc(id)}/tasks/import`, { text: text.value, format: format.value });
          close();
          toast(r.proposals.length ? `${r.count} tasks added, ${r.proposals.length} wait for an area owner` : `${r.count} tasks added`);
          render();
        } catch (e) { preview.replaceChildren(h("p", { class: "note bad" }, e.message)); }
      }));
      text.addEventListener("input", () => { go.disabled = true; parsed = null; });
      return [h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), check, go];
    },
  });
}

// ---------- negotiations (moment: the ledger) ----------

async function negotiationsTab(id, plan, selected) {
  const active = (await api("GET", `/api/v1/namespaces/${enc(id)}/negotiations?status=active`)).negotiations;
  const closed = (await api("GET", `/api/v1/namespaces/${enc(id)}/negotiations?status=closed&limit=30`)).negotiations;
  const all = [...active, ...closed];
  if (!all.length) return h("div", { class: "empty" }, "No negotiations yet. They start when an agent proposes a change someone else has to agree to");
  const current = selected || all[0].id;
  const detail = (await api("GET", `/api/v1/namespaces/${enc(id)}/negotiations/${enc(current)}`)).negotiation;
  const item = (n) => h("a", { class: "neg-item", href: `#/ns/${enc(id)}/negotiations/${enc(n.id)}`, "aria-current": n.id === current ? "true" : null },
    seal(n),
    h("div", {}, h("div", {}, h("span", { class: "mono" }, n.id), " ", h("span", { class: "dim" }, n.proposer)),
      h("div", { class: "about clamp" }, (n.current_offer && n.current_offer.changes || []).map(describe).join("; "))));
  return h("div", { class: "ledger" },
    h("div", { class: "neg-list" },
      active.length ? h("div", { class: "nav-label" }, "Open") : null, active.map(item),
      closed.length ? h("div", { class: "nav-label section" }, "Closed") : null, closed.map(item)),
    negotiationPanel(detail, id, plan.your_role === "admin"));
}

function negotiationPanel(n, ns, canResolve) {
  const offers = n.offers || (n.current_offer ? [n.current_offer] : []);
  const stanceOf = (m) => (n.stances && n.stances[m] && n.stances[m].on_current_round) || null;
  const rounds = offers.map((o) => {
    const isCurrent = o.round === n.round;
    const applied = n.applied_round === o.round;
    const note = o.note && o.note.untrusted_text ? o.note : null;
    return h("div", { class: `round${applied ? " applied" : ""}` },
      h("div", { class: "round-no" }, `R${o.round}`),
      h("div", { class: "round-body" },
        h("div", {}, h("span", {}, o.author), h("span", { class: "muted" }, o.round === 1 ? " proposes" : " counters")),
        h("ul", { class: "changes" }, o.changes.map((c) => h("li", {}, describe(c)))),
        o.changes.filter((c) => c.op === "cancel_task" && c.reason).map((c) => h("div", { class: "quote" }, h("span", { class: "from" }, `reason from ${o.author}`), c.reason)),
        note ? h("div", { class: "quote" }, h("span", { class: "from" }, `note from ${note.from}`), note.untrusted_text) : null,
        isCurrent ? h("div", { class: "stances" }, n.participants.map((m) => {
          const st = stanceOf(m);
          const cls = st === "accept" ? "accept" : st === "reject" ? "reject" : "waiting";
          return h("span", { class: `tag stance ${cls}` }, h("span", { class: "dot" }), m, h("span", { class: "muted" }, st || "waiting"));
        })) : null));
  });
  const closed = n.status === "closed";
  const verdict = closed
    ? h("div", { class: "verdict" }, seal(n, 40),
      h("div", {}, h("div", {}, (OUTCOMES[n.outcome] || ["", n.outcome])[1]), h("div", { class: "dim" }, n.explanation)))
    : null;
  const resolve = !closed && canResolve
    ? h("div", { class: "section" },
      h("div", { class: "section-head" }, h("h3", { class: "section-title" }, "Settle it as an admin")),
      resolveControls(n, ns))
    : null;
  return h("article", { class: "neg-panel" },
    h("div", { class: "neg-head" }, seal(n, 48),
      h("div", {},
        h("h3", {}, n.id),
        h("div", { class: "dim" }, `${n.risk} change · proposed by ${n.proposer} · round ${n.round} of ${n.max_rounds}`),
        !closed ? h("div", { class: "muted" },
          n.awaiting.length ? `Waiting for ${n.awaiting.join(", ")} · deadline ${until(n.deadline)}` : `Deadline ${until(n.deadline)}`) : null)),
    n.rule && !closed ? h("div", { class: "rule" }, humanTimes(n.rule)) : null,
    h("div", { class: "rounds" }, rounds),
    verdict,
    resolve);
}

function resolveControls(n, ns) {
  const note = input({ placeholder: "Why (kept in the decision log)" });
  const run = async (btn, round) => busy(btn, async () => {
    try {
      await api("POST", `/api/v1/namespaces/${enc(ns)}/negotiations/${enc(n.id)}/resolve`, { round, note: note.value });
      toast(`${n.id} settled`);
      render();
    } catch (e) { toast(e.message, true); }
  });
  const rounds = Array.from({ length: n.round }, (_, i) => i + 1);
  return h("div", { class: "field" }, note,
    h("div", { class: "row" },
      rounds.map((r) => { const b = h("button", { class: "btn btn-sm", type: "button" }, `Apply round ${r}`); b.addEventListener("click", () => run(b, r)); return b; }),
      (() => { const b = h("button", { class: "btn btn-sm btn-quiet", type: "button" }, "Keep things as they are"); b.addEventListener("click", () => run(b, null)); return b; })()));
}

// ---------- members ----------

async function membersTab(id, plan) {
  const data = await api("GET", `/api/v1/namespaces/${enc(id)}/members`);
  const manage = data.can_manage;
  const rows = data.members.map((m) => {
    const role = manage ? select(ROLES, m.role, { "aria-label": `Role of ${m.id}` }) : null;
    if (role) role.addEventListener("change", async () => {
      try { await api("PUT", `/api/v1/namespaces/${enc(id)}/members/${enc(m.id)}`, { role: role.value }); toast(`${m.id} is now ${role.value}`); }
      catch (e) { toast(e.message, true); role.value = m.role; }
    });
    const remove = manage && m.id !== state.me.principal ? h("button", { class: "btn btn-quiet btn-sm btn-danger", type: "button" }, "Remove") : null;
    if (remove) remove.addEventListener("click", () => busy(remove, async () => {
      if (!confirm(`Remove ${m.id} from ${id}? Their tasks go back to the pool`)) return;
      try { await api("DELETE", `/api/v1/namespaces/${enc(id)}/members/${enc(m.id)}`); toast(`${m.id} removed`); render(); }
      catch (e) { toast(e.message, true); }
    }));
    return h("tr", {},
      h("td", {}, h("div", { class: "cell-main" }, h("span", {}, m.display_name), h("span", { class: "mono muted" }, m.id))),
      h("td", { class: "dim" }, m.human),
      h("td", {}, role || h("span", { class: "dim" }, m.role)),
      h("td", { class: "dim" }, m.areas.join(", ")),
      h("td", { class: "num" }, m.active_tasks),
      h("td", { class: "dim" }, ago(m.last_seen_at)),
      h("td", { class: "num" }, remove));
  });
  return [
    h("div", { class: "section-head" },
      h("h2", { class: "section-title" }, "Members"),
      manage ? h("div", { class: "head-actions" },
        h("button", { class: "btn btn-primary", type: "button", on: { click: () => addMemberDialog(id, plan) } }, icon("plus"), "Add member")) : null),
    h("table", { class: "table" },
      h("thead", {}, h("tr", {}, h("th", {}, "Member"), h("th", {}, "Human"), h("th", {}, "Role"), h("th", {}, "Areas"), h("th", { class: "num" }, "Active tasks"), h("th", {}, "Last seen"), h("th", {}, ""))),
      h("tbody", {}, rows)),
    data.all_namespace_access.length ? h("p", { class: "note section" },
      `Also reach every namespace: ${data.all_namespace_access.map((g) => `${g.principal} (${g.role})`).join(", ")}`) : null,
  ];
}

function addMemberDialog(id, plan) {
  const pid = input({ placeholder: "carol", autocomplete: "off", class: "input mono" });
  const name = input({ placeholder: "Carol's agent" });
  const human = input({ placeholder: "Carol" });
  const role = select(ROLES, "member");
  const areas = h("textarea", { class: "input code", placeholder: "mobile = app/**, ios/**" });
  const note = h("p", { class: "note" });
  const dlg = openDialog({
    title: `Add a member to ${plan.namespace.name}`,
    body: [h("div", { class: "form-grid" },
      field("Id", pid, "An existing account joins with its current tokens"), field("Name", name),
      field("Human", human, "The person this agent works for"), field("Role", role, ROLE_HINT),
      h("div", { class: "wide" }, field("Areas this member owns", areas, "One per line: area = path globs"))), note],
    actions: (close) => {
      const ok = h("button", { class: "btn btn-primary", type: "button" }, "Add member");
      ok.addEventListener("click", () => busy(ok, async () => {
        try {
          const parsed = areas.value.split("\n").map((l) => l.trim()).filter(Boolean).map((l) => {
            const [n, globs = ""] = l.split("=").map((x) => x.trim());
            return { name: n, paths: globs.split(",").map((g) => g.trim()).filter(Boolean) };
          });
          const r = await api("POST", `/api/v1/namespaces/${enc(id)}/members`, {
            id: pid.value.trim(), display_name: name.value.trim(), human: human.value.trim(), role: role.value, areas: parsed,
          });
          if (r.token) {
            dlg.body.replaceChildren(passCard(r.added, r.token, r.connections));
            dlg.dlg.querySelector(".dlg-foot").replaceChildren(h("button", { class: "btn btn-primary", type: "button", on: { click: () => { close(); render(); } } }, "Done"));
          } else {
            close();
            toast(`${r.added} joined ${id} with their existing tokens`);
            render();
          }
        } catch (e) { note.className = "note bad"; note.textContent = e.message; }
      }));
      return [h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), ok];
    },
  });
  pid.focus();
}

// ---------- the access pass (moment) ----------

function passCard(who, token, connections) {
  const list = (connections && connections.connections) || [];
  const shown = list.slice(0, 4);
  return h("div", { class: "pass" },
    h("div", { class: "pass-head" },
      h("div", {}, h("div", { class: "muted" }, "Access for"), h("h3", { class: "pass-title" }, who)),
      passSeal()),
    h("div", { class: "scope" },
      list.map((c) => h("div", {}, h("span", {}, c.namespace), h("span", { class: "mono dim" }, c.role))),
      connections && connections.all_namespaces ? h("div", {}, h("span", { class: "dim" }, "Every namespace created later"), h("span", { class: "mono dim" }, "")) : null),
    h("div", { class: "field" }, h("span", {}, "Token"), h("div", { class: "secret" }, h("code", {}, token), copyButton(token))),
    shown.map((c) => h("div", { class: "cmd" },
      h("div", { class: "row" }, h("span", { class: "dim" }, `Claude Code · ${c.namespace}`), copyButton(c.claude_code, "Copy command")),
      h("pre", {}, c.claude_code))),
    list.length > shown.length ? h("p", { class: "note" }, `${list.length - shown.length} more namespaces use the same token at /mcp/<namespace>`) : null,
    h("p", { class: "note" }, "Shown once. Send it privately to the person it belongs to"));
}

// ---------- connectors ----------

const KINDS = {
  github: { label: "GitHub", project: "Repository", projectHint: "owner/repo", base: "https://api.github.com", secret: "Token (fine-grained, issues read and write)" },
  gitlab: { label: "GitLab", project: "Project", projectHint: "group/project", base: "https://gitlab.com", secret: "Access token (api scope)" },
  jira: { label: "Jira", project: "Project key", projectHint: "ENG", base: "", secret: "API token" },
  linear: { label: "Linear", project: "Team key", projectHint: "ENG", base: "https://api.linear.app/graphql", secret: "API key" },
  plane: { label: "Plane", project: "Workspace and project", projectHint: "acme/WEB", base: "https://api.plane.so", secret: "API key" },
};

async function connectorsTab(id, plan) {
  const data = await api("GET", `/api/v1/namespaces/${enc(id)}/connectors`);
  const manage = data.can_manage;
  const canAttach = ["lead", "admin"].includes(plan.your_role);
  const cards = data.connectors.map((c) => connectorCard(id, plan, c, manage, canAttach));
  return [
    h("div", { class: "section-head" },
      h("h2", { class: "section-title" }, "Task trackers"),
      h("span", { class: "muted" }, "Issues from a tracker become tasks here; finished tasks are commented and closed there"),
      manage ? h("div", { class: "head-actions" },
        h("button", { class: "btn btn-primary", type: "button", on: { click: () => connectorDialog(id, plan) } }, icon("plus"), "Add connector")) : null),
    cards.length ? h("div", { class: "cards" }, cards) : h("div", { class: "empty" }, "No trackers connected"),
  ];
}

function reportText(r) {
  if (!r || !r.report) return "";
  const x = r.report;
  const parts = [`${x.fetched} fetched`];
  if (x.imported) parts.push(`${x.imported} imported`);
  if (x.updated) parts.push(`${x.updated} updated`);
  if (x.closed) parts.push(`${x.closed} closed`);
  if (x.reopened) parts.push(`${x.reopened} reopened`);
  return parts.join(", ");
}

function connectorCard(id, plan, c, manage, canAttach) {
  const k = KINDS[c.kind] || { label: c.kind };
  const cfg = c.config;
  const filter = cfg.jql ? `JQL: ${cfg.jql}` : cfg.labels && cfg.labels.length ? `labels: ${cfg.labels.join(", ")}` : "every issue";
  const writes = [cfg.writeback.comment_on_claim && "comment on claim", cfg.writeback.comment_on_done && "comment when done", cfg.writeback.close_on_done && "close when done"].filter(Boolean);
  const action = (label, iconName, fn, quiet = true) => {
    const b = h("button", { class: `btn btn-sm${quiet ? " btn-quiet" : ""}`, type: "button" }, icon(iconName), label);
    b.addEventListener("click", () => busy(b, fn));
    return b;
  };
  const base = `/api/v1/namespaces/${enc(id)}/connectors/${enc(c.id)}`;
  return h("article", { class: "card" },
    h("div", { class: "card-head" }, icon("plug", "icon-18"), h("h4", {}, c.name),
      h("span", { class: "head-actions muted" }, c.enabled ? `${k.label} · every ${dur(c.sync_every_secs)}` : "paused")),
    h("dl", { class: "kv" },
      h("dt", {}, k.project || "Project"), h("dd", { class: "mono" }, cfg.project),
      h("dt", {}, "Imports"), h("dd", {}, cfg.auto_import ? filter : "only issues attached by hand"),
      h("dt", {}, "Writes back"), h("dd", {}, writes.length ? writes.join(", ") : "nothing"),
      h("dt", {}, "Last sync"), h("dd", {}, c.last_sync_at ? `${ago(c.last_sync_at)}${c.last_result ? ` · ${reportText(c.last_result)}` : ""}` : "not yet"),
      h("dt", {}, "Linked tasks"), h("dd", { class: "mono" }, c.linked_tasks),
      c.pending_writes ? h("dt", {}, "Retrying") : null, c.pending_writes ? h("dd", { class: "err" }, `${c.pending_writes} writes to the tracker`) : null,
      c.last_error ? h("dt", {}, "Error") : null, c.last_error ? h("dd", { class: "err" }, c.last_error) : null),
    h("div", { class: "row" },
      manage ? action("Test", "play", async () => {
        const r = await api("POST", `${base}/test`);
        toast(r.ok ? r.detail : `Failed: ${r.detail}`, !r.ok);
      }) : null,
      manage ? action("Sync now", "refresh-cw", async () => {
        try { const r = await api("POST", `${base}/sync`); toast(r.skipped ? "Paused" : `Synced: ${reportText(r)}`); render(); }
        catch (e) { toast(e.message, true); render(); }
      }) : null,
      canAttach ? action("Attach issue", "link", async () => attachDialog(id, c)) : null,
      action("Activity", "list-checks", async () => activityDialog(id, c)),
      manage ? action("Edit", "pencil", async () => connectorDialog(id, plan, c)) : null));
}

function attachDialog(id, c) {
  const ref = input({ placeholder: "https://… or a key like ENG-12 or #34", autocomplete: "off" });
  const note = h("p", { class: "note" }, "The issue becomes a task in this namespace and stays linked to the tracker");
  openDialog({
    title: `Attach an issue from ${c.name}`,
    body: [field("Issue", ref), note],
    actions: (close) => {
      const ok = h("button", { class: "btn btn-primary", type: "button" }, "Attach");
      ok.addEventListener("click", () => busy(ok, async () => {
        try {
          const r = await api("POST", `/api/v1/namespaces/${enc(id)}/connectors/${enc(c.id)}/attach`, { reference: ref.value.trim() });
          close();
          toast(r.already_linked ? `Already linked as ${r.task.id}` : `Attached as ${r.task.id}`);
          render();
        } catch (e) { note.className = "note bad"; note.textContent = e.message; }
      }));
      return [h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), ok];
    },
  });
  ref.focus();
}

async function activityDialog(id, c) {
  const r = await api("GET", `/api/v1/namespaces/${enc(id)}/connectors/${enc(c.id)}`);
  const a = r.activity;
  openDialog({
    title: `${c.name}: linked tasks and writes`,
    wide: true,
    body: [
      a.links.length ? h("table", { class: "table" },
        h("thead", {}, h("tr", {}, h("th", {}, "Task"), h("th", {}, "Status"), h("th", {}, "Issue"), h("th", {}, "In the tracker"))),
        h("tbody", {}, a.links.map((l) => h("tr", {},
          h("td", {}, h("div", { class: "cell-main" }, h("span", {}, l.title), h("span", { class: "mono muted" }, l.task_id))),
          h("td", { class: "dim" }, l.task_status.replace("_", " ")),
          h("td", {}, h("a", { href: l.url, target: "_blank", rel: "noopener noreferrer" }, l.external_key, " ", icon("external-link"))),
          h("td", { class: "dim" }, l.external_open ? l.external_state || "open" : l.external_state || "closed"))))) : h("div", { class: "empty" }, "No linked tasks yet"),
      a.writes.length ? h("table", { class: "table section" },
        h("thead", {}, h("tr", {}, h("th", {}, "Write"), h("th", {}, "Task"), h("th", {}, "Issue"), h("th", { class: "num" }, "Attempts"), h("th", {}, "Result"))),
        h("tbody", {}, a.writes.map((w) => h("tr", {},
          h("td", {}, w.action), h("td", { class: "mono" }, w.task_id), h("td", { class: "mono" }, w.external_key),
          h("td", { class: "num" }, w.attempts),
          h("td", { class: w.done_at ? "ok" : w.last_error ? "err" : "dim" }, w.done_at ? `sent ${ago(w.done_at)}` : w.last_error || "queued"))))) : null,
    ],
  });
}

function connectorDialog(id, plan, existing) {
  const editing = Boolean(existing);
  const cfg = existing ? existing.config : { labels: ["agents"], auto_import: true, area_by_label: {}, bug_labels: ["bug"], writeback: { comment_on_claim: false, comment_on_done: true, close_on_done: true } };
  let kind = existing ? existing.kind : "github";
  const kindSwitch = h("div", { class: "segmented", role: "group", "aria-label": "Tracker" });
  const name = input({ value: existing ? existing.name : "", placeholder: "Shop backlog" });
  const project = input({ value: cfg.project || "", class: "input mono" });
  const baseUrl = input({ value: cfg.base_url || "", class: "input mono" });
  const labels = input({ value: (cfg.labels || []).join(", "), placeholder: "agents" });
  const jql = input({ value: cfg.jql || "", class: "input mono", placeholder: "project = ENG AND labels = agents" });
  const email = input({ value: cfg.email || "", placeholder: "ops@company.com" });
  const webUrl = input({ value: cfg.web_url || "", class: "input mono", placeholder: "https://app.plane.so" });
  const secret = input({ type: "password", autocomplete: "off", class: "input mono", placeholder: editing ? `unchanged (${existing.credential || "not set"})` : "" });
  const auto = h("input", { type: "checkbox", checked: cfg.auto_import });
  const mapping = h("textarea", { class: "input code", placeholder: "backend = backend\nui = frontend" });
  mapping.value = Object.entries(cfg.area_by_label || {}).map(([l, a]) => `${l} = ${a}`).join("\n");
  const defArea = select([["", "Shared"], ...plan.areas.map((a) => [a.name, a.name])], cfg.default_area || "");
  const bugs = input({ value: (cfg.bug_labels || []).join(", ") });
  const milestone = select([["", "None"], ...plan.milestones.map((m) => [m.id, `${m.id} · ${m.title}`])], cfg.milestone || "");
  const onClaim = h("input", { type: "checkbox", checked: cfg.writeback.comment_on_claim });
  const onDone = h("input", { type: "checkbox", checked: cfg.writeback.comment_on_done });
  const close_ = h("input", { type: "checkbox", checked: cfg.writeback.close_on_done });
  const every = select([["60", "Every minute"], ["120", "Every 2 minutes"], ["300", "Every 5 minutes"], ["900", "Every 15 minutes"], ["3600", "Every hour"]],
    String(existing ? existing.sync_every_secs : 120));
  const enabled = h("input", { type: "checkbox", checked: existing ? existing.enabled : true });
  const note = h("p", { class: "note" });
  const kindFields = h("div", { class: "form-grid" });
  const drawKind = () => {
    const k = KINDS[kind];
    kindSwitch.replaceChildren(...Object.entries(KINDS).map(([key, v]) => {
      const b = h("button", { type: "button", "aria-pressed": key === kind ? "true" : "false", disabled: editing && key !== kind }, v.label);
      b.addEventListener("click", () => { kind = key; drawKind(); });
      return b;
    }));
    baseUrl.placeholder = k.base || "https://acme.atlassian.net";
    project.placeholder = k.projectHint;
    fill(kindFields,
      field(k.project, project),
      field(kind === "jira" ? "Site address" : "API address", baseUrl, kind === "jira" ? "Required" : "Leave empty for the cloud service"),
      kind === "jira" ? field("Account e-mail", email) : null,
      kind === "jira" ? field("JQL", jql, "Optional; replaces project and labels") : null,
      kind === "plane" ? field("Web address", webUrl, "For links; empty uses the cloud app") : null,
      field(k.secret, secret, "Stored encrypted, never shown again"),
      field("Labels to import", labels, kind === "linear" || kind === "plane" ? "Any of them, comma-separated" : "All of them, comma-separated"));
  };
  drawKind();
  openDialog({
    title: editing ? `Edit ${existing.name}` : "Connect a task tracker",
    wide: true,
    body: [
      editing ? null : kindSwitch,
      h("div", { class: "form-grid" }, h("div", { class: "wide" }, field("Name", name))),
      kindFields,
      h("div", { class: "section-head" }, h("h3", { class: "section-title" }, "How issues become tasks")),
      h("div", { class: "form-grid" },
        h("label", { class: "check wide" }, auto, "Import every matching issue automatically"),
        field("Label to area", mapping, "One per line: label = area"),
        h("div", { class: "field" }, field("Area without a matching label", defArea), field("Milestone", milestone)),
        field("Labels that mean a bug", bugs),
        field("Sync", every)),
      h("div", { class: "section-head" }, h("h3", { class: "section-title" }, "What goes back to the tracker")),
      h("div", { class: "row" },
        h("label", { class: "check" }, onClaim, "Comment when an agent takes it"),
        h("label", { class: "check" }, onDone, "Comment when done"),
        h("label", { class: "check" }, close_, "Close when done")),
      editing ? h("label", { class: "check" }, enabled, "Enabled") : null,
      note,
    ],
    actions: (close) => {
      const del = editing ? h("button", { class: "btn btn-quiet btn-danger", type: "button" }, icon("trash-2"), "Delete") : null;
      if (del) del.addEventListener("click", () => busy(del, async () => {
        if (!confirm(`Delete ${existing.name}? Its tasks stay in the namespace`)) return;
        await api("DELETE", `/api/v1/namespaces/${enc(id)}/connectors/${enc(existing.id)}`);
        close();
        render();
      }));
      const ok = h("button", { class: "btn btn-primary", type: "button" }, editing ? "Save" : "Connect");
      ok.addEventListener("click", () => busy(ok, async () => {
        const list = (v) => v.split(",").map((x) => x.trim()).filter(Boolean);
        const areaMap = {};
        for (const line of mapping.value.split("\n")) {
          const [l, a] = line.split("=").map((x) => (x || "").trim());
          if (l && a) areaMap[l] = a;
        }
        const config = {
          project: project.value.trim(), base_url: baseUrl.value.trim() || undefined, labels: list(labels.value),
          jql: jql.value.trim() || undefined, email: email.value.trim() || undefined, web_url: webUrl.value.trim() || undefined,
          auto_import: auto.checked, area_by_label: areaMap, default_area: defArea.value || undefined, bug_labels: list(bugs.value),
          milestone: milestone.value || undefined,
          writeback: { comment_on_claim: onClaim.checked, comment_on_done: onDone.checked, close_on_done: close_.checked },
        };
        try {
          let c;
          if (editing) {
            c = (await api("PATCH", `/api/v1/namespaces/${enc(id)}/connectors/${enc(existing.id)}`, {
              name: name.value.trim() || undefined, config, secret: secret.value || undefined, enabled: enabled.checked, sync_every_secs: Number(every.value),
            })).connector;
          } else {
            c = (await api("POST", `/api/v1/namespaces/${enc(id)}/connectors`, {
              kind, name: name.value.trim(), config, secret: secret.value, sync_every_secs: Number(every.value),
            })).connector;
          }
          const t = await api("POST", `/api/v1/namespaces/${enc(id)}/connectors/${enc(c.id)}/test`);
          if (t.ok) { close(); toast(t.detail); render(); }
          else { note.className = "note bad"; note.textContent = `Saved, but the tracker answered: ${t.detail}`; }
        } catch (e) { note.className = "note bad"; note.textContent = e.message; }
      }));
      return [del, h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), ok];
    },
  });
}

// ---------- settings ----------

function settingsTab(id, plan) {
  const p = plan.namespace.policies;
  const name = input({ value: plan.namespace.name });
  const goal = h("textarea", { class: "input" });
  goal.value = plan.goal;
  const turn = input({ value: dur(p.turn_timeout_secs), class: "input mono" });
  const esc = input({ value: dur(p.escalation_timeout_secs), class: "input mono" });
  const lease = input({ value: dur(p.lease_secs), class: "input mono" });
  const rounds = input({ value: String(p.max_rounds), type: "number", min: "1", max: "20", class: "input mono" });
  const note = h("p", { class: "note" });
  const save = h("button", { class: "btn btn-primary", type: "button" }, "Save");
  save.addEventListener("click", () => busy(save, async () => {
    try {
      const body = { name: name.value.trim(), turn_timeout: turn.value, escalation_timeout: esc.value, lease: lease.value, max_rounds: Number(rounds.value) };
      if (goal.value !== plan.goal) body.goal = goal.value;
      await api("PATCH", `/api/v1/namespaces/${enc(id)}`, body);
      await refreshMe();
      toast("Saved");
      render();
    } catch (e) { note.className = "note bad"; note.textContent = e.message; }
  }));
  return h("div", { class: "form-grid" },
    field("Name", name), h("div", {}),
    h("div", { class: "wide" }, field("Goal", goal, "Agents see it in every sync. Changing it here skips the negotiation")),
    field("Time to answer a proposal", turn, "e.g. 30m"), field("Time for humans on escalations", esc, "e.g. 24h"),
    field("Lease on a claimed task", lease, "A silent holder loses the task after this"), field("Rounds per negotiation", rounds),
    h("div", { class: "wide row" }, save, note));
}

// ---------- people and tokens ----------

async function accessPage() {
  const data = await api("GET", "/api/v1/principals");
  const canCreate = data.is_admin || data.namespaces_you_manage.length > 0;
  const head = h("header", { class: "page-head" },
    h("div", {}, h("h1", { class: "page-title" }, "People and tokens"),
      h("div", { class: "page-sub" }, data.is_admin ? "Every account of the hub" : "You and the accounts you manage")),
    h("div", { class: "head-actions" },
      canCreate ? h("button", { class: "btn btn-primary", type: "button", on: { click: () => newAccountDialog(data) } }, icon("plus"), "New account") : null));
  const grantTag = (g) => h("span", { class: "tag" }, g.namespace === "*" ? "All namespaces" : g.namespace, h("span", { class: "mono muted" }, g.role));
  const rows = data.principals.map((p) => {
    const active = p.tokens.filter((t) => !t.revoked_at);
    const last = active.map((t) => t.last_used_at).filter(Boolean).sort().pop();
    return h("tr", { class: "link", on: { click: () => accountDialog(p.id, data) } },
      h("td", {}, h("div", { class: "cell-main" }, h("span", { class: p.active ? "" : "muted" }, p.display_name), h("span", { class: "mono muted" }, p.active ? p.id : `${p.id} · deactivated`))),
      h("td", { class: "dim" }, p.human),
      h("td", {}, h("div", { class: "row" }, p.is_admin ? h("span", { class: "tag" }, icon("shield"), "hub admin") : null, p.grants.map(grantTag))),
      h("td", { class: "num" }, active.length),
      h("td", { class: "dim" }, ago(last)));
  });
  return {
    node: h("div", {}, head, h("div", { class: "content" },
      h("table", { class: "table" },
        h("thead", {}, h("tr", {}, h("th", {}, "Account"), h("th", {}, "Human"), h("th", {}, "Access"), h("th", { class: "num" }, "Tokens"), h("th", {}, "Last used"))),
        h("tbody", {}, rows)))),
  };
}

function namespaceChoices(data, withAll) {
  const own = state.me.namespaces.filter((n) => data.is_admin || data.namespaces_you_manage.includes(n.id)).map((n) => [n.id, n.name]);
  // A specific namespace comes first: access to everything is never the default.
  return withAll && data.is_admin ? [...own, ["*", "All namespaces, including future ones"]] : own;
}

function grantRows(data) {
  const box = h("div", { class: "field" });
  const rows = [];
  const add = () => {
    const ns = select(namespaceChoices(data, true));
    const role = select(ROLES, "member");
    const rm = h("button", { class: "btn btn-quiet icon-btn", type: "button", "aria-label": "Remove" }, icon("x"));
    const row = h("div", { class: "row" }, ns, role, rm);
    rows.push({ ns, role, row });
    rm.addEventListener("click", () => { row.remove(); rows.splice(rows.findIndex((r) => r.row === row), 1); });
    list.append(row);
  };
  const list = h("div", { class: "field" });
  box.append(h("span", {}, "Access"), list,
    h("div", {}, h("button", { class: "btn btn-quiet btn-sm", type: "button", on: { click: add } }, icon("plus"), "Add access")),
    h("small", {}, ROLE_HINT));
  add();
  return { node: box, value: () => rows.map((r) => ({ namespace: r.ns.value, role: r.role.value })) };
}

function newAccountDialog(data) {
  const pid = input({ placeholder: "carol", autocomplete: "off", class: "input mono" });
  const name = input({ placeholder: "Carol's agent" });
  const human = input({ placeholder: "Carol" });
  const admin = h("input", { type: "checkbox" });
  const label = input({ placeholder: "laptop", value: "first token" });
  const grants = grantRows(data);
  const note = h("p", { class: "note" });
  const dlg = openDialog({
    title: "New account",
    body: [h("div", { class: "form-grid" },
      field("Id", pid), field("Name", name), field("Human", human, "The person this account belongs to"), field("Token label", label),
      h("div", { class: "wide" }, grants.node),
      data.is_admin ? h("label", { class: "check wide" }, admin, "Hub admin: manages every namespace, account and connector") : null), note],
    actions: (close) => {
      const ok = h("button", { class: "btn btn-primary", type: "button" }, "Create and issue a token");
      ok.addEventListener("click", () => busy(ok, async () => {
        try {
          const r = await api("POST", "/api/v1/principals", {
            id: pid.value.trim(), display_name: name.value.trim(), human: human.value.trim(), is_admin: admin.checked,
            grants: grants.value(), label: label.value.trim(),
          });
          dlg.body.replaceChildren(passCard(r.principal.display_name, r.token, r.connections));
          dlg.dlg.querySelector(".dlg-foot").replaceChildren(h("button", { class: "btn btn-primary", type: "button", on: { click: () => { close(); render(); } } }, "Done"));
        } catch (e) { note.className = "note bad"; note.textContent = e.message; }
      }));
      return [h("button", { class: "btn btn-quiet", type: "button", on: { click: close } }, "Cancel"), ok];
    },
  });
  pid.focus();
}

async function accountDialog(pid, data) {
  const p = (await api("GET", `/api/v1/principals/${enc(pid)}`)).principal;
  const isSelf = p.id === state.me.principal;
  const manageable = (ns) => data.is_admin || (ns !== "*" && data.namespaces_you_manage.includes(ns));
  const body = h("div", { class: "dlg-body-inner" });
  const dlg = openDialog({ title: p.display_name, wide: true, body });
  const draw = (pp) => {
    const grants = h("table", { class: "table" },
      h("thead", {}, h("tr", {}, h("th", {}, "Namespace"), h("th", {}, "Role"), h("th", {}, ""))),
      h("tbody", {}, pp.grants.map((g) => {
        const can = manageable(g.namespace) && !isSelf;
        const role = can ? select(ROLES, g.role) : h("span", { class: "dim" }, g.role);
        if (can) role.addEventListener("change", async () => {
          try { draw((await api("PUT", `/api/v1/principals/${enc(pp.id)}/grants/${enc(g.namespace)}`, { role: role.value })).principal); }
          catch (e) { toast(e.message, true); }
        });
        const rm = can ? h("button", { class: "btn btn-quiet btn-sm btn-danger", type: "button" }, "Remove") : null;
        if (rm) rm.addEventListener("click", () => busy(rm, async () => {
          try { draw((await api("DELETE", `/api/v1/principals/${enc(pp.id)}/grants/${enc(g.namespace)}`)).principal); }
          catch (e) { toast(e.message, true); }
        }));
        return h("tr", {}, h("td", {}, g.namespace === "*" ? "All namespaces" : g.namespace), h("td", {}, role), h("td", { class: "num" }, rm));
      })));
    const addNs = select(namespaceChoices(data, true));
    const addRole = select(ROLES, "member");
    const addBtn = h("button", { class: "btn btn-sm", type: "button" }, icon("plus"), "Give access");
    addBtn.addEventListener("click", () => busy(addBtn, async () => {
      try { draw((await api("PUT", `/api/v1/principals/${enc(pp.id)}/grants/${enc(addNs.value)}`, { role: addRole.value })).principal); }
      catch (e) { toast(e.message, true); }
    }));
    const tokens = h("table", { class: "table" },
      h("thead", {}, h("tr", {}, h("th", {}, "Token"), h("th", {}, "Label"), h("th", {}, "Created"), h("th", {}, "Last used"), h("th", {}, ""))),
      h("tbody", {}, pp.tokens.map((t) => {
        const rv = !t.revoked_at ? h("button", { class: "btn btn-quiet btn-sm btn-danger", type: "button" }, "Revoke") : h("span", { class: "muted" }, `revoked ${ago(t.revoked_at)}`);
        if (!t.revoked_at) rv.addEventListener("click", () => busy(rv, async () => {
          if (!confirm(`Revoke ${t.id}? Agents using it lose access right away`)) return;
          try { await api("DELETE", `/api/v1/tokens/${enc(t.id)}`); draw((await api("GET", `/api/v1/principals/${enc(pp.id)}`)).principal); }
          catch (e) { toast(e.message, true); }
        }));
        return h("tr", {}, h("td", { class: "mono" }, t.id), h("td", { class: "dim" }, t.label), h("td", { class: "dim" }, ago(t.created_at)),
          h("td", { class: "dim" }, ago(t.last_used_at)), h("td", { class: "num" }, rv));
      })));
    const label = input({ placeholder: "Label, e.g. work laptop" });
    const issue = h("button", { class: "btn btn-primary btn-sm", type: "button" }, icon("key-round"), "Issue a token");
    issue.addEventListener("click", () => busy(issue, async () => {
      try {
        const r = await api("POST", `/api/v1/principals/${enc(pp.id)}/tokens`, { label: label.value.trim() });
        body.replaceChildren(passCard(pp.display_name, r.token, r.connections),
          h("div", { class: "row" }, h("button", { class: "btn", type: "button", on: { click: async () => draw((await api("GET", `/api/v1/principals/${enc(pp.id)}`)).principal) } }, icon("arrow-left"), "Back to the account")));
      } catch (e) { toast(e.message, true); }
    }));
    const deactivate = !isSelf && pp.active ? h("button", { class: "btn btn-quiet btn-danger btn-sm", type: "button" }, "Deactivate account") : null;
    if (deactivate) deactivate.addEventListener("click", () => busy(deactivate, async () => {
      if (!confirm(`Deactivate ${pp.id}? All tokens stop working and their tasks go back to the pool`)) return;
      try { await api("PATCH", `/api/v1/principals/${enc(pp.id)}`, { active: false }); dlg.close(); render(); }
      catch (e) { toast(e.message, true); }
    }));
    fill(body,
      h("div", { class: "row" }, h("span", { class: "mono muted" }, pp.id), pp.human ? h("span", { class: "dim" }, `works for ${pp.human}`) : null,
        pp.is_admin ? h("span", { class: "tag" }, icon("shield"), "hub admin") : null, pp.active ? null : h("span", { class: "tag" }, "deactivated")),
      h("div", { class: "section" }, h("div", { class: "section-head" }, h("h3", { class: "section-title" }, "Access")), grants,
        !isSelf ? h("div", { class: "row section" }, addNs, addRole, addBtn) : null),
      h("div", { class: "section" }, h("div", { class: "section-head" }, h("h3", { class: "section-title" }, "Tokens")), tokens,
        pp.active ? h("div", { class: "row section" }, label, issue) : null),
      deactivate ? h("div", { class: "section" }, deactivate) : null);
  };
  draw(p);
}

boot();
