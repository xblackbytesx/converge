// Converge Playground (M8). Plain JavaScript, no library: the CSP allows
// only scripts from this server. Conversations live in sessionStorage,
// never on the server. Everything from an answer goes into the page with
// textContent, never as HTML.
(function () {
  "use strict";

  var root = document.getElementById("pg");
  if (!root) return;
  var data = JSON.parse(root.getAttribute("data-playground"));
  var STORE = "converge.playground.v1";
  var MAX = 3;
  var models = {}, personas = {};
  data.models.forEach(function (m) { models[m.id] = m; });
  data.personas.forEach(function (p) { personas[p.id] = p; });

  // Per column, never stored: the running request and live DOM nodes.
  var live = [];
  var state = load();

  function column(target) {
    return { target: target || "", system: "", params: {}, messages: [], send: true, settings: false, draft: null };
  }

  function firstTarget() {
    if (data.personas.length) return data.personas[0].id;
    return data.models.length ? data.models[0].id : "";
  }

  function load() {
    var s = null;
    try { s = JSON.parse(sessionStorage.getItem(STORE)); } catch (e) { s = null; }
    if (!s || !Array.isArray(s.columns) || !s.columns.length) s = { columns: [column(data.pick || firstTarget())] };
    else if (data.pick && !s.columns.some(function (c) { return c.target === data.pick; })) {
      if (s.columns.length < MAX) s.columns.push(column(data.pick));
      else s.columns[s.columns.length - 1] = column(data.pick);
    }
    // A pending answer from before a reload will never finish.
    s.columns.forEach(function (c) {
      c.messages.forEach(function (m) { if (m.pending) { m.pending = false; m.error = m.error || "Interrupted by a reload."; } });
    });
    if (data.pick && window.history.replaceState) window.history.replaceState(null, "", "/playground");
    return s;
  }

  function save() {
    try { sessionStorage.setItem(STORE, JSON.stringify(state)); } catch (e) { /* full or blocked: keep going */ }
  }

  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined && text !== null) e.textContent = text;
    return e;
  }

  function button(text, onClick, cls) {
    var b = el("button", "small" + (cls ? " " + cls : ""), text);
    b.type = "button";
    b.addEventListener("click", onClick);
    return b;
  }

  function running() { return live.some(function (l) { return l && l.ctrl; }); }

  function ms(n) {
    if (n === null || n === undefined) return "-";
    return n < 1000 ? Math.round(n) + " ms" : (n / 1000).toFixed(2) + " s";
  }

  function num(n) { return (n || 0).toLocaleString(); }

  // ---- Rendering ----------------------------------------------------

  function render() {
    root.textContent = "";
    live = state.columns.map(function (c, i) { return { ctrl: live[i] && live[i].ctrl, nodes: {} }; });
    state.columns.forEach(function (c, i) { root.appendChild(renderColumn(c, i)); });
    document.getElementById("pg-add").disabled = state.columns.length >= MAX;
    var busy = running();
    document.getElementById("pg-send").disabled = busy;
    document.getElementById("pg-stop").hidden = !busy;
  }

  function targetSelect(c, i) {
    var s = el("select");
    s.setAttribute("aria-label", "Model or persona");
    var known = models[c.target] || personas[c.target];
    if (!known) {
      var o = el("option", null, c.target ? c.target + " (not available)" : "Pick a model or persona");
      o.value = c.target;
      s.appendChild(o);
    }
    [["Personas", data.personas, function (p) { return p.title + " (" + p.id + ")"; }],
     ["Models", data.models, function (m) { return m.id; }]].forEach(function (g) {
      if (!g[1].length) return;
      var og = el("optgroup");
      og.label = g[0];
      g[1].forEach(function (x) {
        var o = el("option", null, g[2](x));
        o.value = x.id;
        og.appendChild(o);
      });
      s.appendChild(og);
    });
    s.value = c.target;
    s.addEventListener("change", function () {
      c.target = s.value;
      c.draft = null;
      save();
      render();
    });
    return s;
  }

  function renderColumn(c, i) {
    var col = el("section", "card pg-col");
    var head = el("div", "pg-head");
    head.appendChild(targetSelect(c, i));
    var send = el("label", "check");
    var box = el("input");
    box.type = "checkbox";
    box.checked = c.send;
    box.addEventListener("change", function () { c.send = box.checked; save(); });
    send.appendChild(box);
    send.appendChild(document.createTextNode(" send"));
    head.appendChild(send);
    col.appendChild(head);

    var p = personas[c.target];
    var about = el("p", "hint pg-about");
    if (p) about.textContent = "Persona" + (p.model ? ", on " + (c.draft ? modelName(c.draft.model) + " (draft)" : p.model) : "");
    else if (models[c.target]) about.textContent = "Model on " + models[c.target].connection;
    col.appendChild(about);

    var tools = el("div", "row pg-tools");
    tools.appendChild(button(c.settings ? "Hide settings" : "Settings", function () { c.settings = !c.settings; save(); render(); }));
    if (data.admin && p) {
      tools.appendChild(c.draft
        ? button("Drop draft", function () { c.draft = null; save(); render(); })
        : button("Draft", function () { startDraft(c, p); }));
    }
    if (state.columns.length < MAX) tools.appendChild(button("Duplicate", function () { duplicate(i); }));
    tools.appendChild(button("Clear", function () { c.messages = []; save(); render(); }));
    if (state.columns.length > 1) tools.appendChild(button("Remove", function () { stop(i); state.columns.splice(i, 1); save(); render(); }, "danger"));
    col.appendChild(tools);

    // While answers stream, columns keep their place and their target.
    if (running()) {
      tools.querySelectorAll("button").forEach(function (b) { if (b.textContent !== "Settings" && b.textContent !== "Hide settings") b.disabled = true; });
      head.querySelector("select").disabled = true;
    }
    if (c.settings) col.appendChild(settingsPanel(c));
    if (c.draft) col.appendChild(draftPanel(c, p));

    var list = el("div", "pg-messages");
    c.messages.forEach(function (m, k) { list.appendChild(renderMessage(m, i, k)); });
    if (!c.messages.length) list.appendChild(el("p", "muted", "No messages yet."));
    col.appendChild(list);
    col.appendChild(totals(c));
    return col;
  }

  function settingsPanel(c) {
    var box = el("div", "pg-panel stack");
    var lab = el("label", null, "System prompt ");
    lab.appendChild(el("span", "hint", "sent as the client's own; a persona's prompt still applies"));
    var ta = el("textarea");
    ta.rows = 3;
    ta.value = c.system;
    ta.addEventListener("input", function () { c.system = ta.value; save(); });
    lab.appendChild(ta);
    box.appendChild(lab);
    var grid = el("div", "pg-params");
    data.params.forEach(function (pr) {
      var l = el("label", null, pr.key);
      var inp = el("input");
      inp.type = "text";
      inp.placeholder = pr.hint;
      inp.value = c.params[pr.key] || "";
      inp.addEventListener("input", function () {
        if (inp.value.trim()) c.params[pr.key] = inp.value; else delete c.params[pr.key];
        save();
      });
      l.appendChild(inp);
      grid.appendChild(l);
    });
    box.appendChild(grid);
    box.appendChild(el("p", "hint", "These are the client's values: a persona's locks still win, its defaults fill what is empty."));
    return box;
  }

  function modelName(uuid) {
    var m = data.models.filter(function (x) { return x.uuid === uuid; })[0];
    return m ? m.id : "(model off)";
  }

  function startDraft(c, p) {
    fetch("/playground/persona/" + p.uuid, { credentials: "same-origin" })
      .then(function (r) { if (!r.ok) throw new Error("status " + r.status); return r.json(); })
      .then(function (d) { c.draft = d; c.settings = false; save(); render(); })
      .catch(function (e) { alert("Could not load the persona: " + e.message); });
  }

  function draftPanel(c, p) {
    var d = c.draft;
    var box = el("div", "pg-panel pg-draft stack");
    box.appendChild(el("p", "hint", "Draft of " + p.id + ": requests from this column use it instead of what is saved."));
    var ml = el("label", null, "Model");
    var ms_ = el("select");
    data.models.forEach(function (m) {
      var o = el("option", null, m.id);
      o.value = m.uuid;
      ms_.appendChild(o);
    });
    ms_.value = d.model;
    ms_.addEventListener("change", function () { d.model = ms_.value; save(); render(); });
    ml.appendChild(ms_);
    box.appendChild(ml);
    var pl = el("label", null, "System prompt");
    var ta = el("textarea", "prompt");
    ta.rows = 8;
    ta.value = d.prompt;
    ta.addEventListener("input", function () { d.prompt = ta.value; save(); });
    pl.appendChild(ta);
    box.appendChild(pl);
    var mode = el("select");
    [["prepend", "Keep the client's prompt, after this one"], ["replace", "Only this prompt counts"]].forEach(function (x) {
      var o = el("option", null, x[1]);
      o.value = x[0];
      mode.appendChild(o);
    });
    mode.value = d.mode;
    mode.addEventListener("change", function () { d.mode = mode.value; save(); });
    var modeL = el("label", null, "When the client sends its own system prompt");
    modeL.appendChild(mode);
    box.appendChild(modeL);

    var table = el("table", "list params");
    data.params.forEach(function (pr) {
      var tr = el("tr");
      tr.appendChild(el("td")).appendChild(el("code", null, pr.key));
      var sel = el("select");
      ["off", "default", "lock"].forEach(function (v) { var o = el("option", null, v); o.value = v; sel.appendChild(o); });
      sel.value = d.params[pr.key + "_mode"] || "off";
      sel.setAttribute("aria-label", pr.key);
      sel.addEventListener("change", function () {
        if (sel.value === "off") delete d.params[pr.key + "_mode"]; else d.params[pr.key + "_mode"] = sel.value;
        save();
      });
      tr.appendChild(el("td")).appendChild(sel);
      var inp = el("input");
      inp.type = "text";
      inp.placeholder = pr.hint;
      inp.value = d.params[pr.key + "_value"] || "";
      inp.setAttribute("aria-label", pr.key + " value");
      inp.addEventListener("input", function () { d.params[pr.key + "_value"] = inp.value; save(); });
      tr.appendChild(el("td")).appendChild(inp);
      table.appendChild(tr);
    });
    box.appendChild(table);

    var row = el("div", "row");
    var note = el("span", "hint");
    row.appendChild(button("Save to persona", function () {
      note.textContent = "Saving...";
      fetch("/playground/persona/" + p.uuid, {
        method: "POST", credentials: "same-origin",
        headers: { "Content-Type": "application/json" }, body: JSON.stringify(d)
      }).then(function (r) {
        return r.json().catch(function () { return {}; }).then(function (j) {
          if (!r.ok) throw new Error((j.error && j.error.message) || "status " + r.status);
          p.model = modelName(d.model);
          note.textContent = "Saved. It is in the persona's history, where it can be undone.";
        });
      }).catch(function (e) { note.textContent = "Not saved: " + e.message; });
    }, "primary"));
    row.appendChild(button("Back to saved", function () { startDraft(c, p); }));
    row.appendChild(note);
    box.appendChild(row);
    return box;
  }

  function renderMessage(m, i, k) {
    var box = el("div", "pg-msg " + m.role);
    if (m.role === "user") {
      box.appendChild(el("div", "pg-text", m.content));
      return box;
    }
    var nodes = { reasoning: null, text: el("div", "pg-text", m.content), diag: el("div", "pg-diag") };
    if (m.reasoning) {
      var det = el("details", "pg-reasoning");
      det.appendChild(el("summary", null, "Reasoning"));
      nodes.reasoning = el("div", "pg-text", m.reasoning);
      det.appendChild(nodes.reasoning);
      box.appendChild(det);
    } else {
      nodes.reasoningBox = box;
    }
    box.appendChild(nodes.text);
    if (m.pending && !m.content) nodes.text.appendChild(el("span", "muted", "Waiting..."));
    box.appendChild(nodes.diag);
    fillDiag(nodes.diag, m);
    if (k === state.columns[i].messages.length - 1) live[i].nodes = nodes;
    return box;
  }

  function fillDiag(box, m) {
    box.textContent = "";
    if (m.error) box.appendChild(el("p", "error", m.error));
    if (m.stopped) box.appendChild(el("p", "hint", "Stopped."));
    var parts = [];
    var t = m.timing || {};
    if (t.ttfb !== undefined) parts.push("first byte " + ms(t.ttfb));
    if (t.ttft !== undefined && t.ttft !== null) parts.push("first token " + ms(t.ttft));
    if (t.total !== undefined) parts.push("total " + ms(t.total));
    if (t.tps) parts.push(t.tps.toFixed(1) + " tok/s");
    var u = m.usage;
    if (u) {
      var cached = u.prompt_tokens_details && u.prompt_tokens_details.cached_tokens;
      var reason = u.completion_tokens_details && u.completion_tokens_details.reasoning_tokens;
      parts.push(num(u.prompt_tokens) + " in" + (cached ? " (" + num(cached) + " cached)" : "") + " / " +
        num(u.completion_tokens) + " out" + (reason ? " (" + num(reason) + " reasoning)" : ""));
    }
    if (m.finish) parts.push(m.finish);
    if (m.status && m.status >= 400) parts.push("status " + m.status);
    if (parts.length) box.appendChild(el("p", "hint", parts.join(" · ")));
    var info = m.info;
    if (info) {
      var who = [];
      if (info.persona) who.push("persona " + info.persona);
      if (info.model) who.push((info.persona ? "on " : "model ") + info.model);
      if (info.upstream_model) who.push("upstream " + info.upstream_model + " at " + info.connection);
      if (who.length) box.appendChild(el("p", "hint", who.join(", ")));
      if (info.dropped && info.dropped.length) box.appendChild(el("p", "hint warn-text", "Dropped for this upstream: " + info.dropped.join(", ")));
      if (info.sent) {
        var det = el("details");
        det.appendChild(el("summary", "hint", "Request as sent upstream"));
        det.appendChild(el("pre", "prompt-view", JSON.stringify(info.sent, null, 2)));
        box.appendChild(det);
      }
    }
  }

  function totals(c) {
    var n = 0, inT = 0, outT = 0, tps = [];
    c.messages.forEach(function (m) {
      if (m.role !== "assistant") return;
      n++;
      if (m.usage) { inT += m.usage.prompt_tokens || 0; outT += m.usage.completion_tokens || 0; }
      if (m.timing && m.timing.tps) tps.push(m.timing.tps);
    });
    var text = n ? n + (n === 1 ? " answer" : " answers") + " · " + num(inT) + " in / " + num(outT) + " out" : "";
    if (tps.length) text += " · " + (tps.reduce(function (a, b) { return a + b; }, 0) / tps.length).toFixed(1) + " tok/s on average";
    return el("p", "hint pg-totals", text);
  }

  // ---- Columns ------------------------------------------------------

  function duplicate(i) {
    if (state.columns.length >= MAX) return;
    var copy = JSON.parse(JSON.stringify(state.columns[i]));
    copy.messages.forEach(function (m) { m.pending = false; });
    state.columns.splice(i + 1, 0, copy);
    save();
    render();
  }

  function stop(i) {
    if (live[i] && live[i].ctrl) live[i].ctrl.abort();
  }

  document.getElementById("pg-add").addEventListener("click", function () {
    if (state.columns.length >= MAX) return;
    var last = state.columns[state.columns.length - 1];
    state.columns.push(column(last ? last.target : firstTarget()));
    save();
    render();
  });
  document.getElementById("pg-clear").addEventListener("click", function () {
    if (running()) return;
    state.columns.forEach(function (c) { c.messages = []; });
    save();
    render();
  });
  document.getElementById("pg-stop").addEventListener("click", function () {
    state.columns.forEach(function (c, i) { stop(i); });
  });

  // ---- Sending ------------------------------------------------------

  var input = document.getElementById("pg-input");
  input.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      document.getElementById("pg-compose").requestSubmit();
    }
  });
  document.getElementById("pg-compose").addEventListener("submit", function (e) {
    e.preventDefault();
    var text = input.value.trim();
    if (!text || running()) return;
    var targets = state.columns.filter(function (c) { return c.send && c.target; });
    if (!targets.length) return;
    targets.forEach(function (c) {
      c.messages.push({ role: "user", content: text });
      c.messages.push({ role: "assistant", content: "", reasoning: "", pending: true });
    });
    input.value = "";
    save();
    render();
    state.columns.forEach(function (c, i) { if (c.send && c.target) run(c, i); });
  });

  function requestBody(c) {
    // Failed or stopped answers are left out, as a client would retry.
    var history = c.messages.slice(0, -1).filter(function (m) {
      return m.role === "user" || (m.content && !m.error);
    }).map(function (m) { return { role: m.role, content: m.content }; });
    var body = { target: c.target, messages: history, system: c.system, params: c.params };
    if (c.draft) body.draft = c.draft;
    return JSON.stringify(body);
  }

  function update(i, m) {
    var n = live[i] && live[i].nodes;
    if (!n || !n.text) return;
    n.text.textContent = m.content;
    if (m.reasoning) {
      if (!n.reasoning && n.reasoningBox) {
        var det = el("details", "pg-reasoning");
        det.appendChild(el("summary", null, "Reasoning"));
        n.reasoning = el("div", "pg-text");
        det.appendChild(n.reasoning);
        n.reasoningBox.insertBefore(det, n.text);
      }
      if (n.reasoning) n.reasoning.textContent = m.reasoning;
    }
    fillDiag(n.diag, m);
  }

  function finish(i, m) {
    m.pending = false;
    live[i].ctrl = null;
    save();
    render();
  }

  function run(c, i) {
    var m = c.messages[c.messages.length - 1];
    var ctrl = new AbortController();
    live[i].ctrl = ctrl;
    render();
    var t0 = performance.now();
    var timing = { ttft: null };
    m.timing = timing;
    fetch("/playground/send", {
      method: "POST", credentials: "same-origin", signal: ctrl.signal,
      headers: { "Content-Type": "application/json" }, body: requestBody(c)
    }).then(function (resp) {
      var type = resp.headers.get("content-type") || "";
      m.status = resp.status;
      if (type.indexOf("text/event-stream") !== 0) {
        timing.ttfb = performance.now() - t0;
        return resp.text().then(function (t) {
          var j = null;
          try { j = JSON.parse(t); } catch (e) { j = null; }
          if (!j) m.error = type.indexOf("text/html") === 0 ? "You are logged out: reload the page." : "Status " + resp.status;
          else {
            m.info = j.converge || null;
            var a = j.answer || {};
            var ch = a.choices && a.choices[0];
            if (resp.ok && ch) {
              // An upstream that answered whole although asked to stream.
              var msg = ch.message || {};
              m.content = msg.content || "";
              m.reasoning = msg.reasoning_content || msg.reasoning || "";
              m.finish = ch.finish_reason;
              m.usage = a.usage;
              timing.ttft = timing.ttfb;
            } else {
              var err = a.error || j.error || {};
              m.error = err.message || ("Status " + resp.status);
            }
            if (j.status) m.status = j.status;
          }
          timing.total = performance.now() - t0;
        });
      }
      return read(resp, m, i, t0);
    }).catch(function (e) {
      if (e.name === "AbortError") m.stopped = true;
      else m.error = m.error || "The connection broke off.";
      timing.total = performance.now() - t0;
    }).then(function () { finish(i, m); });
  }

  function read(resp, m, i, t0) {
    var reader = resp.body.getReader();
    var decoder = new TextDecoder();
    var buf = "";
    var timing = m.timing;
    var queued = false;
    function paint() {
      if (queued) return;
      queued = true;
      requestAnimationFrame(function () { queued = false; update(i, m); });
    }
    function onEvent(block) {
      var type = "message", lines = [];
      block.split("\n").forEach(function (line) {
        if (line.indexOf("event:") === 0) type = line.slice(6).trim();
        else if (line.indexOf("data:") === 0) lines.push(line.slice(5).replace(/^ /, ""));
      });
      var payload = lines.join("\n");
      if (!payload) return;
      if (type === "converge") { try { m.info = JSON.parse(payload); } catch (e) { /* ignore */ } return; }
      if (payload === "[DONE]") return;
      var chunk;
      try { chunk = JSON.parse(payload); } catch (e) { return; }
      if (chunk.error) { m.error = chunk.error.message || "The upstream reported an error."; return; }
      var ch = chunk.choices && chunk.choices[0];
      if (ch) {
        var d = ch.delta || {};
        var r = d.reasoning_content || d.reasoning;
        if ((d.content || r) && timing.ttft === null) timing.ttft = performance.now() - t0;
        if (d.content) m.content += d.content;
        if (r) m.reasoning = (m.reasoning || "") + r;
        if (ch.finish_reason) m.finish = ch.finish_reason;
      }
      if (chunk.usage) m.usage = chunk.usage;
    }
    function pump() {
      return reader.read().then(function (r) {
        if (timing.ttfb === undefined) timing.ttfb = performance.now() - t0;
        if (r.done) {
          if (buf.trim()) onEvent(buf);
          timing.total = performance.now() - t0;
          var out = m.usage && m.usage.completion_tokens;
          var gen = timing.total - (timing.ttft || timing.total);
          if (out && gen > 0) timing.tps = out / (gen / 1000);
          return;
        }
        buf += decoder.decode(r.value, { stream: true }).replace(/\r\n/g, "\n");
        var at;
        while ((at = buf.indexOf("\n\n")) >= 0) {
          onEvent(buf.slice(0, at));
          buf = buf.slice(at + 2);
        }
        paint();
        return pump();
      });
    }
    return pump();
  }

  render();
})();
